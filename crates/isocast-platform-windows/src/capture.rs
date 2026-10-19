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
    /// Actual frames received from Windows, not a duration × refresh-rate estimate.
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
                        if let Err(error) = crate::native::video_thread() {
                            eprintln!("Windows screen capture scheduling: {error:#}");
                        }
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

pub use duplication::capture_monitor;

// Kept separate from WGC deliberately: raw callers may share their WGC device
// with NVENC, which is NOT safe for AcquireNextFrame/NvEncLockBitstream.
mod duplication {
    use super::{CaptureTarget, Counters, DisplayCapture};
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, bail};
    use isocast_engine::{CaptureFormat, Time, VideoSink};
    use windows::Win32::Foundation::{POINT, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows::Win32::Graphics::Direct3D::Fxc::{
        D3DCOMPILE_ENABLE_STRICTNESS, D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCompile,
    };
    use windows::Win32::Graphics::Direct3D::{
        D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, ID3DBlob, ID3DInclude,
    };
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::Win32::Graphics::Dxgi::*;
    use windows::Win32::Graphics::Gdi::{HMONITOR, MONITOR_DEFAULTTOPRIMARY, MonitorFromPoint};
    use windows::Win32::System::Threading::WaitForSingleObject;
    use windows::core::{Interface, PCSTR, s};

    use crate::clock::from_counter;
    use crate::frames::{Frame, GraphicsDevice, TexturePool};
    use crate::native::{Apartment, Event, video_thread};

    /// GPU-only SDR, unrotated monitor capture for a DISPOSABLE capture helper.
    /// `device` must be dedicated to this capture: never open NVENC on it, nor
    /// submit other rendering. Import the resulting shared frames on the parent
    /// device instead. Existing same-device callers must keep `capture_screen`.
    ///
    /// Cursor inclusion is supported, including separate color/AND/XOR shapes.
    /// Exclusion is rejected: DXGI may already have composited a software cursor
    /// into its image. WGC remains the API for guaranteed cursor exclusion.
    /// Mode/access loss stops the source; reopen explicitly. A stalled/failed
    /// GPU submission quarantines one acquired resource until helper exit,
    /// never releasing a desktop surface while the GPU might still read it.
    pub fn capture_monitor(
        device: &GraphicsDevice,
        target: CaptureTarget,
        fps: u32,
        cursor: bool,
        sink: VideoSink<Frame>,
    ) -> Result<(DisplayCapture, CaptureFormat)> {
        if !(1..=240).contains(&fps) {
            bail!("screen.fps: {fps}; valid range: 1..=240");
        }
        if matches!(target, CaptureTarget::Window(_)) {
            bail!(
                "screen.target: desktop duplication accepts PrimaryDisplay or Monitor, not Window"
            );
        }
        if !cursor {
            bail!(
                "screen.cursor: desktop duplication cannot guarantee exclusion; use capture_screen (WGC)"
            );
        }
        let stop = Arc::new(Event::new()?);
        let counts = Arc::new(Counters::default());
        let (started, ready) = mpsc::sync_channel(1);
        let worker = {
            let device = device.clone();
            let stop = stop.clone();
            let counts = counts.clone();
            thread::Builder::new()
                .name("Windows monitor duplication".into())
                .spawn(move || {
                    let mut running = false;
                    let result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                            let _apartment = Apartment::new()?;
                            if let Err(error) = video_thread() {
                                eprintln!("Windows monitor capture scheduling: {error:#}");
                            }
                            let mut capture = Duplication::new(&device, target)?;
                            let format = CaptureFormat {
                                width: capture.width,
                                height: capture.height,
                                fps,
                            };
                            if started.send(Ok(format)).is_err() {
                                return Ok(());
                            }
                            running = true;
                            capture.run(&device, &stop, &counts, &sink, fps)
                        }));
                    let error = match result {
                        Ok(Ok(())) => return,
                        Ok(Err(error)) => format!("Windows monitor duplication: {error:#}"),
                        Err(_) => "Windows monitor duplication worker panicked".into(),
                    };
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
                // The helper's process/job boundary contains hung native opens.
                drop(capture);
                Err(error).context("Windows monitor duplication did not start in time")
            }
        }
    }

    struct Duplication {
        raw: IDXGIOutputDuplication,
        output: IDXGIOutput6,
        width: u32,
        height: u32,
        copied: Arc<Event>,
        pointer: Pointer,
    }

    // On any error/panic after submission begins, preserve ALL the native lease
    // and event until process teardown. One failed capture cannot grow a retry
    // queue; the source stops and its disposable helper must exit.
    struct Acquired {
        held: Option<(
            IDXGIOutputDuplication,
            Option<IDXGIResource>,
            GraphicsDevice,
            Arc<Event>,
        )>,
        in_flight: bool,
    }

    impl Acquired {
        fn release(mut self) -> Result<()> {
            let held = self.held.take().context("desktop frame already released")?;
            unsafe { held.0.ReleaseFrame() }.context("release desktop frame")
        }
    }

    impl Drop for Acquired {
        fn drop(&mut self) {
            if self.in_flight {
                std::mem::forget(self.held.take());
            } else if let Some(held) = self.held.take() {
                let _ = unsafe { held.0.ReleaseFrame() };
            }
        }
    }

    impl Duplication {
        fn new(device: &GraphicsDevice, target: CaptureTarget) -> Result<Self> {
            let monitor = match target {
                CaptureTarget::PrimaryDisplay => unsafe {
                    MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY)
                },
                CaptureTarget::Monitor(handle) => HMONITOR(handle as *mut _),
                CaptureTarget::Window(_) => bail!("desktop duplication cannot capture a window"),
            };
            let adapter = unsafe { device.raw().cast::<IDXGIDevice>()?.GetAdapter() }?;
            let mut selected = None;
            for index in 0.. {
                let output = match unsafe { adapter.EnumOutputs(index) } {
                    Ok(output) => output,
                    Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
                    Err(error) => return Err(error.into()),
                };
                if unsafe { output.GetDesc() }?.Monitor == monitor {
                    selected = Some(output.cast::<IDXGIOutput6>()?);
                    break;
                }
            }
            let output = selected.context(
                "screen.monitor: not on the capture device's adapter; no cross-adapter fallback",
            )?;
            validate_output(&output)?;
            let raw = unsafe { output.DuplicateOutput(device.raw()) }.context(
                "DuplicateOutput requires the interactive desktop and a dedicated capture device",
            )?;
            let desc = unsafe { raw.GetDesc() };
            if desc.Rotation != DXGI_MODE_ROTATION_IDENTITY
                || desc.ModeDesc.Format != DXGI_FORMAT_B8G8R8A8_UNORM
                || desc.DesktopImageInSystemMemory.as_bool()
                || [desc.ModeDesc.Width, desc.ModeDesc.Height]
                    .iter()
                    .any(|n| !(1..=16384).contains(n))
            {
                bail!(
                    "screen.monitor: requires unrotated GPU-resident BGRA8 with dimensions in 1..=16384"
                );
            }
            // Output desktop rectangles can be DPI-virtualized (observed 1920
            // vs 3840). DDA texture and pointer coordinates are physical pixels.
            Ok(Self {
                raw,
                output,
                width: desc.ModeDesc.Width,
                height: desc.ModeDesc.Height,
                copied: Arc::new(Event::new()?),
                pointer: Pointer::new(device)?,
            })
        }

        fn run(
            &mut self,
            device: &GraphicsDevice,
            stop: &Event,
            counts: &Counters,
            sink: &VideoSink<Frame>,
            fps: u32,
        ) -> Result<()> {
            let mut pool = TexturePool::new(device.clone());
            let mut pending: Option<(Frame, Time)> = None;
            let interval = Duration::from_nanos(1_000_000_000 / u64::from(fps));
            let mut deliver_at = Instant::now();
            let mut image_seen = false;
            loop {
                match unsafe { WaitForSingleObject(stop.handle(), 0) } {
                    WAIT_OBJECT_0 => return Ok(()),
                    WAIT_TIMEOUT => {}
                    _ => bail!("monitor stop event failed"),
                }
                if pending.is_some() && Instant::now() >= deliver_at {
                    let (frame, at) = pending.take().expect("pending desktop frame");
                    deliver_at += interval;
                    if deliver_at < Instant::now() {
                        deliver_at = Instant::now() + interval;
                    }
                    counts.delivered.fetch_add(1, Ordering::Relaxed);
                    (sink.frame)(frame, at);
                }
                // An idle desktop must still publish its final update. Keep
                // only the newest completed owned frame until the cadence
                // deadline, and wake at that deadline even without another
                // AcquireNextFrame success. Its timestamp stays the real QPC
                // update time, never the publication deadline.
                let wait_ms = if pending.is_some() {
                    // AcquireNextFrame's millisecond timeout is not a precise
                    // media timer: a requested 1ms waited up to 16ms on the
                    // qualified driver. Wait on the owned high-resolution
                    // timer, then poll DXGI once to coalesce the latest update
                    // BEFORE publishing at the deadline. Cap each wait so Stop
                    // is checked even at a low requested capture frame rate.
                    let delay = deliver_at
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(16));
                    crate::clock::sleep_until(Time::from_nanos(
                        crate::clock::now()
                            .as_nanos()
                            .saturating_add(delay.as_nanos() as i64),
                    ))?;
                    0
                } else {
                    16
                };
                let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
                let mut resource = None;
                match unsafe { self.raw.AcquireNextFrame(wait_ms, &mut info, &mut resource) } {
                    Ok(()) => {}
                    Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => continue,
                    Err(error) => {
                        return Err(error)
                            .context("AcquireNextFrame failed; reopen the monitor capture");
                    }
                }
                let mut acquired = Acquired {
                    held: Some((
                        self.raw.clone(),
                        resource.clone(),
                        device.clone(),
                        self.copied.clone(),
                    )),
                    in_flight: false,
                };
                counts.received.fetch_add(1, Ordering::Relaxed);
                validate_output(&self.output)?;
                if info.ProtectedContentMaskedOut.as_bool() {
                    bail!(
                        "screen.monitor: protected content was masked; cannot deliver complete pixels"
                    );
                }
                self.pointer.update(device, &self.raw, &info)?;
                image_seen |= info.LastPresentTime > 0;
                // The driver can deliver ONLY initial pointer metadata before
                // its first desktop picture. Do not publish uninitialized pixels.
                if !image_seen {
                    continue;
                }
                let ticks = info.LastPresentTime.max(info.LastMouseUpdateTime);
                if ticks <= 0 {
                    bail!("desktop frame has no QPC timestamp");
                }
                let at = from_counter(ticks);
                let source: ID3D11Texture2D = resource
                    .context("DXGI returned no desktop texture")?
                    .cast()?;
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                unsafe { source.GetDesc(&mut desc) };
                if desc.Width != self.width
                    || desc.Height != self.height
                    || desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM
                    || desc.ArraySize != 1
                    || desc.BindFlags & D3D11_BIND_SHADER_RESOURCE.0 as u32 == 0
                {
                    bail!(
                        "screen.monitor: texture geometry/format changed or driver lacks shader-resource binding; reopen, no CPU fallback"
                    );
                }
                let mut view = None;
                unsafe {
                    device
                        .raw()
                        .CreateShaderResourceView(&source, None, Some(&mut view))
                }?;
                let view = view.context("D3D11 returned no desktop shader view")?;
                acquired.in_flight = true;
                let frame =
                    pool.copy_with(&source, 0, self.width, self.height, |context, target| {
                        self.pointer
                            .draw(device, context, target, &view, self.width, self.height)
                    })?;
                let Some(frame) = frame else {
                    acquired.in_flight = false;
                    counts.pool_dropped.fetch_add(1, Ordering::Relaxed);
                    continue;
                };
                device.notify(frame.ready_value(), &self.copied)?;
                if unsafe { WaitForSingleObject(self.copied.handle(), 2_000) } != WAIT_OBJECT_0
                    || device.completed()? < frame.ready_value()
                {
                    bail!(
                        "desktop GPU copy/cursor fence did not complete within 2s; acquired resource quarantined until helper exit"
                    );
                }
                acquired.in_flight = false;
                acquired.release()?;
                if pending.replace((frame, at)).is_some() {
                    counts.rate_skipped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    fn validate_output(output: &IDXGIOutput6) -> Result<()> {
        let desc = unsafe { output.GetDesc1() }?;
        if !desc.AttachedToDesktop.as_bool()
            || desc.Rotation != DXGI_MODE_ROTATION_IDENTITY
            || desc.ColorSpace != DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709
        {
            bail!(
                "screen.monitor: only attached, unrotated SDR Rec.709 outputs are supported; HDR/rotation require an explicit conversion"
            );
        }
        Ok(())
    }

    const CURSOR_SHADER: &str = r#"
Texture2D<float4> desktop : register(t0);
Texture2D<float4> pointer : register(t1);
cbuffer Position : register(b0) { int2 origin; uint kind; uint pad; };
float4 vs(uint id : SV_VertexID) : SV_Position {
    return float4(id == 2 ? 3.0 : -1.0, id == 1 ? 3.0 : -1.0, 0.0, 1.0);
}
float4 ps(float4 position : SV_Position) : SV_Target {
    int2 p = int2(position.xy);
    float4 back = desktop.Load(int3(p, 0));
    float4 shape = pointer.Load(int3(p - origin, 0));
    if (kind == 2) return float4(shape.rgb + back.rgb * (1.0 - shape.a), back.a);
    uint3 b = uint3(round(back.rgb * 255.0));
    uint3 c = uint3(round(shape.rgb * 255.0));
    uint mask = uint(round(shape.a * 255.0));
    uint3 rgb = kind == 1 ? ((b & mask) ^ c) : (mask == 255 ? (b ^ c) : c);
    return float4(float3(rgb) / 255.0, back.a);
}
"#;

    struct Pointer {
        visible: bool,
        position: POINT,
        width: u32,
        height: u32,
        kind: u32,
        view: Option<ID3D11ShaderResourceView>,
        vertex: ID3D11VertexShader,
        pixel: ID3D11PixelShader,
        constants: ID3D11Buffer,
        raster: ID3D11RasterizerState,
    }

    impl Pointer {
        fn new(device: &GraphicsDevice) -> Result<Self> {
            unsafe {
                let vertex_code = shader(s!("vs"), s!("vs_5_0"))?;
                let pixel_code = shader(s!("ps"), s!("ps_5_0"))?;
                let mut vertex = None;
                let mut pixel = None;
                device.raw().CreateVertexShader(
                    std::slice::from_raw_parts(
                        vertex_code.GetBufferPointer().cast(),
                        vertex_code.GetBufferSize(),
                    ),
                    None,
                    Some(&mut vertex),
                )?;
                device.raw().CreatePixelShader(
                    std::slice::from_raw_parts(
                        pixel_code.GetBufferPointer().cast(),
                        pixel_code.GetBufferSize(),
                    ),
                    None,
                    Some(&mut pixel),
                )?;
                let mut constants = None;
                device.raw().CreateBuffer(
                    &D3D11_BUFFER_DESC {
                        ByteWidth: 16,
                        Usage: D3D11_USAGE_DEFAULT,
                        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                        ..Default::default()
                    },
                    None,
                    Some(&mut constants),
                )?;
                let mut raster = None;
                device.raw().CreateRasterizerState(
                    &D3D11_RASTERIZER_DESC {
                        FillMode: D3D11_FILL_SOLID,
                        CullMode: D3D11_CULL_NONE,
                        DepthClipEnable: true.into(),
                        ..Default::default()
                    },
                    Some(&mut raster),
                )?;
                Ok(Self {
                    visible: false,
                    position: POINT::default(),
                    width: 0,
                    height: 0,
                    kind: 0,
                    view: None,
                    vertex: vertex.context("no cursor vertex shader")?,
                    pixel: pixel.context("no cursor pixel shader")?,
                    constants: constants.context("no cursor constants")?,
                    raster: raster.context("no cursor rasterizer")?,
                })
            }
        }

        fn update(
            &mut self,
            device: &GraphicsDevice,
            duplication: &IDXGIOutputDuplication,
            info: &DXGI_OUTDUPL_FRAME_INFO,
        ) -> Result<()> {
            if info.LastMouseUpdateTime > 0 {
                self.visible = info.PointerPosition.Visible.as_bool();
                self.position = info.PointerPosition.Position;
            }
            if info.PointerShapeBufferSize == 0 {
                return Ok(());
            }
            if info.PointerShapeBufferSize > 4 * 1024 * 1024 {
                bail!("screen.cursor: shape exceeds 4MiB bound");
            }
            let mut bytes = vec![0u8; info.PointerShapeBufferSize as usize];
            let mut required = 0;
            let mut shape = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            unsafe {
                duplication.GetFramePointerShape(
                    bytes.len() as u32,
                    bytes.as_mut_ptr().cast(),
                    &mut required,
                    &mut shape,
                )
            }?;
            let mono = shape.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32;
            if ![1, 2, 4].contains(&shape.Type)
                || !(1..=1024).contains(&shape.Width)
                || shape.Height == 0
                || shape.Height > if mono { 2048 } else { 1024 }
                || (mono && !shape.Height.is_multiple_of(2))
                || shape.Pitch
                    < if mono {
                        shape.Width.div_ceil(8)
                    } else {
                        shape.Width * 4
                    }
                || u64::from(shape.Pitch) * u64::from(shape.Height) > u64::from(required)
                || required as usize > bytes.len()
            {
                bail!(
                    "screen.cursor: invalid shape type/geometry/pitch; supported types: monochrome, color, masked color (up to 1024x1024)"
                );
            }
            let height = if mono { shape.Height / 2 } else { shape.Height };
            let mut pixels = Vec::with_capacity((shape.Width * height * 4) as usize);
            for y in 0..height as usize {
                for x in 0..shape.Width as usize {
                    if mono {
                        let bit = 0x80 >> (x % 8);
                        let and = if bytes[y * shape.Pitch as usize + x / 8] & bit != 0 {
                            255
                        } else {
                            0
                        };
                        let xor = if bytes[(y + height as usize) * shape.Pitch as usize + x / 8]
                            & bit
                            != 0
                        {
                            255
                        } else {
                            0
                        };
                        pixels.extend_from_slice(&[xor, xor, xor, and]);
                    } else {
                        let offset = y * shape.Pitch as usize + x * 4;
                        let pixel = &bytes[offset..offset + 4];
                        if shape.Type == 4 && pixel[3] != 0 && pixel[3] != 255 {
                            bail!("screen.cursor: masked-color alpha must be 0 or 255");
                        }
                        pixels.extend_from_slice(pixel);
                    }
                }
            }
            let mut texture = None;
            let mut view = None;
            unsafe {
                device.raw().CreateTexture2D(
                    &D3D11_TEXTURE2D_DESC {
                        Width: shape.Width,
                        Height: height,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Usage: D3D11_USAGE_IMMUTABLE,
                        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                        ..Default::default()
                    },
                    Some(&D3D11_SUBRESOURCE_DATA {
                        pSysMem: pixels.as_ptr().cast(),
                        SysMemPitch: shape.Width * 4,
                        SysMemSlicePitch: 0,
                    }),
                    Some(&mut texture),
                )?;
                device.raw().CreateShaderResourceView(
                    &texture.context("no cursor texture")?,
                    None,
                    Some(&mut view),
                )?;
            }
            self.view = Some(view.context("no cursor shader view")?);
            self.width = shape.Width;
            self.height = height;
            self.kind = shape.Type;
            Ok(())
        }

        fn draw(
            &self,
            device: &GraphicsDevice,
            context: &ID3D11DeviceContext4,
            target: &ID3D11Texture2D,
            desktop: &ID3D11ShaderResourceView,
            width: u32,
            height: u32,
        ) -> Result<()> {
            if !self.visible {
                return Ok(());
            }
            let view = self
                .view
                .as_ref()
                .context("screen.cursor: visible pointer without shape metadata")?;
            // HotSpot is informational: DXGI already reports the image origin.
            let left = i64::from(self.position.x).max(0);
            let top = i64::from(self.position.y).max(0);
            let right = (i64::from(self.position.x) + i64::from(self.width)).min(i64::from(width));
            let bottom =
                (i64::from(self.position.y) + i64::from(self.height)).min(i64::from(height));
            if left >= right || top >= bottom {
                return Ok(());
            }
            let mut target_view = None;
            unsafe {
                device
                    .raw()
                    .CreateRenderTargetView(target, None, Some(&mut target_view))?;
                let target_view = target_view.context("no cursor target view")?;
                let constants = [self.position.x, self.position.y, self.kind as i32, 0];
                context.UpdateSubresource(
                    &self.constants,
                    0,
                    None,
                    constants.as_ptr().cast(),
                    0,
                    0,
                );
                context.ClearState();
                context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                context.RSSetState(&self.raster);
                context.RSSetViewports(Some(&[D3D11_VIEWPORT {
                    TopLeftX: left as f32,
                    TopLeftY: top as f32,
                    Width: (right - left) as f32,
                    Height: (bottom - top) as f32,
                    MinDepth: 0.0,
                    MaxDepth: 1.0,
                }]));
                context.OMSetRenderTargets(Some(&[Some(target_view)]), None);
                context.VSSetShader(&self.vertex, None);
                context.PSSetShader(&self.pixel, None);
                context.PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
                context.PSSetShaderResources(0, Some(&[Some(desktop.clone()), Some(view.clone())]));
                context.Draw(3, 0);
                context.ClearState();
            }
            Ok(())
        }
    }

    fn shader(entry: PCSTR, profile: PCSTR) -> Result<ID3DBlob> {
        let mut code = None;
        let mut errors = None;
        let result = unsafe {
            D3DCompile(
                CURSOR_SHADER.as_ptr().cast(),
                CURSOR_SHADER.len(),
                s!("desktop-cursor.hlsl"),
                None,
                None::<&ID3DInclude>,
                entry,
                profile,
                D3DCOMPILE_ENABLE_STRICTNESS | D3DCOMPILE_OPTIMIZATION_LEVEL3,
                0,
                &mut code,
                Some(&mut errors),
            )
        };
        if let Err(error) = result {
            let detail = errors
                .map(|blob| unsafe {
                    String::from_utf8_lossy(std::slice::from_raw_parts(
                        blob.GetBufferPointer().cast::<u8>(),
                        blob.GetBufferSize(),
                    ))
                    .into_owned()
                })
                .unwrap_or_default();
            return Err(error).with_context(|| format!("compile desktop cursor shader: {detail}"));
        }
        code.context("no desktop cursor shader bytecode")
    }
}
