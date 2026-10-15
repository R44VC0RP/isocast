//! The monitors' pictures in the window: the daemon's frames (shared GPU
//! surfaces, never copied) with, over the edited one, the selected item's
//! box drawn at the layout of the setup revision each frame was drawn
//! from, so box and picture can't drift apart, even while the item is
//! dragged. One `Preview` per monitor: the edited one (`program`, or in
//! Studio Mode `preview`) and Studio Mode's program.
//!
//! Two ways to put them on screen:
//! - macOS: Core Animation layers under the interface (`layers.rs`); the
//!   interface isn't redrawn for each frame.
//! - Anywhere (`--preview image`): the surface as a wgpu texture in a Slint
//!   `Image`, and the box drawn by Slint, set in the same event so they
//!   are drawn in the same frame. Slint then redraws the window per frame.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};

use isocast_client::shared::SharedFrame;
use slint::ComponentHandle;
use slint::wgpu_30::wgpu;

use crate::frames::Feed;
#[cfg(target_os = "macos")]
pub use crate::layers::{BoxShape, BoxStyle};
use crate::{AppWindow, CanvasArea, Theme, Ui};

/// The selected item's box (see layers.rs; the same off macOS).
#[cfg(not(target_os = "macos"))]
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BoxShape {
    pub rect: [f32; 4],
    pub rotate: f32,
}

#[cfg(not(target_os = "macos"))]
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BoxStyle {
    pub locked: bool,
    pub cropping: bool,
}

/// How many layouts (setup revisions) are kept for frames still on the way.
const LAYOUTS: usize = 128;
/// How long a frame waits for its revision's layout before it is shown
/// anyway (with the newest layout known).
const LAYOUT_WAIT: Duration = Duration::from_millis(150);

/// The items' boxes in the edited scene at one setup revision.
#[derive(Debug, Clone)]
struct Layout {
    revision: u64,
    scene: String,
    rects: HashMap<String, BoxShape>,
}

#[derive(Default)]
struct Layouts(VecDeque<Layout>);

impl Layouts {
    fn newest(&self) -> u64 {
        self.0.back().map_or(0, |l| l.revision)
    }

    /// The layout a frame of `revision` shows: the newest at or before it.
    fn at(&self, revision: Option<u64>) -> Option<&Layout> {
        match revision {
            Some(revision) => self.0.iter().rev().find(|l| l.revision <= revision),
            None => self.0.back(),
        }
    }

    fn insert(&mut self, layout: Layout) {
        let at = self.0.partition_point(|l| l.revision < layout.revision);
        if self
            .0
            .get(at)
            .is_some_and(|l| l.revision == layout.revision)
        {
            self.0[at] = layout;
        } else {
            self.0.insert(at, layout);
        }
        while self.0.len() > LAYOUTS {
            self.0.pop_front();
        }
    }
}

/// What the preview counted (`--bench`, `--check`).
#[derive(Debug, Default, Clone)]
pub struct Stats {
    /// Frames from the daemon.
    pub received: u64,
    /// Frames put on screen.
    pub shown: u64,
    /// Frames that waited for their revision's layout.
    pub waited_for_layout: u64,
    /// Frames shown without their revision's layout (it didn't come in
    /// time): their box may be off.
    pub shown_without_layout: u64,
    /// Content checks (`--check`): frames read, black ones, ones whose
    /// rows were the same as the frame before (a stale picture), the
    /// longest run of those, and the box checked against the picture.
    pub checked: u64,
    pub black: u64,
    pub repeated: u64,
    pub longest_repeat: u64,
    pub sync_checked: u64,
    pub sync_off: u64,
    pub first_sync_off: Option<String>,
    /// The longest gap between shown frames.
    pub longest_gap: Duration,
    /// New picture sizes asked of the daemon (each makes it a new ring
    /// of surfaces).
    pub size_requests: u64,
    /// Times the stream was lost.
    pub lost: u64,
    pub last_lost: Option<String>,
}

/// While a monitor is being resized: the daemon is asked for a size that
/// keeps changing at most every `RESIZE_EVERY`, and for one that has held
/// for `RESIZE_SETTLE` at once (pausing mid-drag can ask sooner; every
/// request, settled or not, restarts the `RESIZE_EVERY` period).
const RESIZE_EVERY: Duration = Duration::from_millis(500);
const RESIZE_SETTLE: Duration = Duration::from_millis(120);
/// After a handle drag: how long the box stays where the pointer left it
/// at most, waiting for a frame that shows it there (one that never comes
/// if the daemon refused the change).
const LIVE_BOX_HOLD: Duration = Duration::from_millis(500);

/// A dragged handle's box: the item's (path, in `scene`) only.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveBox {
    pub scene: String,
    pub path: String,
    pub shape: BoxShape,
    ended: Option<Instant>,
}

impl LiveBox {
    pub fn new(scene: &str, path: &str, shape: BoxShape) -> Self {
        Self {
            scene: scene.to_string(),
            path: path.to_string(),
            shape,
            ended: None,
        }
    }
}

/// Which monitor a preview fills.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Role {
    /// The edited monitor, with the selected item's box.
    Editor,
    /// Studio Mode's program monitor.
    Program,
}

/// Where pictures go: the shared layers (macOS), or Slint images.
#[derive(Clone)]
pub enum Output {
    #[cfg(target_os = "macos")]
    Layers(Rc<RefCell<crate::layers::Layers>>),
    Image(wgpu::Device),
}

impl Output {
    /// The layers or the images under `app`, as `mode` says.
    pub fn new(
        app: &AppWindow,
        mode: Mode,
        device: &wgpu::Device,
        test_outline: bool,
    ) -> anyhow::Result<Self> {
        app.global::<CanvasArea>().set_test_outline(test_outline);
        Ok(match mode {
            #[cfg(target_os = "macos")]
            Mode::Layers => {
                app.global::<CanvasArea>().set_native_picture(true);
                Output::Layers(Rc::new(RefCell::new(crate::layers::Layers::attach(
                    app.window(),
                    theme_colours(app),
                    test_outline,
                )?)))
            }
            #[cfg(not(target_os = "macos"))]
            Mode::Layers => anyhow::bail!("--preview layers is macOS only; use --preview image"),
            Mode::Image => {
                app.global::<CanvasArea>().set_native_picture(false);
                Output::Image(device.clone())
            }
        })
    }
}

/// The layers' colours, from the theme (so a theme's change follows).
#[cfg(target_os = "macos")]
fn theme_colours(app: &AppWindow) -> crate::layers::Colours {
    let theme = app.global::<Theme>();
    let rgb = |c: slint::Color| [c.red(), c.green(), c.blue()];
    let rgba = |c: slint::Color| [c.red(), c.green(), c.blue(), c.alpha()];
    crate::layers::Colours {
        window: rgb(theme.get_surface_chrome()),
        stage: rgb(theme.get_surface_stage()),
        monitor: rgb(theme.get_surface_monitor()),
        accent: rgb(theme.get_selection_ring()),
        ring: rgba(theme.get_box_ring()),
        locked: rgb(theme.get_text_2()),
        bone: rgb(theme.get_bone()),
        handle_edge: rgba(theme.get_handle_edge()),
        meter: crate::layers::MeterColours {
            inset: rgb(theme.get_surface_inset()),
            track: rgb(theme.get_meter_capsule_track()),
            zones: [
                rgb(theme.get_meter_ok()),
                rgb(theme.get_meter_warn()),
                rgb(theme.get_meter_clip()),
            ],
            peaks: [
                rgba(theme.get_meter_ok_peak()),
                rgba(theme.get_meter_warn_peak()),
                rgba(theme.get_meter_clip_peak()),
            ],
            warn_db: theme.get_meter_warn_db(),
            clip_db: theme.get_meter_clip_db(),
            radius: f64::from(theme.get_r()) - 1.0,
        },
    }
}

pub struct Preview {
    app: slint::Weak<AppWindow>,
    role: Role,
    output: Output,
    /// Image mode: the frames' images by slot.
    images: HashMap<(u64, u32), slint::Image>,
    feed: Feed,
    /// What the feed shows (none: nothing).
    target: Option<String>,
    fps: u32,
    hidden: bool,
    /// The picture's size in pixels on screen; the frames are asked for it
    /// (see `window_drawn`).
    size: (u32, u32),
    /// A size not asked for yet (the monitor is still being resized), the
    /// timer that asks once it settles, and when the daemon was last asked.
    pending_size: Rc<Cell<Option<(u32, u32)>>>,
    size_timer: slint::Timer,
    size_asked_at: Rc<Cell<Option<Instant>>>,
    /// The size last asked of the daemon (the motion harness waits for
    /// frames of it).
    size_asked: Rc<Cell<(u32, u32)>>,
    size_requests: Rc<Cell<u64>>,
    layouts: Layouts,
    /// The box as the pointer has it while a handle is dragged, drawn at
    /// once instead of with the frame showing it; kept after the drag
    /// (with when it ended) until a frame shows the box there.
    live_box: Option<LiveBox>,
    style: BoxStyle,
    /// The frame on screen, and the one before it (its slot isn't handed
    /// back until the screen has surely moved on from it).
    showing: Option<SharedFrame>,
    previous: Option<SharedFrame>,
    waiting: Option<(SharedFrame, Instant)>,
    last_shown_at: Option<Instant>,
    check: bool,
    last_rows: Option<u64>,
    repeat_run: u64,
    pub stats: Stats,
}

/// How the preview is put on screen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    /// Core Animation layers (macOS).
    Layers,
    /// A Slint image.
    Image,
}

impl Mode {
    pub fn default_for_platform() -> Self {
        if cfg!(target_os = "macos") {
            Mode::Layers
        } else {
            Mode::Image
        }
    }
}

impl Preview {
    /// Call once the window is shown (the layers need its view).
    pub fn new(
        app: &AppWindow,
        role: Role,
        output: Output,
        feed: Feed,
        fps: u32,
        check: bool,
    ) -> Self {
        let preview = Self {
            app: app.as_weak(),
            role,
            output,
            images: HashMap::new(),
            feed,
            target: None,
            fps,
            hidden: false,
            size: (0, 0),
            pending_size: Rc::default(),
            size_timer: slint::Timer::default(),
            size_asked_at: Rc::default(),
            size_asked: Rc::default(),
            size_requests: Rc::default(),
            layouts: Layouts::default(),
            live_box: None,
            style: BoxStyle::default(),
            showing: None,
            previous: None,
            waiting: None,
            last_shown_at: None,
            check,
            last_rows: None,
            repeat_run: 0,
            stats: Stats::default(),
        };
        preview.set_waiting(app, true, "");
        preview
    }

    /// The window was hidden (covered, minimized) or shown again: a hidden
    /// window takes one frame a second, so the daemon hardly draws for it.
    pub fn set_hidden(&mut self, hidden: bool) {
        self.hidden = hidden;
        self.feed.set_fps(if hidden { 1 } else { self.fps });
    }

    /// At most `fps` frames a second while the window shows.
    pub fn set_fps(&mut self, fps: u32) {
        self.fps = fps;
        if !self.hidden {
            self.feed.set_fps(fps);
        }
    }

    /// Show `target` (none: nothing; the monitor says why itself).
    pub fn set_target(&mut self, target: Option<&str>) {
        if self.target.as_deref() == target {
            return;
        }
        self.target = target.map(str::to_string);
        self.feed.set_target(target);
        // Waiting (a spinner) until the new target's first frame; the
        // frame on screen is held until then.
        self.previous = self.showing.take();
        if let Some(app) = self.app.upgrade() {
            self.set_waiting(&app, target.is_some(), "");
        }
    }

    /// How the box looks (locked, cropping).
    pub fn set_style(&mut self, style: BoxStyle) {
        if style != self.style {
            self.style = style;
            self.redraw_box();
        }
    }

    fn set_waiting(&self, app: &AppWindow, waiting: bool, error: &str) {
        let canvas = app.global::<CanvasArea>();
        match self.role {
            Role::Editor => {
                canvas.set_waiting(waiting);
                canvas.set_picture_error(error.into());
            }
            Role::Program => canvas.set_program_waiting(waiting),
        }
    }

    /// A frame from the daemon.
    pub fn frame(&mut self, frame: SharedFrame) {
        self.stats.received += 1;
        if self.role == Role::Program {
            self.show(frame);
            return;
        }
        // How long frames have been held for a layout (the newer frame
        // replaces a held one, but the wait goes on from the first).
        let held_since = self.waiting.take().map(|(_, since)| since);
        let unknown =
            frame.revision.is_some_and(|r| r > self.layouts.newest()) && self.layouts.newest() > 0;
        if unknown && held_since.is_none_or(|since| crate::clock::since(since) < LAYOUT_WAIT) {
            // Drawn from a setup this window hasn't read yet: hold it until
            // the layout comes, so the box isn't drawn at the old one.
            self.stats.waited_for_layout += 1;
            self.waiting = Some((frame, held_since.unwrap_or_else(crate::clock::now)));
            return;
        }
        if unknown {
            // Its layout never came: show it with the newest one.
            self.stats.shown_without_layout += 1;
        }
        self.show(frame);
    }

    /// The stream was lost (the daemon went away); it reconnects itself.
    pub fn lost(&mut self, why: String) {
        self.stats.lost += 1;
        self.waiting = None;
        match &mut self.output {
            #[cfg(target_os = "macos")]
            Output::Layers(layers) => layers.borrow_mut().clear(self.role == Role::Program),
            Output::Image(_) => {
                if let Some(app) = self.app.upgrade() {
                    let canvas = app.global::<CanvasArea>();
                    match self.role {
                        Role::Editor => canvas.set_picture(slint::Image::default()),
                        Role::Program => canvas.set_program_picture(slint::Image::default()),
                    }
                }
            }
        }
        self.images.clear();
        self.previous = None;
        self.showing = None;
        if let Some(app) = self.app.upgrade() {
            self.set_waiting(&app, false, &format!("No picture: {why}"));
        }
        self.stats.last_lost = Some(why);
    }

    /// The items' boxes (by path) in scene `scene` at setup `revision`:
    /// frames drawn from that revision show the box there.
    pub fn layout(&mut self, revision: u64, scene: &str, rects: HashMap<String, BoxShape>) {
        self.layouts.insert(Layout {
            revision,
            scene: scene.to_string(),
            rects,
        });
        self.layouts_changed();
    }

    /// The daemon went away: a new one numbers its revisions afresh.
    pub fn forget_layouts(&mut self) {
        self.layouts = Layouts::default();
    }

    fn layouts_changed(&mut self) {
        if let Some((frame, since)) = self.waiting.take() {
            if frame.revision.is_some_and(|r| r <= self.layouts.newest()) {
                self.show(frame);
            } else {
                self.waiting = Some((frame, since));
            }
        } else {
            self.redraw_box();
        }
    }

    /// A handle is being dragged: draw the box at `shape` now, not when a
    /// frame drawn from it comes (a frame or more later, and unevenly
    /// while the daemon is busy), so the handle stays under the pointer
    /// (GitHub #24). None: the drag ended; the box goes back to the
    /// frames' layout once one shows it where the drag left it.
    pub fn set_live_box(&mut self, live: Option<LiveBox>) {
        self.live_box = match (live, self.live_box.take()) {
            (Some(live), _) => Some(live),
            (None, Some(held)) => Some(LiveBox {
                ended: held.ended.or(Some(crate::clock::now())),
                ..held
            }),
            (None, None) => None,
        };
        self.redraw_box();
    }

    /// The selection or the window's layout changed: the box again, over
    /// the same frame.
    pub fn redraw_box(&mut self) {
        if self.role != Role::Editor {
            return;
        }
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let revision = self.showing.as_ref().and_then(|f| f.revision);
        let rect = self.box_rect(&app, revision);
        self.put(&app, None, rect);
    }

    /// `--drag-test`: the pointer's mark in the native layers, with the
    /// box (false: no layers, Slint draws it).
    pub fn show_test_pointer(&self, rect: [f64; 4]) -> bool {
        match &self.output {
            #[cfg(target_os = "macos")]
            Output::Layers(layers) => layers.borrow().show_test_pointer(rect),
            _ => {
                let _ = rect;
                false
            }
        }
    }

    /// Slint is about to draw the window: if its layout moved or resized
    /// the monitor, follow (the layers' place, the frames' size).
    pub fn window_drawn(&mut self) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let size = match self.role {
            Role::Editor => (
                app.get_picture_pixel_width(),
                app.get_picture_pixel_height(),
            ),
            Role::Program => (
                app.get_program_pixel_width(),
                app.get_program_pixel_height(),
            ),
        };
        let size = (size.0.max(16) as u32, size.1.max(16) as u32);
        if size != self.size {
            self.size = size;
            self.ask_size(size);
        }
        #[cfg(target_os = "macos")]
        if self.role == Role::Editor
            && let Output::Layers(layers) = &self.output
            && layers.borrow().geometry() != geometry(&app)
        {
            self.redraw_box();
        }
    }

    /// Ask the daemon for frames of `size`. Each new size makes it a new
    /// ring of surfaces, so while the monitor is being resized (a splitter
    /// or the window dragged: a new size every frame, GitHub #24) it is
    /// asked at most every `RESIZE_EVERY` and once the size settles; the
    /// layers (or the image) scale the frames meanwhile.
    fn ask_size(&mut self, size: (u32, u32)) {
        let due = self
            .size_asked_at
            .get()
            .is_none_or(|at| crate::clock::since(at) >= RESIZE_EVERY);
        if due {
            self.pending_size.set(None);
            self.size_timer.stop();
            self.size_asked_at.set(Some(crate::clock::now()));
            self.size_asked.set(size);
            self.feed.set_size(size.0, size.1);
            self.size_requests.set(self.size_requests.get() + 1);
            return;
        }
        self.pending_size.set(Some(size));
        let (feed, pending, requests, asked_at, asked) = (
            self.feed.clone(),
            self.pending_size.clone(),
            self.size_requests.clone(),
            self.size_asked_at.clone(),
            self.size_asked.clone(),
        );
        self.size_timer
            .start(slint::TimerMode::SingleShot, RESIZE_SETTLE, move || {
                if let Some((width, height)) = pending.take() {
                    asked_at.set(Some(crate::clock::now()));
                    asked.set((width, height));
                    feed.set_size(width, height);
                    requests.set(requests.get() + 1);
                }
            });
    }

    /// The picture size last asked of the daemon.
    pub fn size_asked(&self) -> (u32, u32) {
        self.size_asked.get()
    }

    pub fn stats(&self) -> Stats {
        Stats {
            size_requests: self.size_requests.get(),
            ..self.stats.clone()
        }
    }

    fn box_rect(&mut self, app: &AppWindow, revision: Option<u64>) -> Option<BoxShape> {
        let ui = app.global::<Ui>();
        let selected = ui.get_selected_item();
        let scene = ui.get_editing_scene();
        // Held only for the item and scene it was dragged in: another
        // selection (or scene) shows its own box at once.
        if self
            .live_box
            .as_ref()
            .is_some_and(|live| live.path != selected.as_str() || live.scene != scene.as_str())
        {
            self.live_box = None;
        }
        if selected.is_empty() {
            return None;
        }
        if let Some(live) = &self.live_box {
            let caught_up = self
                .layouts
                .at(revision)
                .filter(|layout| layout.scene == scene.as_str())
                .and_then(|layout| layout.rects.get(selected.as_str()))
                == Some(&live.shape);
            match live.ended {
                Some(at) if caught_up || crate::clock::since(at) > LIVE_BOX_HOLD => {
                    self.live_box = None
                }
                _ => return Some(live.shape),
            }
        }
        // Only over a picture of the scene being edited.
        self.layouts
            .at(revision)
            .filter(|layout| layout.scene == scene.as_str())
            .and_then(|layout| layout.rects.get(selected.as_str()).copied())
    }

    fn show(&mut self, frame: SharedFrame) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let rect = match self.role {
            Role::Editor => self.box_rect(&app, frame.revision),
            Role::Program => None,
        };
        if self.check && self.role == Role::Editor {
            self.check_frame(&app, &frame, rect.map(|b| b.rect));
        }
        if self.showing.is_none() {
            self.set_waiting(&app, false, "");
        }
        self.put(&app, Some(&frame), rect);
        self.stats.shown += 1;
        let now = crate::clock::now();
        if let Some(last) = self.last_shown_at.replace(now) {
            self.stats.longest_gap = self.stats.longest_gap.max(now - last);
        }
        self.previous = self.showing.replace(frame);
    }

    fn put(&mut self, app: &AppWindow, frame: Option<&SharedFrame>, rect: Option<BoxShape>) {
        match &self.output {
            #[cfg(target_os = "macos")]
            Output::Layers(layers) => {
                let mut layers = layers.borrow_mut();
                match (self.role, frame) {
                    (Role::Program, Some(frame)) => layers.show_program(frame),
                    (Role::Program, None) => {}
                    (Role::Editor, _) => layers.show(frame, rect, self.style, geometry(app)),
                }
            }
            Output::Image(device) => {
                let canvas = app.global::<CanvasArea>();
                if let Some(frame) = frame {
                    let key = frame.slot();
                    if !self.images.contains_key(&key) {
                        // A new ring: the old one's images go.
                        self.images
                            .retain(|(generation, _), _| *generation == key.0);
                        match frame
                            .texture(device)
                            .and_then(|t| slint::Image::try_from(t).map_err(Into::into))
                        {
                            Ok(image) => {
                                self.images.insert(key, image);
                            }
                            Err(error) => {
                                isocast_diagnostics::diagnostic!("warn";
                                    "preview: can't show a shared frame: {error:#}"
                                );
                                return;
                            }
                        }
                    }
                    match self.role {
                        Role::Editor => canvas.set_picture(self.images[&key].clone()),
                        Role::Program => canvas.set_program_picture(self.images[&key].clone()),
                    }
                }
                if self.role == Role::Editor {
                    canvas.set_shown_box_visible(rect.is_some());
                    if let Some(BoxShape {
                        rect: [x, y, w, h],
                        rotate,
                    }) = rect
                    {
                        canvas.set_shown_box_x(x);
                        canvas.set_shown_box_y(y);
                        canvas.set_shown_box_w(w);
                        canvas.set_shown_box_h(h);
                        canvas.set_shown_box_rotate(rotate);
                    }
                }
            }
        }
    }

    /// `--check`: read the frame's surface (a few rows and a grid) for a
    /// black or stale picture, and, while an item is selected, whether the
    /// box is where the item is in the picture (the drag test's block is
    /// pure magenta).
    fn check_frame(&mut self, app: &AppWindow, frame: &SharedFrame, rect: Option<[f32; 4]>) {
        let canvas = (
            app.global::<Ui>().get_canvas_width().max(1) as f32,
            app.global::<Ui>().get_canvas_height().max(1) as f32,
        );
        // Just inside and outside each edge of the box, through its middle
        // (6 canvas pixels off the edge), as fractions of the picture.
        let probes: Vec<(f32, f32)> = rect
            .map(|[x, y, w, h]| {
                let (cx, cy) = (x + w * 0.5, y + h * 0.5);
                [
                    (x + 6.0, cy),
                    (x - 6.0, cy),
                    (x + w - 6.0, cy),
                    (x + w + 6.0, cy),
                    (cx, y + 6.0),
                    (cx, y - 6.0),
                    (cx, y + h - 6.0),
                    (cx, y + h + 6.0),
                ]
                .map(|(px, py)| (px / canvas.0, py / canvas.1))
                .to_vec()
            })
            .unwrap_or_default();
        let Some(read) = read_surface(frame, &probes) else {
            return;
        };
        self.stats.checked += 1;
        if read
            .grid
            .iter()
            .all(|p| p[0] < 16 && p[1] < 16 && p[2] < 16)
        {
            self.stats.black += 1;
        }
        if self.last_rows.replace(read.rows) == Some(read.rows) {
            self.stats.repeated += 1;
            self.repeat_run += 1;
            self.stats.longest_repeat = self.stats.longest_repeat.max(self.repeat_run);
        } else {
            self.repeat_run = 0;
        }
        // Edges off the picture can't be checked; inside ones must be the
        // item's colour and outside ones not.
        let edges: Vec<(bool, bool)> = read
            .probes
            .chunks(2)
            .filter_map(|pair| Some((is_magenta(pair[0]?), is_magenta(pair[1]?))))
            .collect();
        if !edges.is_empty() && edges.iter().any(|(inside, _)| *inside) {
            self.stats.sync_checked += 1;
            if edges.iter().any(|(inside, outside)| !inside || *outside) {
                self.stats.sync_off += 1;
                self.stats.first_sync_off.get_or_insert_with(|| {
                    format!(
                        "revision {:?}: box {rect:?}, edges (inside, outside) magenta: {edges:?}",
                        frame.revision
                    )
                });
            }
        }
    }
}

/// Where the stage and the pictures are in the window.
#[cfg(target_os = "macos")]
fn geometry(app: &AppWindow) -> crate::layers::Geometry {
    let (ax, ay) = (app.get_stage_x() as f64, app.get_stage_y() as f64);
    crate::layers::Geometry {
        area: [
            ax,
            ay,
            app.get_stage_width() as f64,
            app.get_stage_height() as f64,
        ],
        picture: [
            app.get_picture_left() as f64 - ax,
            app.get_picture_top() as f64 - ay,
            app.get_picture_width() as f64,
            app.get_picture_height() as f64,
        ],
        program: [
            app.get_program_x() as f64 - ax,
            app.get_program_y() as f64 - ay,
            app.get_program_width() as f64,
            app.get_program_height() as f64,
        ],
        scale: app.get_canvas_scale() as f64,
        meter: meter_rect(app),
    }
}

/// The control bar's master level's inside, in window points (its native
/// layer's place).
pub fn meter_rect(app: &AppWindow) -> [f64; 4] {
    [
        app.get_master_meter_x() as f64,
        app.get_master_meter_y() as f64,
        app.get_master_meter_width() as f64,
        app.get_master_meter_height() as f64,
    ]
}

fn is_magenta(p: [u8; 4]) -> bool {
    p[0] > 220 && p[1] < 60 && p[2] > 220
}

/// What `--check` reads of a frame.
struct Read {
    /// An 8×8 grid of pixels.
    grid: Vec<[u8; 4]>,
    /// A hash of three whole rows: the same as the frame before means a
    /// picture that didn't move.
    rows: u64,
    /// The pixels asked for (`None` off the picture).
    probes: Vec<Option<[u8; 4]>>,
}

/// Read the frame's grid and rows, and the pixels at `probes` (fractions
/// of the width and height).
#[cfg(target_os = "macos")]
fn read_surface(frame: &SharedFrame, probes: &[(f32, f32)]) -> Option<Read> {
    use std::hash::{Hash, Hasher};

    use objc2_io_surface::IOSurfaceLockOptions;

    let surface = frame.surface().io_surface();
    let (width, height) = (surface.width(), surface.height());
    // SAFETY: a read-only lock of a surface the daemon has finished
    // drawing; rows are read within its bounds.
    unsafe {
        if surface.lock(IOSurfaceLockOptions::ReadOnly, std::ptr::null_mut()) != 0 {
            return None;
        }
        let base = surface.base_address().as_ptr() as *const u8;
        let stride = surface.bytes_per_row();
        let row = |y: usize| std::slice::from_raw_parts(base.add(y * stride), width * 4);
        let pixel = |x: usize, y: usize| {
            let r = row(y);
            [r[x * 4], r[x * 4 + 1], r[x * 4 + 2], r[x * 4 + 3]]
        };
        let mut grid = Vec::with_capacity(64);
        for gy in 0..8 {
            for gx in 0..8 {
                grid.push(pixel((gx * 2 + 1) * width / 16, (gy * 2 + 1) * height / 16));
            }
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for y in [height / 5, height / 2, height * 9 / 10] {
            row(y).hash(&mut hasher);
        }
        let probes = probes
            .iter()
            .map(|&(fx, fy)| {
                let (x, y) = (fx * width as f32, fy * height as f32);
                (x >= 0.0 && y >= 0.0 && x < width as f32 && y < height as f32)
                    .then(|| pixel(x as usize, y as usize))
            })
            .collect();
        surface.unlock(IOSurfaceLockOptions::ReadOnly, std::ptr::null_mut());
        Some(Read {
            grid,
            rows: hasher.finish(),
            probes,
        })
    }
}

#[cfg(not(target_os = "macos"))]
fn read_surface(_: &SharedFrame, _: &[(f32, f32)]) -> Option<Read> {
    None
}
