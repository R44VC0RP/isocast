//! WGC owns a small capture pool. Copy its pictures into a bounded pool of
//! shareable D3D11 textures before returning them: keeping WGC frames in the
//! engine would stall capture as soon as a writer retained its small pool.
//! The copy stays on GPU; WGC leases live until its completion fence passes.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use isocast_engine::{CaptureFormat, VideoSink};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HWND, POINT, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{HMONITOR, MONITOR_DEFAULTTOPRIMARY, MonitorFromPoint};
use windows::Win32::System::Threading::{INFINITE, WaitForMultipleObjects};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::IsWindow;
use windows::core::{IInspectable, Interface};

use crate::clock::from_100ns;
use crate::frames::{Frame, GraphicsDevice, TexturePool};
use crate::native::{Apartment, Event};

const CAPTURE_BUFFERS: i32 = 8;
const PIXEL_FORMAT: DirectXPixelFormat = DirectXPixelFormat::B8G8R8A8UIntNormalized;

/// Native targets resolved by the platform's device/window discovery. These
/// handles are local to the interactive desktop, not persisted source ids.
#[derive(Clone, Copy, Debug)]
pub enum CaptureTarget {
    PrimaryDisplay,
    Monitor(usize),
    Window(usize),
}

#[derive(Clone, Copy, Default, Debug)]
pub struct CaptureStats {
    /// Actual frames received from WGC, not a duration × refresh-rate estimate.
    pub received: u64,
    pub delivered: u64,
    pub pool_dropped: u64,
    pub rate_skipped: u64,
    pub resize_skipped: u64,
}

#[derive(Default)]
struct Counters {
    received: AtomicU64,
    delivered: AtomicU64,
    pool_dropped: AtomicU64,
    rate_skipped: AtomicU64,
    resize_skipped: AtomicU64,
}

/// Dropping a capture stops it and joins its worker. All sink callbacks run
/// on that worker; none can outlive a completed drop on another thread.
pub struct DisplayCapture {
    stop: Arc<Event>,
    worker: Option<JoinHandle<()>>,
    counts: Arc<Counters>,
}

impl DisplayCapture {
    pub fn stats(&self) -> CaptureStats {
        CaptureStats {
            received: self.counts.received.load(Ordering::Relaxed),
            delivered: self.counts.delivered.load(Ordering::Relaxed),
            pool_dropped: self.counts.pool_dropped.load(Ordering::Relaxed),
            rate_skipped: self.counts.rate_skipped.load(Ordering::Relaxed),
            resize_skipped: self.counts.resize_skipped.load(Ordering::Relaxed),
        }
    }
}

impl Drop for DisplayCapture {
    fn drop(&mut self) {
        self.stop.signal();
        if let Some(worker) = self.worker.take()
            && worker.thread().id() != thread::current().id()
        {
            let _ = worker.join();
        }
    }
}

pub fn capture_screen(
    device: &GraphicsDevice,
    target: CaptureTarget,
    fps: u32,
    cursor: bool,
    sink: VideoSink<Frame>,
) -> Result<(DisplayCapture, CaptureFormat)> {
    if !(1..=240).contains(&fps) {
        bail!("screen.fps: {fps}; valid range: 1..=240");
    }
    let stop = Arc::new(Event::new()?);
    let counts = Arc::new(Counters::default());
    let (started, ready) = mpsc::sync_channel(1);
    let worker = {
        let device = device.clone();
        let stop = stop.clone();
        let counts = counts.clone();
        thread::Builder::new()
            .name("Windows screen capture".into())
            .spawn(move || {
                let mut running = false;
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                        let _apartment = Apartment::new()?;
                        let mut capture = Session::new(&device, target, cursor)?;
                        let size = capture.size;
                        let format = CaptureFormat {
                            width: size.Width as u32,
                            height: size.Height as u32,
                            fps,
                        };
                        capture.session.StartCapture()?;
                        if started.send(Ok(format)).is_err() {
                            return Ok(());
                        }
                        running = true;
                        capture.run(&device, &stop, &counts, &sink, fps)
                    }));
                let error = match result {
                    Ok(Ok(())) => return,
                    Ok(Err(error)) => format!("Windows screen capture: {error:#}"),
                    Err(_) => "Windows screen capture worker panicked".into(),
                };
                // Startup errors go to the caller, runtime errors to the source.
                if running {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        (sink.stopped)(error)
                    }));
                } else {
                    let _ = started.send(Err(error));
                }
            })?
    };
    let capture = DisplayCapture {
        stop,
        worker: Some(worker),
        counts,
    };
    match ready.recv_timeout(Duration::from_secs(8)) {
        Ok(Ok(format)) => Ok((capture, format)),
        Ok(Err(error)) => bail!("{error}"),
        Err(error) => {
            // Dropping the handle still owns and joins startup; the engine's
            // existing bounded native-open worker contains a hung OS call.
            drop(capture);
            Err(error).context("Windows screen capture did not start in time")
        }
    }
}

struct Lease(Direct3D11CaptureFrame);

impl Drop for Lease {
    fn drop(&mut self) {
        let _ = self.0.Close();
    }
}

struct Session {
    // Close session/pool explicitly before the COM apartment is left.
    session: GraphicsCaptureSession,
    pool: Direct3D11CaptureFramePool,
    item: GraphicsCaptureItem,
    arrived: Arc<Event>,
    closed: Arc<Event>,
    copied: Event,
    on_frame: Option<i64>,
    on_close: Option<i64>,
    pending: VecDeque<(u64, Lease)>,
    device: IDirect3DDevice,
    size: SizeInt32,
    target: CaptureTarget,
}

impl Session {
    fn new(device: &GraphicsDevice, target: CaptureTarget, cursor: bool) -> Result<Self> {
        if !GraphicsCaptureSession::IsSupported()? {
            bail!("WGC is unavailable on this desktop");
        }
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe {
            match target {
                CaptureTarget::PrimaryDisplay => interop.CreateForMonitor(MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY)),
                CaptureTarget::Monitor(handle) => interop.CreateForMonitor(HMONITOR(handle as *mut _)),
                CaptureTarget::Window(handle) => interop.CreateForWindow(HWND(handle as *mut _)),
            }
        }.context("cannot capture the requested target; use the interactive desktop, not a service session")?;
        let size = item.Size()?;
        if size.Width <= 0 || size.Height <= 0 {
            bail!("screen target has no drawable area");
        }
        let dxgi: IDXGIDevice = device.raw().cast()?;
        let device: IDirect3DDevice =
            unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }?.cast()?;
        let arrived = Arc::new(Event::new()?);
        let closed = Arc::new(Event::new()?);
        let copied = Event::new()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &device,
            PIXEL_FORMAT,
            CAPTURE_BUFFERS,
            size,
        )?;
        let session = pool.CreateCaptureSession(&item).inspect_err(|_| {
            let _ = pool.Close();
        })?;
        let mut capture = Self {
            session,
            pool,
            item,
            arrived,
            closed,
            copied,
            on_frame: None,
            on_close: None,
            pending: VecDeque::new(),
            device,
            size,
            target,
        };
        // Do not silently include the cursor when exclusion was requested.
        capture
            .session
            .SetIsCursorCaptureEnabled(cursor)
            .context("screen.cursor requires Windows 10 version 2004 or newer")?;
        capture.on_frame = Some(capture.pool.FrameArrived(&TypedEventHandler::<
            Direct3D11CaptureFramePool,
            IInspectable,
        >::new({
            let arrived = capture.arrived.clone();
            move |_, _| {
                arrived.signal();
                Ok(())
            }
        }))?);
        capture.on_close = Some(capture.item.Closed(&TypedEventHandler::<
            GraphicsCaptureItem,
            IInspectable,
        >::new({
            let closed = capture.closed.clone();
            move |_, _| {
                closed.signal();
                Ok(())
            }
        }))?);
        Ok(capture)
    }

    fn run(
        &mut self,
        device: &GraphicsDevice,
        stop: &Event,
        counts: &Counters,
        sink: &VideoSink<Frame>,
        fps: u32,
    ) -> Result<()> {
        let mut textures = TexturePool::new(device.clone());
        let handles = [
            stop.handle(),
            self.closed.handle(),
            self.arrived.handle(),
            self.copied.handle(),
        ];
        let mut size = self.size;
        let mut resize = None;
        let mut last = None;
        loop {
            // Windows 10 can stop sending frames without Item.Closed when
            // the owning process exits (observed on build 19045). Bound that
            // silent wait for HWND captures; a static window is not a failure.
            let timeout = if matches!(self.target, CaptureTarget::Window(_)) {
                250
            } else {
                INFINITE
            };
            let event = unsafe { WaitForMultipleObjects(&handles, false, timeout) };
            if event == WAIT_FAILED {
                return Err(windows::core::Error::from_thread().into());
            }
            if event == WAIT_OBJECT_0 {
                return Ok(());
            }
            if let CaptureTarget::Window(handle) = self.target
                && !unsafe { IsWindow(Some(HWND(handle as *mut _))) }.as_bool()
            {
                bail!("the captured window closed");
            }
            if event == WAIT_TIMEOUT {
                continue;
            }
            match event.0 - WAIT_OBJECT_0.0 {
                0 => return Ok(()),
                1 => bail!("the captured display or window closed"),
                2 | 3 => {}
                _ => bail!("unexpected capture wait result: {}", event.0),
            }
            let completed = device.completed()?;
            while self
                .pending
                .front()
                .is_some_and(|(value, _)| *value <= completed)
            {
                self.pending.pop_front();
            }
            if let Some(next) = resize
                && self.pending.is_empty()
            {
                self.pool
                    .Recreate(&self.device, PIXEL_FORMAT, CAPTURE_BUFFERS, next)?;
                size = next;
                resize = None;
            }
            loop {
                let captured = match self.pool.TryGetNextFrame() {
                    Ok(frame) => Lease(frame),
                    // WGC returns S_OK + a null interface for an empty pool.
                    // windows-rs represents that as Error::empty (code S_OK),
                    // not E_POINTER. Actual failed HRESULTs must still fail.
                    Err(error) if error.code().is_ok() => break,
                    Err(error) => return Err(error.into()),
                };
                counts.received.fetch_add(1, Ordering::Relaxed);
                let content = captured.0.ContentSize()?;
                if content != size || resize.is_some() {
                    if content.Width > 0 && content.Height > 0 {
                        resize = Some(content);
                    }
                    counts.resize_skipped.fetch_add(1, Ordering::Relaxed);
                    // Wake even if no more frame arrives (a window minimized
                    // during resize); the outer loop can now recreate safely.
                    self.copied.signal();
                    break;
                }
                let at = from_100ns(captured.0.SystemRelativeTime()?.Duration);
                if last.is_some_and(|previous| {
                    at.saturating_since(previous).as_nanos() + 1_000_000
                        < 1_000_000_000 / u128::from(fps)
                }) {
                    counts.rate_skipped.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                let access: IDirect3DDxgiInterfaceAccess = captured.0.Surface()?.cast()?;
                let source: ID3D11Texture2D = unsafe { access.GetInterface() }?;
                let Some(frame) = textures.copy(&source, size.Width as u32, size.Height as u32)?
                else {
                    counts.pool_dropped.fetch_add(1, Ordering::Relaxed);
                    continue;
                };
                self.pending.push_back((frame.ready_value(), captured));
                last = Some(at);
                counts.delivered.fetch_add(1, Ordering::Relaxed);
                (sink.frame)(frame, at);
            }
            if let Some((value, _)) = self.pending.front() {
                device.notify(*value, &self.copied)?;
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(token) = self.on_frame {
            let _ = self.pool.RemoveFrameArrived(token);
        }
        if let Some(token) = self.on_close {
            let _ = self.item.RemoveClosed(token);
        }
        // Stop the producer before releasing any in-flight lease on teardown.
        let _ = self.session.Close();
        self.pending.clear();
        let _ = self.pool.Close();
    }
}
