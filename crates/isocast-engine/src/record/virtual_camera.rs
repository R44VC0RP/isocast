//! A camera is an independent, bounded output. Its worker never blocks a
//! compositor, recording or stream. There is no work while it is off.
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use super::program::{Compositor, Feed};
use super::scenes::ProgramScenes;
use crate::gpu::{FrameImporter, Gpu};
use crate::render::{Layer, Renderer};
use crate::{FramePool, Platform, Time};
use anyhow::Result;
use isocast_protocol::virtual_camera::{VirtualCameraState, VirtualCameraStatus};

/// The native sink must enqueue without waiting for a camera application.
/// False means its bounded queue is full. It retains accepted frames until
/// consumers finish; Drop disconnects the producer, restoring the card.
pub trait CameraSink<F>: Send {
    fn send(&mut self, frame: &F, pts: Time) -> Result<bool>;
    fn close(&mut self) -> Result<()> {
        Ok(())
    }
    fn transport(&self) -> &'static str {
        "cmio-iosurface"
    }
}

pub(super) enum Input<F> {
    Frame(F, Time),
    Scene(Arc<ProgramScenes<F>>, String),
}

pub type OnCameraChange = Arc<dyn Fn(VirtualCameraStatus) + Send + Sync>;

pub struct VirtualCamera {
    status: Arc<Mutex<VirtualCameraStatus>>,
    stop: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
    timed_out: AtomicBool,
}

impl VirtualCamera {
    pub fn status(&self) -> VirtualCameraStatus {
        self.status.lock().clone()
    }
    pub fn finished(&self) -> bool {
        self.thread
            .lock()
            .as_ref()
            .is_none_or(JoinHandle::is_finished)
    }

    pub fn wait_ready(&self) -> VirtualCameraStatus {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while self.status().state == VirtualCameraState::Starting {
            if std::time::Instant::now() >= deadline {
                self.stop.store(true, Ordering::Release);
                self.fail_timeout("virtual_camera: startup exceeded 3 seconds");
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        self.status()
    }

    fn fail_timeout(&self, message: &str) {
        self.timed_out.store(true, Ordering::Release);
        let mut status = self.status.lock();
        status.state = VirtualCameraState::Failed;
        status.last_error = Some(message.into());
    }

    /// Bounded even if a GPU driver fails to return. Keep the unfinished
    /// handle in the controller, refusing another worker until it finishes.
    pub fn stop(&self) -> VirtualCameraStatus {
        self.stop.store(true, Ordering::Release);
        {
            let mut status = self.status.lock();
            if matches!(
                status.state,
                VirtualCameraState::Starting | VirtualCameraState::Running
            ) {
                status.state = VirtualCameraState::Stopping;
            }
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !self.finished() && !self.timed_out.load(Ordering::Acquire) {
            if std::time::Instant::now() >= deadline {
                self.fail_timeout("virtual_camera: worker stop exceeded 3 seconds; restart is refused until it exits");
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let finished = if self.finished() {
            self.thread.lock().take()
        } else {
            None
        };
        if let Some(thread) = finished {
            let _ = thread.join(); // Already finished; no unbounded join.
        }
        self.status()
    }
}

impl Drop for VirtualCamera {
    fn drop(&mut self) {
        self.stop();
    }
}

pub(super) fn start<P: Platform>(
    platform: &P,
    clocks: Arc<crate::pacing::Clocks>,
    mut status: VirtualCameraStatus,
    input: impl Fn(Time) -> Result<Option<Input<P::Frame>>> + Send + 'static,
    on_change: OnCameraChange,
) -> Result<VirtualCamera> {
    // All native/GPU setup and destruction belong to the worker, never to
    // the daemon's state lock. Running is announced only after setup succeeds.
    status.state = VirtualCameraState::Starting;
    let status = Arc::new(Mutex::new(status));
    let stop = Arc::new(AtomicBool::new(false));
    let platform = platform.clone();
    let thread = std::thread::Builder::new()
        .name("virtual camera".into())
        .spawn({
            let status = status.clone();
            let stop = stop.clone();
            move || {
                platform.real_time_thread();
                on_change(status.lock().clone());
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                        let initial = status.lock().clone();
                        let sink = platform.virtual_camera_sink(
                            initial.width,
                            initial.height,
                            initial.fps,
                        )?;
                        let gpu = Gpu::new()?;
                        let renderer = Renderer::new(&gpu, wgpu::TextureFormat::Bgra8Unorm);
                        let importer = platform.frame_importer(&gpu)?;
                        let pool = platform.frame_pool(initial.width, initial.height)?;
                        if stop.load(Ordering::Acquire) {
                            return Ok(());
                        }
                        {
                            let mut status = status.lock();
                            status.transport = sink.transport().into();
                            status.state = VirtualCameraState::Running;
                        }
                        on_change(status.lock().clone());
                        let mut output = Output {
                            gpu,
                            renderer,
                            importer,
                            pool,
                            sink,
                        };
                        let result = run(&platform, &clocks, &status, &stop, &input, &mut output);
                        let closed = output.sink.close();
                        result.and(closed)
                        // Output destruction, including bounded native close, is here.
                    }))
                    .unwrap_or_else(|_| {
                        Err(anyhow::anyhow!("virtual_camera: output worker panicked"))
                    });
                let final_status = {
                    let mut status = status.lock();
                    match result {
                        _ if status.state == VirtualCameraState::Failed => {}
                        Ok(()) => status.state = VirtualCameraState::Stopped,
                        Err(error) => {
                            status.state = VirtualCameraState::Failed;
                            status.last_error = Some(format!("{error:#}"));
                        }
                    }
                    status.clone()
                };
                on_change(final_status);
            }
        })?;
    Ok(VirtualCamera {
        status,
        stop,
        thread: Mutex::new(Some(thread)),
        timed_out: AtomicBool::new(false),
    })
}

struct Output<P: Platform> {
    gpu: Gpu,
    renderer: Renderer,
    importer: P::Importer,
    pool: P::FramePool,
    sink: Box<dyn CameraSink<P::Frame>>,
}

fn run<P: Platform>(
    platform: &P,
    clocks: &crate::pacing::Clocks,
    status: &Mutex<VirtualCameraStatus>,
    stop: &AtomicBool,
    input: &impl Fn(Time) -> Result<Option<Input<P::Frame>>>,
    output: &mut Output<P>,
) -> Result<()> {
    let initial = status.lock().clone();
    let size = (initial.width, initial.height);
    let fps = initial.fps;
    let period = Duration::from_secs(1) / fps;
    let mut clock = clocks.start("virtual_camera".into(), period);
    let mut scene: Option<(Compositor<P>, u64, (u32, u32))> = None;
    // Prepare a named scene's extra compositor before its frame clock starts.
    if initial.settings.target == isocast_protocol::virtual_camera::Target::Scene
        && let Some(Input::Scene(scenes, _)) = input(Time::from_nanos(0))?
    {
        let canvas = scenes.compiled.canvas;
        let mut compositor = Compositor::new(platform, canvas)?;
        let version = scenes.version;
        compositor.adopt(scenes, platform.now());
        scene = Some((compositor, version, canvas));
    }
    let epoch = platform.now();
    let mut next = 0;
    let mut after = Time::from_nanos(0);
    let mut latencies = Vec::with_capacity(600);
    loop {
        let tick = epoch.add_ticks(next, fps);
        while platform.now() < tick && !stop.load(Ordering::Acquire) {
            clock.sleep_until(
                platform,
                tick.min(platform.now() + Duration::from_millis(10)),
            );
        }
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        // Skip overdue ticks, never replay a backlog into the sink.
        let now = platform.now();
        if now > tick + period * 2 {
            let current = now.ticks_since(epoch, fps).max(0) as u64;
            status.lock().frames_dropped += current.saturating_sub(next);
            next = current;
            continue;
        }
        next += 1;
        let Some(input) = input(after)? else {
            status.lock().frames_dropped += 1;
            continue;
        };
        let (frame, pts) = match input {
            Input::Frame(frame, time) => (
                frame,
                if initial.settings.target == isocast_protocol::virtual_camera::Target::Source {
                    tick
                } else {
                    time
                },
            ),
            Input::Scene(scenes, id) => {
                let canvas = scenes.compiled.canvas;
                if scene.as_ref().is_none_or(|(_, _, size)| *size != canvas) {
                    scene = Some((Compositor::new(platform, canvas)?, 0, canvas));
                }
                let (compositor, version, _) = scene.as_mut().unwrap();
                if scenes.version != *version {
                    *version = scenes.version;
                    compositor.adopt(scenes, now);
                }
                let selected = super::OnAir {
                    scene: id.into(),
                    from: None,
                };
                let Some((frame, _)) = compositor.draw(Feed::Preview, Some(&selected), now, now)?
                else {
                    status.lock().frames_dropped += 1;
                    continue;
                };
                (frame, tick)
            }
        };
        after = pts;
        let Some(buffer) = output.pool.take()? else {
            status.lock().frames_dropped += 1;
            continue;
        };
        let picture = output.importer.import(&frame)?;
        let target = output.importer.target(&buffer)?;
        let mut layer = Layer::full(&picture, size.0, size.1);
        layer.fit = isocast_protocol::Fit::Contain;
        layer.background = Some([0.0, 0.0, 0.0, 1.0]);
        let submitted = output.renderer.render(&output.gpu, &target, &[layer], size);
        output.gpu.device.poll(wgpu::PollType::Wait {
            submission_index: Some(submitted),
            timeout: Some(Duration::from_secs(1)),
        })?;
        let sent = output.sink.send(&buffer, pts)?;
        let mut status = status.lock();
        if sent {
            status.frames_sent += 1;
            latencies.push(platform.now().saturating_since(pts).as_secs_f64() * 1e6);
            if latencies.len() >= 600 || latencies.len() % fps as usize == 0 {
                let mut sorted = latencies.clone();
                sorted.sort_by(f64::total_cmp);
                status.enqueue_latency_us.p50 = sorted[sorted.len() / 2];
                status.enqueue_latency_us.p99 =
                    sorted[(sorted.len() * 99 / 100).min(sorted.len() - 1)];
                if latencies.len() >= 600 {
                    latencies.clear();
                }
            }
        } else {
            status.frames_dropped += 1;
        }
    }
}
