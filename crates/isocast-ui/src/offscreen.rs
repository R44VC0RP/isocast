//! The window drawn offscreen, by the same Skia renderer on the same GPU
//! API as on screen, into a texture: for screenshots (`--snapshot`),
//! scripted states (`--state`), the accessibility tree (`--a11y`) and a
//! long run that checks every frame (`--soak`). Nothing appears on screen.
//! Time is the snapshot's own, so the same state draws the same picture.

use std::cell::Cell;
use std::path::Path;
use std::rc::{Rc, Weak};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use slint::platform::skia_renderer::SkiaWGPU30Renderer;
use slint::platform::{Platform, PointerEventButton, Renderer, WindowAdapter, WindowEvent};
use slint::wgpu_30::wgpu;
use slint::{ComponentHandle, LogicalPosition, LogicalSize, PhysicalSize};

use crate::app::{App, Mode};
use crate::areas;
use crate::daemon::{self, Link, Update};
use crate::gpu::Gpu;
use crate::picture::ITEM_RGB;
use crate::usage::{self, Meter};
use crate::{AppWindow, Menus, Options, ToastKind, Ui};
use isocast_protocol::api::Event;

struct OffscreenWindow {
    window: slint::Window,
    renderer: SkiaWGPU30Renderer,
    size: Cell<PhysicalSize>,
}

impl WindowAdapter for OffscreenWindow {
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
}

/// The snapshot's platform: the offscreen main window (and the editor
/// window, C.3, once opened), and a clock that only moves when the
/// snapshot says so.
struct OffscreenPlatform {
    window: Rc<OffscreenWindow>,
    /// Windows after the first (the editor's), made as asked for.
    others: Rc<std::cell::RefCell<Vec<Rc<OffscreenWindow>>>>,
    gpu: Gpu,
    made: Cell<bool>,
    now: Rc<Cell<Duration>>,
}

impl Platform for OffscreenPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        if !self.made.replace(true) {
            return Ok(self.window.clone());
        }
        let renderer = SkiaWGPU30Renderer::new(
            self.gpu.instance.clone(),
            self.gpu.adapter.clone(),
            self.gpu.device.clone(),
            self.gpu.queue.clone(),
        )
        .map_err(|e| slint::PlatformError::Other(format!("Skia could not use the GPU: {e}")))?;
        let size = self.window.size.get();
        let scale = self.window.window.scale_factor();
        let window = Rc::new_cyclic(|weak: &Weak<OffscreenWindow>| OffscreenWindow {
            window: slint::Window::new(weak.clone()),
            renderer,
            size: Cell::new(size),
        });
        window
            .window
            .dispatch_event(WindowEvent::ScaleFactorChanged {
                scale_factor: scale,
            });
        window.window.dispatch_event(WindowEvent::Resized {
            size: size.to_logical(scale),
        });
        self.others.borrow_mut().push(window.clone());
        Ok(window)
    }

    fn duration_since_start(&self) -> Duration {
        self.now.get()
    }

    /// Slint's `debug()` output, less the element search's complaint about
    /// missing debug info, which the interpreter (`live-slint`) makes for
    /// each of thousands of internal items on every search.
    fn debug_log(&self, arguments: core::fmt::Arguments) {
        let text = arguments.to_string();
        if !text.starts_with("The use of the ElementHandle API requires") {
            eprintln!("{text}");
        }
    }
}

struct Offscreen {
    gpu: Gpu,
    window: Rc<OffscreenWindow>,
    /// The editor window's (C.3), once opened: snapshots draw it then.
    others: Rc<std::cell::RefCell<Vec<Rc<OffscreenWindow>>>>,
    app: AppWindow,
    hub: Rc<App>,
    updates: Receiver<Update>,
    now: Rc<Cell<Duration>>,
    target: Option<wgpu::Texture>,
    scale: f32,
    /// ISOCAST_SNAPSHOT_STILL: leave out the readouts that change from run
    /// to run (CPU, audio levels), so CI can compare two builds' pictures.
    still: bool,
}

pub fn run(options: &Options) -> Result<()> {
    let gpu = Gpu::new()?;
    // For CI (scripts/visual-shots.sh): which device drew the pictures.
    let info = gpu.adapter.get_info();
    eprintln!(
        "gpu: {} ({:?}, {:?}, driver {:?} {:?})",
        info.name, info.backend, info.device_type, info.driver, info.driver_info
    );
    let (width, height) = options.size;
    let scale = options.scale;
    let renderer = SkiaWGPU30Renderer::new(
        gpu.instance.clone(),
        gpu.adapter.clone(),
        gpu.device.clone(),
        gpu.queue.clone(),
    )
    .map_err(|e| anyhow::anyhow!("Skia could not use the GPU: {e}"))?;
    let window = Rc::new_cyclic(|weak: &Weak<OffscreenWindow>| OffscreenWindow {
        window: slint::Window::new(weak.clone()),
        renderer,
        size: Cell::new(PhysicalSize::new(
            (width as f32 * scale).round() as u32,
            (height as f32 * scale).round() as u32,
        )),
    });
    let now = Rc::new(Cell::new(Duration::ZERO));
    let others = Rc::new(std::cell::RefCell::new(Vec::new()));
    slint::platform::set_platform(Box::new(OffscreenPlatform {
        window: window.clone(),
        others: others.clone(),
        gpu: gpu.clone(),
        made: Cell::new(false),
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
    #[cfg(feature = "live-slint")]
    live_resize(&window.window, width as f32, height as f32);

    let (tx, updates) = channel();
    let commands = daemon::start(move |update| {
        let _ = tx.send(update);
    });
    let clock_now = now.clone();
    let clock: Box<dyn Fn() -> f32> = Box::new(move || clock_now.get().as_secs_f32());
    let canvas = areas::canvas::Canvas::new(&gpu, clock, true);
    let hub = App::new(&app, commands, Mode { offscreen: true }, canvas);
    hub.sources.set_device(gpu.device.clone());

    let mut offscreen = Offscreen {
        gpu,
        window,
        others,
        app,
        hub,
        updates,
        now,
        target: None,
        scale,
        still: std::env::var_os("ISOCAST_SNAPSHOT_STILL").is_some(),
    };
    offscreen.wait_for_daemon(Duration::from_millis(options.wait_ms))?;
    let mut done = 0;
    for (path, states) in &options.snapshots {
        offscreen.states(&options.states[done..(*states).max(done)])?;
        done = done.max(*states);
        offscreen.settle();
        let started = Instant::now();
        offscreen.snapshot(path, options.crop.as_deref())?;
        isocast_diagnostics::diagnostic!(
            "saved {} ({}x{} at {}x, drawn in {} ms)",
            path.display(),
            width,
            height,
            scale,
            started.elapsed().as_millis()
        );
    }
    offscreen.states(&options.states[done..])?;
    offscreen.settle();

    if options.a11y {
        // Drawn once first: dialogs' and menus' popups open as their
        // conditional elements are made, which drawing does.
        offscreen.render()?;
        offscreen.settle();
        crate::a11y::print(&offscreen.app)?;
    }
    if let Some(dir) = &options.watch {
        offscreen.watch(dir, options.crop.as_deref())?;
    }
    if let Some(seconds) = options.soak {
        offscreen.soak(seconds, options.check, options.still)?;
    }
    Ok(())
}

impl Offscreen {
    fn states(&mut self, states: &[String]) -> Result<()> {
        for state in states {
            self.state(state)
                .with_context(|| format!("--state {state}"))?;
        }
        Ok(())
    }

    /// Apply what the daemon sent until it has answered (or said it can't
    /// be reached), then a moment longer for anything that follows.
    fn wait_for_daemon(&mut self, extra: Duration) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let (ready, link) = {
                let s = self.hub.session.borrow();
                (s.loaded && s.status.is_some(), s.link)
            };
            if ready || link == Some(Link::Failed) {
                break;
            }
            if Instant::now() > deadline {
                if link.is_some() {
                    break;
                }
                bail!("isocastd did not answer within 10 s");
            }
            self.pump(Duration::from_millis(20));
        }
        let until = Instant::now() + extra;
        while Instant::now() < until {
            self.pump(Duration::from_millis(20));
        }
        Ok(())
    }

    /// Apply the daemon's updates that arrive within `wait`.
    fn pump(&mut self, wait: Duration) {
        if let Ok(update) = self.updates.recv_timeout(wait) {
            self.apply(update);
            while let Ok(update) = self.updates.try_recv() {
                self.apply(update);
            }
        }
        slint::platform::update_timers_and_animations();
        self.hub.flush();
    }

    fn apply(&mut self, mut update: Update) {
        if self.still {
            match &mut update {
                Update::Event(event) if matches!(**event, Event::Levels { .. }) => return,
                Update::Reply {
                    result: Ok(value), ..
                } => {
                    if let Some(cpu) = value.get_mut("cpu_percent") {
                        *cpu = serde_json::Value::Null;
                    }
                }
                _ => {}
            }
        }
        self.hub.apply(update);
    }

    /// Run timers and animations to the end, a frame at a time.
    fn settle(&mut self) {
        for _ in 0..30 {
            self.advance(Duration::from_millis(16));
        }
    }

    fn advance(&mut self, by: Duration) {
        self.now.set(self.now.get() + by);
        slint::platform::update_timers_and_animations();
    }

    /// A scripted state (`--state NAME[:ARG]`), run in order. The
    /// window-wide ones are here; each area answers its own (see
    /// src/areas/mod.rs):
    /// `dialog:KIND[:ARG]` (open a dialog), `menu:OWNER` (ask for a menu,
    /// in the middle of the window), `menu-bar:ID` (a menu bar command),
    /// `menu-bar-open:TITLE` (the menu bar drawn in the window, as where
    /// there is no native one, with menu TITLE open),
    /// `toast:KIND:TITLE` (info, success, warning, error), `kit`,
    /// `kit-dialog`, `kit-menu`, `kit-dropdown` (the widget gallery),
    /// `type:TEXT` (into the focused field), `key:NAME` (escape, return,
    /// tab, up, down, left, right, f2, delete, or a character),
    /// `drag:X1,Y1,X2,Y2` (a pointer drag in window points), `hover:X,Y`
    /// (the pointer moved there, not pressed), `hide:DOCK`
    /// (scenes, sources, mixer, transitions, controls, toolbar, status),
    /// `wait:MS` (for the daemon), `quit:WHAT` (the quit question),
    /// `events:FILE` (replay `isocast watch --json` lines).
    fn state(&mut self, state: &str) -> Result<()> {
        let (name, arg) = state.split_once(':').unwrap_or((state, ""));
        let app = &self.app;
        let ui = app.global::<Ui>();
        match name {
            "dialog" => {
                let (kind, arg) = arg.split_once(':').unwrap_or((arg, ""));
                self.hub.open_dialog(kind, arg);
            }
            "menu" => {
                let (w, h) = (app.get_preview_width(), app.get_preview_height());
                app.global::<Menus>()
                    .invoke_request(arg.into(), w / 2.0, h / 2.0, 0.0, 0.0);
            }
            "menu-bar" => self.hub.menu(arg),
            "menu-bar-open" => self.open_menu_bar(arg)?,
            "toast" => {
                let (kind, title) = arg.split_once(':').unwrap_or((arg, "A toast"));
                let kind = match kind {
                    "success" => ToastKind::Success,
                    "warning" => ToastKind::Warning,
                    "error" => ToastKind::Error,
                    _ => ToastKind::Info,
                };
                self.hub.toast(kind, title, "");
            }
            "quit" => crate::quit::sample(&self.hub, arg)?,
            "kit" => app.set_kit_gallery(true),
            "kit-dialog" => {
                app.set_kit_gallery(true);
                app.set_kit_dialog(true);
            }
            "kit-menu" => {
                app.set_kit_gallery(true);
                self.hub
                    .show_menu("kit", crate::kit::sample_menu(), [380.0, 120.0, 0.0, 0.0]);
            }
            "kit-dropdown" => {
                app.set_kit_gallery(true);
                app.set_kit_dropdown(app.get_kit_dropdown() + 1);
            }
            "type" => {
                // Drawn first: a field shown by the state before is made
                // (and takes focus) as it is drawn.
                self.render()?;
                for c in arg.chars() {
                    let text = slint::SharedString::from(c.to_string());
                    self.key(text);
                }
            }
            "key" => {
                let text: slint::SharedString = match arg {
                    "escape" => slint::platform::Key::Escape.into(),
                    "return" => slint::platform::Key::Return.into(),
                    "tab" => slint::platform::Key::Tab.into(),
                    "up" => slint::platform::Key::UpArrow.into(),
                    "down" => slint::platform::Key::DownArrow.into(),
                    "left" => slint::platform::Key::LeftArrow.into(),
                    "right" => slint::platform::Key::RightArrow.into(),
                    "f2" => slint::platform::Key::F2.into(),
                    "delete" => slint::platform::Key::Delete.into(),
                    other => other.into(),
                };
                self.key(text);
            }
            "drag" => {
                let [x1, y1, x2, y2] = numbers::<4>(arg)?;
                self.drag((x1, y1), (x2, y2), 12);
            }
            "hover" => {
                let [x, y] = numbers::<2>(arg)?;
                self.pointer(WindowEvent::PointerMoved {
                    position: LogicalPosition::new(x, y),
                });
                self.advance(Duration::from_millis(16));
            }
            // What a screen reader's press does: the accessible default
            // action of the first element named `arg`.
            "press" => {
                let found = i_slint_backend_testing::ElementHandle::find_by_accessible_label(
                    &self.app, arg,
                )
                .next();
                let Some(element) = found else {
                    bail!("no element named {arg:?} (see --a11y)");
                };
                element.invoke_accessible_default_action();
                self.advance(Duration::from_millis(16));
            }
            "hide" => match arg {
                "scenes" => ui.set_show_scenes(false),
                "sources" => ui.set_show_sources(false),
                "mixer" => ui.set_show_mixer(false),
                "transitions" => ui.set_show_transitions(false),
                "controls" => ui.set_show_controls(false),
                "toolbar" => ui.set_show_source_toolbar(false),
                "status" => ui.set_show_status_bar(false),
                other => bail!(
                    "no dock {other:?}; one of scenes, sources, mixer, transitions, controls, toolbar, status"
                ),
            },
            // Replay daemon events saved by `isocast watch --json` (one per
            // line), as if they had just come.
            "events" => {
                let text = std::fs::read_to_string(arg).with_context(|| format!("events:{arg}"))?;
                for line in text.lines().filter(|l| !l.trim().is_empty()) {
                    let event: isocast_protocol::api::Event =
                        serde_json::from_str(line).with_context(|| format!("events:{arg}"))?;
                    self.hub.event(event);
                }
            }
            "wait" => {
                let ms: u64 = arg.parse().context("wait:MS takes milliseconds")?;
                let until = Instant::now() + Duration::from_millis(ms);
                while Instant::now() < until {
                    self.pump(Duration::from_millis(20));
                    // The snapshot's clock moves too, so timers waiting on
                    // it (a gesture's paced edits) fire.
                    self.advance(Duration::from_millis(20));
                }
            }
            other => {
                let answer = self
                    .hub
                    .areas()
                    .iter()
                    .find_map(|area| area.state(&self.hub, app, other, arg));
                match answer {
                    Some(result) => result?,
                    None => {
                        let areas: Vec<&str> = areas::STATES
                            .iter()
                            .flat_map(|s| s.iter().copied())
                            .collect();
                        bail!(
                            "unknown state {other:?}; one of dialog:KIND[:ARG], menu:OWNER, menu-bar:ID, menu-bar-open:TITLE, toast:KIND:TITLE, kit, kit-dialog, kit-menu, kit-dropdown, type:TEXT, key:NAME, drag:X1,Y1,X2,Y2, hover:X,Y, press:LABEL, hide:DOCK, wait:MS, quit:recording|streaming|both, {}",
                            areas.join(", ")
                        )
                    }
                }
            }
        }
        // Let what the state changed show (and the daemon answer).
        self.pump(Duration::from_millis(20));
        self.hub.flush();
        self.advance(Duration::from_millis(16));
        Ok(())
    }

    /// Show the menu bar in the window (Slint draws its own where there is
    /// no native one) and click menu `title` open: its items as the menu
    /// bar has them, for a snapshot.
    fn open_menu_bar(&mut self, title: &str) -> Result<()> {
        self.app.set_native_menu(true);
        self.advance(Duration::from_millis(16));
        self.render()?;
        // The menu's title, in the bar along the top of the window.
        let found =
            i_slint_backend_testing::ElementHandle::find_by_accessible_label(&self.app, title)
                .find(|e| e.absolute_position().y < 40.0 && e.size().height > 0.0);
        let Some(element) = found else {
            bail!(
                "no menu {title:?} in the menu bar (File, Edit, View, Docks, Profile, Scene \
                 Collection, Tools, Window, Help)"
            );
        };
        let (at, size) = (element.absolute_position(), element.size());
        let position = LogicalPosition::new(at.x + size.width / 2.0, at.y + size.height / 2.0);
        self.pointer(WindowEvent::PointerMoved { position });
        self.pointer(WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        });
        self.advance(Duration::from_millis(16));
        self.pointer(WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        });
        self.advance(Duration::from_millis(16));
        Ok(())
    }

    /// Keys and the pointer go to the window drawn (the editor's while it
    /// is open).
    fn key(&mut self, text: slint::SharedString) {
        let drawn = self.drawn();
        let window = &drawn.window;
        window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        window.dispatch_event(WindowEvent::KeyReleased { text });
        self.advance(Duration::from_millis(16));
    }

    fn pointer(&self, event: WindowEvent) {
        self.drawn().window.dispatch_event(event);
    }

    /// Press, move in `steps`, release.
    fn drag(&mut self, from: (f32, f32), to: (f32, f32), steps: u32) {
        let at = |t: f32| {
            LogicalPosition::new(from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t)
        };
        self.pointer(WindowEvent::PointerMoved { position: at(0.0) });
        self.pointer(WindowEvent::PointerPressed {
            position: at(0.0),
            button: PointerEventButton::Left,
        });
        for step in 1..=steps {
            self.advance(Duration::from_millis(16));
            self.pointer(WindowEvent::PointerMoved {
                position: at(step as f32 / steps as f32),
            });
        }
        self.pointer(WindowEvent::PointerReleased {
            position: at(1.0),
            button: PointerEventButton::Left,
        });
        self.advance(Duration::from_millis(16));
    }

    /// The window snapshots draw: the editor's while it is open, else the
    /// main window.
    fn drawn(&self) -> Rc<OffscreenWindow> {
        match self.others.borrow().last() {
            Some(editor) if self.hub.create.editor_open() => editor.clone(),
            _ => self.window.clone(),
        }
    }

    /// Draw the window into the target texture.
    fn render(&mut self) -> Result<wgpu::Texture> {
        let drawn = self.drawn();
        let size = drawn.size.get();
        let stale = self
            .target
            .as_ref()
            .is_none_or(|t| t.width() != size.width || t.height() != size.height);
        if stale {
            self.target = Some(self.gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("offscreen window"),
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
        // wgpu doesn't see Skia's drawing (it goes to Metal directly) and
        // would zero a texture it thinks was never written before copying
        // from it: clearing it through wgpu first marks it written.
        let view = target.create_view(&Default::default());
        let mut encoder = self.gpu.device.create_command_encoder(&Default::default());
        encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("offscreen window"),
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
        drawn
            .renderer
            .render_to_texture(&target)
            .map_err(|e| anyhow::anyhow!("could not draw the window: {e}"))?;
        Ok(target)
    }

    fn snapshot(&mut self, path: &Path, crop: Option<&str>) -> Result<()> {
        // Two passes: the first lays out and makes the preview texture at
        // its final size.
        self.render()?;
        let target = self.render()?;
        let (x, y, w, h) = match crop {
            Some(crop) => self.crop(crop, &target)?,
            None => (0, 0, target.width(), target.height()),
        };
        let rgba = read_pixels(&self.gpu, &target, x, y, w, h)?;
        let mut file = std::io::BufWriter::new(
            std::fs::File::create(path)
                .with_context(|| format!("can't write {}", path.display()))?,
        );
        let mut encoder = png::Encoder::new(&mut file, w, h);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&rgba)?;
        writer.finish()?;
        Ok(())
    }

    /// `--snapshot-watch DIR`: draw DIR/snapshot.png now and again after
    /// every edit to a .slint file, until stopped. The new interface is
    /// put in place of the old (the interpreter reads ui/ again); what the
    /// Rust side gave it (properties, models, callbacks) and the daemon's
    /// state carry over, so the states don't run again. Each picture is
    /// written whole (renamed into place) and announced on stdout.
    #[cfg(feature = "live-slint")]
    fn watch(&mut self, dir: &Path, crop: Option<&str>) -> Result<()> {
        std::fs::create_dir_all(dir).with_context(|| format!("can't make {}", dir.display()))?;
        let out = dir.join("snapshot.png");
        let partial = dir.join(".snapshot.png.partial");
        let ui = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui");
        let mut seen = slint_files(&ui);
        let draw = |this: &mut Self, started: Instant, reload_ms: u128| -> Result<()> {
            this.snapshot(&partial, crop)?;
            std::fs::rename(&partial, &out)?;
            println!(
                "drawn {} in {} ms (reload {reload_ms} ms)",
                out.display(),
                started.elapsed().as_millis()
            );
            Ok(())
        };
        draw(self, Instant::now(), 0)?;
        eprintln!(
            "watching {} for .slint edits (stop with Ctrl-C)",
            ui.display()
        );
        loop {
            self.pump(Duration::from_millis(25));
            let now = slint_files(&ui);
            if now == seen {
                continue;
            }
            seen = now;
            let started = Instant::now();
            if !self.app.live_reload() {
                println!("not drawn: the interface has errors (above); fix and save again");
                continue;
            }
            let size = self.window.size.get().to_logical(self.scale);
            live_resize(&self.window.window, size.width, size.height);
            let reload_ms = started.elapsed().as_millis();
            self.pump(Duration::from_millis(1));
            self.settle();
            // A picture that can't be drawn now (the cropped element gone
            // mid-edit, DIR removed) is reported; the next save tries again.
            if let Err(error) = draw(self, started, reload_ms) {
                println!("not drawn: {error:#}");
            }
        }
    }

    #[cfg(not(feature = "live-slint"))]
    fn watch(&mut self, _dir: &Path, _crop: Option<&str>) -> Result<()> {
        bail!(
            "--snapshot-watch reads ui/*.slint at run time, which needs a development build: \
             cargo build --profile ui-dev -p isocast-ui --features live-slint"
        )
    }

    /// `--crop AREA|X,Y,W,H` as a rectangle of the drawn texture's pixels.
    fn crop(&self, crop: &str, target: &wgpu::Texture) -> Result<(u32, u32, u32, u32)> {
        let (x, y, w, h) = if let Ok([x, y, w, h]) = numbers::<4>(crop) {
            (x, y, w, h)
        } else {
            use i_slint_backend_testing::{ElementHandle, ElementRoot};
            // An id is qualified by the component that declares it
            // (`Workspace::controls`).
            let suffix = format!("::{}", crop.replace('_', "-"));
            let shown = |e: &ElementHandle| e.size().width > 0.0 && e.size().height > 0.0;
            let element = self
                .app
                .root_element()
                .query_descendants()
                .match_predicate(move |e| e.id().is_some_and(|id| id.ends_with(&suffix)))
                .find_all()
                .into_iter()
                .chain(ElementHandle::find_by_element_type_name(&self.app, crop))
                .find(shown);
            let Some(element) = element else {
                bail!(
                    "--crop: no shown element {crop:?}; an element id in ui/app.slint (scenes, \
                     sources, mixer, transitions, controls, stage, bar), the first shown element of a \
                     component (ControlsDock) or X,Y,W,H in window points"
                );
            };
            let (at, size) = (element.absolute_position(), element.size());
            (at.x, at.y, size.width, size.height)
        };
        let s = self.scale;
        let px = |v: f32| (v * s).round().max(0.0) as u32;
        let (x, y) = (
            px(x).min(target.width() - 1),
            px(y).min(target.height() - 1),
        );
        let w = px(w).clamp(1, target.width() - x);
        let h = px(h).clamp(1, target.height() - y);
        Ok((x, y, w, h))
    }

    /// Draw at 60 fps for `seconds` while dragging the selection box back
    /// and forth, as a person would (or holding it still, with `still`). With `check`, read back each frame
    /// and count frames whose preview is black, and frames where the box
    /// and the item it outlines in the picture are not in the same place.
    fn soak(&mut self, seconds: f64, check: bool, still: bool) -> Result<()> {
        let frame = Duration::from_secs_f64(1.0 / 60.0);
        if self.app.global::<Ui>().get_selected_item().is_empty() {
            bail!("--soak needs a selected item (the scene on air has none)");
        }
        // Grab the box away from its handles.
        self.render()?;
        let s = self.scale;
        let grab = |app: &AppWindow| {
            LogicalPosition::new(
                app.get_box_left() + app.get_box_width() * 0.3,
                app.get_box_top() + app.get_box_height() * 0.3,
            )
        };
        let mut position = grab(&self.app);
        let start = position;
        self.pointer(WindowEvent::PointerMoved { position });
        self.pointer(WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        });

        let mut meter = Meter::start();
        let started = Instant::now();
        let mut next = started;
        let (mut frames, mut black, mut drift, mut late) = (0u64, 0u64, 0u64, 0u64);
        let mut first_drift: Option<String> = None;
        let mut step = if still { 0.0 } else { 9.0f32 };
        while started.elapsed().as_secs_f64() < seconds {
            // The pointer moves 9 points a frame, turning before the box
            // leaves the picture.
            let left = self.app.get_box_left();
            if left + step < self.app.get_picture_left() + 20.0 {
                step = step.abs();
            } else if left + self.app.get_box_width() + step > self.app.get_picture_right() - 20.0 {
                step = -step.abs();
            }
            position.x += step;
            self.pointer(WindowEvent::PointerMoved { position });
            self.advance(frame);
            let target = self.render()?;
            frames += 1;

            if check {
                let (bx, by) = (self.app.get_box_left(), self.app.get_box_top());
                let (bw, bh) = (self.app.get_box_width(), self.app.get_box_height());
                // A row across the box's left edge, a quarter of the way
                // down (clear of the handles), 8 points each side.
                let y = ((by + bh * 0.25) * s).round() as u32;
                let x0 = ((bx - 8.0) * s).round().max(0.0) as u32;
                let row = read_pixels(&self.gpu, &target, x0, y, (16.0 * s) as u32, 1)?;
                let at = |points: f32| {
                    let i = ((points * s).round() as usize).min(row.len() / 4 - 1) * 4;
                    [row[i], row[i + 1], row[i + 2]]
                };
                let inside = at(8.0 + 4.0);
                let outside = at(8.0 - 4.0);
                if !is_item(inside) || is_item(outside) {
                    drift += 1;
                    first_drift.get_or_insert_with(|| {
                        format!(
                            "frame {frames}: box at x={bx:.1} w={bw:.1}; inside {inside:?}, outside {outside:?}"
                        )
                    });
                }
                // The middle of the box and the preview's corner area.
                let mid = read_pixels(
                    &self.gpu,
                    &target,
                    ((bx + bw * 0.5) * s) as u32,
                    ((by + bh * 0.5) * s) as u32,
                    1,
                    1,
                )?;
                if mid[..3] == [0, 0, 0] {
                    black += 1;
                }
            }

            next += frame;
            let now = Instant::now();
            if now < next {
                std::thread::sleep(next - now);
            } else if now - next > frame {
                late += 1;
                next = now;
            }
            // Keep up with the daemon without waiting for it.
            while let Ok(update) = self.updates.try_recv() {
                self.hub.apply(update);
            }
        }
        // Drop the box where it started, so the run leaves the setup as
        // it found it.
        self.pointer(WindowEvent::PointerMoved { position: start });
        self.pointer(WindowEvent::PointerReleased {
            position: start,
            button: PointerEventButton::Left,
        });
        let seconds = meter.seconds();
        let cpu = meter.lap();
        let applies = crate::app::applies_json(&self.hub);
        println!(
            "{}",
            serde_json::json!({
                "mode": "offscreen",
                "applies": applies,
                "checked": check,
                "dragging": !still,
                "seconds": (seconds * 10.0).round() / 10.0,
                "frames": frames,
                "fps": (frames as f64 / seconds * 10.0).round() / 10.0,
                "late_frames": late,
                "black_frames": black,
                "drift_frames": drift,
                "first_drift": first_drift,
                "cpu_percent_of_one_core": (cpu * 10.0).round() / 10.0,
                "peak_memory_mb": usage::peak_memory() / (1 << 20),
                "size": [self.window.size.get().width, self.window.size.get().height],
            })
        );
        if check && (black > 0 || drift > 0) {
            bail!("{black} black frames and {drift} frames where the box and picture differ");
        }
        Ok(())
    }
}

/// The item's verdigris, or its lighter checker squares.
fn is_item(rgb: [u8; 3]) -> bool {
    let near = |a: [u8; 3], b: [u8; 3]| a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 6);
    near(rgb, ITEM_RGB) || near(rgb, [86, 169, 150])
}

/// The interpreter (`live-slint`) runs the window's `init` before the
/// Rust side hides the in-window menu bar, and doesn't run `changed
/// height` when the bar goes, so `Viewport` keeps the window's height less
/// the bar's (the docks came out 229 points high instead of 240). A resize
/// there and back sets it, as the compiled interface has it from the
/// start. After every reload too, which runs `init` again.
#[cfg(feature = "live-slint")]
fn live_resize(window: &slint::Window, width: f32, height: f32) {
    for h in [height + 1.0, height] {
        window.dispatch_event(WindowEvent::Resized {
            size: LogicalSize::new(width, h),
        });
        slint::platform::update_timers_and_animations();
    }
}

/// Every .slint file under `dir` with its modification time and size.
#[cfg(feature = "live-slint")]
fn slint_files(dir: &Path) -> Vec<(std::path::PathBuf, std::time::SystemTime, u64)> {
    let mut files = Vec::new();
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "slint") {
                let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                files.push((path, modified, meta.len()));
            }
        }
    }
    files.sort();
    files
}

fn numbers<const N: usize>(text: &str) -> Result<[f32; N]> {
    let values: Vec<f32> = text
        .split(',')
        .map(|v| v.trim().parse::<f32>())
        .collect::<Result<_, _>>()
        .with_context(|| format!("expected {N} numbers separated by commas, got {text:?}"))?;
    values
        .try_into()
        .map_err(|v: Vec<f32>| anyhow::anyhow!("expected {N} numbers, got {}", v.len()))
}

/// Read a rectangle of an RGBA8 texture back from the GPU.
fn read_pixels(
    gpu: &Gpu,
    texture: &wgpu::Texture,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<Vec<u8>> {
    let x = x.min(texture.width() - 1);
    let y = y.min(texture.height() - 1);
    let w = w.clamp(1, texture.width() - x);
    let h = h.clamp(1, texture.height() - y);
    let row = (w * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: row as u64 * h as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d { x, y, z: 0 },
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
