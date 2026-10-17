//! Projectors (G.4, #19): the daemon keeps the open projectors
//! (`projector_*`, isocast-protocol projector.rs); the app shows each in a
//! window of its own (ui/areas/projectors/projector.slint), full screen on
//! its display or windowed, and reports where it is so it opens there next
//! time. Opened from the right-click menus of the preview, the scenes and
//! the sources, from View › Multiview / Program Projector / Preview
//! Projector, and by anyone on the method table (CLI, MCP).
//!
//! Each picture is the daemon's shared surface (frames.rs), shown in place
//! as a Slint image: no copy, no encode, one GPU scale per viewer in the
//! daemon. A multiview's tiles each have a feed of their own: the preview,
//! the program, and each scene (`scene:ID`, drawn by the engine's scene
//! feeds while watched). A click on a scene tile chooses it as the scenes
//! dock does (Studio Mode stages it, otherwise it goes on air); a double
//! click cuts to it. With a vertical canvas (#96) a projector shows its
//! program, preview or a scene as it arranges it (`canvas: vertical`),
//! and a multiview's last tile can show its program (`vertical`).
//!
//! A full-screen projector whose display goes away is closed (the daemon
//! does it on `device_changed`; the window checks too, every second).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use isocast_client::shared::SharedFrame;
use isocast_protocol::api::{Event, Method};
use isocast_protocol::devices::Display;
use isocast_protocol::projector::{
    CloseParams, MultiviewLayout, OpenParams, PlaceParams, Placement, Projector, ProjectorView,
    Projectors as ProjectorList,
};
use slint::wgpu_30::wgpu;
use slint::{ComponentHandle, Model, VecModel};

use super::{Area, Changed};
use crate::app::App;
use crate::frames::{Feed, Incoming};
use crate::kit::MenuBuilder;
use crate::{AppWindow, ProjectorDisplay, ProjectorTile, ProjectorWindow, ProjectorsArea};

/// Snapshot states (`--state NAME[:ARG]`).
pub const STATES: &[&str] = &["projector:[vertical-]VIEW[:ID|LAYOUT]"];

/// The vertical monitor's menu owner (its picture's right-click, #96).
pub const VERTICAL_MONITOR: &str = "vertical-monitor";
/// Marks a menu item's view as of the vertical canvas (`program@vertical`).
const VERTICAL_SUFFIX: &str = "@vertical";

/// How long after a window stops moving its place is reported.
const REPORT_AFTER: Duration = Duration::from_millis(500);
/// Scene tiles of a multiview are drawn at most this often (the preview
/// and program tiles at the canvas's rate).
const SCENE_TILE_FPS: u32 = 30;

/// A tile's newest picture, handed over by its feed's thread.
#[derive(Default)]
struct Arrived {
    frame: Option<SharedFrame>,
    lost: Option<String>,
}

/// What a tile shows.
#[derive(Debug, Clone, PartialEq)]
enum Role {
    /// A projector's one picture.
    Single,
    Preview,
    Program,
    Scene(String),
    /// A multiview's last cell, showing the vertical canvas's program.
    VerticalProgram,
    /// A multiview cell with no scene to show.
    Empty,
}

struct Tile {
    role: Role,
    feed: Option<Feed>,
    target: Option<String>,
    arrived: Arc<Mutex<Arrived>>,
    /// The frame on screen and the one before (kept until the GPU is done
    /// with it: dropping a frame hands its surface back to the daemon).
    showing: Option<SharedFrame>,
    previous: Option<SharedFrame>,
    images: HashMap<(u64, u32), slint::Image>,
    /// Feed size asked for, in pixels.
    size: (u32, u32),
}

impl Drop for Tile {
    /// A tile that goes (its window closed, or the grid changed) stops its
    /// stream: the daemon stops drawing for it.
    fn drop(&mut self) {
        if let Some(feed) = &self.feed {
            feed.stop();
        }
    }
}

impl Tile {
    fn new(role: Role) -> Self {
        Self {
            role,
            feed: None,
            target: None,
            arrived: Arc::default(),
            showing: None,
            previous: None,
            images: HashMap::new(),
            size: (0, 0),
        }
    }
}

/// An open projector's window.
struct Shown {
    projector: RefCell<Projector>,
    window: ProjectorWindow,
    model: Rc<VecModel<ProjectorTile>>,
    tiles: RefCell<Vec<Tile>>,
    /// The placement last applied to the window or reported to the daemon.
    placed: RefCell<Placement>,
    /// The window's last place out of full screen.
    windowed: Cell<Option<isocast_protocol::projector::Frame>>,
    report: slint::Timer,
    /// Applies full screen once the native window exists.
    apply: slint::Timer,
    /// It covers a display (full screen) now.
    covering: Cell<bool>,
    /// Closed by the app (the daemon is being told): ignore its events.
    closing: Cell<bool>,
    latency: Rc<RefCell<Latency>>,
}

/// `ISOCAST_PROJECTOR_STATS=FILE`: how long after its time each frame of
/// a single projector reached the window's thread (Slint draws it at the
/// next display refresh), and frames shown, written as JSON every second.
#[derive(Default)]
struct Latency {
    /// How long after its time each frame reached the window's thread,
    /// and frames put in the window (every tile).
    arrivals: Vec<f64>,
    shown: u64,
}

#[derive(Default)]
pub struct Projectors {
    device: RefCell<Option<wgpu::Device>>,
    shown: RefCell<HashMap<String, Rc<Shown>>>,
    displays: RefCell<Vec<Display>>,
    /// Offscreen there is no event loop to wake: frames are looked for
    /// every frame instead.
    poll: slint::Timer,
    /// Every second: tile sizes, displays that went away, stats.
    housekeeping: slint::Timer,
    app: RefCell<Weak<App>>,
}

fn host_now() -> u64 {
    #[cfg(target_os = "macos")]
    {
        #[repr(C)]
        struct Timebase {
            numer: u32,
            denom: u32,
        }
        unsafe extern "C" {
            fn mach_timebase_info(info: *mut Timebase) -> i32;
            fn mach_absolute_time() -> u64;
        }
        let mut info = Timebase { numer: 0, denom: 0 };
        // SAFETY: fills `info`; reads the host's tick counter.
        let ticks = unsafe {
            mach_timebase_info(&mut info);
            mach_absolute_time()
        };
        (u128::from(ticks) * u128::from(info.numer.max(1)) / u128::from(info.denom.max(1))) as u64
    }
    #[cfg(not(target_os = "macos"))]
    {
        0
    }
}

/// Full screen on the placement's display (or out of it, centred on its
/// display if it has no frame yet), once the native window exists. False
/// while it doesn't.
///
/// Full screen is a borderless window covering the display above the menu
/// bar, as OBS's projectors are, not a macOS full-screen space: it opens
/// at once on any display, leaves the other displays alone, and works for
/// a background (accessory) app, which macOS doesn't give a space.
fn apply_native(shown: &Shown) -> bool {
    use slint::winit_030::{WinitWindowAccessor, winit};
    let placement = shown.placed.borrow().clone();
    // No title bar while it covers a display (Slint keeps the native
    // window's decorations in step with `no-frame`).
    shown.window.set_covering(placement.fullscreen);
    let restore = Cell::new(false);
    let done = shown
        .window
        .window()
        .with_winit_window(|window| {
            let monitor = placement.display.as_ref().and_then(|id| {
                window
                    .available_monitors()
                    .find(|m| monitor_id(m).as_deref() == Some(id.as_str()))
            });
            if placement.fullscreen {
                // Its display isn't connected: never cover another one
                // (housekeeping closes it).
                if placement.display.is_some() && monitor.is_none() {
                    return true;
                }
                if let Some(monitor) = monitor
                    .or_else(|| window.current_monitor())
                    .or_else(|| window.primary_monitor())
                {
                    // Onto the display first: covering takes the screen the
                    // window is on.
                    let on = window.current_monitor().is_some_and(|m| m == monitor);
                    if !on {
                        window.set_outer_position(monitor.position());
                        return false;
                    }
                    cover(window, true);
                    above_menu_bar(window, true);
                    shown.covering.set(true);
                }
                return true;
            }
            if shown.covering.replace(false) {
                above_menu_bar(window, false);
                cover(window, false);
                restore.set(true);
            }
            if placement.frame.is_none()
                && let Some(monitor) = monitor
            {
                let scale = monitor.scale_factor();
                let at = monitor.position().to_logical::<f64>(scale);
                let size = monitor.size().to_logical::<f64>(scale);
                let inner = window.inner_size().to_logical::<f64>(window.scale_factor());
                window.set_outer_position(winit::dpi::LogicalPosition::new(
                    at.x + (size.width - inner.width) / 2.0,
                    at.y + (size.height - inner.height) / 2.0,
                ));
            }
            true
        })
        .unwrap_or(false);
    // Out of full screen: back where its window was.
    // Once its title bar is back, so the picture lands where it was.
    if restore.get()
        && let Some(frame) = placement.frame.or(shown.windowed.get())
    {
        let weak = shown.window.as_weak();
        slint::Timer::single_shot(Duration::from_millis(150), move || {
            if let Some(projector) = weak.upgrade() {
                put_frame(&projector, frame);
            }
        });
    }
    done
}

/// The window covers its screen (macOS: winit's "simple full screen",
/// a borderless frame the size of the screen, no space of its own), or
/// goes back to its frame.
#[cfg(target_os = "macos")]
fn cover(window: &slint::winit_030::winit::window::Window, on: bool) {
    use slint::winit_030::winit::platform::macos::WindowExtMacOS;
    if window.simple_fullscreen() != on {
        window.set_simple_fullscreen(on);
    }
}

#[cfg(not(target_os = "macos"))]
fn cover(window: &slint::winit_030::winit::window::Window, on: bool) {
    use slint::winit_030::winit::window::Fullscreen;
    window.set_fullscreen(on.then(|| Fullscreen::Borderless(window.current_monitor())));
}

/// Put the window's picture at `frame` (its top left and size). Before the
/// native window exists Slint keeps it; after, the native window is placed
/// by its title bar, so its height comes off.
fn put_frame(projector: &ProjectorWindow, frame: isocast_protocol::projector::Frame) {
    use slint::winit_030::{WinitWindowAccessor, winit};
    let placed = projector.window().with_winit_window(|window| {
        let scale = window.scale_factor();
        let title = window
            .outer_position()
            .ok()
            .zip(window.inner_position().ok())
            .map_or(0.0, |(outer, inner)| f64::from(inner.y - outer.y) / scale);
        window.set_outer_position(winit::dpi::LogicalPosition::new(frame.x, frame.y - title));
        let _ = window.request_inner_size(winit::dpi::LogicalSize::new(frame.width, frame.height));
    });
    if placed.is_none() {
        let window = projector.window();
        window.set_position(slint::LogicalPosition::new(frame.x as f32, frame.y as f32));
        window.set_size(slint::LogicalSize::new(
            frame.width as f32,
            frame.height as f32,
        ));
    }
}

/// macOS: the window above the menu bar (and the Dock) while it covers a
/// display, back at the normal level after.
#[cfg(target_os = "macos")]
fn above_menu_bar(window: &slint::winit_030::winit::window::Window, above: bool) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    // NSMainMenuWindowLevel (24) + 1, and NSNormalWindowLevel.
    let level: isize = if above { 25 } else { 0 };
    let Ok(RawWindowHandle::AppKit(handle)) = window.window_handle().map(|h| h.as_raw()) else {
        return;
    };
    // SAFETY: winit's view, alive as long as the window; on the main
    // thread (Slint's event loop).
    let view: &AnyObject = unsafe { handle.ns_view.cast::<AnyObject>().as_ref() };
    let ns_window: Option<objc2::rc::Retained<AnyObject>> = unsafe { msg_send![view, window] };
    if let Some(ns_window) = ns_window {
        let _: () = unsafe { msg_send![&*ns_window, setLevel: level] };
    }
}

#[cfg(not(target_os = "macos"))]
fn above_menu_bar(window: &slint::winit_030::winit::window::Window, above: bool) {
    use slint::winit_030::winit::window::WindowLevel;
    window.set_window_level(if above {
        WindowLevel::AlwaysOnTop
    } else {
        WindowLevel::Normal
    });
}

/// A display's id as the daemon gives it (its UUID), from a winit
/// monitor.
#[cfg(target_os = "macos")]
fn monitor_id(monitor: &slint::winit_030::winit::monitor::MonitorHandle) -> Option<String> {
    use objc2_core_foundation::{CFRetained, CFUUID};
    use slint::winit_030::winit::platform::macos::MonitorHandleExtMacOS;
    unsafe extern "C" {
        fn CGDisplayCreateUUIDFromDisplayID(display: u32) -> *mut CFUUID;
    }
    let uuid =
        std::ptr::NonNull::new(unsafe { CGDisplayCreateUUIDFromDisplayID(monitor.native_id()) })?;
    // SAFETY: the function follows the create rule: this reference is ours.
    let uuid: CFRetained<CFUUID> = unsafe { CFRetained::from_raw(uuid) };
    Some(CFUUID::new_string(None, Some(&uuid))?.to_string())
}

#[cfg(not(target_os = "macos"))]
fn monitor_id(monitor: &slint::winit_030::winit::monitor::MonitorHandle) -> Option<String> {
    monitor.name()
}

fn view_name(view: ProjectorView) -> &'static str {
    match view {
        ProjectorView::Program => "program",
        ProjectorView::Preview => "preview",
        ProjectorView::Scene => "scene",
        ProjectorView::Source => "source",
        ProjectorView::Multiview => "multiview",
    }
}

fn cut() -> Option<isocast_protocol::Transition> {
    serde_json::from_value(serde_json::json!({"kind": "cut"})).ok()
}

impl Projectors {
    pub fn set_device(&self, device: wgpu::Device) {
        *self.device.borrow_mut() = Some(device);
    }

    /// Connected (again): show the daemon's projectors.
    pub fn load(&self, app: &Rc<App>) {
        *self.app.borrow_mut() = Rc::downgrade(app);
        app.call(Method::ProjectorList, |app, result| match result {
            Ok(value) => match serde_json::from_value::<ProjectorList>(value) {
                Ok(list) => {
                    app.projectors.set_displays(app, list.displays);
                    app.projectors.sync(app, list.projectors, None);
                }
                Err(error) => {
                    isocast_diagnostics::diagnostic!("warn"; "projectors: {error}");
                }
            },
            // An older isocastd: no projectors.
            Err(error) if error.is_unknown_method() => {}
            Err(error) => {
                isocast_diagnostics::diagnostic!("warn"; "projectors: {error}");
            }
        });
    }

    fn set_displays(&self, app: &Rc<App>, displays: Vec<Display>) {
        if let Some(window) = app.window() {
            let rows: Vec<ProjectorDisplay> = displays
                .iter()
                .map(|d| ProjectorDisplay {
                    id: d.id.as_str().into(),
                    name: if d.name.is_empty() {
                        format!("Display {}×{}", d.width, d.height).into()
                    } else {
                        d.name.as_str().into()
                    },
                })
                .collect();
            window
                .global::<ProjectorsArea>()
                .set_displays(slint::ModelRc::new(VecModel::from(rows)));
        }
        *self.displays.borrow_mut() = displays;
    }

    /// Whether a projector window is open (offscreen snapshots draw the
    /// newest).
    pub fn any_open(&self) -> bool {
        !self.shown.borrow().is_empty()
    }

    /// Open `view` (of `arg`: a scene or source id) windowed, or full
    /// screen on display `display`: of the vertical canvas with
    /// `vertical` (a multiview: with the vertical program in a tile).
    pub fn open(
        &self,
        app: &Rc<App>,
        view: ProjectorView,
        arg: Option<&str>,
        display: Option<&str>,
        vertical: bool,
    ) {
        let multiview = view == ProjectorView::Multiview;
        let params = OpenParams {
            canvas: (vertical && !multiview)
                .then(|| isocast_protocol::projector::VERTICAL_CANVAS.to_string()),
            vertical: multiview.then_some(vertical),
            view,
            scene: (view == ProjectorView::Scene).then(|| arg.unwrap_or_default().to_string()),
            source: (view == ProjectorView::Source).then(|| arg.unwrap_or_default().to_string()),
            layout: (view == ProjectorView::Multiview)
                .then(|| arg.and_then(|a| serde_json::from_value(serde_json::json!(a)).ok()))
                .flatten(),
            fullscreen: Some(display.is_some()),
            display: display.map(str::to_string),
            ..Default::default()
        };
        app.call(Method::ProjectorOpen(Box::new(params)), |app, result| {
            if let Err(error) = result {
                app.failed("Couldn’t open the projector", &error);
            }
        });
    }

    fn close(&self, app: &Rc<App>, id: &str, reason: Option<String>) {
        if let Some(shown) = self.shown.borrow_mut().remove(id) {
            shown.closing.set(true);
            let _ = shown.window.hide();
        }
        app.call(
            Method::ProjectorClose(CloseParams {
                id: Some(id.to_string()),
                all: false,
                reason,
            }),
            |_, _| {},
        );
    }

    /// Show `projectors` (the daemon's list): open windows for new ones,
    /// close the rest, move the ones whose placement changed elsewhere.
    fn sync(&self, app: &Rc<App>, projectors: Vec<Projector>, reason: Option<String>) {
        let gone: Vec<String> = self
            .shown
            .borrow()
            .keys()
            .filter(|id| !projectors.iter().any(|p| &p.id == *id))
            .cloned()
            .collect();
        for id in gone {
            if let Some(shown) = self.shown.borrow_mut().remove(&id) {
                let _ = shown.window.hide();
            }
            if let Some(reason) = &reason {
                app.toast(crate::ToastKind::Info, "Projector closed", reason.clone());
            }
        }
        for projector in projectors {
            let existing = self.shown.borrow().get(&projector.id).cloned();
            match existing {
                Some(shown) => {
                    if shown.closing.get() {
                        continue;
                    }
                    let moved = *shown.placed.borrow() != projector.placement;
                    let relaid = {
                        let old = shown.projector.borrow();
                        old.layout != projector.layout || old.vertical != projector.vertical
                    };
                    *shown.projector.borrow_mut() = projector.clone();
                    if relaid {
                        self.build_tiles(app, &shown);
                    }
                    if moved {
                        *shown.placed.borrow_mut() = projector.placement.clone();
                        self.place(app, &shown);
                    }
                }
                None => {
                    // Full screen on a display that isn't connected (it
                    // went just now): closed, never shown on another.
                    if let Some(display) = projector
                        .placement
                        .display
                        .as_deref()
                        .filter(|_| projector.placement.fullscreen)
                    {
                        use slint::winit_030::WinitWindowAccessor;
                        let present = app.window().and_then(|window| {
                            window.window().with_winit_window(|window| {
                                window
                                    .available_monitors()
                                    .any(|m| monitor_id(&m).as_deref() == Some(display))
                            })
                        });
                        if present == Some(false) {
                            self.close(
                                app,
                                &projector.id,
                                Some("its display isn't connected".to_string()),
                            );
                            continue;
                        }
                    }
                    if let Err(error) = self.show(app, projector) {
                        isocast_diagnostics::diagnostic!("warn";
                            "projector: couldn't open its window: {error:#}"
                        );
                    }
                }
            }
        }
        self.start_timers(app);
    }

    fn start_timers(&self, app: &Rc<App>) {
        let any = self.any_open();
        if !any {
            self.poll.stop();
            self.housekeeping.stop();
            return;
        }
        if app.mode.offscreen && !self.poll.running() {
            let weak = Rc::downgrade(app);
            self.poll.start(
                slint::TimerMode::Repeated,
                Duration::from_millis(16),
                move || {
                    if let Some(app) = weak.upgrade() {
                        app.projectors.show_frames();
                    }
                },
            );
        }
        if !self.housekeeping.running() {
            let weak = Rc::downgrade(app);
            self.housekeeping.start(
                slint::TimerMode::Repeated,
                Duration::from_secs(1),
                move || {
                    if let Some(app) = weak.upgrade() {
                        app.projectors.housekeep(&app);
                    }
                },
            );
        }
    }

    /// Open a window for `projector`.
    fn show(&self, app: &Rc<App>, projector: Projector) -> anyhow::Result<()> {
        let window = ProjectorWindow::new()?;
        let model = Rc::new(VecModel::<ProjectorTile>::default());
        window.set_tiles(slint::ModelRc::from(model.clone()));
        window.set_multiview(projector.view == ProjectorView::Multiview);
        let shown = Rc::new(Shown {
            placed: RefCell::new(projector.placement.clone()),
            windowed: Cell::new(projector.placement.frame),
            projector: RefCell::new(projector.clone()),
            window,
            model,
            tiles: RefCell::default(),
            report: slint::Timer::default(),
            apply: slint::Timer::default(),
            covering: Cell::new(false),
            closing: Cell::new(false),
            latency: Rc::default(),
        });
        let id = projector.id.clone();
        {
            let weak = Rc::downgrade(app);
            let id = id.clone();
            shown.window.on_tile_clicked(move |index| {
                if let Some(app) = weak.upgrade() {
                    app.projectors.clicked(&app, &id, index as usize, false);
                }
            });
        }
        {
            let weak = Rc::downgrade(app);
            let id = id.clone();
            shown.window.on_tile_double_clicked(move |index| {
                if let Some(app) = weak.upgrade() {
                    app.projectors.clicked(&app, &id, index as usize, true);
                }
            });
        }
        {
            let weak = Rc::downgrade(app);
            let id = id.clone();
            shown.window.on_key(move |key| {
                if let Some(app) = weak.upgrade() {
                    app.projectors.key(&app, &id, &key);
                }
            });
        }
        {
            let weak = Rc::downgrade(app);
            let id = id.clone();
            shown.window.window().on_close_requested(move || {
                if let Some(app) = weak.upgrade() {
                    app.projectors.close(&app, &id, None);
                }
                slint::CloseRequestResponse::HideWindow
            });
        }
        self.build_tiles(app, &shown);
        // A vertical canvas's projector opens upright the first time (the
        // window's own size is the landscape one).
        if projector.is_vertical()
            && projector.view != ProjectorView::Multiview
            && projector.placement.frame.is_none()
            && !projector.placement.fullscreen
        {
            shown
                .window
                .window()
                .set_size(slint::LogicalSize::new(405.0, 720.0));
        }
        // Its place and size before it shows (Slint keeps them for the
        // native window).
        self.place(app, &shown);
        shown.window.show()?;
        self.watch_window(app, &shown, &id);
        self.shown.borrow_mut().insert(id.clone(), shown.clone());
        // Shown, and where it ended up, once it has settled.
        let weak = Rc::downgrade(app);
        shown.report.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(1500),
            move || {
                if let Some(app) = weak.upgrade() {
                    let shown = app.projectors.shown.borrow().get(&id).cloned();
                    if let Some(shown) = shown {
                        app.projectors.report(&app, &shown);
                    }
                }
            },
        );
        Ok(())
    }

    /// Follow the window's moves, resizes and full screen, to report them.
    fn watch_window(&self, app: &Rc<App>, shown: &Rc<Shown>, id: &str) {
        use slint::winit_030::{EventResult, WinitWindowAccessor, winit};
        let weak = Rc::downgrade(app);
        let id = id.to_string();
        shown
            .window
            .window()
            .on_winit_window_event(move |_, event| {
                if matches!(
                    event,
                    winit::event::WindowEvent::Moved(_) | winit::event::WindowEvent::Resized(_)
                ) && let Some(app) = weak.upgrade()
                {
                    let shown = app.projectors.shown.borrow().get(&id).cloned();
                    if let Some(shown) = shown {
                        let weak = Rc::downgrade(&app);
                        let id = id.clone();
                        shown
                            .report
                            .start(slint::TimerMode::SingleShot, REPORT_AFTER, move || {
                                if let Some(app) = weak.upgrade() {
                                    let shown = app.projectors.shown.borrow().get(&id).cloned();
                                    if let Some(shown) = shown {
                                        app.projectors.report(&app, &shown);
                                    }
                                }
                            });
                    }
                }
                EventResult::Propagate
            });
    }

    /// Put the window where its placement says. A window's place and size
    /// go through Slint (kept until the native window exists); full screen
    /// and centring on a display need the native window, so they wait for
    /// it (it is made a moment after `show`).
    fn place(&self, app: &Rc<App>, shown: &Rc<Shown>) {
        let placement = shown.placed.borrow().clone();
        // Not while it covers a display: leaving full screen puts it back.
        if let (false, false, Some(frame)) =
            (placement.fullscreen, shown.covering.get(), placement.frame)
        {
            put_frame(&shown.window, frame);
        }
        if app.mode.offscreen {
            return;
        }
        let weak = Rc::downgrade(shown);
        let tries = Cell::new(0);
        shown.apply.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(50),
            move || {
                let Some(shown) = weak.upgrade() else { return };
                tries.set(tries.get() + 1);
                if apply_native(&shown) || tries.get() > 100 {
                    shown.apply.stop();
                }
            },
        );
    }

    /// Tell the daemon where the window is now.
    fn report(&self, app: &Rc<App>, shown: &Rc<Shown>) {
        use slint::winit_030::WinitWindowAccessor;
        if shown.closing.get() {
            return;
        }
        let asked = shown.placed.borrow().clone();
        // Full screen is the app's to say (going in and out of it takes a
        // moment); out of it, the window says where it is.
        let measured = shown.window.window().with_winit_window(|window| {
            if asked.fullscreen || shown.covering.get() {
                return None;
            }
            let scale = window.scale_factor();
            let display = window.current_monitor().as_ref().and_then(monitor_id);
            // The picture's top left, as Slint's set_position takes it.
            let at = window
                .inner_position()
                .map(|p| p.to_logical::<f64>(scale))
                .unwrap_or_default();
            let size = window.inner_size().to_logical::<f64>(scale);
            Some((
                display,
                isocast_protocol::projector::Frame {
                    x: at.x,
                    y: at.y,
                    width: size.width,
                    height: size.height,
                },
            ))
        });
        let placement = match measured.flatten() {
            Some((display, frame)) => {
                shown.windowed.set(Some(frame));
                Placement {
                    fullscreen: false,
                    display: display.or(asked.display),
                    frame: Some(frame),
                }
            }
            // Full screen entered from the window (F, double click) covers
            // the display it is on: named, so unplugging it closes it.
            None => Placement {
                frame: shown.windowed.get().or(asked.frame),
                display: asked.display.clone().or_else(|| {
                    shown
                        .window
                        .window()
                        .with_winit_window(|window| {
                            window.current_monitor().as_ref().and_then(monitor_id)
                        })
                        .flatten()
                }),
                ..asked
            },
        };
        *shown.placed.borrow_mut() = placement.clone();
        let id = shown.projector.borrow().id.clone();
        app.call(
            Method::ProjectorPlace(Box::new(PlaceParams {
                id,
                placement,
                shown: true,
            })),
            |_, _| {},
        );
    }

    /// Lay out `shown`'s tiles (again) from its view and the setup.
    fn build_tiles(&self, app: &Rc<App>, shown: &Rc<Shown>) {
        let projector = shown.projector.borrow().clone();
        let roles: Vec<(Role, [f32; 4])> = match projector.view {
            ProjectorView::Multiview => {
                let layout = projector.layout.unwrap_or(MultiviewLayout::Top8);
                let (scenes, has_vertical): (Vec<String>, bool) = {
                    let s = app.session.borrow();
                    (
                        s.setup
                            .as_ref()
                            .map(|s| s.scenes.iter().map(|s| s.id.clone()).collect())
                            .unwrap_or_default(),
                        s.vertical.is_some(),
                    )
                };
                let tiles = layout.tiles();
                let last = tiles.len() - 1;
                tiles
                    .into_iter()
                    .enumerate()
                    .map(|(i, rect)| {
                        let role = match i {
                            0 => Role::Preview,
                            1 => Role::Program,
                            n if n == last && projector.vertical && has_vertical => {
                                Role::VerticalProgram
                            }
                            n => scenes.get(n - 2).cloned().map_or(Role::Empty, Role::Scene),
                        };
                        (role, rect)
                    })
                    .collect()
            }
            _ => vec![(Role::Single, [0.0, 0.0, 1.0, 1.0])],
        };
        let same = {
            let tiles = shown.tiles.borrow();
            tiles.len() == roles.len() && tiles.iter().zip(&roles).all(|(t, (r, _))| t.role == *r)
        };
        if !same {
            let mut tiles = shown.tiles.borrow_mut();
            // Kept where they show the same, so pictures don't blink.
            let mut old: Vec<Tile> = std::mem::take(&mut *tiles);
            for (role, _) in &roles {
                match old.iter().position(|t| t.role == *role) {
                    Some(at) => tiles.push(old.remove(at)),
                    None => tiles.push(Tile::new(role.clone())),
                }
            }
            let rows: Vec<ProjectorTile> = roles
                .iter()
                .map(|(_, [x, y, w, h])| ProjectorTile {
                    x: *x,
                    y: *y,
                    w: *w,
                    h: *h,
                    ..Default::default()
                })
                .collect();
            shown.model.set_vec(rows);
            // Pictures already showing stay.
            for (index, tile) in tiles.iter().enumerate() {
                if let Some(frame) = &tile.showing
                    && let Some(image) = tile.images.get(&frame.slot())
                    && let Some(mut row) = shown.model.row_data(index)
                {
                    row.picture = image.clone();
                    row.shown = true;
                    shown.model.set_row_data(index, row);
                }
            }
        }
        self.label(app, shown);
        self.feed(app, shown);
    }

    /// The tiles' labels, tally and empty text, and the window's title.
    fn label(&self, app: &Rc<App>, shown: &Shown) {
        let s = app.session.borrow();
        let projector = shown.projector.borrow();
        let name_of = |id: &str| -> String {
            s.setup
                .as_ref()
                .and_then(|setup| setup.scenes.iter().find(|x| x.id == id))
                .map_or_else(
                    || id.to_string(),
                    |scene| crate::model::scene_name(scene).to_string(),
                )
        };
        let source_name = |id: &str| -> String {
            s.setup
                .as_ref()
                .and_then(|setup| crate::model::find_source(setup, id))
                .map_or_else(
                    || id.to_string(),
                    |source| crate::model::source_name(source).to_string(),
                )
        };
        let vertical = projector.is_vertical();
        let program = if vertical {
            s.vertical_scene.clone().or_else(|| s.scene.clone())
        } else {
            s.scene.clone()
        };
        let preview = if vertical {
            s.vertical_preview
                .clone()
                .or_else(|| s.preview.clone())
                .filter(|_| s.studio)
        } else {
            s.preview.clone().filter(|_| s.studio)
        };
        if let Some(window) = app.window() {
            let ui = window.global::<crate::Ui>();
            let (width, height) = if vertical {
                (ui.get_vertical_width(), ui.get_vertical_height())
            } else {
                (ui.get_canvas_width(), ui.get_canvas_height())
            };
            shown
                .window
                .set_aspect(width.max(1) as f32 / height.max(1) as f32);
        }
        let canvas_word = if vertical { "Vertical " } else { "" };
        let heading = match projector.view {
            ProjectorView::Program => format!("{canvas_word}Program Projector"),
            ProjectorView::Preview => format!("{canvas_word}Preview Projector"),
            ProjectorView::Scene => format!(
                "{canvas_word}Scene Projector — {}",
                name_of(projector.scene.as_deref().unwrap_or(""))
            ),
            ProjectorView::Source => format!(
                "Source Projector — {}",
                source_name(projector.source.as_deref().unwrap_or(""))
            ),
            ProjectorView::Multiview => "Multiview".to_string(),
        };
        shown.window.set_heading(heading.into());
        let scenes: Vec<String> = s
            .setup
            .as_ref()
            .map(|setup| setup.scenes.iter().map(|x| x.id.clone()).collect())
            .unwrap_or_default();
        let tiles = shown.tiles.borrow();
        for (index, tile) in tiles.iter().enumerate() {
            let Some(mut row) = shown.model.row_data(index) else {
                continue;
            };
            let (tag, name, tally, safe, empty) = match &tile.role {
                Role::Single => {
                    let stopped = vertical
                        && s.status
                            .as_ref()
                            .and_then(|x| x.engine.as_ref())
                            .and_then(|e| e.vertical.as_ref())
                            .is_some_and(|v| v.stopped.is_some());
                    let empty = match projector.view {
                        _ if stopped => "The vertical canvas stopped: restart the engine",
                        ProjectorView::Preview if preview.is_none() => {
                            "Nothing in preview: turn on Studio Mode"
                        }
                        _ => "",
                    };
                    (String::new(), String::new(), 0, false, empty.to_string())
                }
                Role::Preview => (
                    "PVW".into(),
                    preview
                        .as_deref()
                        .map(name_of)
                        .unwrap_or_else(|| "Studio Mode off".into()),
                    1,
                    true,
                    if preview.is_none() {
                        "Turn on Studio Mode to stage a scene".into()
                    } else {
                        String::new()
                    },
                ),
                Role::Program => (
                    "PGM".into(),
                    program.as_deref().map(name_of).unwrap_or_default(),
                    2,
                    true,
                    String::new(),
                ),
                Role::VerticalProgram => (
                    "VERT".into(),
                    s.vertical_scene
                        .as_deref()
                        .or(s.scene.as_deref())
                        .map(name_of)
                        .unwrap_or_default(),
                    2,
                    false,
                    String::new(),
                ),
                Role::Scene(id) => {
                    let number = scenes.iter().position(|x| x == id).map_or(0, |n| n + 1);
                    let tally = if program.as_deref() == Some(id) {
                        2
                    } else if preview.as_deref() == Some(id) {
                        1
                    } else {
                        0
                    };
                    (number.to_string(), name_of(id), tally, false, String::new())
                }
                Role::Empty => (String::new(), String::new(), 0, false, String::new()),
            };
            let changed = row.tag.as_str() != tag
                || row.name.as_str() != name
                || row.tally != tally
                || row.safe != safe
                || row.empty.as_str() != empty;
            if changed {
                row.tag = tag.into();
                row.name = name.into();
                row.tally = tally;
                row.safe = safe;
                row.empty = empty.into();
                shown.model.set_row_data(index, row);
            }
        }
    }

    /// Start, retarget or stop each tile's feed.
    fn feed(&self, app: &Rc<App>, shown: &Rc<Shown>) {
        let projector = shown.projector.borrow().clone();
        let (studio, vertical_studio, canvas_fps) = {
            let s = app.session.borrow();
            (
                s.studio && s.preview.is_some(),
                s.studio && (s.vertical_preview.is_some() || s.preview.is_some()),
                s.setup.as_ref().map_or(30, |s| s.canvas.fps.clamp(1, 60)),
            )
        };
        let size = shown.window.window().size();
        let mut tiles = shown.tiles.borrow_mut();
        for (index, tile) in tiles.iter_mut().enumerate() {
            let target = match &tile.role {
                // A vertical preview with nothing staged has no feed.
                Role::Single
                    if projector.is_vertical()
                        && projector.view == ProjectorView::Preview
                        && !vertical_studio =>
                {
                    None
                }
                Role::Single => projector.target(),
                Role::Preview => studio.then(|| "preview".to_string()),
                Role::Program => Some("program".into()),
                Role::VerticalProgram => Some(isocast_protocol::VERTICAL_PROGRAM.into()),
                Role::Scene(id) => Some(isocast_protocol::projector::scene_target(id)),
                Role::Empty => None,
            };
            let fps = match tile.role {
                Role::Scene(_) => canvas_fps.min(SCENE_TILE_FPS),
                _ => canvas_fps,
            };
            let cell = shown
                .model
                .row_data(index)
                .map_or((1.0, 1.0), |r| (r.w, r.h));
            let want = (
                ((size.width as f32 * cell.0).round() as u32).max(16),
                ((size.height as f32 * cell.1).round() as u32).max(16),
            );
            if target.is_none() {
                if let Some(feed) = tile.feed.take() {
                    feed.stop();
                }
                tile.target = None;
                tile.showing = None;
                tile.previous = None;
                if let Some(mut row) = shown.model.row_data(index)
                    && row.shown
                {
                    row.shown = false;
                    shown.model.set_row_data(index, row);
                }
                continue;
            }
            match &tile.feed {
                Some(feed) => {
                    if tile.target != target {
                        feed.set_target(target.as_deref());
                    }
                    if tile.size != want {
                        feed.set_size(want.0, want.1);
                    }
                    feed.set_fps(fps);
                }
                None => {
                    let arrived = tile.arrived.clone();
                    let woken = app.mode.offscreen;
                    tile.feed = Some(Feed::start(target.as_deref(), want, fps, move |incoming| {
                        {
                            let mut arrived = arrived.lock().unwrap();
                            match incoming {
                                Incoming::Frame(frame) => {
                                    arrived.frame = Some(frame);
                                    arrived.lost = None;
                                }
                                Incoming::Lost(why) => arrived.lost = Some(why),
                            }
                        }
                        if !woken {
                            let _ = slint::invoke_from_event_loop(|| {
                                crate::window::with_app(|app| app.projectors.show_frames());
                            });
                        }
                    }));
                }
            }
            tile.target = target;
            tile.size = want;
        }
    }

    /// Put the pictures that arrived in their windows.
    fn show_frames(&self) {
        let device = self.device.borrow().clone();
        let Some(device) = device else { return };
        let stats = std::env::var_os("ISOCAST_PROJECTOR_STATS").is_some();
        for shown in self.shown.borrow().values() {
            let mut tiles = shown.tiles.borrow_mut();
            for (index, tile) in tiles.iter_mut().enumerate() {
                let (frame, lost) = {
                    let mut arrived = tile.arrived.lock().unwrap();
                    (arrived.frame.take(), arrived.lost.take())
                };
                if let Some(why) = lost
                    && frame.is_none()
                    && let Some(mut row) = shown.model.row_data(index)
                    && row.empty.is_empty()
                    && !row.shown
                {
                    let _ = why;
                    row.empty = "Waiting for the picture…".into();
                    shown.model.set_row_data(index, row);
                }
                let Some(frame) = frame else { continue };
                let key = frame.slot();
                if !tile.images.contains_key(&key) {
                    tile.images
                        .retain(|(generation, _), _| *generation == key.0);
                    match frame
                        .texture(&device)
                        .and_then(|t| slint::Image::try_from(t).map_err(Into::into))
                    {
                        Ok(image) => {
                            tile.images.insert(key, image);
                        }
                        Err(error) => {
                            isocast_diagnostics::diagnostic!("warn";
                                "projector: can't show a shared frame: {error:#}"
                            );
                            continue;
                        }
                    }
                }
                if let Some(mut row) = shown.model.row_data(index) {
                    row.picture = tile.images[&key].clone();
                    row.shown = true;
                    shown.model.set_row_data(index, row);
                }
                if stats {
                    let mut latency = shown.latency.borrow_mut();
                    latency.shown += 1;
                    if tile.role == Role::Single {
                        let ms = host_now().saturating_sub(frame.time) as f64 / 1e6;
                        latency.arrivals.push(ms);
                    }
                }
                tile.previous = tile.showing.replace(frame);
            }
        }
    }

    /// Every second: tile sizes follow the window, full-screen projectors
    /// whose display went away close, and stats are written.
    fn housekeep(&self, app: &Rc<App>) {
        use slint::winit_030::WinitWindowAccessor;
        let shown: Vec<Rc<Shown>> = self.shown.borrow().values().cloned().collect();
        for shown in shown {
            self.feed(app, &shown);
            let placement = shown.placed.borrow().clone();
            if placement.fullscreen
                && let Some(display) = &placement.display
            {
                let present = shown.window.window().with_winit_window(|window| {
                    window
                        .available_monitors()
                        .any(|m| monitor_id(&m).as_deref() == Some(display.as_str()))
                });
                if present == Some(false) {
                    let id = shown.projector.borrow().id.clone();
                    self.close(app, &id, Some("its display was disconnected".to_string()));
                    app.toast(
                        crate::ToastKind::Info,
                        "Projector closed",
                        "Its display was disconnected.",
                    );
                    continue;
                }
            }
            if let Some(path) = std::env::var_os("ISOCAST_PROJECTOR_STATS") {
                let latency = shown.latency.borrow();
                let quantiles = |samples: &[f64]| {
                    let mut samples = samples.to_vec();
                    samples.sort_by(f64::total_cmp);
                    let at = |q: f64| {
                        samples
                            .get(((samples.len() as f64 - 1.0) * q).round().max(0.0) as usize)
                            .copied()
                    };
                    serde_json::json!({"n": samples.len(), "p50": at(0.5), "p95": at(0.95), "p99": at(0.99), "max": samples.last()})
                };
                let id = shown.projector.borrow().id.clone();
                let report = serde_json::json!({
                    "id": id,
                    "frames_shown": latency.shown,
                    "arrived_ms_after_frame_time": quantiles(&latency.arrivals),
                });
                let path = std::path::PathBuf::from(path).with_extension(format!("{id}.json"));
                let _ = std::fs::write(path, report.to_string());
            }
        }
    }

    fn clicked(&self, app: &Rc<App>, id: &str, index: usize, double: bool) {
        let Some(shown) = self.shown.borrow().get(id).cloned() else {
            return;
        };
        let role = shown.tiles.borrow().get(index).map(|t| t.role.clone());
        match role {
            Some(Role::Scene(scene)) => {
                if double {
                    let studio = app.session.borrow().studio;
                    if studio {
                        app.choose_scene(&scene);
                        app.take(cut());
                    } else {
                        app.call(
                            Method::SwitchScene {
                                scene,
                                transition: cut(),
                                canvas: Default::default(),
                            },
                            |app, result| {
                                if let Err(error) = result {
                                    app.failed("Couldn’t switch scenes", &error);
                                }
                            },
                        );
                    }
                } else {
                    app.choose_scene(&scene);
                }
            }
            // The preview double-clicked: take it (Studio Mode).
            Some(Role::Preview) if double => app.take(None),
            // A single projector: double click goes in and out of full
            // screen.
            Some(Role::Single) if double => self.toggle_fullscreen(app, &shown),
            _ => {}
        }
    }

    fn toggle_fullscreen(&self, app: &Rc<App>, shown: &Rc<Shown>) {
        use slint::winit_030::WinitWindowAccessor;
        // Into full screen on the display the window is on now.
        let on = shown
            .window
            .window()
            .with_winit_window(|window| window.current_monitor().as_ref().and_then(monitor_id))
            .flatten();
        {
            let mut placed = shown.placed.borrow_mut();
            placed.fullscreen = !placed.fullscreen;
            if placed.fullscreen && on.is_some() {
                placed.display = on;
            }
        }
        self.place(app, shown);
        self.report(app, shown);
    }

    fn key(&self, app: &Rc<App>, id: &str, key: &str) {
        let Some(shown) = self.shown.borrow().get(id).cloned() else {
            return;
        };
        match key {
            "close" => self.close(app, id, None),
            // Escape leaves full screen; a window stays.
            "escape" if shown.placed.borrow().fullscreen => self.toggle_fullscreen(app, &shown),
            "fullscreen" => self.toggle_fullscreen(app, &shown),
            _ => {
                if let Some(n) = key
                    .strip_prefix("scene:")
                    .and_then(|n| n.parse::<usize>().ok())
                {
                    let scene = app
                        .session
                        .borrow()
                        .setup
                        .as_ref()
                        .and_then(|s| s.scenes.get(n.saturating_sub(1)).map(|x| x.id.clone()));
                    if let Some(scene) = scene {
                        app.choose_scene(&scene);
                    }
                }
            }
        }
    }

    /// The "Open Projector" submenu for the menu of `owner`, if it shows
    /// something a projector can show.
    pub fn extend_menu(&self, app: &Rc<App>, owner: &str, menu: MenuBuilder) -> MenuBuilder {
        let (studio, has_vertical) = {
            let s = app.session.borrow();
            (s.studio, s.vertical.is_some())
        };
        // A label, the view, its scene or source, and whether it is of the
        // vertical canvas (a multiview: with the vertical program).
        let mut wanted: Vec<(&str, ProjectorView, String, bool)> = Vec::new();
        if owner == "canvas" || owner.starts_with("canvas:item:") {
            if studio {
                wanted.push((
                    "Preview Projector",
                    ProjectorView::Preview,
                    String::new(),
                    false,
                ));
            }
            wanted.push((
                "Program Projector",
                ProjectorView::Program,
                String::new(),
                false,
            ));
            wanted.push(("Multiview", ProjectorView::Multiview, String::new(), false));
        }
        // The vertical monitor (#96).
        if (owner == VERTICAL_MONITOR || owner == "viewer:vertical") && has_vertical {
            if studio {
                wanted.push((
                    "Vertical Preview Projector",
                    ProjectorView::Preview,
                    String::new(),
                    true,
                ));
            }
            wanted.push((
                "Vertical Program Projector",
                ProjectorView::Program,
                String::new(),
                true,
            ));
        }
        if let Some(scene) = owner.strip_prefix("scenes:")
            && !scene.is_empty()
        {
            wanted.push((
                "Scene Projector",
                ProjectorView::Scene,
                scene.to_string(),
                false,
            ));
            if has_vertical {
                wanted.push((
                    "Vertical Scene Projector",
                    ProjectorView::Scene,
                    scene.to_string(),
                    true,
                ));
            }
        }
        if owner.starts_with("sources:item:") {
            // The sources area selected the item before building its menu.
            let source = {
                let s = app.session.borrow();
                let id = s
                    .selected(|_, chain| chain[chain.len() - 1].source.clone())
                    .flatten();
                s.setup.as_ref().zip(id).and_then(|(setup, id)| {
                    crate::model::find_source(setup, &id)
                        .filter(|x| x.kind.has_video())
                        .map(|x| x.id.clone())
                })
            };
            if let Some(source) = source {
                wanted.push(("Source Projector", ProjectorView::Source, source, false));
            }
        }
        if wanted.is_empty() {
            return menu;
        }
        let displays = self.displays.borrow().clone();
        let mut menu = menu.separator();
        for (label, view, arg, vertical) in wanted {
            let base = format!(
                "projector|{}{}|{arg}|",
                view_name(view),
                if vertical { VERTICAL_SUFFIX } else { "" }
            );
            let mut items = MenuBuilder::new().item(&format!("{base}window"), "Windowed");
            if !displays.is_empty() {
                items = items.separator().heading("Full Screen");
                for display in &displays {
                    let name = if display.name.is_empty() {
                        format!("Display {}×{}", display.width, display.height)
                    } else {
                        display.name.clone()
                    };
                    items = items.item(&format!("{base}{}", display.id), &name);
                }
            }
            menu = menu.submenu(label, items);
        }
        menu
    }

    /// A projector menu item (`projector|VIEW|ARG|window` or
    /// `…|DISPLAY_ID`), from a context menu or the menu bar. True if it
    /// was one.
    pub fn chosen(&self, app: &Rc<App>, id: &str) -> bool {
        if id == "projector_close_all" {
            app.call(
                Method::ProjectorClose(CloseParams {
                    all: true,
                    ..Default::default()
                }),
                |_, _| {},
            );
            return true;
        }
        let Some(rest) = id.strip_prefix("projector|") else {
            return false;
        };
        let mut parts = rest.splitn(3, '|');
        let (Some(view), Some(arg), Some(place)) = (parts.next(), parts.next(), parts.next())
        else {
            return true;
        };
        let (view, vertical) = match view.strip_suffix(VERTICAL_SUFFIX) {
            Some(view) => (view, true),
            None => (view, false),
        };
        let Ok(view) = serde_json::from_value::<ProjectorView>(serde_json::json!(view)) else {
            return true;
        };
        let arg = (!arg.is_empty()).then_some(arg);
        let display = (place != "window").then_some(place);
        // The app's multiview shows the vertical program whenever there is
        // a vertical canvas.
        let vertical = vertical
            || (view == ProjectorView::Multiview && app.session.borrow().vertical.is_some());
        self.open(app, view, arg, display, vertical);
        true
    }
}

impl Area for Projectors {
    fn connect(&self, app: &Rc<App>, _window: &AppWindow) {
        *self.app.borrow_mut() = Rc::downgrade(app);
    }

    fn update(&self, app: &Rc<App>, _window: &AppWindow, changed: Changed) {
        if !changed.any(Changed::SETUP | Changed::SCENES | Changed::CONFIRMED) {
            // A vertical projector says when the vertical canvas stopped.
            if changed.any(Changed::STATUS) {
                for shown in self.shown.borrow().values() {
                    if shown.projector.borrow().is_vertical() {
                        self.label(app, shown);
                    }
                }
            }
            return;
        }
        let shown: Vec<Rc<Shown>> = self.shown.borrow().values().cloned().collect();
        for shown in shown {
            self.build_tiles(app, &shown);
        }
    }

    fn menu(&self, app: &Rc<App>, id: &str) -> bool {
        self.chosen(app, id)
    }

    fn menu_chosen(&self, app: &Rc<App>, _owner: &str, id: &str) -> bool {
        self.chosen(app, id)
    }

    fn event(&self, app: &Rc<App>, event: &Event) {
        match event {
            Event::Projectors { projectors, reason } => {
                self.sync(app, projectors.clone(), reason.clone());
            }
            // A display came or went: the menus' list follows.
            Event::DeviceChanged {
                kind: isocast_protocol::api::DeviceKind::Display,
                ..
            } => self.load(app),
            _ => {}
        }
    }

    fn state(
        &self,
        app: &Rc<App>,
        _window: &AppWindow,
        name: &str,
        arg: &str,
    ) -> Option<anyhow::Result<()>> {
        if name != "projector" {
            return None;
        }
        let (view, rest) = arg.split_once(':').unwrap_or((arg, ""));
        // `vertical-program`, `vertical-scene:ID`…: of the vertical canvas;
        // `vertical-multiview`: with the vertical program in a tile.
        let (view, vertical) = match view.strip_prefix("vertical-") {
            Some(view) => (view, true),
            None => (view, false),
        };
        let Ok(view) = serde_json::from_value::<ProjectorView>(serde_json::json!(view)) else {
            return Some(Err(anyhow::anyhow!(
                "projector:[vertical-]VIEW: VIEW is one of {}",
                ProjectorView::NAMES.join(", ")
            )));
        };
        self.open(
            app,
            view,
            (!rest.is_empty()).then_some(rest),
            None,
            vertical,
        );
        Some(Ok(()))
    }
}
