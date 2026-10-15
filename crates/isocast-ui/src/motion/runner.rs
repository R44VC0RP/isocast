//! One run: the real interface offscreen on a virtual display, a scripted
//! interaction replayed into it a frame at a time, and what each frame
//! showed recorded (see docs/motion-testing.md).

use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::{Rc, Weak};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use i_slint_backend_testing::ElementHandle;
use isocast_protocol::api::EngineState;
use isocast_protocol::api::Event;
use slint::platform::skia_renderer::SkiaWGPU30Renderer;
use slint::platform::{Platform, PointerEventButton, Renderer, WindowAdapter, WindowEvent};
use slint::wgpu_30::wgpu;
use slint::{ComponentHandle, LogicalPosition, LogicalSize, PhysicalSize};

use super::measure::{Crop, Frame, Image, Tracker};
use super::scenario::{Run, ease};
use crate::app::{App, Mode};
use crate::areas;
use crate::daemon::{self, Request, Update};
use crate::frames::Incoming;
use crate::gpu::Gpu;
use crate::{AppWindow, CanvasArea, Theme, Ui};

struct MotionWindow {
    window: slint::Window,
    renderer: SkiaWGPU30Renderer,
    size: Cell<PhysicalSize>,
    /// Times Slint asked for the window to be drawn again.
    redraws: Cell<u64>,
}

impl WindowAdapter for MotionWindow {
    fn window(&self) -> &slint::Window {
        &self.window
    }

    fn size(&self) -> PhysicalSize {
        self.size.get()
    }

    fn set_size(&self, size: slint::WindowSize) {
        let scale = self.window.scale_factor();
        self.size.set(size.to_physical(scale));
        self.window.dispatch_event(WindowEvent::Resized {
            size: size.to_logical(scale),
        });
    }

    fn renderer(&self) -> &dyn Renderer {
        &self.renderer
    }

    fn request_redraw(&self) {
        self.redraws.set(self.redraws.get() + 1);
    }
}

/// A display whose clock moves only when the run says so.
struct MotionPlatform {
    window: Rc<MotionWindow>,
    now: Rc<Cell<Duration>>,
}

impl Platform for MotionPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }

    fn duration_since_start(&self) -> Duration {
        self.now.get()
    }
}

/// Calls to the engine, as the interface made them.
#[derive(Default)]
pub struct Calls {
    pub total: u64,
    pub bytes: u64,
    pub by_method: BTreeMap<String, (u64, u64)>,
}

/// How long lockstep waits on the wall clock for the engine before calling
/// the run invalid.
const SYNC_LIMIT: Duration = Duration::from_secs(2);

/// A fault put in on purpose (`inject` in a scenario), so the harness can
/// be shown to catch it: `lag:N` (inputs reach the app N frames late),
/// `jump:F` (frame F's pointer 30 points off), `blank:F` (frame F not
/// drawn), `freeze:F` (the pointer's moves in frames F to F+9 never reach
/// the app: its edge stands still while the pointer goes on).
#[derive(Default)]
struct Inject {
    lag: usize,
    jump: Option<usize>,
    blank: Option<usize>,
    freeze: Option<usize>,
}

impl Inject {
    fn parse(text: Option<&str>) -> Result<Self> {
        let mut out = Inject::default();
        for part in text.unwrap_or("").split(',').filter(|p| !p.is_empty()) {
            let (kind, n) = part.split_once(':').unwrap_or((part, ""));
            let n: usize = n
                .parse()
                .with_context(|| format!("inject {part:?}: KIND:N, a whole number"))?;
            match kind {
                "lag" => out.lag = n,
                "jump" => out.jump = Some(n),
                "blank" => out.blank = Some(n),
                "freeze" => out.freeze = Some(n),
                other => bail!("inject {other:?} isn't one of lag:N, jump:F, blank:F, freeze:F"),
            }
        }
        Ok(out)
    }
}

/// An input event at a time in the run.
#[derive(Debug, Clone)]
enum Input {
    Move(f32, f32),
    Press(f32, f32),
    Release(f32, f32),
    Wheel(f32, f32, f32, f32),
    Key(String),
    State(String),
}

pub struct Runner {
    gpu: Gpu,
    window: Rc<MotionWindow>,
    app: AppWindow,
    hub: Rc<App>,
    updates: Receiver<Update>,
    pictures: Receiver<(bool, Incoming)>,
    now: Rc<Cell<Duration>>,
    target: Option<wgpu::Texture>,
    run: Run,
    calls: Arc<Mutex<Calls>>,
    /// The newest engine frame's setup revision, and its size.
    picture_revision: Option<u64>,
    picture_size: (u32, u32),
    /// The picture size (pixels) the preview asked the engine for when the
    /// window was last drawn (preview.rs window_drawn).
    asked_size: (u32, u32),
    frames_in: u64,
    pointer: (f32, f32),
    pressed: bool,
}

/// A replay's frames, their crops, the engine waits that hit their limit
/// and the tracker's scan line.
type Replayed = (Vec<Frame>, Vec<Crop>, u64, Option<f32>);

/// What a finished run recorded.
pub struct Recorded {
    pub frames: Vec<Frame>,
    pub images: Vec<Crop>,
    pub calls: Calls,
    /// Lockstep: times the engine didn't catch up within SYNC_LIMIT (any
    /// makes the run invalid).
    pub sync_timeouts: u64,
    pub recording_started: Option<bool>,
    pub gpu: String,
    pub size: (u32, u32),
    pub edits_sent: u64,
    pub most_in_flight: usize,
    pub stale_applies: u64,
    pub preview: crate::preview::Stats,
    /// Where the tracker searched the pixels (see Tracker::scan_line).
    pub scan_line: Option<f32>,
}

pub fn run(run: Run, keep_images: bool, dump: &[usize], dir: &std::path::Path) -> Result<Recorded> {
    let gpu = Gpu::new()?;
    let info = gpu.adapter.get_info();
    let gpu_name = format!("{} ({:?})", info.name, info.backend);
    let (width, height) = run.size;
    let scale = run.scale;
    let renderer = SkiaWGPU30Renderer::new(
        gpu.instance.clone(),
        gpu.adapter.clone(),
        gpu.device.clone(),
        gpu.queue.clone(),
    )
    .map_err(|e| anyhow::anyhow!("Skia could not use the GPU: {e}"))?;
    let window = Rc::new_cyclic(|weak: &Weak<MotionWindow>| MotionWindow {
        window: slint::Window::new(weak.clone()),
        renderer,
        size: Cell::new(PhysicalSize::new(
            (width as f32 * scale).round() as u32,
            (height as f32 * scale).round() as u32,
        )),
        redraws: Cell::new(0),
    });
    let now = Rc::new(Cell::new(Duration::ZERO));
    crate::clock::set_virtual(Duration::ZERO);
    slint::platform::set_platform(Box::new(MotionPlatform {
        window: window.clone(),
        now: now.clone(),
    }))
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    window
        .window
        .dispatch_event(WindowEvent::ScaleFactorChanged {
            scale_factor: scale,
        });
    let app = AppWindow::new()?;
    app.set_native_menu(false);
    window.window.dispatch_event(WindowEvent::Resized {
        size: LogicalSize::new(width as f32, height as f32),
    });
    app.show()?;

    // The interface's calls pass through here on the way to the engine,
    // counted (the interface never waits on them: they go on a channel).
    let (tx, updates) = channel();
    let to_daemon = daemon::start(move |update| {
        let _ = tx.send(update);
    });
    let calls = Arc::new(Mutex::new(Calls::default()));
    let (commands, from_app) = channel::<Request>();
    {
        let calls = calls.clone();
        std::thread::Builder::new()
            .name("motion calls".into())
            .spawn(move || {
                while let Ok(request) = from_app.recv() {
                    let json = serde_json::to_value(&request.method).unwrap_or_default();
                    let name = json
                        .get("method")
                        .and_then(|m| m.as_str())
                        .unwrap_or("?")
                        .to_string();
                    let bytes = json.to_string().len() as u64;
                    if let Ok(mut c) = calls.lock() {
                        c.total += 1;
                        c.bytes += bytes;
                        let entry = c.by_method.entry(name).or_default();
                        entry.0 += 1;
                        entry.1 += bytes;
                    }
                    if to_daemon.send(request).is_err() {
                        break;
                    }
                }
            })?;
    }
    let clock_now = now.clone();
    let clock: Box<dyn Fn() -> f32> = Box::new(move || clock_now.get().as_secs_f32());
    let canvas = areas::canvas::Canvas::new(&gpu, clock, !run.engine_picture);
    let hub = App::new(&app, commands, Mode { offscreen: true }, canvas);
    hub.sources.set_device(gpu.device.clone());
    let (picture_tx, pictures) = channel();

    let mut runner = Runner {
        gpu,
        window,
        app,
        hub,
        updates,
        pictures,
        now,
        target: None,
        run,
        calls,
        picture_revision: None,
        picture_size: (0, 0),
        asked_size: (0, 0),
        frames_in: 0,
        pointer: (0.0, 0.0),
        pressed: false,
    };
    runner.wait_until(Duration::from_secs(10), "the engine to answer", |r| {
        let s = r.hub.session.borrow();
        s.loaded && s.status.is_some()
    })?;
    runner.render()?;
    if runner.run.engine_picture {
        let output = crate::preview::Output::new(
            &runner.app,
            crate::preview::Mode::Image,
            &runner.gpu.device,
            true,
        )?;
        runner
            .hub
            .canvas
            .start_pictures(&runner.hub, output, false, move |program, incoming| {
                let _ = picture_tx.send((program, incoming));
            });
        runner.wait_until(Duration::from_secs(10), "the engine's first frame", |r| {
            r.frames_in > 0
        })?;
    }
    runner.settle(30)?;

    let mut recording_started = None;
    if runner.run.variant == "recording" {
        runner.state("record")?;
        let ok = runner
            .wait_until(Duration::from_secs(15), "the recording to start", |r| {
                r.recording()
            })
            .is_ok();
        recording_started = Some(ok);
        if !ok {
            bail!("the recording didn't start within 15 s (see the engine's log)");
        }
        // Let the encoders get going: their start is not what is measured.
        runner.settle(60)?;
    }
    for state in runner.run.before.clone() {
        runner
            .state(&state)
            .with_context(|| format!("before: {state}"))?;
    }
    runner.settle(20)?;

    let recorded = runner.replay(keep_images, dump, dir);
    // Leave the engine as it was, whatever happened.
    if runner.pressed {
        let (x, y) = runner.pointer;
        runner.dispatch(&Input::Release(x, y));
    }
    if runner.recording() {
        let _ = runner.state("stop-record");
        let _ = runner.wait_until(Duration::from_secs(15), "the recording to stop", |r| {
            !r.recording()
        });
    }
    let (frames, images, waits, scan_line) = recorded?;
    let calls = std::mem::take(&mut *runner.calls.lock().expect("calls"));
    let s = runner.hub.session.borrow();
    let size = runner.window.size.get();
    Ok(Recorded {
        frames,
        images,
        calls,
        sync_timeouts: waits,
        recording_started,
        gpu: gpu_name,
        size: (size.width, size.height),
        edits_sent: s.applies_sent,
        most_in_flight: s.most_in_flight,
        stale_applies: s.applies_stale,
        preview: runner.hub.canvas.preview_stats(),
        scan_line,
    })
}

impl Runner {
    fn recording(&self) -> bool {
        self.hub
            .session
            .borrow()
            .status
            .as_ref()
            .is_some_and(|s| s.state == EngineState::Recording)
    }

    /// Apply what has come from the engine (replies, events, frames).
    /// Lockstep applies a batch's replies before its events: they come on
    /// two connections, so their order is a race otherwise.
    fn drain(&mut self) {
        let mut batch: Vec<Update> = self.updates.try_iter().collect();
        if self.run.lockstep {
            batch.sort_by_key(|u| matches!(u, Update::Event(_)));
        }
        for update in batch {
            // The mixer's levels move with the sound: left out, so a meter
            // isn't taken for flicker.
            if let Update::Event(event) = &update
                && matches!(**event, Event::Levels { .. })
            {
                continue;
            }
            let mut update = update;
            if self.run.lockstep {
                // The engine's live readouts (recording time, counters,
                // CPU, disk) follow the wall clock: stilled, as snapshots
                // still them, so two runs draw the same frames.
                match &mut update {
                    Update::Event(event) if matches!(**event, Event::Status { .. }) => continue,
                    Update::Reply {
                        result: Ok(value), ..
                    } => {
                        still(value);
                        if let Some(object) = value.as_object_mut()
                            && object.contains_key("state")
                        {
                            for key in ["live", "engine"] {
                                if object.contains_key(key) {
                                    object.insert(key.into(), serde_json::Value::Null);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            self.hub.apply(update);
        }
        while let Ok((program, incoming)) = self.pictures.try_recv() {
            if !program && let Incoming::Frame(frame) = &incoming {
                self.frames_in += 1;
                self.picture_revision = frame.revision.or(self.picture_revision);
                self.picture_size = (frame.width(), frame.height());
            }
            self.hub.canvas.incoming(program, incoming);
        }
    }

    /// Pump the engine on the wall clock (and the display's clock with
    /// it) until `done`, for setup steps before anything is measured.
    fn wait_until(
        &mut self,
        limit: Duration,
        what: &str,
        done: impl Fn(&Self) -> bool,
    ) -> Result<()> {
        let deadline = Instant::now() + limit;
        loop {
            self.drain();
            self.hub.flush();
            if done(self) {
                return Ok(());
            }
            if Instant::now() > deadline {
                bail!("gave up waiting for {what} after {limit:?}");
            }
            std::thread::sleep(Duration::from_millis(10));
            // Lockstep: the display's clock doesn't move while waiting on
            // the wall clock (timers that are due still run).
            if self.run.lockstep {
                self.advance(Duration::ZERO);
            } else {
                self.advance(Duration::from_millis(10));
            }
        }
    }

    /// `frames` frames with nothing happening, on the wall clock.
    fn settle(&mut self, frames: u32) -> Result<()> {
        let frame = self.frame_duration();
        for _ in 0..frames {
            std::thread::sleep(frame);
            self.advance(frame);
            self.drain();
            self.hub.flush();
            if self.run.lockstep && !self.sync() {
                bail!("the engine didn't answer within {SYNC_LIMIT:?} while settling");
            }
            self.render()?;
        }
        Ok(())
    }

    fn frame_duration(&self) -> Duration {
        Duration::from_secs_f64(1.0 / self.run.fps as f64)
    }

    fn advance(&mut self, by: Duration) {
        self.advance_to(self.now.get() + by);
    }

    /// The display's clock (and the interface's, src/clock.rs) to `at`,
    /// running the timers due by then.
    fn advance_to(&mut self, at: Duration) {
        if at > self.now.get() {
            self.now.set(at);
        }
        crate::clock::set_virtual(self.now.get());
        slint::platform::update_timers_and_animations();
    }

    /// Lockstep: wait (on the wall clock) until every call the interface
    /// has made is answered, applying what comes back. False if the engine
    /// didn't answer within SYNC_LIMIT: the run is then invalid.
    fn sync(&mut self) -> bool {
        let deadline = Instant::now() + SYNC_LIMIT;
        loop {
            self.drain();
            self.hub.flush();
            if self.hub.calls_pending() == 0 {
                return true;
            }
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// A `--state` step: the areas' own (see each area's STATES), and a
    /// few of the harness's: `select-first-item`, `snap:off`, `hide:DOCK`.
    fn state(&mut self, state: &str) -> Result<()> {
        // The variant's own record/stop-record go through here too.
        super::scenario::check_state(state)?;
        let (name, arg) = state.split_once(':').unwrap_or((state, ""));
        let ui = self.app.global::<Ui>();
        match name {
            "select-first-item" => self.hub.canvas.select_first_item(&self.hub),
            "snap" => ui.set_snap_enabled(arg != "off"),
            "hide" => match arg {
                "scenes" => ui.set_show_scenes(false),
                "sources" => ui.set_show_sources(false),
                "mixer" => ui.set_show_mixer(false),
                "transitions" => ui.set_show_transitions(false),
                "controls" => ui.set_show_controls(false),
                other => {
                    bail!("hide:{other}: one of scenes, sources, mixer, transitions, controls")
                }
            },
            _ => {
                let answer = self
                    .hub
                    .areas()
                    .iter()
                    .find_map(|area| area.state(&self.hub, &self.app, name, arg));
                match answer {
                    Some(result) => result?,
                    None => {
                        let areas: Vec<&str> = areas::STATES
                            .iter()
                            .flat_map(|s| s.iter().copied())
                            .collect();
                        bail!(
                            "unknown state {name:?}; one of select-first-item, snap:off, hide:DOCK, {}",
                            areas.join(", ")
                        )
                    }
                }
            }
        }
        self.hub.flush();
        Ok(())
    }

    fn dispatch(&mut self, input: &Input) {
        let window = &self.window.window;
        match input {
            Input::Move(x, y) => {
                self.pointer = (*x, *y);
                window.dispatch_event(WindowEvent::PointerMoved {
                    position: LogicalPosition::new(*x, *y),
                });
            }
            Input::Press(x, y) => {
                self.pointer = (*x, *y);
                self.pressed = true;
                let position = LogicalPosition::new(*x, *y);
                window.dispatch_event(WindowEvent::PointerMoved { position });
                window.dispatch_event(WindowEvent::PointerPressed {
                    position,
                    button: PointerEventButton::Left,
                });
            }
            Input::Release(x, y) => {
                self.pointer = (*x, *y);
                self.pressed = false;
                window.dispatch_event(WindowEvent::PointerReleased {
                    position: LogicalPosition::new(*x, *y),
                    button: PointerEventButton::Left,
                });
            }
            Input::Wheel(x, y, dx, dy) => {
                window.dispatch_event(WindowEvent::PointerScrolled {
                    position: LogicalPosition::new(*x, *y),
                    delta_x: *dx,
                    delta_y: *dy,
                });
            }
            Input::Key(name) => {
                let text: slint::SharedString = match name.as_str() {
                    "escape" => slint::platform::Key::Escape.into(),
                    "return" => slint::platform::Key::Return.into(),
                    "tab" => slint::platform::Key::Tab.into(),
                    other => other.into(),
                };
                window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
                window.dispatch_event(WindowEvent::KeyReleased { text });
            }
            Input::State(_) => {}
        }
    }

    /// The scripted inputs, timed from the start of the replay.
    fn timeline(&self, grab: (f32, f32)) -> Result<Vec<(f64, Input)>> {
        let mut out = Vec::new();
        let mut t = 0.0f64;
        let mut at = grab;
        for (i, step) in self.run.steps.iter().enumerate() {
            let ms = step.ms.max(0.0);
            let hz = step.hz.unwrap_or(120.0).max(1.0);
            let easing = step.easing.as_deref();
            if let Some(press) = &step.press {
                let p = match press {
                    toml::Value::String(s) if s == "target" => grab,
                    toml::Value::Array(xy) if xy.len() == 2 => {
                        let n = |v: &toml::Value| {
                            v.as_float()
                                .or_else(|| v.as_integer().map(|i| i as f64))
                                .unwrap_or(0.0) as f32
                        };
                        (n(&xy[0]), n(&xy[1]))
                    }
                    other => bail!("steps[{i}].press: \"target\" or [x, y], not {other}"),
                };
                at = p;
                out.push((t, Input::Press(p.0, p.1)));
                t += ms / 1000.0;
            } else if step.move_by.is_some() || step.to.is_some() {
                let from = at;
                let to = match (step.move_by, step.to) {
                    (Some([dx, dy]), _) => (from.0 + dx, from.1 + dy),
                    (_, Some([x, y])) => (x, y),
                    _ => unreachable!("checked when loading"),
                };
                let n = ((ms / 1000.0) * hz).round().max(1.0) as usize;
                for k in 1..=n {
                    let f = ease(easing, k as f64 / n as f64) as f32;
                    out.push((
                        t + ms / 1000.0 * k as f64 / n as f64,
                        Input::Move(from.0 + (to.0 - from.0) * f, from.1 + (to.1 - from.1) * f),
                    ));
                }
                at = to;
                t += ms / 1000.0;
            } else if step.release.is_some() {
                out.push((t, Input::Release(at.0, at.1)));
                t += ms / 1000.0;
            } else if step.wait.is_some() {
                t += ms / 1000.0;
            } else if let Some([dx, dy]) = step.wheel {
                let n = ((ms / 1000.0) * hz).round().max(1.0) as usize;
                for k in 1..=n {
                    out.push((
                        t + ms / 1000.0 * k as f64 / n as f64,
                        Input::Wheel(at.0, at.1, dx / n as f32, dy / n as f32),
                    ));
                }
                t += ms / 1000.0;
            } else if let Some(key) = &step.key {
                out.push((t, Input::Key(key.clone())));
                t += ms / 1000.0;
            } else if let Some(state) = &step.state {
                out.push((t, Input::State(state.clone())));
                t += ms / 1000.0;
            }
        }
        out.push((t, Input::State(String::new())));
        Ok(out)
    }

    /// Replay the steps a frame at a time and record each frame. Frame 0
    /// is drawn before any input: the rest the measurements start from.
    fn replay(
        &mut self,
        keep_images: bool,
        dump: &[usize],
        dir: &std::path::Path,
    ) -> Result<Replayed> {
        let fps = self.run.fps as f64;
        let frame = self.frame_duration();
        let budget = frame.as_secs_f64() * 1000.0;
        let mut tracker = Tracker::new(&self.run.target, self.run.engine_picture)?;
        let inject = Inject::parse(self.run.inject.as_deref())?;
        let noisy = self.calibrate()?;
        self.render()?;
        let grab = tracker
            .grab(self)
            .with_context(|| format!("target {:?} isn't on screen", self.run.target))?;
        self.pointer = grab;
        let mut scheduled = grab;
        // The inputs, a frame after the rest frame (and later still when a
        // lag is injected: what the user did is `scheduled`, what the app
        // gets comes `lag` frames after).
        let first = 1.0 / fps;
        let timeline: Vec<(f64, Input)> = self
            .timeline(grab)?
            .into_iter()
            .map(|(t, input)| (t + first, input))
            .collect();
        let delay = inject.lag as f64 / fps;
        // The contact sheet's view stays put: the middle of the pointer's path.
        let (mut lo, mut hi) = (grab, grab);
        for (_, input) in &timeline {
            if let Input::Move(x, y) | Input::Press(x, y) = input {
                lo = (lo.0.min(*x), lo.1.min(*y));
                hi = (hi.0.max(*x), hi.1.max(*y));
            }
        }
        let focus = ((lo.0 + hi.0) / 2.0, (lo.1 + hi.1) / 2.0);
        let end = timeline.last().map_or(0.0, |(t, _)| *t) + delay + 0.4;
        let total = (end * fps).ceil() as usize + 1;
        let (mut next_scheduled, mut next_dispatch) = (0, 0);
        let mut frames = Vec::with_capacity(total);
        let mut images = Vec::new();
        let mut sync_timeouts = 0;
        let mut redraws = self.window.redraws.get();
        let mut previous: Option<Vec<u64>> = None;
        let mut before_previous: Option<Vec<u64>> = None;
        let mut regions: Vec<[f32; 4]> = Vec::new();
        let wall_start = Instant::now();
        let start = self.now.get();
        let calls_at = |r: &Self| r.calls.lock().map(|c| (c.total, c.bytes)).unwrap_or((0, 0));
        let mut prior_calls = calls_at(self);
        let mut prior_in = self.frames_in;
        // Revisions are counted from the replay's start (the engine numbers
        // them from the clock).
        let base_revision = self.hub.session.borrow().revision;
        for index in 0..total {
            let t = index as f64 / fps;
            let started = Instant::now();
            // What the user did by this frame (for the measurements).
            let (mut press, mut release) = (None, None);
            while next_scheduled < timeline.len() && timeline[next_scheduled].0 <= t + 1e-9 {
                match timeline[next_scheduled].1 {
                    Input::Move(x, y) => scheduled = (x, y),
                    Input::Press(x, y) => {
                        scheduled = (x, y);
                        press = Some((x, y));
                    }
                    Input::Release(x, y) => {
                        scheduled = (x, y);
                        release = Some((x, y));
                    }
                    _ => {}
                }
                next_scheduled += 1;
            }
            // What the app gets: each input at its own instant on the
            // display's clock (lockstep), or all at the frame (realtime).
            while next_dispatch < timeline.len() && timeline[next_dispatch].0 + delay <= t + 1e-9 {
                let (at, input) = timeline[next_dispatch].clone();
                if self.run.lockstep {
                    self.advance_to(start + Duration::from_secs_f64(at + delay));
                    sync_timeouts += u64::from(!self.sync());
                }
                if let Input::State(state) = &input
                    && !state.is_empty()
                {
                    self.state(state)?;
                }
                let input = match (&input, inject.jump) {
                    (Input::Move(x, y), Some(f)) if f == index => Input::Move(x + 30.0, *y),
                    _ => input,
                };
                let frozen = matches!(input, Input::Move(..))
                    && inject.freeze.is_some_and(|f| (f..f + 10).contains(&index));
                if !frozen {
                    self.dispatch(&input);
                }
                if self.run.lockstep {
                    sync_timeouts += u64::from(!self.sync());
                }
                next_dispatch += 1;
            }
            let pressed = self.pressed;
            self.advance_to(start + Duration::from_secs_f64(t));
            self.drain();
            self.hub.flush();
            let ui_time = started.elapsed();

            // The engine: caught up with (lockstep: every call answered and
            // the picture showing the newest setup, or the run is invalid),
            // or as it is now (realtime: the rest of the frame's wall time).
            let wait_start = Instant::now();
            if self.run.lockstep {
                sync_timeouts += u64::from(!self.sync());
                if self.run.engine_picture {
                    let deadline = Instant::now() + SYNC_LIMIT;
                    loop {
                        self.drain();
                        let revision = self.hub.session.borrow().revision;
                        // The newest setup, at the size last asked for (as
                        // the engine fits the canvas in it).
                        let ui = self.app.global::<Ui>();
                        let canvas = (
                            ui.get_canvas_width().max(1) as u32,
                            ui.get_canvas_height().max(1) as u32,
                        );
                        let fits = self.picture_size == fitted(canvas, self.asked_size);
                        if self.picture_revision.is_some_and(|r| r >= revision) && fits {
                            break;
                        }
                        if Instant::now() > deadline {
                            sync_timeouts += 1;
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    sync_timeouts += u64::from(!self.sync());
                }
            } else {
                let due = wall_start + Duration::from_secs_f64((index + 1) as f64 / fps);
                if let Some(rest) = due.checked_duration_since(Instant::now()) {
                    std::thread::sleep(rest);
                }
                self.drain();
            }
            let engine_wait = wait_start.elapsed();
            let flush_start = Instant::now();
            self.hub.flush();
            let flush_time = flush_start.elapsed();

            let render_start = Instant::now();
            let texture = if inject.blank == Some(index) {
                // A frame the interface never drew: the cleared texture.
                self.clear_target()?
            } else {
                self.render()?
            };
            let render_time = render_start.elapsed();
            let read_start = Instant::now();
            let size = (texture.width(), texture.height());
            let rgba = read_pixels(&self.gpu, &texture)?;
            let gpu_wait = read_start.elapsed();

            let image = Image {
                width: size.0,
                height: size.1,
                scale: self.run.scale,
                rgba,
            };
            if dump.contains(&index) {
                super::sheet::write_png(&dir.join(format!("frame-{index:05}.png")), &image)?;
            }
            let probe = tracker.probe(self, &image);
            // The dragged object's area over the last three frames (with a
            // margin for its outline and handles) isn't checked for flicker.
            if let Some(region) = tracker.moving_region(self) {
                regions.push(region);
            }
            let recent = &regions[regions.len().saturating_sub(3)..];
            let tile_pt = 16.0;
            let tiles_across = image
                .width
                .div_ceil(((tile_pt * image.scale) as u32).max(1))
                as usize;
            let moving = |i: usize| {
                let (tx, ty) = (
                    (i % tiles_across) as f32 * tile_pt,
                    (i / tiles_across) as f32 * tile_pt,
                );
                recent.iter().any(|[x0, y0, x1, y1]| {
                    tx + tile_pt > x0 - 8.0
                        && tx < x1 + 8.0
                        && ty + tile_pt > y0 - 8.0
                        && ty < y1 + 8.0
                })
            };
            let tiles = image.tile_hashes(16);
            let flicker: Vec<usize> = match (&before_previous, &previous) {
                (Some(a), Some(b)) => tiles
                    .iter()
                    .zip(a)
                    .zip(b)
                    .enumerate()
                    .filter(|(i, ((now, two_ago), one_ago))| {
                        now == two_ago && now != one_ago && !noisy.contains(i) && !moving(*i)
                    })
                    .map(|(i, _)| i)
                    .collect(),
                _ => Vec::new(),
            };
            let columns = image.width.div_ceil(((16.0 * image.scale) as u32).max(1)) as usize;
            before_previous = previous.take();
            previous = Some(tiles);
            let calls = calls_at(self);
            let picture_rect = self.picture_rect();
            let ms = |d: Duration| d.as_secs_f64() * 1000.0;
            let cpu_ms = ms(ui_time + flush_time + render_time);
            let frame_ms = cpu_ms + ms(gpu_wait);
            let frame_record = Frame {
                index,
                t,
                pointer: scheduled,
                pressed,
                press,
                release,
                model: probe.model,
                shown: probe.shown,
                pixel: probe.pixel,
                outline: probe.outline,
                cpu_ms,
                frame_ms,
                gpu_ms: ms(gpu_wait),
                engine_wait_ms: ms(engine_wait),
                late: frame_ms > budget,
                redraws_requested: self.window.redraws.get() - redraws,
                calls: calls.0 - prior_calls.0,
                call_bytes: calls.1 - prior_calls.1,
                engine_frames: self.frames_in - prior_in,
                picture_revision: self
                    .picture_revision
                    .map(|r| r as i64 - base_revision as i64),
                setup_revision: self.hub.session.borrow().revision as i64 - base_revision as i64,
                black_picture: self.run.engine_picture && image.black_fraction(picture_rect) > 0.95,
                blank_window: image.uniform_fraction() > 0.97,
                aba_tiles: flicker.len() as u32,
                aba_at: flicker
                    .iter()
                    .take(4)
                    .map(|i| [(i % columns) as u32 * 16, (i / columns) as u32 * 16])
                    .collect(),
                frame_hash: image.hash(),
                picture_size: self.run.engine_picture.then_some(self.picture_size),
                asked_size: self.run.engine_picture.then_some(self.asked_size),
            };
            redraws = self.window.redraws.get();
            prior_calls = calls;
            prior_in = self.frames_in;
            frames.push(frame_record);
            images.push(if keep_images {
                image.into()
            } else {
                image.crop_around(focus, (360.0, 220.0))
            });
        }
        Ok((frames, images, sync_timeouts, tracker.scan_line()))
    }

    /// Tiles that change and change back with nothing happening (a
    /// blinking recording light, a meter): watched for 1.5 s at rest, then
    /// left out of the flicker check.
    fn calibrate(&mut self) -> Result<std::collections::HashSet<usize>> {
        let frame = self.frame_duration();
        let mut noisy = std::collections::HashSet::new();
        let mut history: Vec<Vec<u64>> = Vec::new();
        for _ in 0..(self.run.fps * 3 / 2) {
            self.advance(frame);
            std::thread::sleep(frame);
            self.drain();
            self.hub.flush();
            if self.run.lockstep && !self.sync() {
                bail!("the engine didn't answer within {SYNC_LIMIT:?} while calibrating");
            }
            let texture = self.render()?;
            let image = Image {
                width: texture.width(),
                height: texture.height(),
                scale: self.run.scale,
                rgba: read_pixels(&self.gpu, &texture)?,
            };
            history.push(image.tile_hashes(16));
            if let [.., a, b, c] = history.as_slice() {
                for (i, ((now, one_ago), two_ago)) in c.iter().zip(b).zip(a).enumerate() {
                    if now == two_ago && now != one_ago {
                        noisy.insert(i);
                    }
                }
            }
        }
        Ok(noisy)
    }

    /// The edited monitor's picture, in window points.
    fn picture_rect(&self) -> [f32; 4] {
        [
            self.app.get_picture_left(),
            self.app.get_picture_top(),
            self.app.get_picture_width(),
            self.app.get_picture_height(),
        ]
    }

    /// The target texture cleared and not drawn (`inject blank:F`).
    fn clear_target(&mut self) -> Result<wgpu::Texture> {
        let target = self.target.clone().context("nothing drawn yet")?;
        let view = target.create_view(&Default::default());
        let mut encoder = self.gpu.device.create_command_encoder(&Default::default());
        encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("motion blank"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        self.gpu.queue.submit([encoder.finish()]);
        Ok(target)
    }

    /// Draw the window into the target texture.
    fn render(&mut self) -> Result<wgpu::Texture> {
        let size = self.window.size.get();
        let stale = self
            .target
            .as_ref()
            .is_none_or(|t| t.width() != size.width || t.height() != size.height);
        if stale {
            self.target = Some(self.gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("motion window"),
                size: wgpu::Extent3d {
                    width: size.width,
                    height: size.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            }));
        }
        let target = self.target.clone().expect("made above");
        // As offscreen.rs: clear through wgpu first, so wgpu doesn't zero
        // what Skia drew when it is copied out.
        let view = target.create_view(&Default::default());
        let mut encoder = self.gpu.device.create_command_encoder(&Default::default());
        encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("motion window"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        self.gpu.queue.submit([encoder.finish()]);
        self.window
            .renderer
            .render_to_texture(&target)
            .map_err(|e| anyhow::anyhow!("could not draw the window: {e}"))?;
        // What the preview actually asked the engine for (it throttles
        // requests while the monitor is being resized).
        if let Some(asked) = self
            .hub
            .canvas
            .preview
            .borrow()
            .as_ref()
            .map(|p| p.size_asked())
            .filter(|s| s.0 > 0)
        {
            self.asked_size = asked;
        }
        Ok(target)
    }

    // ---------- What the trackers read ----------

    pub fn app(&self) -> &AppWindow {
        &self.app
    }

    /// The `n`th visible element of Slint type `type_name`, left to right
    /// (then top to bottom): its position and size in window points.
    pub fn element(&self, type_name: &str, n: usize) -> Option<[f32; 4]> {
        let mut found: Vec<[f32; 4]> =
            ElementHandle::find_by_element_type_name(&self.app, type_name)
                .filter(|e| e.is_valid())
                .map(|e| {
                    let (p, s) = (e.absolute_position(), e.size());
                    [p.x, p.y, s.width, s.height]
                })
                .filter(|r| r[2] > 0.0 || r[3] > 0.0)
                .collect();
        found.sort_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
        found.dedup_by(|a, b| a[0] == b[0] && a[1] == b[1]);
        found.get(n).copied()
    }

    /// Canvas pixels to window points, on the edited monitor.
    pub fn canvas_to_window(&self, x: f32, y: f32) -> (f32, f32) {
        let k = self.app.get_canvas_scale();
        (
            self.app.get_picture_left() + x * k,
            self.app.get_picture_top() + y * k,
        )
    }

    pub fn canvas_box(&self, shown: bool) -> [f32; 4] {
        let c = self.app.global::<CanvasArea>();
        if shown {
            [
                c.get_shown_box_x(),
                c.get_shown_box_y(),
                c.get_shown_box_w(),
                c.get_shown_box_h(),
            ]
        } else {
            [c.get_box_x(), c.get_box_y(), c.get_box_w(), c.get_box_h()]
        }
    }

    pub fn theme_line(&self) -> [u8; 3] {
        let c = self.app.global::<Theme>().get_line();
        [c.red(), c.green(), c.blue()]
    }
}

/// The size the engine draws a `canvas` picture asked for at `max`
/// (isocast-engine record/shared_frames.rs `fitted`, the same rounding).
fn fitted((width, height): (u32, u32), (max_w, max_h): (u32, u32)) -> (u32, u32) {
    let across = f64::from(max_w) / f64::from(width);
    let down = f64::from(max_h) / f64::from(height);
    let scale = across.min(down).min(1.0);
    let scaled = |side: u32| ((f64::from(side) * scale).round() as u32).max(1);
    (scaled(width), scaled(height))
}

/// Lockstep: readings that follow the wall clock or the machine (the
/// engine's CPU, the recording disk's free space, uptime), in any reply,
/// left out, as snapshots leave the CPU out.
fn still(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                if matches!(
                    key.as_str(),
                    "cpu_percent" | "recording_free_bytes" | "uptime_seconds"
                ) {
                    *v = serde_json::Value::Null;
                } else {
                    still(v);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(still),
        _ => {}
    }
}

/// The whole texture, read back from the GPU (RGBA8).
fn read_pixels(gpu: &Gpu, texture: &wgpu::Texture) -> Result<Vec<u8>> {
    let (w, h) = (texture.width(), texture.height());
    let row = (w * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("motion readback"),
        size: row as u64 * h as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row),
                rows_per_image: Some(h),
            },
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
    let submission = gpu.queue.submit([encoder.finish()]);
    let slice = buffer.slice(..);
    let (tx, rx) = channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    gpu.device
        .poll(wgpu::PollType::Wait {
            submission_index: Some(submission),
            timeout: Some(Duration::from_secs(5)),
        })
        .context("the GPU did not finish drawing")?;
    rx.recv()
        .context("the GPU did not return the picture")?
        .context("could not read the picture back")?;
    let mapped = slice
        .get_mapped_range()
        .context("could not read the picture back")?;
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for line in 0..h as usize {
        let start = line * row as usize;
        out.extend_from_slice(&mapped[start..start + w as usize * 4]);
    }
    drop(mapped);
    buffer.unmap();
    Ok(out)
}
