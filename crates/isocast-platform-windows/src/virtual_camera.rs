//! A latest-picture publisher, not a registered Windows camera. Each local
//! reader owns at most one in-flight CPU picture. Neither pipe I/O nor a
//! downstream DXGI lease runs on the program/recording path.
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use isocast_engine::{Time, record::CameraSink};
use isocast_ipc::{LocalListener, Stream};
use isocast_vcam_wire::{Header, Mode};
use parking_lot::{Condvar, Mutex};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;

use crate::{Frame, GraphicsDevice, PixelFormat, convert::Converter};

const READERS: usize = 4;
const IDLE: Duration = Duration::from_millis(200);
const WRITE_LIMIT: Duration = Duration::from_millis(500);

#[derive(Clone)]
struct Picture {
    sequence: u64,
    pts: Time,
    published: Time,
    repeated: bool,
    nv12: Arc<Vec<u8>>,
}

struct Feed {
    latest: Mutex<Option<Picture>>,
    changed: Condvar,
    stopped: AtomicBool,
    mode: Mode,
}

struct Publisher {
    feed: Arc<Feed>,
    converter: Converter,
    staging: Option<((u32, u32), ID3D11Texture2D)>,
    destination: String,
    listener: Option<JoinHandle<Result<()>>>,
}

pub(crate) fn open(
    device: &GraphicsDevice,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<Box<dyn CameraSink<Frame>>> {
    let mode = Mode::new(width, height, fps)?;
    let converter = Converter::for_software(device, width, height, fps, PixelFormat::Bgra8, 8)?;
    let session = session_id(std::process::id())?;
    let path = isocast_vcam_wire::destination(&isocast_protocol::api::socket_path(), session);
    let listener =
        LocalListener::bind(&path).context("virtual_camera: bind owner-only publisher pipe")?;
    let feed = Arc::new(Feed {
        latest: Mutex::new(None),
        changed: Condvar::new(),
        stopped: AtomicBool::new(false),
        mode,
    });
    let shared = Arc::clone(&feed);
    let worker = thread::Builder::new()
        .name("camera pipe".into())
        .spawn(move || serve(listener, session, shared))?;
    Ok(Box::new(Publisher {
        converter,
        staging: None,
        destination: path.display().to_string(),
        feed,
        listener: Some(worker),
    }))
}

fn session_id(pid: u32) -> windows::core::Result<u32> {
    let mut session = 0;
    // SAFETY: our own PID or the authenticated pipe peer (IPC holds its process handle).
    unsafe { ProcessIdToSessionId(pid, &mut session)? };
    Ok(session)
}

fn serve(listener: LocalListener, session: u32, feed: Arc<Feed>) -> Result<()> {
    let mut readers: Vec<JoinHandle<()>> = Vec::with_capacity(READERS);
    let result = (|| -> Result<()> {
        while !feed.stopped.load(Ordering::Acquire) {
            let client = match listener.accept_timeout(IDLE) {
                Ok(pipe) => Stream::Local(pipe),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::TimedOut
                            | io::ErrorKind::PermissionDenied
                            | io::ErrorKind::BrokenPipe
                    ) =>
                {
                    continue;
                }
                Err(e) => {
                    // A client can exit during peer verification. Only that
                    // connection failed; leave the feed and its readers live.
                    isocast_diagnostics::diagnostic!("warn"; "virtual_camera: accept reader: {e}");
                    thread::sleep(IDLE); // bound retries on a persistent OS error
                    continue;
                }
            };
            for i in (0..readers.len()).rev() {
                if readers[i].is_finished() {
                    readers
                        .swap_remove(i)
                        .join()
                        .map_err(|_| anyhow::anyhow!("virtual_camera reader panicked"))?;
                }
            }
            if readers.len() >= READERS
                || isocast_ipc::peer_pid(&client)
                    .ok()
                    .and_then(|pid| session_id(pid).ok())
                    != Some(session)
            {
                continue;
            }
            let feed = Arc::clone(&feed);
            readers.push(thread::Builder::new().name("camera reader".into()).spawn(move || {
                // Closed clients and stalled writes end only this subscription.
                if let Err(error) = deliver(client, &feed) {
                    isocast_diagnostics::diagnostic!("debug"; "camera reader disconnected: {error}");
                }
            })?);
        }
        Ok(())
    })();
    feed.stopped.store(true, Ordering::Release);
    feed.changed.notify_all();
    for reader in readers {
        reader
            .join()
            .map_err(|_| anyhow::anyhow!("virtual_camera reader panicked"))?;
    }
    result
}

fn deliver(mut pipe: Stream, feed: &Feed) -> io::Result<()> {
    pipe.set_write_timeout(Some(WRITE_LIMIT))?;
    let mut seen = 0;
    loop {
        let picture = {
            let mut latest = feed.latest.lock();
            while !feed.stopped.load(Ordering::Acquire)
                && latest.as_ref().is_none_or(|p| p.sequence == seen)
            {
                if feed.changed.wait_for(&mut latest, IDLE).timed_out() {
                    drop(latest);
                    if pipe.hung_up() {
                        return Ok(());
                    }
                    latest = feed.latest.lock();
                }
            }
            if feed.stopped.load(Ordering::Acquire) {
                return Ok(());
            }
            latest.as_ref().expect("new picture above").clone()
        };
        // V1: 48 little-endian header bytes, followed by tightly packed
        // Rec.709 limited NV12, Y then interleaved UV. A new pipe = a new
        // sequence space. Even a late join during hold gets a complete picture.
        let header = Header {
            mode: feed.mode,
            repeated: picture.repeated,
            sequence: picture.sequence,
            source_ns: picture.pts.as_nanos(),
            delivery_ns: picture.published.as_nanos(),
        };
        pipe.write_all(&header.encode())?;
        pipe.write_all(&picture.nv12)?;
        seen = picture.sequence;
    }
}

impl Publisher {
    fn publish(&self, live: Option<(Time, Vec<u8>)>) -> Result<bool> {
        ensure!(
            !self.feed.stopped.load(Ordering::Acquire),
            "virtual_camera: publisher stopped"
        );
        let Some(mut latest) = self.feed.latest.try_lock() else {
            return Ok(false);
        };
        let sequence = latest.as_ref().map_or(1, |p| p.sequence + 1);
        let published = crate::now();
        match (live, latest.as_mut()) {
            (Some((pts, bytes)), _) => {
                *latest = Some(Picture {
                    sequence,
                    pts,
                    published,
                    repeated: false,
                    nv12: Arc::new(bytes),
                })
            }
            (None, Some(p)) => {
                p.sequence = sequence;
                p.published = published;
                p.repeated = true;
            }
            (None, None) => return Ok(false),
        }
        drop(latest);
        self.feed.changed.notify_all();
        Ok(true)
    }
}

impl CameraSink<Frame> for Publisher {
    fn fits_canvas(&self) -> bool {
        // Server 2022 WARP can lose the recording device when re-importing
        // its composited BGRA as a second DX12 draw's input. Read it once
        // and fit on this expendable worker. Hardware keeps the GPU fit.
        isocast_engine::governor::is_software_renderer()
    }
    fn send(&mut self, frame: &Frame, pts: Time) -> Result<bool> {
        let Some(nv12) = self.converter.camera_nv12(frame, &mut self.staging)? else {
            return Ok(false);
        };
        self.publish(Some((pts, nv12)))
    }
    fn repeat_last(&mut self) -> Result<bool> {
        self.publish(None)
    }
    fn transport(&self) -> &'static str {
        "windows-pipe-nv12"
    }
    fn destination(&self) -> Option<&str> {
        Some(&self.destination)
    }
    fn close(&mut self) -> Result<()> {
        self.feed.stopped.store(true, Ordering::Release);
        self.feed.changed.notify_all();
        if let Some(worker) = self.listener.take() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("virtual_camera publisher panicked"))??;
        }
        Ok(())
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        if let Err(error) = self.close() {
            isocast_diagnostics::diagnostic!("warn"; "virtual camera close: {error:#}");
        }
    }
}
