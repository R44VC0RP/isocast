//! A camera is an independent, bounded output. Its worker never blocks a
//! compositor, recording or stream. There is no work while it is off.
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::JoinHandle;
use std::time::Duration;

use super::program::{Compositor, Feed};
use super::scenes::ProgramScenes;
use crate::gpu::{FrameImporter, Gpu, InFlight, PendingFrame};
use crate::pacing::Wake;
use crate::render::{Layer, Renderer};
use crate::{FramePool, Platform, Time};
use anyhow::Result;
use isocast_protocol::virtual_camera::{VirtualCameraState, VirtualCameraStatus};

/// The native sink must enqueue without waiting for a camera application.
/// False means its bounded queue is full. It retains accepted frames until
/// consumers finish; Drop disconnects the producer, restoring the card.
pub trait CameraSink<F>: Send {
    fn send(&mut self, frame: &F, pts: Time) -> Result<bool>;
    /// Re-send only an already delivered, converted frame. No input/GPU work.
    fn repeat_last(&mut self) -> Result<bool> {
        Ok(false)
    }
    fn close(&mut self) -> Result<()> {
        Ok(())
    }
    fn transport(&self) -> &'static str {
        "cmio-iosurface"
    }
    /// Human-readable destination from the opened device, if available.
    fn destination(&self) -> Option<&str> {
        None
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
    input: impl Fn(Time, u32) -> Result<Option<Input<P::Frame>>> + Send + 'static,
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
                if !P::CAMERA_IS_MONITOR {
                    platform.real_time_thread();
                }
                on_change(status.lock().clone());
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                        let initial = status.lock().clone();
                        let sink = platform.virtual_camera_sink(
                            initial.width,
                            initial.height,
                            initial.fps,
                        )?;
                        let size = (initial.width, initial.height);
                        // Don't compile GPU pipelines at start on a governed camera:
                        // a held camera and a matching program canvas need no draw.
                        let draw = (!P::CAMERA_IS_MONITOR)
                            .then(|| Draw::new(&platform, size))
                            .transpose()?;
                        if stop.load(Ordering::Acquire) {
                            return Ok(());
                        }
                        {
                            let mut status = status.lock();
                            status.transport = sink.transport().into();
                            status.destination = sink.destination().map(str::to_owned);
                            status.state = VirtualCameraState::Running;
                        }
                        on_change(status.lock().clone());
                        // At most three scene draws and three camera draws
                        // in flight. Both wake this worker as soon as ready.
                        let (completed, ready) = sync_channel(6);
                        let mut output = Output {
                            draw,
                            sink,
                            completed,
                            ready,
                            size,
                        };
                        let result = run(
                            &platform,
                            &clocks,
                            &status,
                            &stop,
                            &input,
                            &mut output,
                            &on_change,
                        );
                        let closed = output.sink.close();
                        result.and(closed)
                        // Output destruction, including bounded native close, is here.
                    }))
                    .unwrap_or_else(|_| {
                        Err(anyhow::anyhow!("virtual_camera: output worker panicked"))
                    });
                let final_status = {
                    let mut status = status.lock();
                    status.governor_fps = None;
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
    draw: Option<Draw<P>>,
    sink: Box<dyn CameraSink<P::Frame>>,
    completed: SyncSender<Ready<P::Frame>>,
    ready: Receiver<Ready<P::Frame>>,
    size: (u32, u32),
}

struct Draw<P: Platform> {
    gpu: Gpu,
    renderer: Renderer,
    importer: P::Importer,
    pool: P::FramePool,
    in_flight: InFlight,
}

impl<P: Platform> Draw<P> {
    fn new(platform: &P, (width, height): (u32, u32)) -> Result<Self> {
        let gpu = Gpu::for_platform(platform)?;
        Ok(Self {
            renderer: Renderer::new(&gpu, wgpu::TextureFormat::Bgra8Unorm),
            importer: platform.frame_importer(&gpu)?,
            pool: platform.frame_pool(width, height)?,
            gpu,
            in_flight: InFlight::default(),
        })
    }
}

enum Ready<F> {
    Scene(F, Time),
    Camera(F, Time),
}

fn run<P: Platform>(
    platform: &P,
    clocks: &crate::pacing::Clocks,
    status: &Arc<Mutex<VirtualCameraStatus>>,
    stop: &AtomicBool,
    input: &impl Fn(Time, u32) -> Result<Option<Input<P::Frame>>>,
    output: &mut Output<P>,
    on_change: &OnCameraChange,
) -> Result<()> {
    let initial = status.lock().clone();
    let fps = initial.fps;
    let period = Duration::from_secs(1) / fps;
    let mut clock = clocks.start("virtual_camera".into(), period);
    let mut scene: Option<(Compositor<P>, u64, (u32, u32))> = None;
    // Prepare a named scene's extra compositor before its frame clock starts.
    if !P::CAMERA_IS_MONITOR
        && initial.settings.target == isocast_protocol::virtual_camera::Target::Scene
        && let Some(Input::Scene(scenes, _)) = input(Time::from_nanos(0), fps)?
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
    let mut limit = None;
    loop {
        let tick = epoch.add_ticks(next, fps);
        while let Ok(ready) = output.ready.try_recv() {
            receive(platform, status, output, ready, &mut latencies, fps)?;
        }
        // GPU completions hand frames over while this waits for the tick;
        // the clock still records one truthful wake per tick.
        clock.wait_until(platform, tick, |limit| -> Result<Wake> {
            if stop.load(Ordering::Acquire) {
                return Ok(Wake::Stop);
            }
            match output.ready.recv_timeout(limit) {
                Ok(ready) => {
                    receive(platform, status, output, ready, &mut latencies, fps)?;
                    Ok(Wake::HandOff)
                }
                Err(_) => Ok(Wake::Timeout),
            }
        })?;
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        // Skip overdue ticks, never replay a backlog into the sink.
        let now = platform.now();
        let rate = if P::CAMERA_IS_MONITOR {
            crate::governor::monitor_fps(fps)
        } else {
            fps
        };
        if now > tick + period * 2 {
            let current = now.ticks_since(epoch, fps).max(0) as u64;
            let mut counters = status.lock();
            if !P::CAMERA_IS_MONITOR || counters.frames_sent > 0 {
                counters.frames_dropped += overdue(next, current, fps, rate);
            }
            next = current;
            continue;
        }
        let n = next;
        next += 1;
        let reduced = (rate < fps).then_some(rate);
        if reduced != limit {
            limit = reduced;
            let snapshot = {
                let mut status = status.lock();
                status.governor_fps = limit;
                status.clone()
            };
            on_change(snapshot);
        }
        // Stay on the existing clock's grid; register no compositor demand for
        // skipped ticks. Keep the negotiated device format even at zero live fps.
        if !due(n, fps, rate) {
            continue;
        }
        if rate == 0 {
            repeat(status, output)?;
            continue;
        }
        let Some(input) = input(after, rate)? else {
            if P::CAMERA_IS_MONITOR {
                // Camera clients consume a fixed cadence. Until there is a
                // first picture we have nothing to send; thereafter a late
                // canvas repeats the last accepted bytes, without readback.
                repeat(status, output)?;
            } else {
                status.lock().frames_dropped += 1;
            }
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
                let Some((frame, _)) =
                    compositor.draw(Feed::Preview, Some(&selected), now, now, true)?
                else {
                    status.lock().frames_dropped += 1;
                    continue;
                };
                let ready = output.completed.clone();
                let status = status.clone();
                frame.then(move |(rgb, _), _| {
                    if ready.try_send(Ready::Scene(rgb, tick)).is_err() {
                        status.lock().frames_dropped += 1;
                    }
                });
                after = tick;
                continue;
            }
        };
        after = pts;
        if P::CAMERA_IS_MONITOR
            && platform.frame_size(&frame) == output.size
            && initial.settings.target != isocast_protocol::virtual_camera::Target::Source
        {
            // This canvas is already composited at the negotiated size.
            send(platform, status, output, frame, pts, &mut latencies, fps)?;
            continue;
        }
        draw(platform, status, output, frame, pts)?;
    }
}

fn due(tick: u64, fps: u32, live_fps: u32) -> bool {
    let cadence = cadence(fps, live_fps);
    (tick % u64::from(fps)) as u32 * cadence % fps < cadence
}

fn cadence(fps: u32, live_fps: u32) -> u32 {
    if live_fps == 0 { 2.min(fps) } else { live_fps }
}

/// Count only scheduled sends in [start, end). Whole seconds have exactly
/// `cadence` sends, so a long system sleep never requires walking the backlog.
fn overdue(start: u64, end: u64, fps: u32, live_fps: u32) -> u64 {
    let ticks = end.saturating_sub(start);
    let second = u64::from(fps);
    ticks / second * u64::from(cadence(fps, live_fps))
        + (start..start + ticks % second)
            .filter(|&n| due(n, fps, live_fps))
            .count() as u64
}

fn repeat<P: Platform>(status: &Mutex<VirtualCameraStatus>, output: &mut Output<P>) -> Result<()> {
    let sent = output.sink.repeat_last()?;
    let mut status = status.lock();
    if sent {
        status.frames_sent += 1;
        status.frames_repeated += 1;
    } else if status.frames_sent != 0 {
        status.frames_dropped += 1;
    }
    Ok(())
}

fn draw<P: Platform>(
    platform: &P,
    status: &Arc<Mutex<VirtualCameraStatus>>,
    output: &mut Output<P>,
    frame: P::Frame,
    pts: Time,
) -> Result<()> {
    let size = output.size;
    let draw = match &mut output.draw {
        Some(draw) => draw,
        slot @ None => slot.insert(Draw::new(platform, size)?),
    };
    let Some(frame) = P::retain_frame(&frame, crate::FrameUse::Auxiliary) else {
        status.lock().frames_dropped += 1;
        return Ok(());
    };
    if !draw.in_flight.available()? {
        status.lock().frames_dropped += 1;
        return Ok(());
    }
    let Some(buffer) = draw.pool.take()? else {
        status.lock().frames_dropped += 1;
        return Ok(());
    };
    let picture = draw.importer.import(&frame)?;
    let target = draw.importer.target(&buffer)?;
    let mut layer = Layer::full(&picture, size.0, size.1);
    layer.fit = isocast_protocol::Fit::Contain;
    layer.background = Some([0.0, 0.0, 0.0, 1.0]);
    let mut encoder = draw.gpu.device.create_command_encoder(&Default::default());
    draw.renderer.encode(
        &draw.gpu,
        &mut encoder,
        &target,
        &[layer],
        size,
        wgpu::Color::BLACK,
    );
    let done = draw.in_flight.submit(&draw.gpu, encoder.finish());
    let completed = output.completed.clone();
    let status = status.clone();
    PendingFrame::new(buffer, done.clone()).then(move |buffer, _| {
        let _held = (frame, picture);
        if completed.try_send(Ready::Camera(buffer, pts)).is_err() {
            status.lock().frames_dropped += 1;
        }
    });
    Ok(())
}

fn receive<P: Platform>(
    platform: &P,
    status: &Arc<Mutex<VirtualCameraStatus>>,
    output: &mut Output<P>,
    ready: Ready<P::Frame>,
    latencies: &mut Vec<f64>,
    fps: u32,
) -> Result<()> {
    // A draw queued before the pause must not trigger a new draw or readback.
    if P::CAMERA_IS_MONITOR && crate::governor::monitor_fps(fps) == 0 {
        return Ok(());
    }
    match ready {
        Ready::Scene(frame, pts) => draw(platform, status, output, frame, pts),
        Ready::Camera(frame, pts) => send(platform, status, output, frame, pts, latencies, fps),
    }
}

fn send<P: Platform>(
    platform: &P,
    status: &Mutex<VirtualCameraStatus>,
    output: &mut Output<P>,
    buffer: P::Frame,
    pts: Time,
    latencies: &mut Vec<f64>,
    fps: u32,
) -> Result<()> {
    let sent = output.sink.send(&buffer, pts)?;
    let mut status = status.lock();
    if sent {
        status.frames_sent += 1;
        latencies.push(platform.now().saturating_since(pts).as_secs_f64() * 1e6);
        if latencies.len() >= 600 || latencies.len().is_multiple_of(fps as usize) {
            let mut sorted = latencies.clone();
            sorted.sort_by(f64::total_cmp);
            status.enqueue_latency_us.p50 = sorted[sorted.len() / 2];
            status.enqueue_latency_us.p99 = sorted[(sorted.len() * 99 / 100).min(sorted.len() - 1)];
            if latencies.len() >= 600 {
                latencies.clear();
            }
        }
    } else {
        status.frames_dropped += 1;
    }
    Ok(())
}

#[cfg(test)]
mod cadence_tests {
    use super::{due, overdue};

    #[test]
    fn overdue_counts_only_sends_due_before_the_wake_tick() {
        for (rate, missed) in [(30, 30), (15, 15), (5, 5), (0, 2)] {
            assert_eq!(overdue(1, 31, 30, rate), missed);
        }
        // A partial second off-grid, across a second boundary and at a
        // non-divisor rate; the wake tick itself hasn't been missed.
        for rate in [0, 5, 15, 24] {
            for start in 0..24 {
                for end in start..=start + 50 {
                    assert_eq!(
                        overdue(start, end, 24, rate),
                        (start..end).filter(|&n| due(n, 24, rate)).count() as u64
                    );
                }
            }
        }
    }

    #[test]
    fn reduced_and_held_camera_stay_on_the_canvas_grid_and_recover_without_backlog() {
        for fps in [1, 24, 25, 30, 60, 120] {
            for (sec, cap) in [fps, fps.min(15), fps.min(5), 0, fps]
                .into_iter()
                .enumerate()
            {
                let start = sec as u64 * u64::from(fps);
                let n = (start..start + u64::from(fps))
                    .filter(|tick| due(*tick, fps, cap))
                    .count();
                assert_eq!(n as u32, if cap == 0 { fps.min(2) } else { cap });
            }
        }
    }
}
