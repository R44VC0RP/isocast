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

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};

use isocast_client::shared::SharedFrame;
use slint::ComponentHandle;
use slint::wgpu_30::wgpu;

use crate::frames::Feed;
#[cfg(target_os = "macos")]
pub use crate::layers::{BoxShape, BoxStyle};
use crate::{AppWindow, CanvasArea, Ui};

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
    /// Times the stream was lost.
    pub lost: u64,
    pub last_lost: Option<String>,
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
                    crate::layers::Colours {
                        window: [0x14, 0x16, 0x19],
                        stage: [0x11, 0x13, 0x15],
                        monitor: [0x0b, 0x0c, 0x0d],
                        accent: [0xb4, 0x68, 0x3f],
                        locked: [0xb0, 0xab, 0xa3],
                        bone: [0xee, 0xea, 0xe2],
                    },
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
    /// The picture's size in pixels the frames are asked for.
    size: (u32, u32),
    layouts: Layouts,
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
            layouts: Layouts::default(),
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
        if unknown && held_since.is_none_or(|since| since.elapsed() < LAYOUT_WAIT) {
            // Drawn from a setup this window hasn't read yet: hold it until
            // the layout comes, so the box isn't drawn at the old one.
            self.stats.waited_for_layout += 1;
            self.waiting = Some((frame, held_since.unwrap_or_else(Instant::now)));
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
            self.feed.set_size(size.0, size.1);
        }
        #[cfg(target_os = "macos")]
        if self.role == Role::Editor
            && let Output::Layers(layers) = &self.output
            && layers.borrow().geometry() != geometry(&app)
        {
            self.redraw_box();
        }
    }

    fn box_rect(&self, app: &AppWindow, revision: Option<u64>) -> Option<BoxShape> {
        let ui = app.global::<Ui>();
        let selected = ui.get_selected_item();
        if selected.is_empty() {
            return None;
        }
        // Only over a picture of the scene being edited.
        self.layouts
            .at(revision)
            .filter(|layout| layout.scene == ui.get_editing_scene().as_str())
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
        let now = Instant::now();
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
                                eprintln!("preview: can't show a shared frame: {error:#}");
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
    }
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
