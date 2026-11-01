//! The app in a real window, with the platform's menu bar, showing the
//! daemon's program picture.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use slint::ComponentHandle;
use slint::LogicalPosition;
use slint::platform::{PointerEventButton, WindowEvent};
use slint::winit_030::{EventResult, WinitWindowAccessor, winit};

use crate::app::{App, Mode};
use crate::areas::canvas::Canvas;
use crate::background;
use crate::daemon::{self, Update};
use crate::gpu::Gpu;
use crate::usage::{self, Meter};
use crate::{AppWindow, Options, Ui};

thread_local! {
    static APP: RefCell<Option<Rc<App>>> = const { RefCell::new(None) };
    /// `--drag-test`: one step per frame from the daemon.
    static DRAG_STEP: RefCell<Option<Box<dyn FnMut()>>> = const { RefCell::new(None) };
    /// `--drag-test`: what it dragged and how the window kept up.
    static DRAG_RESULT: RefCell<Option<serde_json::Value>> = const { RefCell::new(None) };
}

/// What `--drag-test` drags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragMode {
    /// The selected item, by its middle.
    Move,
    /// The selected item's lower right corner handle, along its diagonal.
    Resize,
    /// The same, then select the scene's bottom item the moment it is let
    /// go (the pointer's mark disappears then).
    ResizeSelect,
    /// The splitter between the Scenes and Sources docks.
    Columns,
    /// The splitter above the docks.
    Rows,
}

pub(crate) fn with_app(f: impl FnOnce(&Rc<App>)) {
    APP.with(|app| {
        if let Some(app) = app.borrow().as_ref() {
            f(app);
        }
    });
}

/// The app's id on Linux: its Wayland app id, X11 class and desktop file
/// name (packaging/linux/dev.isocast.Isocast.desktop).
#[cfg(target_os = "linux")]
const APP_ID: &str = "dev.isocast.Isocast";

/// Linux: the app's colour scheme is dark, whatever the desktop's. Slint's
/// winit backend hands each window the scheme it holds for the app (the
/// desktop's, from the settings portal, or none) as the theme of the title
/// bar a window draws itself where the compositor draws none (GNOME,
/// weston), so it came out white over a dark app. Held dark here, and put
/// back whenever the desktop's setting arrives or changes, as long as the
/// returned tracker lives.
#[cfg(target_os = "linux")]
fn keep_dark(app: &AppWindow) -> slint::private_unstable_api::re_exports::ChangeTracker {
    use slint::private_unstable_api::re_exports::{ChangeTracker, ColorScheme, WindowInner};
    let context = WindowInner::from_pub(app.window()).context().clone();
    context.set_color_scheme(ColorScheme::Dark);
    let window = app.as_weak();
    let tracker = ChangeTracker::default();
    tracker.init(
        context,
        |context| context.color_scheme(None),
        move |context, scheme| {
            if *scheme == ColorScheme::Dark {
                return;
            }
            context.set_color_scheme(ColorScheme::Dark);
            if let Some(app) = window.upgrade() {
                app.window()
                    .with_winit_window(|window| window.set_theme(Some(winit::window::Theme::Dark)));
            }
        },
    );
    tracker
}

/// The window draws with Skia on the CPU (set before the app is made).
static CPU_WINDOW: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether this process's window is drawn on the CPU (Linux without a GPU
/// adapter): it calls no rendering notifier, and every frame it paints
/// costs CPU time a recording needs.
pub fn cpu_window() -> bool {
    CPU_WINDOW.load(std::sync::atomic::Ordering::Relaxed)
}

/// Whether the window draws with Skia on the CPU (Linux), and why not
/// as asked: `setting` is `ISOCAST_UI_RENDERER` (`software` or `gpu`;
/// unset, the CPU when the adapter is a CPU one, llvmpipe). Measurement
/// runs (`--bench`, `--drag-test`) count the frames drawn through a
/// rendering notifier, which Skia on the CPU never calls: they keep the
/// GPU path.
#[cfg(target_os = "linux")]
fn renderer_choice(
    setting: Option<&str>,
    cpu_adapter: bool,
    measuring: bool,
) -> (bool, Option<String>) {
    let (software, mut warning) = match setting {
        None => (cpu_adapter, None),
        Some("software") => (true, None),
        Some("gpu") => (false, None),
        Some(other) => (
            cpu_adapter,
            Some(format!(
                "ISOCAST_UI_RENDERER={other:?} is not a renderer; valid values are software, gpu (left to the adapter)"
            )),
        ),
    };
    if software && measuring {
        warning = Some(
            "--bench and --drag-test count frames through a rendering notifier, which Skia on the CPU \
             never calls: this run draws with the GPU renderer (Mesa's software Vulkan without a GPU)"
                .into(),
        );
        return (false, warning);
    }
    (software, warning)
}

#[cfg(target_os = "linux")]
fn software_renderer(gpu: &Gpu, options: &Options) -> bool {
    let setting = std::env::var_os("ISOCAST_UI_RENDERER");
    let setting = setting
        .as_ref()
        .map(|value| value.to_str().unwrap_or("(not UTF-8)"));
    let cpu_adapter = gpu.adapter.get_info().device_type == slint::wgpu_30::wgpu::DeviceType::Cpu;
    let measuring = options.bench.is_some() || options.drag_test.is_some();
    let (software, warning) = renderer_choice(setting, cpu_adapter, measuring);
    if let Some(warning) = warning {
        isocast_diagnostics::diagnostic!("warn"; "window: {warning}");
    }
    software
}

pub fn run(options: &Options) -> Result<()> {
    #[cfg(windows)]
    daemon::prewarm_preview();
    let gpu = Gpu::new()?;
    #[cfg(windows)]
    if options.preview == crate::preview::Mode::Layers {
        crate::layers::install(&gpu);
    }
    // Test and measurement runs never take focus (see background.rs).
    let background = background::enabled(options);
    let selector = slint::BackendSelector::new().backend_name("winit".into());
    #[cfg(not(target_os = "linux"))]
    let mut selector = selector
        .renderer_name("skia".into())
        .require_wgpu_30(gpu.slint_configuration());
    // Linux without a GPU driver (a VM, a bare PC): Skia on the CPU, which
    // redraws only what changed, instead of whole frames through Mesa's
    // software Vulkan (about four times the CPU, taken from the recording).
    // Linux previews are RGBA images (frames.rs), so nothing needs the GPU.
    #[cfg(target_os = "linux")]
    let cpu_window = software_renderer(&gpu, options);
    #[cfg(target_os = "linux")]
    CPU_WINDOW.store(cpu_window, std::sync::atomic::Ordering::Relaxed);
    #[cfg(target_os = "linux")]
    let mut selector = if cpu_window {
        isocast_diagnostics::diagnostic!("info"; "window: Skia on the CPU (no GPU adapter)");
        selector.renderer_name("skia-software".into())
    } else {
        selector
            .renderer_name("skia".into())
            .require_wgpu_30(gpu.slint_configuration())
    };
    if background {
        selector = background::configure(selector);
    }
    // Windows: dark title bars and menus, the app icon (chrome_windows.rs).
    #[cfg(windows)]
    {
        selector = crate::chrome_windows::configure(selector, background);
    }
    // Linux: the app id the desktop file names (dev.isocast.Isocast), so
    // the compositor and the dock match the window to its icon and entry;
    // the dark theme for any decorations the compositor draws.
    #[cfg(target_os = "linux")]
    {
        selector = selector.with_winit_window_attributes_hook(move |attributes| {
            use slint::winit_030::winit::platform::wayland::WindowAttributesExtWayland;
            use slint::winit_030::winit::platform::x11::WindowAttributesExtX11;
            let attributes = WindowAttributesExtWayland::with_name(attributes, APP_ID, "isocast");
            let attributes = WindowAttributesExtX11::with_name(attributes, APP_ID, "isocast");
            let attributes =
                attributes.with_theme(Some(slint::winit_030::winit::window::Theme::Dark));
            match background {
                true => attributes.with_active(false),
                false => attributes,
            }
        });
    }
    selector
        .select()
        .context("Slint could not open a window with Skia")?;

    let app = AppWindow::new()?;
    // Decided before the window exists: Slint makes the window
    // see-through for the layers under it when it is created.
    app.global::<crate::CanvasArea>().set_native_picture(
        options.preview == crate::preview::Mode::Layers && crate::engine::is_local(),
    );
    let commands = daemon::start(|update: Update| {
        let _ = slint::invoke_from_event_loop(move || with_app(|app| app.apply(update)));
    });
    let started = Instant::now();
    let canvas = Canvas::new(&gpu, Box::new(|| 0.0), false);
    let shell = App::new(&app, commands, Mode { offscreen: false }, canvas);
    shell.sources.set_device(gpu.device.clone());
    shell.projectors.set_device(gpu.device.clone());
    APP.with(|s| *s.borrow_mut() = Some(shell.clone()));

    // macOS reports a window covered by others as occluded and Slint then
    // stops drawing it: measurements must say so rather than report it.
    let occluded = Rc::new(Cell::new(false));
    let occluded_time = Rc::new(Cell::new(Duration::ZERO));
    let occluded_since = Rc::new(Cell::new(None::<Instant>));
    {
        let (occluded, occluded_time, occluded_since) = (
            occluded.clone(),
            occluded_time.clone(),
            occluded_since.clone(),
        );
        let weak = app.as_weak();
        app.window().on_winit_window_event(move |window, event| {
            if background && matches!(event, winit::event::WindowEvent::Resized(_)) {
                window.with_winit_window(background::keep_in_corner);
            }
            // The Window menu's Maximize/Restore and Full Screen (Windows).
            if let winit::event::WindowEvent::Resized(_) = event
                && let Some(app) = weak.upgrade()
            {
                let ui = app.global::<Ui>();
                ui.set_window_maximized(window.is_maximized());
                ui.set_window_fullscreen(window.is_fullscreen());
            }
            #[cfg(windows)]
            if let winit::event::WindowEvent::Resized(size) = event {
                with_app(|app| {
                    app.canvas
                        .set_hidden(occluded.get() || size.width == 0 || size.height == 0)
                });
            }
            if let winit::event::WindowEvent::Focused(false) = event {
                crate::keys::release_all();
            }
            if let winit::event::WindowEvent::Occluded(now) = event {
                occluded.set(*now);
                if *now {
                    occluded_since.set(Some(Instant::now()));
                } else if let Some(since) = occluded_since.take() {
                    occluded_time.set(occluded_time.get() + since.elapsed());
                }
                with_app(|app| app.canvas.set_hidden(*now));
            }
            EventResult::Propagate
        });
    }

    #[cfg(windows)]
    {
        crate::chrome_windows::take_colors(&app);
        // ui/app.slint's preferred size, within the screen's work area.
        if !background {
            crate::chrome_windows::fit_first_window(app.window(), (1280.0, 800.0));
        }
    }
    #[cfg(target_os = "linux")]
    let _dark = keep_dark(&app);
    app.show()?;
    // Keys pressed in the window go to the daemon's hotkeys too.
    crate::keys::start();
    // The native window exists once the event loop runs: then float it
    // when measuring, and start the picture.
    setup_when_ready(
        app.as_weak(),
        shell.clone(),
        Setup {
            device: gpu.device.clone(),
            mode: options.preview,
            fps: options.fps,
            check: options.frame_check,
            test_outline: options.drag_test.is_some(),
            float: (options.bench.is_some() || options.drag_test.is_some()) && !options.no_float,
            background,
        },
    );

    // Measured runs print the window's own rate and CPU each second.
    let stats = slint::Timer::default();
    if options.bench.is_some() {
        let shell = shell.clone();
        let mut meter = Meter::start();
        let mut shown = 0;
        let occluded = occluded.clone();
        stats.start(
            slint::TimerMode::Repeated,
            Duration::from_secs(1),
            move || {
                let seconds = meter.seconds();
                let cpu = meter.lap();
                let now = shell.canvas.preview_stats().shown;
                let fps = (now - shown) as f64 / seconds;
                shown = now;
                isocast_diagnostics::diagnostic!(
                    "{:6.1}s  {fps:5.1} fps  {cpu:5.1}% CPU{}",
                    started.elapsed().as_secs_f64(),
                    if occluded.get() { "  (occluded)" } else { "" }
                );
            },
        );
    }

    if let Some(seconds) = options.drag_test {
        start_drag_test(&app, seconds, options.drag_mode);
    }

    let bench = slint::Timer::default();
    let warmup = slint::Timer::default();
    if let Some(seconds) = options.bench {
        let shell = shell.clone();
        let meter = Rc::new(RefCell::new(None::<(Meter, u64, u64)>));
        // Measure after the window has settled for a second.
        {
            let meter = meter.clone();
            let shell = shell.clone();
            warmup.start(
                slint::TimerMode::SingleShot,
                Duration::from_secs(2),
                move || {
                    *meter.borrow_mut() = Some((
                        Meter::start(),
                        shell.canvas.frames.get(),
                        shell.canvas.preview_stats().shown,
                    ));
                },
            );
        }
        let preview_mode = format!("{:?}", options.preview).to_lowercase();
        bench.start(
            slint::TimerMode::SingleShot,
            Duration::from_secs_f64(seconds + 2.0),
            move || {
                let Some((meter, renders0, shown0)) = meter.borrow_mut().take() else {
                    return;
                };
                if let Some(since) = occluded_since.take() {
                    occluded_time.set(occluded_time.get() + since.elapsed());
                }
                let wall = meter.seconds();
                let stats = shell.canvas.preview_stats();
                let renders = shell.canvas.frames.get() - renders0;
                let shown = stats.shown - shown0;
                let round = |v: f64| (v * 10.0).round() / 10.0;
                println!(
                    "{}",
                    serde_json::json!({
                        "mode": "window",
                        "preview": preview_mode,
                        "seconds": round(wall),
                        "preview_frames": shown,
                        "preview_fps": round(shown as f64 / wall),
                        "interface_redraws_per_s": round(renders as f64 / wall),
                        "drag_steps_sent": shell.canvas.moves.get(),
                        "drag": DRAG_RESULT.with(|r| r.borrow_mut().take()),
                        "applies": crate::app::applies_json(&shell),
                        "cpu_percent_of_one_core": round(meter.percent()),
                        "peak_memory_mb": usage::peak_memory() / (1 << 20),
                        "occluded_seconds": round(occluded_time.get().as_secs_f64()),
                        "valid": occluded_time.get().is_zero(),
                        "frames": {
                            "received": stats.received,
                            "shown": stats.shown,
                            "waited_for_layout": stats.waited_for_layout,
                            "shown_without_layout": stats.shown_without_layout,
                            "longest_gap_ms": round(stats.longest_gap.as_secs_f64() * 1000.0),
                            "size_requests": stats.size_requests,
                            "lost": stats.lost,
                            "last_lost": stats.last_lost,
                        },
                        "check": stats.checked.gt(&0).then(|| serde_json::json!({
                            "frames_read": stats.checked,
                            "black": stats.black,
                            "same_as_previous": stats.repeated,
                            "longest_same_run": stats.longest_repeat,
                            "box_checked": stats.sync_checked,
                            "box_off": stats.sync_off,
                            "first_box_off": stats.first_sync_off,
                        })),
                    })
                );
                if !occluded_time.get().is_zero() {
                    isocast_diagnostics::diagnostic!("warn";
                        "{}", isocast_protocol::platform_text::select_linux(
                            "the window was covered for part of the run (macOS reported it occluded), so it wasn't drawn then: this measurement is not valid",
                            "the window was covered for part of the run (Windows reported it occluded), so it wasn't drawn then: this measurement is not valid",
                            "the window was covered for part of the run (the compositor reported it occluded), so it wasn't drawn then: this measurement is not valid",
                        )
                    );
                }
                let _ = slint::quit_event_loop();
            },
        );
    }

    let result = app.run();
    #[cfg(windows)]
    crate::update::cleanup();
    #[cfg(windows)]
    crate::layers::uninstall();
    result?;
    APP.with(|s| s.borrow_mut().take());
    Ok(())
}

#[derive(Clone)]
struct Setup {
    device: slint::wgpu_30::wgpu::Device,
    mode: crate::preview::Mode,
    fps: u32,
    check: bool,
    test_outline: bool,
    float: bool,
    background: bool,
}

/// Once the native window exists (soon after the event loop starts), float
/// it if asked (measured windows float above the others, without taking
/// the keyboard from the app in front, so they aren't covered), put it in
/// the corner in background mode, and start the preview.
fn setup_when_ready(weak: slint::Weak<AppWindow>, shell: Rc<App>, setup: Setup) {
    slint::Timer::single_shot(Duration::from_millis(20), move || {
        let Some(app) = weak.upgrade() else { return };
        let ready = app.window().with_winit_window(|window| {
            if setup.float {
                window.set_window_level(winit::window::WindowLevel::AlwaysOnTop);
            }
            if setup.background {
                background::place(window);
            }
        });
        if ready.is_none() {
            return setup_when_ready(weak, shell, setup);
        }
        #[cfg(windows)]
        crate::chrome_windows::style_windows_soon();
        crate::quit::install_terminate_question();
        // Updates replace this machine's app and engine: only a window on
        // this machine's engine runs them (decision 21); a remote window
        // never stops or restarts the local engine.
        if crate::engine::is_local() {
            crate::update::start(&shell);
        }
        // Another computer's engine's video decodes into surfaces the
        // layers show as they show local frames (pictures that come as
        // pixels are put in surfaces first).
        let mode = setup.mode;
        let output = crate::preview::Output::new(&app, mode, &setup.device, setup.test_outline);
        match output {
            Ok(output) => {
                #[cfg(any(target_os = "macos", windows))]
                if let crate::preview::Output::Layers(layers) = &output {
                    shell.mixer.use_native_master(&app, layers.clone());
                }
                shell.canvas.set_max_fps(setup.fps);
                // Decoded pictures (another computer's engine) wait in a
                // one-picture mailbox per monitor: a busy window shows the
                // newest and drops the rest instead of queueing them.
                let mailboxes: std::sync::Arc<
                    [std::sync::Mutex<Option<crate::frames::Incoming>>; 3],
                > = std::sync::Arc::default();
                shell
                    .canvas
                    .start_pictures(&shell, output, setup.check, move |role, incoming| {
                        let index = match role {
                            crate::preview::Role::Editor => 0,
                            crate::preview::Role::Program => 1,
                            crate::preview::Role::Extra => 2,
                        };
                        let mut incoming = Some(incoming);
                        let decoded = match &incoming {
                            Some(crate::frames::Incoming::Still(_)) => true,
                            Some(crate::frames::Incoming::Frame(_)) => !crate::engine::is_local(),
                            _ => false,
                        };
                        if decoded {
                            let waiting = mailboxes[index]
                                .lock()
                                .unwrap()
                                .replace(incoming.take().unwrap());
                            if waiting.is_some() {
                                // Already on its way; it'll take this one.
                                return;
                            }
                        }
                        let mailboxes = mailboxes.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            let incoming =
                                incoming.or_else(|| mailboxes[index].lock().unwrap().take());
                            if let Some(incoming) = incoming {
                                with_app(|app| app.canvas.incoming(role, incoming));
                            }
                            if role == crate::preview::Role::Editor {
                                DRAG_STEP.with(|step| {
                                    if let Some(step) = step.borrow_mut().as_mut() {
                                        step();
                                    }
                                });
                            }
                        });
                    });
            }
            Err(error) => {
                isocast_diagnostics::diagnostic!("warn"; "preview: {error:#}");
                shell.toast(crate::ToastKind::Error, "No preview", format!("{error:#}"));
            }
        }
    });
}

/// `--drag-test SECONDS`: after the picture has settled, drag the selected
/// item back and forth (`--drag-mode`: or its corner, or a dock splitter)
/// with Slint pointer events (the real pointer isn't touched), as
/// `scripts/drag-lag.py` does with the web app. The pointer moves once per
/// frame from the daemon (60 a second, like a person's pointer on a 60 Hz
/// display); a timer would be slowed down by macOS while the app isn't in
/// front. A pure cyan mark shows where the pointer is (window captures
/// leave it out), and the window's frame times are kept for `--bench`.
fn start_drag_test(app: &AppWindow, seconds: f64, mode: DragMode) {
    #[derive(Clone, Copy)]
    struct Path {
        origin: LogicalPosition,
        /// The way the pointer swings, and how far.
        direction: (f32, f32),
        span: f32,
    }
    #[derive(Clone, Copy)]
    enum Phase {
        Waiting,
        Settling(Instant),
        Dragging(Instant, Path),
        Done,
    }
    let weak = app.as_weak();
    let mut phase = Phase::Waiting;
    let mut meter = None::<Meter>;
    let mut steps = 0u64;
    let mut last_step = Instant::now();
    let mark = move |app: &AppWindow, position: LogicalPosition| {
        app.set_test_pointer(true);
        app.set_test_edge(match mode {
            DragMode::Columns => 1,
            DragMode::Rows => 2,
            _ => 0,
        });
        if mode == DragMode::Rows {
            app.set_test_pointer_x(8.0);
            app.set_test_pointer_y(position.y - 1.5);
            app.set_test_pointer_width(24.0);
            app.set_test_pointer_height(3.0);
        } else {
            let below = match mode {
                DragMode::Columns => app.get_docks_y() - 34.0 - position.y,
                DragMode::Move => app.get_box_height() / 2.0 + 14.0,
                _ => 14.0,
            };
            let rect = [position.x - 1.5, position.y + below, 3.0, 24.0];
            // On the canvas, with the box: in the native layers, placed as
            // the pointer moves (Slint's own frame reaches the screen a
            // frame after them, so a mark it drew would trail the box).
            let mut native = false;
            if matches!(
                mode,
                DragMode::Move | DragMode::Resize | DragMode::ResizeSelect
            ) {
                with_app(|shell| {
                    if let Ok(preview) = shell.canvas.preview.try_borrow()
                        && let Some(preview) = preview.as_ref()
                    {
                        native = preview.show_test_pointer(rect.map(f64::from));
                    }
                });
            }
            app.set_test_pointer(!native);
            app.set_test_pointer_x(rect[0]);
            app.set_test_pointer_y(rect[1]);
            app.set_test_pointer_width(rect[2]);
            app.set_test_pointer_height(rect[3]);
        }
    };
    let step = move || {
        let Some(app) = weak.upgrade() else { return };
        let window = app.window();
        match phase {
            Phase::Waiting => {
                if !app.global::<Ui>().get_selected_item().is_empty() {
                    phase = Phase::Settling(Instant::now() + Duration::from_secs(3));
                } else {
                    // Nothing selected yet: the test drags the scene's
                    // first item that doesn't fill the canvas.
                    with_app(|shell| shell.canvas.select_first_item(shell));
                }
            }
            Phase::Settling(until) => {
                let path = match mode {
                    DragMode::Move => Path {
                        origin: LogicalPosition::new(
                            app.get_box_left() + app.get_box_width() / 2.0,
                            app.get_box_top() + app.get_box_height() / 2.0,
                        ),
                        direction: (1.0, 0.0),
                        span: (app.get_box_width() * 2.0).min(app.get_preview_width() / 4.0),
                    },
                    DragMode::Resize | DragMode::ResizeSelect => {
                        let (w, h) = (app.get_box_width(), app.get_box_height());
                        let length = w.hypot(h).max(1.0);
                        Path {
                            origin: LogicalPosition::new(
                                app.get_box_left() + w,
                                app.get_box_top() + h,
                            ),
                            direction: (w / length, h / length),
                            span: length * 0.4,
                        }
                    }
                    DragMode::Columns => {
                        // The width on screen: a narrow window squeezes
                        // the docks below their set widths. The swing
                        // stays clear of Scenes' 140pt minimum.
                        let width = app.get_scenes_right();
                        Path {
                            origin: LogicalPosition::new(
                                app.get_scenes_right(),
                                app.get_docks_y() + app.get_docks_height() / 2.0,
                            ),
                            direction: (1.0, 0.0),
                            span: (width - 150.0).clamp(10.0, 80.0),
                        }
                    }
                    DragMode::Rows => Path {
                        origin: LogicalPosition::new(
                            app.get_scenes_right() / 2.0,
                            app.get_docks_y(),
                        ),
                        direction: (0.0, 1.0),
                        span: (app.get_docks_height() - 130.0).clamp(10.0, 80.0),
                    },
                };
                // The mark shows while settling, so the analysis sees
                // where it rests against the edge.
                mark(&app, path.origin);
                if Instant::now() < until {
                    return;
                }
                let origin = path.origin;
                window.dispatch_event(WindowEvent::PointerMoved { position: origin });
                window.dispatch_event(WindowEvent::PointerPressed {
                    position: origin,
                    button: PointerEventButton::Left,
                });
                isocast_diagnostics::diagnostic!("drag-test: dragging ({mode:?})");
                with_app(|shell| {
                    *shell.canvas.render_log.borrow_mut() = Some(Vec::new());
                    *shell.work_log.borrow_mut() = Some(Default::default());
                });
                meter = Some(Meter::start());
                DRAG_RESULT.with(|r| {
                    *r.borrow_mut() = Some(serde_json::json!({
                        "mode": format!("{mode:?}").to_lowercase(),
                        "span_points": path.span,
                        "direction": [path.direction.0, path.direction.1],
                        "origin": [origin.x, origin.y],
                    }))
                });
                phase = Phase::Dragging(Instant::now(), path);
            }
            Phase::Dragging(start, path) => {
                // Called for each frame from the daemon and by a timer for
                // frames that don't come (a monitor being resized): at
                // most one pointer step a display frame.
                if last_step.elapsed() < Duration::from_millis(12) {
                    return;
                }
                last_step = Instant::now();
                let t = start.elapsed().as_secs_f64();
                let swing = path.span * (2.0 * std::f64::consts::PI * t / 1.5).sin() as f32;
                let position = LogicalPosition::new(
                    path.origin.x + path.direction.0 * swing,
                    path.origin.y + path.direction.1 * swing,
                );
                if t < seconds {
                    steps += 1;
                    window.dispatch_event(WindowEvent::PointerMoved { position });
                    mark(&app, position);
                } else {
                    if mode == DragMode::ResizeSelect {
                        // A last move in the same breath as the release, as
                        // a hand's often is: no frame of it has come yet.
                        window.dispatch_event(WindowEvent::PointerMoved { position });
                    }
                    window.dispatch_event(WindowEvent::PointerReleased {
                        position,
                        button: PointerEventButton::Left,
                    });
                    isocast_diagnostics::diagnostic!("drag-test: done");
                    if mode == DragMode::ResizeSelect {
                        // Right after the release (the next turn of the
                        // event loop, or ISOCAST_DRAG_SELECT_MS): before
                        // isocastd's frame showing the final size (about
                        // 5 ms later on isocast-m2-b), as a click or key
                        // right after letting go can be.
                        let delay = std::env::var("ISOCAST_DRAG_SELECT_MS")
                            .ok()
                            .and_then(|ms| ms.parse().ok())
                            .unwrap_or(0);
                        let weak = app.as_weak();
                        slint::Timer::single_shot(Duration::from_millis(delay), move || {
                            with_app(|shell| {
                                shell.canvas.select_bottom_item(shell);
                                if let Ok(preview) = shell.canvas.preview.try_borrow()
                                    && let Some(preview) = preview.as_ref()
                                {
                                    preview.show_test_pointer([0.0; 4]);
                                }
                            });
                            if let Some(app) = weak.upgrade() {
                                app.set_test_pointer(false);
                            }
                        });
                    }
                    let cpu = meter.take().map_or(0.0, |m| m.percent());
                    let log = APP
                        .with(|a| {
                            a.borrow()
                                .as_ref()
                                .and_then(|shell| shell.canvas.render_log.borrow_mut().take())
                        })
                        .unwrap_or_default();
                    let work = APP
                        .with(|a| {
                            a.borrow()
                                .as_ref()
                                .and_then(|shell| shell.work_log.borrow_mut().take())
                        })
                        .unwrap_or_default();
                    let ms = |xs: &[Duration]| -> Vec<f64> {
                        xs.iter().map(|d| d.as_secs_f64() * 1000.0).collect()
                    };
                    let (mut flushes, mut updates) = (ms(&work.flushes), ms(&work.updates));
                    let total = |xs: &[f64]| (xs.iter().sum::<f64>()).round();
                    let (flush_total, update_total) = (total(&flushes), total(&updates));
                    let mut intervals: Vec<f64> = log
                        .windows(2)
                        .map(|w| (w[1].1 - w[0].1).as_secs_f64() * 1000.0)
                        .collect();
                    let mut draws: Vec<f64> = log
                        .iter()
                        .map(|(a, b)| (*b - *a).as_secs_f64() * 1000.0)
                        .collect();
                    let summary = |xs: &mut Vec<f64>| {
                        xs.sort_by(f64::total_cmp);
                        let at = |q: f64| {
                            xs.get(((xs.len() as f64 - 1.0) * q).round() as usize)
                                .map_or(0.0, |v| (v * 10.0).round() / 10.0)
                        };
                        serde_json::json!([at(0.5), at(0.95), at(1.0)])
                    };
                    DRAG_RESULT.with(|r| {
                        if let Some(serde_json::Value::Object(result)) = r.borrow_mut().as_mut() {
                            result.insert("seconds".into(), seconds.into());
                            result.insert("pointer_steps".into(), steps.into());
                            result.insert("window_frames".into(), log.len().into());
                            result.insert(
                                "frame_interval_ms_p50_p95_max".into(),
                                summary(&mut intervals),
                            );
                            result.insert("draw_ms_p50_p95_max".into(), summary(&mut draws));
                            result.insert("flushes".into(), flushes.len().into());
                            result.insert("flush_ms_total".into(), flush_total.into());
                            result.insert("flush_ms_p50_p95_max".into(), summary(&mut flushes));
                            result.insert("daemon_updates".into(), updates.len().into());
                            result.insert("daemon_update_ms_total".into(), update_total.into());
                            result.insert(
                                "daemon_update_ms_p50_p95_max".into(),
                                summary(&mut updates),
                            );
                            result.insert(
                                "cpu_percent_of_one_core".into(),
                                ((cpu * 10.0).round() / 10.0).into(),
                            );
                        }
                    });
                    phase = Phase::Done;
                }
            }
            Phase::Done => {}
        }
    };
    DRAG_STEP.with(|s| *s.borrow_mut() = Some(Box::new(step)));
    let backup = slint::Timer::default();
    backup.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(17),
        || {
            DRAG_STEP.with(|step| {
                if let Some(step) = step.borrow_mut().as_mut() {
                    step();
                }
            })
        },
    );
    std::mem::forget(backup);
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::renderer_choice;

    #[test]
    fn renderer_follows_the_adapter_the_setting_and_measurement_runs() {
        // (setting, CPU adapter, measuring) → software
        let table = [
            ((None, true, false), true),
            ((None, false, false), false),
            ((Some("software"), false, false), true),
            ((Some("gpu"), true, false), false),
            ((Some("vulkan"), true, false), true),
            ((Some("vulkan"), false, false), false),
            // Measurement runs keep the GPU path, even when asked otherwise.
            ((None, true, true), false),
            ((Some("software"), false, true), false),
        ];
        for ((setting, cpu, measuring), want) in table {
            assert_eq!(
                renderer_choice(setting, cpu, measuring).0,
                want,
                "{setting:?} {cpu} {measuring}"
            );
        }
        let (_, warning) = renderer_choice(Some("vulkan"), true, false);
        assert!(warning.unwrap().contains("valid values are software, gpu"));
        assert!(renderer_choice(Some("software"), false, false).1.is_none());
        assert!(
            renderer_choice(None, true, true)
                .1
                .unwrap()
                .contains("--bench")
        );
    }
}
