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
}

fn with_app(f: impl FnOnce(&Rc<App>)) {
    APP.with(|app| {
        if let Some(app) = app.borrow().as_ref() {
            f(app);
        }
    });
}

pub fn run(options: &Options) -> Result<()> {
    let gpu = Gpu::new()?;
    // Test and measurement runs never take focus (see background.rs).
    let background = background::enabled(options);
    let mut selector = slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("skia".into())
        .require_wgpu_30(gpu.slint_configuration());
    if background {
        selector = background::configure(selector);
    }
    selector
        .select()
        .context("Slint could not open a window with Skia on the GPU")?;

    let app = AppWindow::new()?;
    // Decided before the window exists: Slint makes the window
    // see-through for the layers under it when it is created.
    app.global::<crate::CanvasArea>()
        .set_native_picture(options.preview == crate::preview::Mode::Layers);
    let commands = daemon::start(|update: Update| {
        let _ = slint::invoke_from_event_loop(move || with_app(|app| app.apply(update)));
    });
    let started = Instant::now();
    let canvas = Canvas::new(&gpu, Box::new(|| 0.0), false);
    let shell = App::new(&app, commands, Mode { offscreen: false }, canvas);
    shell.sources.set_device(gpu.device.clone());
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
        app.window().on_winit_window_event(move |window, event| {
            if background && matches!(event, winit::event::WindowEvent::Resized(_)) {
                window.with_winit_window(background::keep_in_corner);
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

    app.show()?;
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
                eprintln!(
                    "{:6.1}s  {fps:5.1} fps  {cpu:5.1}% CPU{}",
                    started.elapsed().as_secs_f64(),
                    if occluded.get() { "  (occluded)" } else { "" }
                );
            },
        );
    }

    if let Some(seconds) = options.drag_test {
        start_drag_test(&app, seconds);
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
                    eprintln!(
                        "the window was covered for part of the run (macOS reported it occluded), so it wasn't drawn then: this measurement is not valid"
                    );
                }
                let _ = slint::quit_event_loop();
            },
        );
    }

    app.run()?;
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
        let output =
            crate::preview::Output::new(&app, setup.mode, &setup.device, setup.test_outline);
        match output {
            Ok(output) => {
                #[cfg(target_os = "macos")]
                if let crate::preview::Output::Layers(layers) = &output {
                    shell.mixer.use_native_master(&app, layers.clone());
                }
                shell.canvas.set_max_fps(setup.fps);
                shell
                    .canvas
                    .start_pictures(&shell, output, setup.check, |program, incoming| {
                        let _ = slint::invoke_from_event_loop(move || {
                            with_app(|app| app.canvas.incoming(program, incoming));
                            if !program {
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
                eprintln!("preview: {error:#}");
                shell.toast(crate::ToastKind::Error, "No preview", format!("{error:#}"));
            }
        }
    });
}

/// `--drag-test SECONDS`: after the picture has settled, drag the selected
/// item back and forth with Slint pointer events (the real pointer isn't
/// touched), as `scripts/drag-lag.py` does with the web app. The pointer
/// moves once per frame from the daemon (60 a second, like a person's
/// pointer on a 60 Hz display); a timer would be slowed down by macOS
/// while the app isn't in front.
fn start_drag_test(app: &AppWindow, seconds: f64) {
    #[derive(Clone, Copy)]
    enum Phase {
        Waiting,
        Settling(Instant),
        Dragging(Instant, LogicalPosition, f32),
        Done,
    }
    let weak = app.as_weak();
    let mut phase = Phase::Waiting;
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
                if Instant::now() < until {
                    return;
                }
                let origin = LogicalPosition::new(
                    app.get_box_left() + app.get_box_width() / 2.0,
                    app.get_box_top() + app.get_box_height() / 2.0,
                );
                let span = (app.get_box_width() * 2.0).min(app.get_preview_width() / 4.0);
                window.dispatch_event(WindowEvent::PointerMoved { position: origin });
                window.dispatch_event(WindowEvent::PointerPressed {
                    position: origin,
                    button: PointerEventButton::Left,
                });
                eprintln!("drag-test: dragging");
                phase = Phase::Dragging(Instant::now(), origin, span);
            }
            Phase::Dragging(start, origin, span) => {
                let t = start.elapsed().as_secs_f64();
                let x = origin.x + span * (2.0 * std::f64::consts::PI * t / 1.5).sin() as f32;
                let position = LogicalPosition::new(x, origin.y);
                if t < seconds {
                    window.dispatch_event(WindowEvent::PointerMoved { position });
                } else {
                    window.dispatch_event(WindowEvent::PointerReleased {
                        position,
                        button: PointerEventButton::Left,
                    });
                    eprintln!("drag-test: done");
                    phase = Phase::Done;
                }
            }
            Phase::Done => {}
        }
    };
    DRAG_STEP.with(|s| *s.borrow_mut() = Some(Box::new(step)));
}
