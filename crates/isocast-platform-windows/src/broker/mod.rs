//! WGC's process-lifetime COM/RPC associations die with this capture's helper.
//! The parent never activates WGC. Private pipes carry only bounded metadata;
//! textures stay GPU resident, with one acknowledged lease per immutable frame.

mod helper;
mod process;
mod wire;

use std::collections::VecDeque;
use std::path::Path;
use std::process::ChildStdout;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use isocast_engine::{CaptureFormat, Time, VideoSink};
use parking_lot::Mutex;
use windows::Win32::Graphics::Direct3D11::{D3D11_TEXTURE2D_DESC, ID3D11Fence, ID3D11Texture2D};

use crate::native::{Apartment, SharedHandle};
use crate::{CaptureStats, CaptureTarget, Frame, GraphicsDevice};
use process::Process;
use wire::{Init, MAX_LEASES, MAX_SURFACES, Message, Picture};

pub use helper::capture_helper_main;

/// Native WGC counters and the additional, separately counted broker stage.
#[derive(Clone, Copy, Default, Debug)]
pub struct IsolatedCaptureStats {
    pub capture: CaptureStats,
    /// Completed GPU pictures written to the pipe by the helper.
    pub published: u64,
    /// Pictures discarded by the broker's bounded queue/lease limit.
    pub backpressure_dropped: u64,
    /// Pictures handed to the parent sink (not an FPS-based estimate).
    pub delivered: u64,
}

struct Control {
    process: Process,
    stopping: AtomicBool,
    failure: Mutex<Option<String>>,
    stats: Mutex<IsolatedCaptureStats>,
}

impl Control {
    fn fail(&self, message: String) {
        if !self.stopping.load(Ordering::Acquire) {
            let mut failure = self.failure.lock();
            if failure.is_none() {
                *failure = Some(message);
            }
        }
        self.process.kill();
    }
}

struct Releases {
    commands: mpsc::SyncSender<Message>,
    control: Weak<Control>,
    outstanding: AtomicUsize,
}

struct RemoteLease {
    sequence: u64,
    releases: Weak<Releases>,
}

impl Drop for RemoteLease {
    fn drop(&mut self) {
        let Some(releases) = self.releases.upgrade() else {
            return;
        };
        releases.outstanding.fetch_sub(1, Ordering::AcqRel);
        let Some(control) = releases.control.upgrade() else {
            return;
        };
        if control.stopping.load(Ordering::Acquire) {
            return;
        }
        // At most 32 outstanding pictures can produce Acks. Never block a GPU
        // completion callback on a pipe, or silently abandon a live lease.
        if releases
            .commands
            .try_send(Message::Ack(self.sequence))
            .is_err()
        {
            control.fail("capture helper acknowledgement channel failed".into());
        }
    }
}

/// One helper per capture. Stop terminates its job before joining pipe workers;
/// neither WGC nor COM teardown can block the parent. Sink callbacks must, as
/// with other captures, return before a stop on another thread can complete.
pub struct IsolatedCapture {
    control: Arc<Control>,
    releases: Arc<Releases>,
    sender: Option<JoinHandle<()>>,
    receiver: Option<JoinHandle<()>>,
}

impl IsolatedCapture {
    pub fn stats(&self) -> IsolatedCaptureStats {
        *self.control.stats.lock()
    }

    /// A 250 ms graceful child deadline, then termination and at most 2 seconds
    /// waiting for that process. No claim of sub-20 ms stop latency is made.
    /// Retained frames remain valid after stop: their shared allocations have
    /// parent native references, and the child's published writes already ended.
    pub fn stop(&mut self) -> IsolatedCaptureStats {
        if !self.control.stopping.swap(true, Ordering::AcqRel) {
            let graceful = self.releases.commands.try_send(Message::Stop).is_ok();
            self.control.process.shutdown(graceful);
        }
        for worker in [&mut self.sender, &mut self.receiver] {
            if let Some(worker) = worker.take()
                && worker.thread().id() != thread::current().id()
            {
                let _ = worker.join();
            }
        }
        // Reentrant Drop in the sink skips only its own join. That worker sees
        // stopping as soon as the callback returns and owns its local resources
        // until then; it cannot call another callback or keep the child alive.
        self.stats()
    }
}

impl Drop for IsolatedCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Explicit helper path for retained/runtime qualification. Production resolves
/// it beside current_exe; never through PATH, user configuration or an env var.
pub fn isolated_capture_screen(
    device: &GraphicsDevice,
    helper: &Path,
    target: CaptureTarget,
    fps: u32,
    cursor: bool,
    sink: VideoSink<Frame>,
) -> Result<(IsolatedCapture, CaptureFormat)> {
    wire::fps(fps)?;
    let deadline = Instant::now() + Duration::from_secs(8);
    let (process, mut input, output) = Process::spawn(helper)?;
    let control = Arc::new(Control {
        process,
        stopping: AtomicBool::new(false),
        failure: Mutex::new(None),
        stats: Mutex::new(IsolatedCaptureStats::default()),
    });
    let (commands, pending) = mpsc::sync_channel(64);
    let releases = Arc::new(Releases {
        commands,
        control: Arc::downgrade(&control),
        outstanding: AtomicUsize::new(0),
    });
    // Construct the guard BEFORE any fallible thread creation or pipe I/O.
    let mut capture = IsolatedCapture {
        control: control.clone(),
        releases: releases.clone(),
        sender: None,
        receiver: None,
    };
    capture.sender = Some(
        thread::Builder::new()
            .name("WGC helper commands".into())
            .spawn({
                let control = control.clone();
                move || {
                    let result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                            loop {
                                match pending.recv_timeout(Duration::from_millis(50)) {
                                    Ok(message) => {
                                        message.write(&mut input)?;
                                        if matches!(message, Message::Stop) {
                                            return Ok(());
                                        }
                                    }
                                    Err(mpsc::RecvTimeoutError::Timeout) => {
                                        if control.stopping.load(Ordering::Acquire)
                                            || control.failure.lock().is_some()
                                        {
                                            return Ok(());
                                        }
                                    }
                                    Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                                }
                            }
                        }));
                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            control.fail(format!("capture helper command pipe: {error:#}"))
                        }
                        Err(_) => control.fail("capture helper command worker panicked".into()),
                    }
                }
            })?,
    );
    let (started, ready) = mpsc::sync_channel(1);
    capture.receiver = Some(
        thread::Builder::new()
            .name("WGC helper frames".into())
            .spawn({
                let control = control.clone();
                let releases = Arc::downgrade(&releases);
                let device = device.clone();
                move || {
                    let mut started = Some(started);
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        receive(
                            output,
                            &device,
                            fps,
                            &control,
                            &releases,
                            &mut started,
                            &sink,
                        )
                    }));
                    let error = match result {
                        Ok(Ok(())) => return,
                        Ok(Err(error)) => format!("capture helper: {error:#}"),
                        Err(_) => "capture helper sink/receiver panicked".into(),
                    };
                    control.fail(error.clone());
                    control.process.shutdown(false);
                    let error = control.failure.lock().clone().unwrap_or(error);
                    // Startup failures belong to the opener, not the source callback.
                    if let Some(started) = started {
                        let _ = started.try_send(Err(error));
                    } else if !control.stopping.load(Ordering::Acquire) {
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            (sink.stopped)(error)
                        }));
                    }
                }
            })?,
    );
    releases
        .commands
        .try_send(Message::Init(Init {
            target,
            fps,
            cursor,
            luid: device.adapter_luid(),
        }))
        .map_err(|_| anyhow::anyhow!("capture helper Init channel failed"))?;
    match ready.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Ok(format)) => Ok((capture, format)),
        Ok(Err(error)) => bail!("{error}"),
        Err(error) => {
            // Kill first, not after the graceful interval, on startup timeout.
            control.stopping.store(true, Ordering::Release);
            control.process.shutdown(false);
            drop(capture);
            Err(error).context("capture helper did not start within eight seconds")
        }
    }
}

struct Surface {
    id: u64,
    borrowed: u64,
    texture: ID3D11Texture2D,
    shared: Arc<SharedHandle>,
    width: u32,
    height: u32,
}

struct Fence {
    borrowed: u64,
    raw: ID3D11Fence,
    _shared: SharedHandle,
}

// A receive worker is one helper epoch. Never share these native handles or
// surface ids with another spawn, even when that process reuses the same ids.
struct Imports {
    surfaces: VecDeque<Surface>,
    size: Option<(u32, u32)>,
    newest_surface: u64,
    fence: Option<Fence>,
    sequence: u64,
    ready: u64,
    nanos: i64,
}

impl Imports {
    fn frame(
        &mut self,
        picture: Picture,
        device: &GraphicsDevice,
        control: &Control,
        releases: &Weak<Releases>,
    ) -> Result<(Frame, Time)> {
        if self.sequence.checked_add(1) != Some(picture.sequence)
            || picture.ready <= self.ready
            || picture.nanos < self.nanos
        {
            bail!("capture helper frame sequence, timeline or timestamp regressed");
        }
        let channel = releases.upgrade().context("capture helper is stopping")?;
        if channel.outstanding.fetch_add(1, Ordering::AcqRel) >= MAX_LEASES {
            channel.outstanding.fetch_sub(1, Ordering::AcqRel);
            bail!("capture helper exceeded its 32-picture lease limit");
        }
        let lease = Box::new(RemoteLease {
            sequence: picture.sequence,
            releases: releases.clone(),
        });
        if self.fence.is_none() {
            let shared = control.process.duplicate(picture.fence)?;
            let mut raw = None;
            unsafe { device.raw().OpenSharedFence(shared.handle(), &mut raw) }?;
            self.fence = Some(Fence {
                borrowed: picture.fence,
                raw: raw.context("capture helper fence missing")?,
                _shared: shared,
            });
        }
        let fence = self
            .fence
            .as_ref()
            .context("capture helper fence missing")?;
        if fence.borrowed != picture.fence {
            bail!("capture helper changed its fence");
        }
        let size = (picture.width, picture.height);
        if self.size != Some(size) {
            // The helper resets its identity cache on the same geometry edge.
            // Old pictures keep their own native references/leases; the cache
            // must not pin obsolete full-size allocations after a resize.
            self.surfaces.clear();
            self.size = Some(size);
        }
        if !self
            .surfaces
            .iter()
            .any(|surface| surface.id == picture.surface)
        {
            if picture.surface <= self.newest_surface {
                bail!("capture helper reused an expired surface id");
            }
            let shared = Arc::new(control.process.duplicate(picture.texture)?);
            let texture: ID3D11Texture2D =
                unsafe { device.raw().OpenSharedResource1(shared.handle()) }?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { texture.GetDesc(&mut desc) };
            if self.surfaces.len() == MAX_SURFACES {
                self.surfaces.pop_front();
            }
            self.surfaces.push_back(Surface {
                id: picture.surface,
                borrowed: picture.texture,
                texture,
                shared,
                width: desc.Width,
                height: desc.Height,
            });
            self.newest_surface = picture.surface;
        }
        let surface = self
            .surfaces
            .iter()
            .find(|surface| surface.id == picture.surface)
            .context("capture helper surface missing")?;
        if surface.borrowed != picture.texture
            || surface.width != picture.width
            || surface.height != picture.height
        {
            bail!("capture helper changed a cached surface's handle or geometry");
        }
        let frame = device.import_shared(
            surface.texture.clone(),
            surface.shared.clone(),
            &fence.raw,
            picture.ready,
            lease,
        )?;
        self.sequence = picture.sequence;
        self.ready = picture.ready;
        self.nanos = picture.nanos;
        Ok((frame, Time::from_nanos(picture.nanos)))
    }
}

fn receive(
    mut output: ChildStdout,
    device: &GraphicsDevice,
    fps: u32,
    control: &Control,
    releases: &Weak<Releases>,
    started: &mut Option<mpsc::SyncSender<Result<CaptureFormat, String>>>,
    sink: &VideoSink<Frame>,
) -> Result<()> {
    let _apartment = Apartment::new()?;
    match Message::read(&mut output)? {
        Message::Ready(format, luid) => {
            if luid != device.adapter_luid() {
                bail!(
                    "capture helper adapter LUID differs from the parent; cross-adapter/readback fallback is not supported"
                );
            }
            if format.fps != fps {
                bail!("capture helper returned a different frame rate");
            }
            started
                .as_ref()
                .context("capture startup already completed")?
                .send(Ok(format))
                .map_err(|_| anyhow::anyhow!("capture startup was cancelled"))?;
            *started = None;
        }
        Message::Error(error) => bail!("{error}"),
        _ => bail!("capture helper must reply Ready before publishing frames"),
    }
    let mut imports = Imports {
        surfaces: VecDeque::new(),
        size: None,
        newest_surface: 0,
        fence: None,
        sequence: 0,
        ready: 0,
        nanos: 0,
    };
    loop {
        let message = match Message::read(&mut output) {
            Ok(message) => message,
            Err(_) if control.stopping.load(Ordering::Acquire) => return Ok(()),
            Err(error) => return Err(error),
        };
        match message {
            Message::Frame(picture) => {
                if control.stopping.load(Ordering::Acquire) {
                    continue;
                }
                let (frame, at) = imports.frame(picture, device, control, releases)?;
                if control.stopping.load(Ordering::Acquire) {
                    continue;
                }
                control.stats.lock().delivered += 1;
                (sink.frame)(frame, at);
            }
            Message::Stats(mut stats) => {
                let mut previous = control.stats.lock();
                if stats.capture.received < previous.capture.received
                    || stats.capture.delivered < previous.capture.delivered
                    || stats.capture.pool_dropped < previous.capture.pool_dropped
                    || stats.capture.rate_skipped < previous.capture.rate_skipped
                    || stats.capture.resize_skipped < previous.capture.resize_skipped
                    || stats.backpressure_dropped < previous.backpressure_dropped
                    || (!control.stopping.load(Ordering::Acquire)
                        && stats.published != imports.sequence)
                    || stats.published < previous.published
                {
                    bail!("capture helper counters regressed or disagree with its frame sequence");
                }
                stats.delivered = previous.delivered;
                *previous = stats;
            }
            Message::Error(error) => {
                if control.stopping.load(Ordering::Acquire) {
                    return Ok(());
                }
                bail!("{error}");
            }
            _ => bail!("capture helper sent an unexpected message after Ready"),
        }
    }
}
