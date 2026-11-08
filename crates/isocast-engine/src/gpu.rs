//! The GPU: frames shown to it without copying, and drawn with it.

use std::future::Future;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, mpsc};
use std::task::{Context as TaskContext, Poll, Waker};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use parking_lot::{Condvar, Mutex};

use crate::render::{TexelKind, Texels};

#[cfg(windows)]
fn needs_serial_builds(adapter: wgpu::DeviceType, compiler: &wgpu::Dx12Compiler) -> bool {
    adapter == wgpu::DeviceType::Cpu || !matches!(compiler, wgpu::Dx12Compiler::Fxc)
}

#[cfg(all(test, windows))]
#[test]
fn only_non_cpu_fxc_allows_concurrent_pipeline_builds() {
    use wgpu::{DeviceType, Dx12Compiler};
    assert!(!needs_serial_builds(
        DeviceType::DiscreteGpu,
        &Dx12Compiler::Fxc
    ));
    assert!(!needs_serial_builds(
        DeviceType::IntegratedGpu,
        &Dx12Compiler::Fxc
    ));
    assert!(needs_serial_builds(DeviceType::Cpu, &Dx12Compiler::Fxc));
    let dxc = Dx12Compiler::DynamicDxc {
        dxc_path: String::new(),
    };
    for adapter in [
        DeviceType::DiscreteGpu,
        DeviceType::IntegratedGpu,
        DeviceType::Cpu,
    ] {
        assert!(needs_serial_builds(adapter, &dxc));
    }
}

/// The GPU device frames are imported into and drawn with. Clones share
/// the device and its queue.
#[derive(Clone)]
pub struct Gpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    poll: mpsc::SyncSender<()>,
    lost: Arc<AtomicBool>,
    fault: Option<Arc<Fault>>,
    // A successful first picture already built the BGRA pipelines. Every
    // canvas on this device reuses those immutable objects, not its draw state.
    #[cfg(windows)]
    pub(crate) warmed_bgra: Arc<std::sync::OnceLock<crate::render::Renderer>>,
    #[cfg(windows)]
    pub(crate) first_canvas: Arc<Mutex<Option<crate::record::CanvasPasses>>>,
    // wgpu-hal 30 shares one IDxcCompiler3 within an instance. Serialize
    // actual builds across clones too (e.g. independent scene viewers).
    // On CPU adapters, competing compiler workers also starve WARP's queue.
    #[cfg(windows)]
    serial_builds: Option<Arc<Mutex<()>>>,
    // Only split the initial FXC library build on hardware with spare CPU
    // threads. WARP needs the same cores for rendering; never fan out there.
    #[cfg(windows)]
    hardware_build_threads: bool,
    #[cfg(windows)]
    pub(crate) generation: isocast_surfaces::generation::Owner,
}

static GPU: Mutex<Option<Gpu>> = Mutex::new(None);
type ImportDevice =
    fn(&wgpu::Adapter, &wgpu::DeviceDescriptor<'_>) -> Option<(wgpu::Device, wgpu::Queue)>;

#[cfg(all(test, windows))]
#[test]
fn idle_start_requires_native_evidence_not_contention_or_logical_loss() -> Result<()> {
    {
        let _opening = GPU.lock();
        assert!(!Gpu::needs_idle_replacement(
            Gpu::cached_windows().as_ref(),
            false,
            false
        ));
    }
    // Isolated wgpu context, not the published renderer; no RemoveDevice or
    // shaders. Model the lost callback on an actually healthy native device.
    let (gpu, _) = Gpu::open_device(
        |_, _| None,
        isocast_surfaces::generation::registry().register("test renderer")?,
    )?;
    gpu.lost.store(true, Ordering::Release);
    assert!(gpu.windows_removed()); // unusable logical context for status
    assert!(!Gpu::needs_idle_replacement(Some(&gpu), false, false));
    assert!(Gpu::needs_idle_replacement(Some(&gpu), true, false));
    assert!(Gpu::needs_idle_replacement(Some(&gpu), false, true));
    Ok(())
}

impl Gpu {
    #[cfg(windows)]
    pub fn generation(&self) -> &isocast_surfaces::generation::Owner {
        &self.generation
    }
    #[cfg(windows)]
    pub fn cached_windows() -> Option<Self> {
        // An open/warm-up owns this lock. Contention is not evidence of loss.
        GPU.try_lock()?.clone()
    }

    #[cfg(windows)]
    pub fn retire_windows_cache() {
        let old = GPU.lock().take();
        if let Some(old) = old {
            old.lost.store(true, Ordering::Release);
        }
    }

    #[cfg(windows)]
    fn windows_removed(&self) -> bool {
        self.is_lost() || self.native_removed() != Some(false)
    }

    #[cfg(windows)]
    pub fn needs_idle_replacement(
        cached: Option<&Self>,
        d3d11_removed: bool,
        retiring: bool,
    ) -> bool {
        retiring || d3d11_removed || cached.and_then(Self::native_removed) == Some(true)
    }

    #[cfg(windows)]
    fn native_removed(&self) -> Option<bool> {
        let hal = unsafe { self.device.as_hal::<wgpu::hal::api::Dx12>() }?;
        Some(unsafe { hal.raw_device().GetDeviceRemovedReason() }.is_err())
    }

    #[cfg(windows)]
    pub fn windows_candidate(
        permit: &isocast_surfaces::generation::Replacement,
        start: impl FnOnce() + Send + 'static,
    ) -> Result<Self> {
        Self::open(start, false, |_, _| None, permit.owner("DX12 renderer"))
    }

    #[cfg(windows)]
    pub fn publish_windows_pair(
        self,
        permit: isocast_surfaces::generation::Replacement,
        publish_native: impl FnOnce(),
    ) -> isocast_surfaces::generation::Published {
        // Caller already has the D3D11 cache locked. No lazy opener can see
        // an empty cache with admission open, or a candidate that's closed.
        let mut cache = GPU.lock();
        permit.publish(|| {
            *cache = Some(self);
            publish_native();
        })
    }
    /// Observe only the published renderer. Status must not race capture to
    /// pin an adapter, initialize DX12, or block while the renderer warms up.
    #[cfg(windows)]
    pub fn opened_adapter() -> Option<isocast_protocol::api::WindowsAdapter> {
        let shared = GPU.try_lock()?;
        let gpu = shared
            .as_ref()
            .filter(|gpu| gpu.generation.check().is_ok() && !gpu.windows_removed())?;
        let hal = unsafe { gpu.device.as_hal::<wgpu::hal::api::Dx12>() }?;
        let luid = unsafe { hal.raw_device().GetAdapterLuid() };
        let info = gpu.device.adapter_info();
        Some(isocast_protocol::api::WindowsAdapter {
            luid: (luid.LowPart, luid.HighPart),
            name: info.name,
            warp: (info.vendor, info.device) == (0x1414, 0x008c),
        })
    }
    pub fn new() -> Result<Self> {
        Self::with_thread(|| {}, false, |_, _| None)
    }

    /// Calibration may open the singleton first. Keep its ordinary worker
    /// priority, but don't pin a device missing the platform's import support.
    pub(crate) fn for_measurement<P: crate::Platform>() -> Result<Self> {
        Self::with_thread(|| {}, false, P::open_import_device)
    }

    /// Completion now hands frames to protected outputs, just as their
    /// real-time drawing threads did before. Keep the same scheduling class
    /// instead of inheriting the control thread that happened to open the GPU.
    pub fn for_platform<P: crate::Platform>(platform: &P) -> Result<Self> {
        let platform = platform.clone();
        Self::with_thread(
            move || platform.real_time_thread(),
            false,
            P::open_import_device,
        )
    }

    #[cfg(windows)]
    pub(crate) fn for_compositor<P: crate::Platform>(platform: &P) -> Result<Self> {
        let platform = platform.clone();
        Self::with_thread(
            move || platform.real_time_thread(),
            true,
            P::open_import_device,
        )
    }

    fn with_thread(
        start: impl FnOnce() + Send + 'static,
        program: bool,
        import_device: ImportDevice,
    ) -> Result<Self> {
        // All canvases, ISO stages and viewers in this process import into
        // the same device. Keep it through stop/start cycles, not one driver
        // connection (and shader cache) per output. A lost device is not
        // reusable: its existing owners drain/fail independently, and new
        // outputs share a fresh device without restarting the process.
        let mut shared = GPU.lock();
        #[cfg(windows)]
        let _keep_native = if let Some(gpu) = shared.as_ref()
            && isocast_surfaces::generation::registry().managed()
        {
            anyhow::ensure!(
                gpu.native_removed() == Some(false),
                "Windows device removed; replacement requires an idle recording boundary"
            );
            // Preserve the existing logical wgpu-context retry, but keep the
            // healthy D3D12 singleton alive so it cannot change generations.
            gpu.is_lost().then(|| gpu.device.clone())
        } else {
            None
        };
        if let Some(gpu) = &*shared
            && !gpu.is_lost()
        {
            #[cfg(windows)]
            gpu.generation.check()?;
            return Ok(gpu.clone());
        }
        *shared = None;
        let gpu = Self::open(
            start,
            program,
            import_device,
            #[cfg(windows)]
            isocast_surfaces::generation::registry().register("DX12 renderer")?,
        )?;
        *shared = Some(gpu.clone());
        Ok(gpu)
    }

    #[cfg(not(windows))]
    fn adapter() -> Result<wgpu::Adapter> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .context("no GPU found")?;
        Ok(adapter)
    }

    /// Report the rendering adapter without creating a device or starting a renderer.
    pub fn adapter_info() -> Result<wgpu::AdapterInfo> {
        #[cfg(windows)]
        let _probe = isocast_surfaces::generation::registry().register("DX12 adapter probe")?;
        #[cfg(windows)]
        let (adapter, _) = isocast_surfaces::engine_adapter()?;
        #[cfg(not(windows))]
        let adapter = Self::adapter()?;
        Ok(adapter.get_info())
    }

    fn open(
        start: impl FnOnce() + Send + 'static,
        _program: bool,
        import_device: ImportDevice,
        #[cfg(windows)] owner: isocast_surfaces::generation::Owner,
    ) -> Result<Self> {
        let candidate = Self::open_device(
            import_device,
            #[cfg(windows)]
            owner,
        )?;
        #[cfg(windows)]
        let candidate = Self::warm_windows(candidate, _program)?;
        let (gpu, submissions) = candidate;
        gpu.start_completion(submissions, start)?;
        Ok(gpu)
    }

    fn open_device(
        import_device: ImportDevice,
        #[cfg(windows)] generation: isocast_surfaces::generation::Owner,
    ) -> Result<(Self, mpsc::Receiver<()>)> {
        #[cfg(windows)]
        let _span = isocast_diagnostics::gpu_startup_span("renderer.device");
        #[cfg(windows)]
        let (adapter, compiler) = isocast_surfaces::engine_adapter()?;
        #[cfg(not(windows))]
        let adapter = Self::adapter()?;
        let info = adapter.get_info();
        crate::governor::software_renderer(info.device_type == wgpu::DeviceType::Cpu);
        isocast_diagnostics::diagnostic!("info"; "graphics adapter: {} ({:?})", info.name, info.device_type);
        // 10-bit frames arrive as 16-bit planes; native Windows cameras use
        // genuine NV12 allocations with two aspect views of one texture.
        let required_features = adapter.features()
            & (wgpu::Features::TEXTURE_FORMAT_16BIT_NORM | wgpu::Features::TEXTURE_FORMAT_NV12);
        let desc = wgpu::DeviceDescriptor {
            label: Some("isocast"),
            required_features,
            ..Default::default()
        };
        let (device, queue) = match import_device(&adapter, &desc) {
            Some(opened) => opened,
            None => block_on(adapter.request_device(&desc)).context("could not open the GPU")?,
        };
        #[cfg(windows)]
        {
            let hal = unsafe { device.as_hal::<wgpu::hal::api::Dx12>() }.context("not DX12")?;
            unsafe {
                hal.raw_device().SetPrivateDataInterface(
                    &isocast_surfaces::generation::RECEIPT_ID,
                    &generation.child("DX12 native device").native_receipt(),
                )
            }?;
        }
        let lost = Arc::new(AtomicBool::new(false));
        let failed = lost.clone();
        device.set_device_lost_callback(move |_, _| failed.store(true, Ordering::Release));
        let fault = Fault::configured().map(Arc::new);
        let (poll, submissions) = mpsc::sync_channel(1);
        Ok((
            Self {
                device,
                queue,
                poll,
                lost,
                #[cfg(windows)]
                generation,
                fault,
                #[cfg(windows)]
                warmed_bgra: Arc::default(),
                #[cfg(windows)]
                first_canvas: Arc::default(),
                #[cfg(windows)]
                serial_builds: needs_serial_builds(info.device_type, &compiler).then(Arc::default),
                #[cfg(windows)]
                hardware_build_threads: matches!(
                    info.device_type,
                    wgpu::DeviceType::DiscreteGpu | wgpu::DeviceType::IntegratedGpu
                ) && std::thread::available_parallelism()
                    .is_ok_and(|n| n.get() >= 8),
            },
            submissions,
        ))
    }

    #[cfg(windows)]
    pub(crate) fn concurrent_pipeline_builds(&self) -> bool {
        self.serial_builds.is_none()
    }

    #[cfg(windows)]
    pub(crate) fn pipeline_build_guard(&self) -> Option<parking_lot::MutexGuard<'_, ()>> {
        self.serial_builds.as_ref().map(|lock| lock.lock())
    }

    #[cfg(windows)]
    pub(crate) fn parallel_warmup(&self) -> bool {
        self.concurrent_pipeline_builds()
            && self.hardware_build_threads
            && self.warmed_bgra.get().is_none()
    }

    #[cfg(windows)]
    fn warm_windows(
        mut candidate: (Self, mpsc::Receiver<()>),
        program: bool,
    ) -> Result<(Self, mpsc::Receiver<()>)> {
        // with_thread holds the process-wide GPU lock. No compositor, completion
        // thread or importer may retain the failed DX12 singleton across retry.
        static QUALIFIED: AtomicBool = AtomicBool::new(false);
        if QUALIFIED.load(Ordering::Relaxed) {
            // An ordinary device recreation is not initial startup. Nothing
            // will fill its warm-up cache, so keep the original build width.
            candidate.0.hardware_build_threads = false;
            return Ok(candidate);
        }
        let result = if let Some(loss) = candidate.0.picture_warmup(program)? {
            let generation = candidate.0.generation.clone();
            drop(candidate);
            isocast_surfaces::retry_with_dxc(&loss)?;
            let retry = (|| {
                let fresh = Self::open_device(|_, _| None, generation)
                    .context("reopening the original DX12 adapter")?;
                if let Some(loss) = fresh.0.picture_warmup(program)? {
                    anyhow::bail!("device lost again: {loss}");
                }
                Ok(fresh)
            })();
            if let Err(error) = &retry {
                isocast_surfaces::fail_dxc_retry(error);
            }
            retry?
        } else {
            candidate
        };
        QUALIFIED.store(true, Ordering::Relaxed);
        Ok(result)
    }

    #[cfg(windows)]
    fn picture_warmup(&self, program: bool) -> Result<Option<String>> {
        // Keep a native reference only for the reason query. An empty submission
        // or clear does not exercise WARP's failure executing fs_picture.
        let raw = unsafe { self.device.as_hal::<wgpu::hal::api::Dx12>() }
            .context("not a DX12 device")?
            .raw_device()
            .clone();
        let picture = || {
            let _span = isocast_diagnostics::gpu_startup_span("warmup.total");
            let format = wgpu::TextureFormat::Bgra8Unorm;
            let renderer = crate::render::Renderer::new(self, format);
            let size = (32, 32);
            let source = GpuFrame {
                width: 32,
                height: 32,
                pixels: GpuPixels::Rgb(crate::scene::canvas_texture(self, format, size)),
            };
            let target = crate::scene::canvas_texture(self, format, size);
            let submit = isocast_diagnostics::gpu_startup_span("warmup.submit");
            let submission = renderer.render(
                self,
                &target,
                &[crate::render::Layer::full(&source, 32, 32)],
                size,
            );
            drop(submit);
            let _wait = isocast_diagnostics::gpu_startup_span("warmup.wait");
            self.device
                .poll(wgpu::PollType::Wait {
                    submission_index: Some(submission),
                    timeout: Some(Duration::from_secs(3)),
                })
                .map(|_| renderer)
        };
        let draw = isocast_diagnostics::contain("DX12 picture warm-up", || {
            if program && self.concurrent_pipeline_builds() {
                let (draw, passes) = crate::record::CanvasPasses::alongside(self, picture);
                draw.map(|r| (r, Some(passes)))
            } else {
                // Qualify the picture first. In serial mode, don't start
                // other families until the canvas has a known-good device.
                picture().map(|r| (r, None))
            }
        });
        // A poll timeout or validation panic alone is NOT a device loss.
        let native = unsafe { raw.GetDeviceRemovedReason() }
            .err()
            .map(|e| e.code().0 as u32);
        // DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_HUNG, DXGI_ERROR_DEVICE_RESET.
        if native.is_some_and(|hr| matches!(hr, 0x887a0005..=0x887a0007)) || self.is_lost() {
            return Ok(Some(native.map_or_else(
                || "wgpu device-lost callback".into(),
                |hr| format!("HRESULT 0x{hr:08X}"),
            )));
        }
        if let Some(hr) = native {
            anyhow::bail!("DX12 picture warm-up failed: HRESULT 0x{hr:08X}");
        }
        let (renderer, passes) = draw
            .map_err(anyhow::Error::msg)?
            .context("waiting for the warm-up picture")?;
        self.warmed_bgra
            .get_or_init(|| renderer.with_fresh_draw_state());
        *self.first_canvas.lock() = passes;
        Ok(None)
    }

    fn start_completion(
        &self,
        submissions: mpsc::Receiver<()>,
        start: impl FnOnce() + Send + 'static,
    ) -> Result<()> {
        #[cfg(windows)]
        let _span = isocast_diagnostics::gpu_startup_span("completion.spawn");
        let poll_fault = self.fault.clone();
        let polling = self.device.clone();
        #[cfg(windows)]
        let worker_owner = self.generation.child("DX12 completion worker");
        let poll_lost = self.lost.clone();
        std::thread::Builder::new()
            .name("GPU completion".into())
            .spawn(move || {
                #[cfg(windows)]
                let _owner = worker_owner;
                start();
                let polled = isocast_diagnostics::contain("GPU completion", || {
                    while submissions.recv().is_ok() {
                        // Coalesce notifications. One thread services
                        // callbacks; drawing threads never wait for the
                        // driver to finish.
                        loop {
                            let kind = wgpu::PollType::Wait {
                                submission_index: None,
                                timeout: Some(Duration::from_secs(1)),
                            };
                            let result = match &poll_fault {
                                Some(fault) => fault.poll(&polling, kind),
                                None => polling.poll(kind),
                            };
                            // A producer stops submitting after its fence
                            // gets old. Keep servicing that fence even with
                            // no more notifications, or a transient stall
                            // freezes it. Not a lost device's: its owners
                            // fail and recreate, and nothing will finish.
                            if !matches!(result, Err(wgpu::PollError::Timeout))
                                || poll_lost.load(Ordering::Acquire)
                            {
                                break;
                            }
                        }
                    }
                });
                // The HAL can panic once its device is gone (or a callback
                // panicked mid-batch, so later fences never finish). Either
                // way this device is no longer trustworthy: its owners see
                // it lost and recreate, new outputs open a fresh device,
                // and this thread stops polling instead of taking the
                // process down.
                if let Err(message) = polled {
                    poll_lost.store(true, Ordering::Release);
                    isocast_diagnostics::diagnostic!("error"; "{message}; the shared GPU is marked lost");
                }
            })
            .context("could not start GPU completion thread")?;
        isocast_diagnostics::diagnostic!("debug"; "opened shared GPU");
        Ok(())
    }

    /// Wake the completion thread to poll again: a `map_async` asked for
    /// after its last poll began fires only on the next one.
    pub(crate) fn nudge(&self) {
        let _ = self.poll.try_send(());
    }

    pub(crate) fn is_lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }

    /// Submit with a callback attached to THIS command buffer, not whatever
    /// another canvas happened to submit last on the shared queue.
    fn submit(&self, commands: wgpu::CommandBuffer, previous: Option<Completion>) -> Completion {
        if let Some(fault) = &self.fault {
            fault.arm(&self.lost);
        }
        let done = Completion(Arc::new(Completed {
            lost: self.lost.clone(),
            ..Completed::default()
        }));
        let callback = done.clone();
        #[cfg(windows)]
        let submission_owner = self.generation.child("DX12 pending submission");
        let fault = self.fault.clone();
        commands.on_submitted_work_done(move || {
            #[cfg(windows)]
            let _owner = submission_owner;
            let finish = move || match previous {
                Some(previous) => previous.then(move |_| callback.finish()),
                None => callback.finish(),
            };
            match fault {
                Some(fault) => fault.complete(Box::new(finish)),
                None => finish(),
            }
        });
        self.queue.submit([commands]);
        let _ = self.poll.try_send(());
        done
    }
}

/// Developer-only recovery exercise, without wedging/resetting the machine's
/// GPU: `ISOCAST_GPU_FAULT=stall:AFTER_SECONDS:DURATION_SECONDS` withholds
/// completion callbacks and returns real wait-time-sized timeouts until the
/// interval ends. Only a later poll releases them, just as wgpu requires.
/// `lost:AFTER_SECONDS` sets the same flag as the device-lost callback.
/// `lost:AFTER_SECONDS:COUNT` also loses the next COUNT-1 devices on their
/// first submission, to exercise recovery back-off without resetting hardware.
/// `panic:AFTER_SECONDS` panics in the completion thread's next poll, as a
/// HAL can after losing its device. Without COUNT these fire once per
/// process, so a replacement device is not immediately lost again.
struct Fault {
    after: Duration,
    kind: FaultKind,
    state: Mutex<FaultState>,
}

enum FaultKind {
    Stall(Duration),
    Lost(u32),
    PollPanic,
}

#[derive(Default)]
struct FaultState {
    began: Option<Instant>,
    until: Option<Instant>,
    held: Vec<Box<dyn FnOnce() + Send>>,
    panic: bool,
    fired: bool,
}

impl Fault {
    fn configured() -> Option<Self> {
        if !isocast_diagnostics::developer() {
            return None;
        }
        let value = std::env::var("ISOCAST_GPU_FAULT").ok()?;
        let parts: Vec<_> = value.split(':').collect();
        let seconds = |text: &str| Duration::try_from_secs_f64(text.parse().ok()?).ok();
        let (after, kind) = match &parts[..] {
            ["stall", after, duration] => (seconds(after)?, FaultKind::Stall(seconds(duration)?)),
            ["lost", after] => (seconds(after)?, FaultKind::Lost(1)),
            ["lost", after, count] => (
                seconds(after)?,
                FaultKind::Lost(count.parse::<u32>().ok()?.clamp(1, 100)),
            ),
            ["panic", after] => (seconds(after)?, FaultKind::PollPanic),
            _ => return None,
        };
        Some(Self {
            after,
            kind,
            state: Mutex::default(),
        })
    }

    fn arm(&self, lost: &AtomicBool) {
        static FIRED: AtomicU32 = AtomicU32::new(0);
        let limit = match self.kind {
            FaultKind::Lost(count) => count,
            _ => 1,
        };
        let fired = FIRED.load(Ordering::Relaxed);
        if fired >= limit {
            return;
        }
        let mut state = self.state.lock();
        let began = *state.began.get_or_insert_with(Instant::now);
        if state.fired || (fired == 0 && began.elapsed() < self.after) {
            return;
        }
        state.fired = true;
        if FIRED.fetch_add(1, Ordering::SeqCst) >= limit {
            return;
        }
        match self.kind {
            FaultKind::Stall(duration) => {
                state.until = Some(Instant::now() + duration);
                isocast_diagnostics::diagnostic!("warn"; "injected GPU completion stall for {} ms", duration.as_millis());
            }
            FaultKind::Lost(_) => {
                lost.store(true, Ordering::Release);
                isocast_diagnostics::diagnostic!("warn"; "injected GPU device loss");
            }
            FaultKind::PollPanic => {
                state.panic = true;
                isocast_diagnostics::diagnostic!("warn"; "injected GPU completion poll panic");
            }
        }
    }

    fn complete(&self, complete: Box<dyn FnOnce() + Send>) {
        let mut state = self.state.lock();
        if state.until.is_some() {
            state.held.push(complete);
        } else {
            drop(state);
            complete();
        }
    }

    fn poll(
        &self,
        device: &wgpu::Device,
        kind: wgpu::PollType,
    ) -> Result<wgpu::PollStatus, wgpu::PollError> {
        let (remaining, held) = {
            let mut state = self.state.lock();
            if std::mem::take(&mut state.panic) {
                drop(state);
                panic!("injected GPU completion poll panic");
            }
            match state.until {
                Some(until) if Instant::now() < until => (
                    Some(until.saturating_duration_since(Instant::now())),
                    Vec::new(),
                ),
                Some(_) => {
                    state.until = None;
                    (None, std::mem::take(&mut state.held))
                }
                None => (None, Vec::new()),
            }
        };
        for complete in held {
            complete();
        }
        if let Some(remaining) = remaining {
            // Native completion may also be observed by queue.submit on a
            // different thread; complete() holds those callbacks too.
            device.poll(wgpu::PollType::Poll)?;
            std::thread::sleep(remaining.min(Duration::from_secs(1)));
            return Err(wgpu::PollError::Timeout);
        }
        device.poll(kind)
    }
}

type OnComplete = Box<dyn FnOnce(Duration) + Send>;

/// A fence for a submitted picture. Callbacks only hand frames to bounded
/// queues; encoding and IO must never run on the polling/submit thread.
#[derive(Clone, Default)]
pub(crate) struct Completion(Arc<Completed>);

struct Completed {
    state: Mutex<(Option<Duration>, Vec<OnComplete>)>,
    changed: Condvar,
    submitted: Instant,
    cancelled: AtomicBool,
    lost: Arc<AtomicBool>,
}

impl Default for Completed {
    fn default() -> Self {
        Self {
            state: Mutex::new((None, Vec::new())),
            changed: Condvar::new(),
            submitted: Instant::now(),
            cancelled: AtomicBool::new(false),
            lost: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Completion {
    fn finish(&self) {
        let elapsed = self.0.submitted.elapsed();
        loop {
            let callbacks = {
                let mut state = self.0.state.lock();
                if state.1.is_empty() {
                    state.0 = Some(elapsed);
                    break;
                }
                std::mem::take(&mut state.1)
            };
            for callback in callbacks {
                if !self.0.cancelled.load(Ordering::Acquire) && !self.0.lost.load(Ordering::Acquire)
                {
                    callback(elapsed);
                }
            }
        }
        self.0.changed.notify_all();
    }

    pub(crate) fn then(&self, callback: impl FnOnce(Duration) + Send + 'static) {
        let mut state = self.0.state.lock();
        if let Some(elapsed) = state.0 {
            drop(state);
            if !self.0.cancelled.load(Ordering::Acquire) && !self.0.lost.load(Ordering::Acquire) {
                callback(elapsed);
            }
        } else {
            state.1.push(Box::new(callback));
        }
    }

    pub(crate) fn ready(&self) -> Result<bool> {
        anyhow::ensure!(
            !self.0.lost.load(Ordering::Acquire),
            "the GPU device was lost; recreate this output"
        );
        if self.0.state.lock().0.is_some() {
            return Ok(true);
        }
        anyhow::ensure!(
            self.0.submitted.elapsed() < Duration::from_secs(1),
            "the GPU did not finish drawing a frame within one second"
        );
        Ok(false)
    }

    /// Only for final draining or a private output worker, never the compositor.
    pub(crate) fn wait(&self) -> Result<()> {
        self.ready()?;
        let mut state = self.0.state.lock();
        self.0.changed.wait_while_for(
            &mut state,
            |state| state.0.is_none(),
            Duration::from_secs(1).saturating_sub(self.0.submitted.elapsed()),
        );
        anyhow::ensure!(state.0.is_some(), "the GPU did not finish drawing a frame");
        Ok(())
    }
}

/// A producer can be at most three pictures ahead of GPU completion.
/// Holding its input frames cannot grow without bound during a driver stall.
pub const MAX_FRAMES_IN_FLIGHT: usize = 3;

#[derive(Default)]
pub(crate) struct InFlight(Vec<Completion>);

impl InFlight {
    pub(crate) fn available(&mut self) -> Result<bool> {
        for done in &self.0 {
            done.ready()?;
        }
        self.0.retain(|done| !done.ready().unwrap_or(false));
        Ok(self.0.len() < MAX_FRAMES_IN_FLIGHT)
    }

    pub(crate) fn submit(&mut self, gpu: &Gpu, commands: wgpu::CommandBuffer) -> Completion {
        // wgpu can run completion callbacks on ANY submitting thread.
        // Preserve this producer's tick order even if a later callback
        // starts on another thread before an earlier one has published.
        let done = gpu.submit(commands, self.0.last().cloned());
        self.0.push(done.clone());
        done
    }

    pub(crate) fn drain(&mut self) -> Result<()> {
        for done in &self.0 {
            done.wait()?;
        }
        self.0.clear();
        Ok(())
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        for done in &self.0 {
            done.0.cancelled.store(true, Ordering::Release);
        }
    }
}

/// A frame whose pixels must not be read until its submission completes.
#[derive(Clone)]
pub(crate) struct PendingFrame<F> {
    frame: F,
    pub(crate) done: Option<Completion>,
}

impl<F: Clone + Send + 'static> PendingFrame<F> {
    pub(crate) fn new(frame: F, done: Completion) -> Self {
        // A caller may abandon the pending result (stop, resize or error).
        // Its native output buffer must still not return to the pool early.
        let held = frame.clone();
        done.then(move |_| drop(held));
        Self {
            frame,
            done: Some(done),
        }
    }

    pub(crate) fn ready(frame: F) -> Self {
        Self { frame, done: None }
    }

    pub(crate) fn into_completed(self) -> Result<F> {
        if let Some(done) = self.done {
            done.wait()?;
            done.ready()?; // A device lost during the wait cannot publish pixels.
        }
        Ok(self.frame)
    }

    pub(crate) fn then(self, send: impl FnOnce(F, Duration) + Send + 'static) {
        match self.done {
            Some(done) => done.then(move |elapsed| send(self.frame, elapsed)),
            None => send(self.frame, Duration::ZERO),
        }
    }
}

/// Run a wgpu future to completion; on native back ends they are ready at
/// once.
fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = TaskContext::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
        std::thread::yield_now();
    }
}

/// A video frame shown to the GPU as textures, without copying it. The
/// textures keep the frame alive until the GPU is done with them (except
/// those from [`FrameImporter::import_cached`]).
#[derive(Clone)]
pub struct GpuFrame {
    pub width: u32,
    pub height: u32,
    pub pixels: GpuPixels,
}

impl GpuFrame {
    /// What its draw records need to know about its texels.
    pub(crate) fn texels(&self) -> Texels {
        Texels {
            width: self.width,
            height: self.height,
            kind: match &self.pixels {
                GpuPixels::Rgb(_) => TexelKind::Rgb,
                GpuPixels::Premultiplied(_) => TexelKind::Premultiplied,
                GpuPixels::YCbCr { coding, .. } | GpuPixels::YCbCrBiPlanar { coding, .. } => {
                    TexelKind::YCbCr(*coding)
                }
            },
        }
    }

    /// Backing textures used as cache identities. A multi-planar allocation
    /// appears twice; the renderer selects its distinct plane aspects.
    pub(crate) fn planes(&self) -> (&wgpu::Texture, &wgpu::Texture) {
        match &self.pixels {
            GpuPixels::Rgb(texture) | GpuPixels::Premultiplied(texture) => (texture, texture),
            GpuPixels::YCbCr { luma, chroma, .. } => (luma, chroma),
            GpuPixels::YCbCrBiPlanar { texture, .. } => (texture, texture),
        }
    }
}

#[derive(Clone)]
pub enum GpuPixels {
    /// RGB in one plane (any channel order the texture format names).
    Rgb(wgpu::Texture),
    /// RGB with premultiplied alpha, like a canvas a scene was drawn on.
    Premultiplied(wgpu::Texture),
    /// 4:2:0 Y'CbCr: a full-size luma plane and a half-size plane of
    /// interleaved Cb and Cr.
    YCbCr {
        luma: wgpu::Texture,
        chroma: wgpu::Texture,
        coding: YCbCr,
    },
    /// One real NV12 allocation, sampled through Plane0/R8Unorm and
    /// Plane1/Rg8Unorm views. Never independently tracked alias textures.
    YCbCrBiPlanar {
        texture: wgpu::Texture,
        coding: YCbCr,
    },
}

/// How Y'CbCr samples encode colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct YCbCr {
    pub matrix: Matrix,
    /// Full range (0–255 for 8 bits) rather than video range (16–235).
    pub full_range: bool,
    /// Bits per sample, kept in the high bits of 8- or 16-bit texels.
    pub bits: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Matrix {
    Bt601,
    Bt709,
    Bt2020,
}

/// Shows a platform's frames to the GPU without copying them.
pub trait FrameImporter: Send {
    type Frame;

    fn import(&mut self, frame: &Self::Frame) -> Result<GpuFrame>;

    /// Show an 8-bit BGRA frame (from a frame pool) to the GPU as a
    /// `Bgra8Unorm` texture it can draw into.
    fn target(&mut self, frame: &Self::Frame) -> Result<wgpu::Texture>;

    /// A BGRA render target cached by surface, without retaining the pool
    /// buffer. The caller holds the frame through GPU completion.
    fn target_cached(&mut self, frame: &Self::Frame) -> Result<Option<wgpu::Texture>> {
        let _ = frame;
        Ok(None)
    }

    /// Show an 8-bit 4:2:0 Y'CbCr frame (from a Y'CbCr frame pool) to the
    /// GPU as textures it can draw into: its luma plane (`R8Unorm`) and its
    /// half-size plane of interleaved Cb and Cr (`Rg8Unorm`).
    fn ycbcr_target(&mut self, frame: &Self::Frame) -> Result<(wgpu::Texture, wgpu::Texture)>;

    /// [`FrameImporter::import`], but with textures kept for the frame's
    /// surface and given again for every frame in it: capture and decoder
    /// pools recycle a few surfaces, so after the first second every frame
    /// finds its textures (and the compositor their bind groups). The
    /// textures don't keep the frame alive: the caller holds the frame until
    /// the GPU has drawn from it. `None` if the platform can't (then use
    /// `import`).
    fn import_cached(&mut self, frame: &Self::Frame) -> Result<Option<GpuFrame>> {
        let _ = frame;
        Ok(None)
    }

    /// [`FrameImporter::ycbcr_target`], with the textures kept for the
    /// frame's surface in the same way; `None` if the platform can't.
    fn ycbcr_target_cached(
        &mut self,
        frame: &Self::Frame,
    ) -> Result<Option<(wgpu::Texture, wgpu::Texture)>> {
        let _ = frame;
        Ok(None)
    }
}
