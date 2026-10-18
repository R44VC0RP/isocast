use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use isocast_engine::{Time, VideoSink};
use parking_lot::Mutex;
use windows::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::System::Threading::WaitForMultipleObjects;

use super::IsolatedCaptureStats;
use super::wire::{self, Init, MAX_LEASES, MAX_SURFACES, Message, Picture};
use crate::native::{Apartment, Event};
use crate::{DisplayCapture, Frame, GraphicsDevice, capture_screen};

enum Command {
    Init(Init),
    Ack(u64),
    Stop,
    Failed(String),
}

/// Entry point of the dedicated binary, not an in-process platform service.
/// Intentionally exits the PROCESS: even a clean WGC/COM close retains RPC
/// associations. Nothing may print to stdout except the framed protocol.
pub fn capture_helper_main() -> ! {
    let mut output = std::io::stdout().lock();
    let wake = match Event::new() {
        Ok(event) => Arc::new(event),
        Err(error) => {
            let _ =
                Message::Error(format!("capture helper wake event: {error:#}")).write(&mut output);
            std::process::exit(1);
        }
    };
    let (send, commands) = mpsc::sync_channel(64);
    let input = thread::Builder::new()
        .name("WGC parent pipe".into())
        .spawn({
            let wake = wake.clone();
            move || {
                let mut input = std::io::stdin().lock();
                let mut initialized = false;
                loop {
                    // EOF/malformed/truncated input exits immediately, even if the
                    // main thread is inside a hung driver or WGC/COM destructor.
                    // Together with the no-Init deadline below, this closes the
                    // parent-crash window BEFORE job assignment.
                    let message = match Message::read(&mut input) {
                        Ok(message) => message,
                        Err(_) => std::process::exit(1),
                    };
                    let command = match message {
                        Message::Init(init) if !initialized => {
                            initialized = true;
                            Command::Init(init)
                        }
                        Message::Ack(sequence) if initialized => Command::Ack(sequence),
                        Message::Stop if initialized => Command::Stop,
                        _ => Command::Failed("unexpected parent capture message".into()),
                    };
                    let stop = matches!(command, Command::Stop | Command::Failed(_));
                    if send.try_send(command).is_err() {
                        std::process::exit(1);
                    }
                    wake.signal();
                    if stop {
                        return;
                    }
                }
            }
        });
    let input = match input {
        Ok(input) => input,
        Err(error) => {
            let _ =
                Message::Error(format!("capture helper input worker: {error}")).write(&mut output);
            std::process::exit(1);
        }
    };
    // No apartment, device, WGC query or target activation before valid Init.
    // The parent cannot send it until its kill-on-close job owns this process.
    let init = match commands.recv_timeout(Duration::from_secs(8)) {
        Ok(Command::Init(init)) => init,
        _ => std::process::exit(1),
    };
    let copied = match Event::new() {
        Ok(event) => event,
        Err(error) => {
            let _ =
                Message::Error(format!("capture helper copy event: {error:#}")).write(&mut output);
            std::process::exit(1);
        }
    };
    // Keep the completion event alive until process exit, including timeout and
    // device-loss paths with an outstanding SetEventOnCompletion registration.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        serve(init, &mut output, &wake, &copied, &commands)
    }));
    let code = match result {
        Ok(Ok(code)) => code,
        Ok(Err(error)) => {
            let _ = Message::Error(format!("capture helper: {error:#}")).write(&mut output);
            1
        }
        Err(_) => {
            let _ = Message::Error("capture helper panicked".into()).write(&mut output);
            1
        }
    };
    // Stop lets the input worker return normally. On source failure it remains
    // the parent-EOF guard until process exit; never join a blocking stdin read.
    if code == 0 || input.is_finished() {
        let _ = input.join();
    }
    std::process::exit(code);
}

struct Queued {
    frame: Frame,
    at: Time,
    arrived: Instant,
}

struct Surface {
    // A native reference, NOT a Frame: it prevents pointer reuse without
    // retaining a lease in TexturePool. FIFO eviction matches the parent.
    texture: ID3D11Texture2D,
    id: u64,
}

struct Publisher {
    queued: mpsc::Receiver<Queued>,
    pending: VecDeque<Queued>,
    leases: BTreeMap<u64, Frame>,
    surfaces: VecDeque<Surface>,
    size: Option<(u32, u32)>,
    sequence: u64,
    surface: u64,
    dropped: Arc<AtomicU64>,
    failed: Arc<Mutex<Option<String>>>,
    armed: Option<u64>,
    last_stats: Instant,
}

fn serve(
    init: Init,
    output: &mut impl Write,
    wake: &Arc<Event>,
    copied: &Event,
    commands: &mpsc::Receiver<Command>,
) -> Result<i32> {
    let _apartment = Apartment::new()?;
    let device = GraphicsDevice::new()?;
    if device.adapter_luid() != init.luid {
        bail!(
            "capture helper adapter LUID differs from the parent; no cross-adapter/readback fallback is available"
        );
    }
    let (send, queued) = mpsc::sync_channel(MAX_LEASES);
    let dropped = Arc::new(AtomicU64::new(0));
    let failed = Arc::new(Mutex::new(None));
    let sink = VideoSink {
        frame: Box::new({
            let dropped = dropped.clone();
            let wake = wake.clone();
            move |frame, at| {
                let frame = Queued {
                    frame,
                    at,
                    arrived: Instant::now(),
                };
                if send.try_send(frame).is_err() {
                    dropped.fetch_add(1, Ordering::Relaxed);
                }
                wake.signal();
            }
        }),
        stopped: Box::new({
            let failed = failed.clone();
            let wake = wake.clone();
            move |error| {
                *failed.lock() = Some(error);
                wake.signal();
            }
        }),
    };
    let (capture, format) = capture_screen(&device, init.target, init.fps, init.cursor, sink)?;
    let mut publisher = Publisher {
        queued,
        pending: VecDeque::new(),
        leases: BTreeMap::new(),
        surfaces: VecDeque::new(),
        size: None,
        sequence: 0,
        surface: 0,
        dropped,
        failed,
        armed: None,
        last_stats: Instant::now(),
    };
    let mut announced = false;
    let result = (|| -> Result<()> {
        wire::geometry(format.width, format.height)?;
        Message::Ready(format, device.adapter_luid()).write(output)?;
        announced = true;
        publisher.run(&capture, &device, output, wake, copied, commands)
    })();
    // Report the actual raw counters and broker losses BEFORE attempting COM
    // teardown. This snapshot is not a fabricated successful capture count.
    if announced {
        let _ = Message::Stats(publisher.stats(&capture)).write(output);
    }
    let code = match result {
        Ok(()) => 0,
        Err(error) => {
            let _ = Message::Error(format!("capture helper: {error:#}")).write(output);
            1
        }
    };
    // Only this disposable process can wait inside raw DisplayCapture::drop.
    // Published frames are retained by publisher until capture has stopped;
    // parent job termination bounds this whole path if an OS call hangs.
    drop(capture);
    Ok(code)
}

impl Publisher {
    fn stats(&self, capture: &DisplayCapture) -> IsolatedCaptureStats {
        IsolatedCaptureStats {
            capture: capture.stats(),
            published: self.sequence,
            backpressure_dropped: self.dropped.load(Ordering::Relaxed),
            delivered: 0,
        }
    }

    fn run(
        &mut self,
        capture: &DisplayCapture,
        device: &GraphicsDevice,
        output: &mut impl Write,
        wake: &Event,
        copied: &Event,
        commands: &mpsc::Receiver<Command>,
    ) -> Result<()> {
        loop {
            // Control always wins over incoming pictures or GPU completion.
            while let Ok(command) = commands.try_recv() {
                match command {
                    Command::Stop => return Ok(()),
                    Command::Ack(sequence) => {
                        if self.leases.remove(&sequence).is_none() {
                            bail!("capture helper received an unknown or repeated Ack");
                        }
                    }
                    Command::Failed(error) => bail!("{error}"),
                    Command::Init(_) => bail!("capture helper received a second Init"),
                }
            }
            if let Some(error) = self.failed.lock().take() {
                bail!("{error}");
            }
            // Bound each drain so a fast producer cannot starve Stop/Ack.
            for _ in 0..MAX_LEASES {
                let Ok(frame) = self.queued.try_recv() else {
                    break;
                };
                if self.leases.len() + self.pending.len() == MAX_LEASES {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                } else {
                    self.pending.push_back(frame);
                }
            }
            let completed = device.completed()?;
            while self.pending.front().is_some_and(|frame| {
                frame.frame.ready_value() <= completed && frame.at <= crate::now()
            }) {
                let frame = self
                    .pending
                    .pop_front()
                    .context("capture helper pending frame missing")?;
                if self.armed == Some(frame.frame.ready_value()) {
                    self.armed = None;
                }
                self.publish(frame, output)?;
            }
            let mut timeout = Duration::from_millis(250).saturating_sub(self.last_stats.elapsed());
            if let Some(frame) = self.pending.front() {
                let remaining = Duration::from_secs(2).saturating_sub(frame.arrived.elapsed());
                if remaining.is_zero() {
                    bail!(
                        "capture helper picture was not ready for publication within two seconds"
                    );
                }
                timeout = timeout.min(remaining);
                let ready = frame.frame.ready_value();
                if ready <= completed {
                    // WGC can deliver a completed picture before its display
                    // presentation timestamp. Publishing it early can replace
                    // every timestamp-eligible picture in the engine's bounded
                    // history. Keep its original QPC stamp and publish when due,
                    // without another copy or a larger queue. Do not re-arm an
                    // already completed fence: that would spin on this event.
                    self.armed = None;
                    timeout = timeout.min(frame.at.saturating_since(crate::now()));
                } else if self.armed != Some(ready) {
                    device.notify(ready, copied)?;
                    self.armed = Some(ready);
                }
            }
            if timeout.is_zero() {
                Message::Stats(self.stats(capture)).write(output)?;
                self.last_stats = Instant::now();
                continue;
            }
            // Event-driven GPU completion, never CPU pixel readback or a spin.
            let event = unsafe {
                WaitForMultipleObjects(
                    &[wake.handle(), copied.handle()],
                    false,
                    timeout.as_millis().max(1) as u32,
                )
            };
            if event == WAIT_FAILED {
                return Err(windows::core::Error::from_thread().into());
            }
            if event != WAIT_OBJECT_0 && event.0 != WAIT_OBJECT_0.0 + 1 && event != WAIT_TIMEOUT {
                bail!("capture helper wait returned an unexpected value");
            }
        }
    }

    fn publish(&mut self, queued: Queued, output: &mut impl Write) -> Result<()> {
        let frame = queued.frame;
        wire::geometry(frame.width(), frame.height())?;
        let size = (frame.width(), frame.height());
        if self.size != Some(size) {
            // Geometry is already carried by every picture, so both sides can
            // retire cached old-size references without another wire message.
            // IDs remain monotonic; leased old pictures are still protected.
            self.surfaces.clear();
            self.size = Some(size);
        }
        let surface = if let Some(surface) = self
            .surfaces
            .iter()
            .find(|surface| surface.texture == *frame.texture())
        {
            surface.id
        } else {
            self.surface = self
                .surface
                .checked_add(1)
                .context("capture helper surface ids exhausted")?;
            if self.surfaces.len() == MAX_SURFACES {
                self.surfaces.pop_front();
            }
            self.surfaces.push_back(Surface {
                texture: frame.texture().clone(),
                id: self.surface,
            });
            self.surface
        };
        let sequence = self
            .sequence
            .checked_add(1)
            .context("capture helper sequence exhausted")?;
        let message = Message::Frame(Picture {
            sequence,
            surface,
            texture: frame.shared_texture().0 as usize as u64,
            fence: frame.shared_fence().0 as usize as u64,
            ready: frame.ready_value(),
            width: frame.width(),
            height: frame.height(),
            nanos: queued.at.as_nanos(),
        });
        // Completion was observed BEFORE publication. A child crash can now
        // neither strand a parent's GPU wait nor mutate a published picture.
        // Hold the Frame until Ack: raw TexturePool's strong-count rule makes
        // reuse impossible while ANY parent GPU consumer retains its lease.
        self.leases.insert(sequence, frame);
        message.write(output)?;
        self.sequence = sequence;
        Ok(())
    }
}
