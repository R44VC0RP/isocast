//! Area E, the canvas: the monitors' pictures (preview.rs: the daemon's
//! shared GPU surfaces as native layers, layers.rs, or as Slint images;
//! frames.rs receives them; picture.rs is the offscreen test picture) and
//! the canvas editor over the edited monitor (ui/areas/canvas/). The web
//! app's stage.js, monitor.js and canvas.js are the behaviour matched:
//!
//! - click an item to select it (the topmost under the pointer; a selected
//!   child of a group over its parents), click nothing to deselect;
//! - drag to move (Shift: along one axis), a handle to resize (a corner
//!   keeps the shape and a side stretches, as in OBS; Shift swaps them),
//!   Option-drag a handle to crop; edges and centres
//!   snap to the canvas and the other items (Settings → General; ⌘ places
//!   freely), with guides and a readout while dragging;
//! - locked items (and a locked preview) can be selected, not changed;
//! - right-click: the item's menu, or the preview's;
//! - each step of a gesture is a keyed live edit, pipelined (app.rs); a
//!   moved item's box is drawn at the layout of the frame shown, so box and
//!   picture move together, while a dragged handle's box is drawn at once,
//!   so the handle stays under the pointer;
//! - Studio Mode: the edited monitor shows `preview`, a second picture the
//!   program; the preview's rate follows Settings → General (30 fps saves
//!   power except while working on the canvas).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use isocast_protocol::{SceneItem, Setup};
use serde_json::{Value, json};
use slint::{ComponentHandle, Model, VecModel};

use super::{Area, Changed};
use crate::app::App;
use crate::frames::{Feed, Incoming};
use crate::gpu::Gpu;
use crate::kit::MenuBuilder;
use crate::model;
use crate::picture::{Scene as TestScene, TestPicture};
use crate::preview::{BoxShape, BoxStyle, LiveBox, Output, Preview, Role};
use crate::session::EditOptions;
use crate::{AppWindow, CanvasArea, CanvasBox, CanvasGuide, Menus, Ui};

/// Snapshot states (`--state NAME[:ARG]`).
pub const STATES: &[&str] = &[
    "box:X,Y",
    "canvas-hover:PATH",
    "canvas-drag:X1,Y1,X2,Y2[,shift|alt|meta]",
    "canvas-hold:X1,Y1,X2,Y2[,shift|alt|meta]",
    "preview-scale:window|canvas",
];

/// Drag steps go to the daemon at most once per display frame.
const FRAME: Duration = Duration::from_millis(16);
/// The most frames a second a monitor shows (the canvas's rate, if lower).
const MAX_FPS: u32 = 60;
/// Frames a second at rest when saving power.
const POWER_SAVING_FPS: u32 = 30;
/// How long after a gesture monitors keep the faster rate.
const GESTURE_TAIL: Duration = Duration::from_millis(500);
/// How far from a handle's centre the pointer takes it, in points.
const HANDLE_REACH: f32 = 6.0;
/// How far the pointer moves before a press becomes a drag, in points.
const DRAG_START: f32 = 3.0;

/// The handles: name, and which side in x and y (-1, 0, 1).
const HANDLES: [(&str, f32, f32); 8] = [
    ("nw", -1.0, -1.0),
    ("n", 0.0, -1.0),
    ("ne", 1.0, -1.0),
    ("e", 1.0, 0.0),
    ("se", 1.0, 1.0),
    ("s", 0.0, 1.0),
    ("sw", -1.0, 1.0),
    ("w", -1.0, 0.0),
];

/// Where an item is drawn: its centre and size on the canvas, rotation
/// (degrees, clockwise) and scale (the web app's `placement`).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Placement {
    cx: f32,
    cy: f32,
    w: f32,
    h: f32,
    rotate: f32,
    scale: f32,
}

impl Placement {
    fn of(item: &SceneItem, origin: (f32, f32)) -> Self {
        let t = &item.style.transform;
        let rect = model::rect_of(item);
        let (w, h) = (rect.width as f32, rect.height as f32);
        Self {
            cx: origin.0 + rect.x as f32 + w / 2.0 + t.translate[0],
            cy: origin.1 + rect.y as f32 + h / 2.0 + t.translate[1],
            w: w * t.scale,
            h: h * t.scale,
            rotate: t.rotate,
            scale: t.scale,
        }
    }

    /// The axis-aligned box around it: left, top, right, bottom.
    fn bounds(&self) -> [f32; 4] {
        let r = self.rotate.to_radians();
        let (c, s) = (r.cos().abs(), r.sin().abs());
        let w = self.w * c + self.h * s;
        let h = self.w * s + self.h * c;
        [
            self.cx - w / 2.0,
            self.cy - h / 2.0,
            self.cx + w / 2.0,
            self.cy + h / 2.0,
        ]
    }

    /// The point in the item's own frame (centred, unrotated).
    fn local(&self, x: f32, y: f32) -> (f32, f32) {
        rotate_by(x - self.cx, y - self.cy, -self.rotate)
    }

    fn contains(&self, x: f32, y: f32) -> bool {
        let (lx, ly) = self.local(x, y);
        lx.abs() <= self.w / 2.0 && ly.abs() <= self.h / 2.0
    }

    fn shape(&self) -> BoxShape {
        BoxShape {
            rect: [
                self.cx - self.w / 2.0,
                self.cy - self.h / 2.0,
                self.w,
                self.h,
            ],
            rotate: self.rotate,
        }
    }
}

fn rotate_by(x: f32, y: f32, degrees: f32) -> (f32, f32) {
    let (s, c) = degrees.to_radians().sin_cos();
    (x * c - y * s, x * s + y * c)
}

/// The top-left corner children of the item at the end of `chain` are
/// placed from: their parents' rects and offsets.
fn origin_of(chain: &[&SceneItem]) -> (f32, f32) {
    let (mut x, mut y) = (0.0, 0.0);
    for item in &chain[..chain.len() - 1] {
        let t = &item.style.transform;
        let rect = model::rect_of(item);
        x += rect.x as f32 + t.translate[0];
        y += rect.y as f32 + t.translate[1];
    }
    (x, y)
}

/// The lines an edge or centre can snap to: the canvas's edges and middle,
/// and the other visible top-level items' edges and centres.
#[derive(Debug, Default)]
struct Targets {
    xs: Vec<f32>,
    ys: Vec<f32>,
}

impl Targets {
    fn new(setup: &Setup, items: &[SceneItem], skip: Option<&str>) -> Self {
        let (w, h) = (setup.canvas.width as f32, setup.canvas.height as f32);
        let mut targets = Self {
            xs: vec![0.0, w / 2.0, w],
            ys: vec![0.0, h / 2.0, h],
        };
        for item in items {
            if !item.visible || Some(model::key(item)) == skip {
                continue;
            }
            let [l, t, r, b] = Placement::of(item, (0.0, 0.0)).bounds();
            targets.xs.extend([l, (l + r) / 2.0, r]);
            targets.ys.extend([t, (t + b) / 2.0, b]);
        }
        targets
    }
}

/// The best snap of any of `values` to `targets` within `limit`: the
/// offset to add, and the line snapped to.
fn snap(values: &[f32], targets: &[f32], limit: f32) -> Option<(f32, f32)> {
    let mut best: Option<(f32, f32)> = None;
    for v in values {
        for t in targets {
            let d = t - v;
            if d.abs() <= limit && best.is_none_or(|(offset, _)| d.abs() < offset.abs()) {
                best = Some((d, *t));
            }
        }
    }
    best
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Move,
    /// Resizing from the handle on sides (ax, ay).
    Size(f32, f32),
    /// Cropping from the handle on sides (ax, ay).
    Crop(f32, f32),
}

/// A press on the editor, until it is let go.
struct Gesture {
    scene: String,
    path: String,
    kind: Kind,
    start: (f32, f32),
    moved: bool,
    p0: Placement,
    origin: (f32, f32),
    rect0: [f32; 4],
    crop0: [f32; 4],
    translate: [f32; 2],
    targets: Targets,
    /// Cropping: the source's own size, if known.
    natural: Option<(f32, f32)>,
    /// The item has no bounds (`fit: none`): its rect is the source's own
    /// size and `style.transform.scale` sizes it, so resizing scales.
    scales: bool,
    key: String,
}

/// The keys held during a pointer event.
#[derive(Debug, Clone, Copy, Default)]
struct Keys {
    alt: bool,
    shift: bool,
    meta: bool,
}

pub struct Canvas {
    /// The edited monitor's picture (the window).
    pub preview: RefCell<Option<Preview>>,
    /// Studio Mode's program picture (the window).
    pub program: RefCell<Option<Preview>>,
    /// The test picture (offscreen runs), drawn before each frame.
    picture: Option<RefCell<TestPicture>>,
    /// The test picture's clock, in seconds.
    clock: Box<dyn Fn() -> f32>,
    /// Frames the window has drawn.
    pub frames: Cell<u64>,
    /// Drag steps sent to the daemon.
    pub moves: Cell<u64>,
    /// `--drag-test`: when each window frame started and finished drawing.
    pub render_log: RefCell<Option<Vec<(Instant, Instant)>>>,
    render_started: Cell<Option<Instant>>,
    /// The newest confirmed layout handed to the preview (revision, scene).
    shown_layout: RefCell<(u64, String)>,
    gesture: RefCell<Option<Gesture>>,
    /// Until when the monitors keep the canvas's rate after a gesture.
    gesture_tail: Cell<Option<Instant>>,
    tail_timer: slint::Timer,
    /// The most frames a second the monitors ask for (`--fps`).
    max_fps: Cell<u32>,
    boxes: Rc<VecModel<CanvasBox>>,
    guides: Rc<VecModel<CanvasGuide>>,
}

impl Canvas {
    /// `test_picture`: draw the offscreen test picture instead of the
    /// daemon's.
    pub fn new(gpu: &Gpu, clock: Box<dyn Fn() -> f32>, test_picture: bool) -> Self {
        Self {
            preview: RefCell::new(None),
            program: RefCell::new(None),
            picture: test_picture.then(|| RefCell::new(TestPicture::new(&gpu.device, &gpu.queue))),
            clock,
            frames: Cell::new(0),
            moves: Cell::new(0),
            render_log: RefCell::new(None),
            render_started: Cell::new(None),
            shown_layout: RefCell::default(),
            gesture: RefCell::new(None),
            gesture_tail: Cell::new(None),
            tail_timer: slint::Timer::default(),
            max_fps: Cell::new(MAX_FPS),
            boxes: Rc::new(VecModel::default()),
            guides: Rc::new(VecModel::default()),
        }
    }

    pub fn preview_stats(&self) -> crate::preview::Stats {
        self.preview
            .borrow()
            .as_ref()
            .map(|p| p.stats())
            .unwrap_or_default()
    }

    /// At most `fps` frames a second, whatever the canvas's rate.
    pub fn set_max_fps(&self, fps: u32) {
        self.max_fps.set(fps.clamp(1, MAX_FPS));
    }

    /// Start the monitors' pictures (the window, once it is shown): the
    /// edited one and Studio Mode's program, each from a feed of its own.
    /// `deliver` hands each feed's frames to the window's thread (true:
    /// the program's).
    pub fn start_pictures(
        &self,
        app: &Rc<App>,
        output: Output,
        check: bool,
        deliver: impl Fn(bool, Incoming) + Send + Clone + 'static,
    ) {
        let Some(window) = app.window() else { return };
        let fps = self.wanted_fps(app);
        let size = |w: i32, h: i32| (w.max(16) as u32, h.max(16) as u32);
        let to_editor = deliver.clone();
        let feed = Feed::start(
            None,
            size(
                window.get_picture_pixel_width(),
                window.get_picture_pixel_height(),
            ),
            fps,
            move |incoming| to_editor(false, incoming),
        );
        *self.preview.borrow_mut() = Some(Preview::new(
            &window,
            Role::Editor,
            output.clone(),
            feed,
            fps,
            check,
        ));
        let feed = Feed::start(
            None,
            size(
                window.get_program_pixel_width(),
                window.get_program_pixel_height(),
            ),
            fps,
            move |incoming| deliver(true, incoming),
        );
        *self.program.borrow_mut() = Some(Preview::new(
            &window,
            Role::Program,
            output,
            feed,
            fps,
            false,
        ));
        *self.shown_layout.borrow_mut() = (0, String::new());
        self.follow_scenes(app);
        self.push_layout(app);
        self.place_box(app, &window);
    }

    /// A frame (or the loss of the stream) for the edited monitor or
    /// (`program`) Studio Mode's program.
    pub fn incoming(&self, program: bool, incoming: Incoming) {
        let cell = if program {
            &self.program
        } else {
            &self.preview
        };
        if let Some(preview) = cell.borrow_mut().as_mut() {
            match incoming {
                Incoming::Frame(frame) => preview.frame(frame),
                Incoming::Lost(why) => preview.lost(why),
            }
        }
    }

    /// The window was hidden or shown again.
    pub fn set_hidden(&self, hidden: bool) {
        for cell in [&self.preview, &self.program] {
            if let Some(preview) = cell.borrow_mut().as_mut() {
                preview.set_hidden(hidden);
            }
        }
    }

    /// What each monitor shows: the edited one `program`, or in Studio
    /// Mode `preview` (and the second one `program`); nothing while there
    /// is nothing to show.
    fn follow_scenes(&self, app: &Rc<App>) {
        let (studio, has_preview, has_program) = {
            let s = app.session.borrow();
            let exists = |id: &Option<String>| {
                id.as_deref().is_some_and(|id| {
                    s.setup
                        .as_ref()
                        .and_then(|x| model::find_scene(x, id))
                        .is_some()
                })
            };
            (s.studio, exists(&s.preview), exists(&s.scene))
        };
        let (editor, program) = if studio {
            (
                has_preview.then_some("preview"),
                has_program.then_some("program"),
            )
        } else {
            (has_program.then_some("program"), None)
        };
        if let Ok(mut preview) = self.preview.try_borrow_mut()
            && let Some(preview) = preview.as_mut()
        {
            preview.set_target(editor);
        }
        if let Ok(mut preview) = self.program.try_borrow_mut()
            && let Some(preview) = preview.as_mut()
        {
            preview.set_target(program);
        }
    }

    /// The monitors' rate: the canvas's (at most 60), or when saving power
    /// 30 except while working on the canvas.
    fn wanted_fps(&self, app: &Rc<App>) -> u32 {
        let canvas = app
            .session
            .borrow()
            .setup
            .as_ref()
            .map_or(MAX_FPS, |s| s.canvas.fps.max(1));
        let saving = app
            .window()
            .is_some_and(|w| w.global::<Ui>().get_preview_rate() == "30");
        let working = self.gesture.borrow().is_some()
            || self
                .gesture_tail
                .get()
                .is_some_and(|t| crate::clock::now() < t);
        let fps = if saving && !working {
            POWER_SAVING_FPS
        } else {
            MAX_FPS
        };
        fps.min(canvas).min(self.max_fps.get()).max(1)
    }

    fn update_fps(&self, app: &Rc<App>) {
        let fps = self.wanted_fps(app);
        for cell in [&self.preview, &self.program] {
            if let Ok(mut preview) = cell.try_borrow_mut()
                && let Some(preview) = preview.as_mut()
            {
                preview.set_fps(fps);
            }
        }
    }

    /// The user worked on the canvas (a drag ended, a nudge): keep the
    /// canvas's rate a moment longer.
    fn worked(&self, app: &Rc<App>) {
        self.gesture_tail
            .set(Some(crate::clock::now() + GESTURE_TAIL));
        self.update_fps(app);
        let weak = Rc::downgrade(app);
        self.tail_timer.start(
            slint::TimerMode::SingleShot,
            GESTURE_TAIL + Duration::from_millis(10),
            move || {
                if let Some(app) = weak.upgrade() {
                    app.canvas.update_fps(&app);
                }
            },
        );
    }

    /// The newest confirmed layout of the scene being edited, for the
    /// frames drawn from it.
    fn push_layout(&self, app: &Rc<App>) {
        let s = app.session.borrow();
        let (Some(setup), Some(scene)) = (s.confirmed_setup(), s.editing_scene()) else {
            return;
        };
        let key = (s.revision, scene.clone());
        if *self.shown_layout.borrow() == key {
            return;
        }
        *self.shown_layout.borrow_mut() = key;
        let rects = layout(&setup, &scene);
        if let Ok(mut preview) = self.preview.try_borrow_mut()
            && let Some(preview) = preview.as_mut()
        {
            preview.layout(s.revision, &scene, rects);
        }
    }

    /// Select the bottom item of the scene being edited (the drag test's
    /// background, after a resize).
    pub fn select_bottom_item(&self, app: &Rc<App>) {
        let path = {
            let s = app.session.borrow();
            s.editing_scene()
                .and_then(|id| s.setup.as_ref().and_then(|x| model::find_scene(x, &id)))
                .and_then(|scene| scene.items.first())
                .map(|item| model::key(item).to_string())
        };
        if let Some(path) = path {
            app.select(&path);
        }
    }

    /// Select the top item of the scene being edited that doesn't fill the
    /// canvas (the drag test's target; scripts that need one).
    pub fn select_first_item(&self, app: &Rc<App>) {
        let path = {
            let s = app.session.borrow();
            let Some(setup) = s.setup.clone() else { return };
            let Some(scene) = s
                .editing_scene()
                .and_then(|id| model::find_scene(&setup, &id).cloned())
            else {
                return;
            };
            let canvas = setup.canvas.width;
            scene
                .items
                .iter()
                .rev()
                .find(|item| item.visible && model::rect_of(item).width < canvas)
                .or(scene.items.last())
                .map(|item| model::key(item).to_string())
        };
        if let Some(path) = path {
            app.select(&path);
        }
    }

    /// Draw the test picture for the frame about to be drawn, from the
    /// same box the frame will show.
    fn draw_picture(&self, window: &AppWindow) {
        let Some(picture) = &self.picture else { return };
        let canvas = window.global::<CanvasArea>();
        let ui = window.global::<Ui>();
        let (cw, ch) = (
            ui.get_canvas_width().max(1) as f32,
            ui.get_canvas_height().max(1) as f32,
        );
        let rect = if ui.get_selected_item().is_empty() {
            [0.0; 4]
        } else {
            [
                canvas.get_box_x() / cw,
                canvas.get_box_y() / ch,
                canvas.get_box_w() / cw,
                canvas.get_box_h() / ch,
            ]
        };
        let width = window.get_picture_pixel_width().max(1) as u32;
        let height = window.get_picture_pixel_height().max(1) as u32;
        let scene = TestScene {
            time: (self.clock)(),
            rect,
        };
        if let Some(image) = picture.borrow_mut().draw(width, height, scene) {
            // Studio Mode's program shows the same test picture.
            if ui.get_studio() {
                canvas.set_program_picture(image.clone());
            }
            canvas.set_picture(image);
        }
        // The test picture is drawn from the box itself.
        canvas.set_waiting(false);
        canvas.set_shown_box_visible(true);
        canvas.set_shown_box_x(canvas.get_box_x());
        canvas.set_shown_box_y(canvas.get_box_y());
        canvas.set_shown_box_w(canvas.get_box_w());
        canvas.set_shown_box_h(canvas.get_box_h());
        canvas.set_shown_box_rotate(canvas.get_box_rotate());
    }

    /// The selected item's box in the setup shown, how it looks, and the
    /// items the pointer can be over.
    fn place_box(&self, app: &Rc<App>, window: &AppWindow) {
        let canvas = window.global::<CanvasArea>();
        let preview_locked = window.global::<Ui>().get_preview_locked();
        let s = app.session.borrow();
        let found = s.selected(|_, chain| {
            let item = chain[chain.len() - 1];
            (Placement::of(item, origin_of(chain)), item.locked)
        });
        let locked = found.is_some_and(|(_, locked)| locked) || preview_locked;
        if let Some((p, _)) = found {
            let BoxShape {
                rect: [x, y, w, h],
                rotate,
            } = p.shape();
            canvas.set_box_x(x);
            canvas.set_box_y(y);
            canvas.set_box_w(w);
            canvas.set_box_h(h);
            canvas.set_box_rotate(rotate);
        }
        canvas.set_box_locked(locked);
        let cropping = matches!(
            self.gesture.borrow().as_ref().map(|g| g.kind),
            Some(Kind::Crop(..))
        );
        canvas.set_cropping(cropping);

        // The boxes: top-level items, and a selected child (with its
        // parents), drawn last so it is on top.
        let mut boxes = Vec::new();
        let scene = s
            .editing_scene()
            .and_then(|id| s.setup.as_ref().and_then(|x| model::find_scene(x, &id)));
        let mut empty = false;
        if let Some(scene) = scene {
            empty = scene.items.is_empty();
            for (path, p, _) in hit_boxes(scene, s.selection.as_ref().map(|x| x.path.as_str())) {
                let BoxShape {
                    rect: [x, y, w, h],
                    rotate,
                } = p.shape();
                boxes.push(CanvasBox {
                    path: path.into(),
                    x,
                    y,
                    w,
                    h,
                    rotate,
                });
            }
        }
        drop(s);
        canvas.set_scene_empty(empty);
        if !same_boxes(&self.boxes, &boxes) {
            self.boxes.set_vec(boxes);
        }
        let style = BoxStyle { locked, cropping };
        if let Ok(mut preview) = self.preview.try_borrow_mut()
            && let Some(preview) = preview.as_mut()
        {
            preview.set_style(style);
            preview.redraw_box();
        }
    }

    // ---------- The pointer ----------

    /// Points per canvas pixel.
    fn k(window: &AppWindow) -> f32 {
        window.get_canvas_scale().max(0.0001)
    }

    fn pointer_down(&self, app: &Rc<App>, x: f32, y: f32, keys: Keys) {
        let Some(window) = app.window() else { return };
        let k = Self::k(&window);
        let preview_locked = window.global::<Ui>().get_preview_locked();
        let s = app.session.borrow();
        let Some(scene) = s
            .editing_scene()
            .and_then(|id| s.setup.as_ref().and_then(|x| model::find_scene(x, &id)))
        else {
            return;
        };
        let selected = s.selection.as_ref().map(|x| x.path.clone());
        // A handle of the selected item first (it reaches past the item).
        let handle = s
            .selected(|_, chain| {
                let item = chain[chain.len() - 1];
                if item.locked || preview_locked {
                    return None;
                }
                let p = Placement::of(item, origin_of(chain));
                handle_at(&p, x, y, HANDLE_REACH / k)
            })
            .flatten();
        let hit = match (&selected, handle) {
            (Some(path), Some(handle)) => Some((path.clone(), Some(handle))),
            _ => topmost(scene, selected.as_deref(), x, y).map(|path| (path, None)),
        };
        let scene_id = scene.id.clone();
        drop(s);
        let Some((path, handle)) = hit else {
            app.select("");
            return;
        };
        if selected.as_deref() != Some(path.as_str()) {
            app.select(&path);
        }
        // A locked item, or a locked preview (Edit → Lock Preview), stays
        // put.
        let s = app.session.borrow();
        let Some(setup) = s.setup.clone() else { return };
        let Some(scene) = model::find_scene(&setup, &scene_id) else {
            return;
        };
        let Some(chain) = model::item_chain(scene, &path) else {
            return;
        };
        let item = chain[chain.len() - 1];
        // So does one placed by its layout (no rect).
        let Some(rect) = item.rect.filter(|_| !item.locked && !preview_locked) else {
            return;
        };
        let kind = match handle {
            Some((ax, ay)) if keys.alt && item.source.is_some() => Kind::Crop(ax, ay),
            Some((ax, ay)) => Kind::Size(ax, ay),
            None => Kind::Move,
        };
        let natural = match kind {
            Kind::Crop(..) => natural_size(app, &setup, item),
            _ => None,
        };
        let origin = origin_of(&chain);
        let crop = &item.crop;
        let gesture = Gesture {
            scene: scene_id.clone(),
            path: path.clone(),
            kind,
            start: (x, y),
            moved: false,
            p0: Placement::of(item, origin),
            origin,
            rect0: [
                rect.x as f32,
                rect.y as f32,
                rect.width as f32,
                rect.height as f32,
            ],
            crop0: [
                crop.left as f32,
                crop.top as f32,
                crop.right as f32,
                crop.bottom as f32,
            ],
            translate: item.style.transform.translate,
            scales: item.fit == isocast_protocol::Fit::None && item.layout.is_none(),
            targets: Targets::new(
                &setup,
                &scene.items,
                (chain.len() == 1).then(|| model::key(item)),
            ),
            natural,
            key: format!(
                "{}:{scene_id}:{path}",
                match kind {
                    Kind::Move => "move",
                    Kind::Size(..) => "size",
                    Kind::Crop(..) => "crop",
                }
            ),
        };
        drop(s);
        *self.gesture.borrow_mut() = Some(gesture);
        self.update_fps(app);
        if matches!(kind, Kind::Crop(..)) {
            window.global::<CanvasArea>().set_cropping(true);
            self.place_box(app, &window);
        }
    }

    fn pointer_moved(&self, app: &Rc<App>, x: f32, y: f32, keys: Keys) {
        let Some(window) = app.window() else { return };
        let k = Self::k(&window);
        if self.gesture.borrow().is_none() {
            self.hover(app, &window, x, y, k);
            return;
        }
        let ui = window.global::<Ui>();
        let (snapping, distance) = (ui.get_snap_enabled(), ui.get_snap_distance());
        let step = {
            let mut gesture = self.gesture.borrow_mut();
            let Some(g) = gesture.as_mut() else { return };
            let (mut dx, mut dy) = (x - g.start.0, y - g.start.1);
            if !g.moved && (dx * k).hypot(dy * k) < DRAG_START {
                return;
            }
            g.moved = true;
            let free = keys.meta || !snapping;
            let limit = distance / k;
            let mut guides = Vec::new();
            match g.kind {
                Kind::Move => {
                    if keys.shift {
                        if dx.abs() > dy.abs() {
                            dy = 0.0;
                        } else {
                            dx = 0.0;
                        }
                    }
                    if !free {
                        let [l, t, r, b] = Placement {
                            cx: g.p0.cx + dx,
                            cy: g.p0.cy + dy,
                            ..g.p0
                        }
                        .bounds();
                        if let Some((offset, line)) =
                            snap(&[l, (l + r) / 2.0, r], &g.targets.xs, limit)
                        {
                            dx += offset;
                            guides.push(CanvasGuide {
                                vertical: true,
                                at: line,
                            });
                        }
                        if let Some((offset, line)) =
                            snap(&[t, (t + b) / 2.0, b], &g.targets.ys, limit)
                        {
                            dy += offset;
                            guides.push(CanvasGuide {
                                vertical: false,
                                at: line,
                            });
                        }
                    }
                    let (nx, ny) = ((g.rect0[0] + dx).round(), (g.rect0[1] + dy).round());
                    Step {
                        fields: json!({ "rect": { "x": nx as i64, "y": ny as i64 } }),
                        guides,
                        readout: format!("{nx}, {ny}"),
                        at: (g.p0.cx + dx - g.p0.w / 2.0, g.p0.cy + dy - g.p0.h / 2.0),
                    }
                }
                Kind::Size(ax, ay) => size_step(g, x, y, ax, ay, keys, free, limit),
                Kind::Crop(ax, ay) => crop_step(g, x, y, ax, ay),
            }
        };
        self.show_step(app, &window, step);
    }

    /// Send one step of the gesture and show its guides and readout.
    fn show_step(&self, app: &Rc<App>, window: &AppWindow, step: Step) {
        let canvas = window.global::<CanvasArea>();
        self.guides.set_vec(step.guides);
        canvas.set_readout(step.readout.into());
        canvas.set_readout_x(step.at.0);
        canvas.set_readout_y(step.at.1);
        let (scene, path, key, handle) = {
            let gesture = self.gesture.borrow();
            let Some(g) = gesture.as_ref() else { return };
            (
                g.scene.clone(),
                g.path.clone(),
                g.key.clone(),
                !matches!(g.kind, Kind::Move),
            )
        };
        let patch = {
            let s = app.session.borrow();
            let Some(setup) = s.setup.as_ref() else {
                return;
            };
            let Some(chain) =
                model::find_scene(setup, &scene).and_then(|sc| model::item_chain(sc, &path))
            else {
                return;
            };
            model::item_patch(&scene, &chain, step.fields)
        };
        self.moves.set(self.moves.get() + 1);
        app.edit(
            patch,
            EditOptions {
                key: Some(key),
                interval: FRAME,
                ..EditOptions::default()
            },
        );
        // A dragged handle stays under the pointer: the box is drawn from
        // the edit at once and the picture follows. A moved item's box
        // stays with the picture (docs/notes.md 2026-10-12, "Drag lag").
        if handle {
            let live = app.session.borrow().selected(|_, chain| {
                let shape = Placement::of(chain[chain.len() - 1], origin_of(chain)).shape();
                LiveBox::new(&scene, &path, shape)
            });
            self.set_live_box(live);
        }
    }

    fn set_live_box(&self, live: Option<LiveBox>) {
        if let Ok(mut preview) = self.preview.try_borrow_mut()
            && let Some(preview) = preview.as_mut()
        {
            preview.set_live_box(live);
        }
    }

    fn pointer_up(&self, app: &Rc<App>) {
        let Some(window) = app.window() else { return };
        let had = self.gesture.borrow_mut().take();
        self.set_live_box(None);
        let canvas = window.global::<CanvasArea>();
        self.guides.set_vec(Vec::new());
        canvas.set_readout("".into());
        if had.is_some() {
            self.worked(app);
            self.place_box(app, &window);
        }
    }

    /// The pointer over the editor without a button: the item under it,
    /// and the pointer's shape.
    fn hover(&self, app: &Rc<App>, window: &AppWindow, x: f32, y: f32, k: f32) {
        let canvas = window.global::<CanvasArea>();
        let preview_locked = window.global::<Ui>().get_preview_locked();
        let s = app.session.borrow();
        let Some(scene) = s
            .editing_scene()
            .and_then(|id| s.setup.as_ref().and_then(|x| model::find_scene(x, &id)))
        else {
            return;
        };
        let selected = s.selection.as_ref().map(|x| x.path.as_str());
        let handle = s
            .selected(|_, chain| {
                let item = chain[chain.len() - 1];
                if item.locked || preview_locked {
                    return None;
                }
                let p = Placement::of(item, origin_of(chain));
                handle_at(&p, x, y, HANDLE_REACH / k).map(|h| (h, p.rotate))
            })
            .flatten();
        let over = topmost(scene, selected, x, y);
        let movable = over.as_deref() == selected
            && s.selected(|_, chain| !chain[chain.len() - 1].locked)
                .unwrap_or(false)
            && !preview_locked;
        drop(s);
        let cursor = match handle {
            Some(((ax, ay), rotate)) => resize_cursor(ax, ay, rotate),
            None if movable => "move",
            None => "",
        };
        if canvas.get_cursor() != cursor {
            canvas.set_cursor(cursor.into());
        }
        let hovered = over.unwrap_or_default();
        if canvas.get_hover_item() != hovered.as_str() {
            canvas.set_hover_item(hovered.into());
        }
    }

    /// A right click: the item's menu (selecting it), or the preview's.
    fn context_menu(&self, app: &Rc<App>, x: f32, y: f32, wx: f32, wy: f32) {
        let Some(window) = app.window() else { return };
        let hit = {
            let s = app.session.borrow();
            s.editing_scene()
                .and_then(|id| s.setup.as_ref().and_then(|x| model::find_scene(x, &id)))
                .and_then(|scene| {
                    topmost(scene, s.selection.as_ref().map(|x| x.path.as_str()), x, y)
                })
        };
        // On an item: the item's menu as the preview shows it, with the
        // preview's own items first and no Rename (the web app's itemMenu
        // on the preview); area A builds both.
        let owner = match hit {
            Some(path) => {
                app.select(&path);
                format!("canvas:item:{path}")
            }
            None => "canvas".to_string(),
        };
        window
            .global::<Menus>()
            .invoke_request(owner.into(), wx, wy, 0.0, 0.0);
    }
}

/// One step of a gesture: the item's new fields, the guides snapped to,
/// the readout and where it goes.
struct Step {
    fields: Value,
    guides: Vec<CanvasGuide>,
    readout: String,
    at: (f32, f32),
}

/// Resizing from the handle on sides (ax, ay): the opposite edge stays
/// put, the dragged one follows the pointer.
#[allow(clippy::too_many_arguments)]
fn size_step(
    g: &Gesture,
    x: f32,
    y: f32,
    ax: f32,
    ay: f32,
    keys: Keys,
    free: bool,
    limit: f32,
) -> Step {
    let p0 = g.p0;
    let (lx, ly) = p0.local(x, y);
    let (hw, hh) = (p0.w / 2.0, p0.h / 2.0);
    let (fx, fy) = (-ax * hw, -ay * hh);
    let mut w = if ax != 0.0 {
        (ax * (lx - fx)).max(1.0)
    } else {
        p0.w
    };
    let mut h = if ay != 0.0 {
        (ay * (ly - fy)).max(1.0)
    } else {
        p0.h
    };
    // A corner keeps the shape and a side stretches, as in OBS; Shift
    // swaps them. An item without bounds only scales (one factor), so
    // every handle keeps its shape.
    let corner = ax != 0.0 && ay != 0.0;
    let keep = g.scales || corner != keys.shift;
    let ratio = p0.w / p0.h;
    if keep {
        if corner {
            if w / ratio > h {
                h = w / ratio;
            } else {
                w = h * ratio;
            }
        } else if ax != 0.0 {
            h = w / ratio;
        } else {
            w = h * ratio;
        }
    }
    // The dragged edges, for snapping.
    let edges = |w: f32, h: f32| {
        let left = p0.cx + if ax != 0.0 { fx.min(fx + ax * w) } else { -hw };
        let right = p0.cx + if ax != 0.0 { fx.max(fx + ax * w) } else { hw };
        let top = p0.cy + if ay != 0.0 { fy.min(fy + ay * h) } else { -hh };
        let bottom = p0.cy + if ay != 0.0 { fy.max(fy + ay * h) } else { hh };
        (
            if ax > 0.0 { right } else { left },
            if ay > 0.0 { bottom } else { top },
        )
    };
    let mut guides = Vec::new();
    if !free && p0.rotate == 0.0 {
        let (edge_x, edge_y) = edges(w, h);
        let snap_x = (ax != 0.0)
            .then(|| snap(&[edge_x], &g.targets.xs, limit))
            .flatten();
        let snap_y = (ay != 0.0)
            .then(|| snap(&[edge_y], &g.targets.ys, limit))
            .flatten();
        if keep && corner {
            // One edge snaps; the other follows the shape.
            if let Some((offset, line)) = snap_x {
                w += ax * offset;
                h = w / ratio;
                guides.push(CanvasGuide {
                    vertical: true,
                    at: line,
                });
            } else if let Some((offset, line)) = snap_y {
                h += ay * offset;
                w = h * ratio;
                guides.push(CanvasGuide {
                    vertical: false,
                    at: line,
                });
            }
        } else if !keep {
            if let Some((offset, line)) = snap_x {
                w += ax * offset;
                guides.push(CanvasGuide {
                    vertical: true,
                    at: line,
                });
            }
            if let Some((offset, line)) = snap_y {
                h += ay * offset;
                guides.push(CanvasGuide {
                    vertical: false,
                    at: line,
                });
            }
        }
    }
    let clx = if ax != 0.0 { fx + ax * w / 2.0 } else { 0.0 };
    let cly = if ay != 0.0 { fy + ay * h / 2.0 } else { 0.0 };
    let (ox, oy) = rotate_by(clx, cly, p0.rotate);
    let (cx, cy) = (p0.cx + ox, p0.cy + oy);
    if g.scales {
        // The rect stays the source's size; the scale makes it this big,
        // about the rect's centre, which moves so the far edge stays.
        let [_, _, rw, rh] = g.rect0;
        let scale = (w / rw.max(1.0) * 10_000.0).round() / 10_000.0;
        let rx = (cx - g.origin.0 - g.translate[0] - rw / 2.0).round();
        let ry = (cy - g.origin.1 - g.translate[1] - rh / 2.0).round();
        return Step {
            fields: json!({
                "rect": { "x": rx as i64, "y": ry as i64, "width": rw as u64, "height": rh as u64 },
                "style": { "transform": { "scale": scale } },
            }),
            guides,
            readout: format!("{} × {} ({:.0}%)", w.round(), h.round(), scale * 100.0),
            at: (cx - w / 2.0, cy - h / 2.0),
        };
    }
    let width = (w / p0.scale).round().max(1.0);
    let height = (h / p0.scale).round().max(1.0);
    let rx = (cx - g.origin.0 - g.translate[0] - width / 2.0).round();
    let ry = (cy - g.origin.1 - g.translate[1] - height / 2.0).round();
    Step {
        fields: json!({ "rect": { "x": rx as i64, "y": ry as i64, "width": width as u64, "height": height as u64 } }),
        guides,
        readout: format!("{width} × {height}"),
        at: (cx - w / 2.0, cy - h / 2.0),
    }
}

/// Option-drag: cut the source's edge, shrinking the item with it so the
/// rest of the picture stays where it is.
fn crop_step(g: &Gesture, x: f32, y: f32, ax: f32, ay: f32) -> Step {
    let p0 = g.p0;
    let (lx, ly) = p0.local(x, y);
    let (hw, hh) = (p0.w / 2.0, p0.h / 2.0);
    let [_, _, rw0, rh0] = g.rect0;
    let [cl, ct, cr, cb] = g.crop0;
    let natural = g.natural.unwrap_or((rw0 + cl + cr, rh0 + ct + cb));
    let per_x = (natural.0 - cl - cr) / rw0;
    let per_y = (natural.1 - ct - cb) / rh0;
    let mut next = g.crop0;
    let (mut width, mut height) = (rw0, rh0);
    let (mut shift_x, mut shift_y) = (0.0, 0.0);
    let limit_in = |visible: f32, per: f32| (visible - 1.0) / per;
    if ax != 0.0 {
        // How far the edge moved inwards, in rect pixels.
        let d = if ax > 0.0 { hw - lx } else { lx + hw } / p0.scale;
        let side = if ax > 0.0 { 2 } else { 0 };
        let d = d
            .max(-g.crop0[side] / per_x)
            .min(limit_in(natural.0 - cl - cr, per_x));
        next[side] = (g.crop0[side] + d * per_x).round();
        width = rw0 - d;
        shift_x = if ax > 0.0 { -d } else { d } / 2.0;
    }
    if ay != 0.0 {
        let d = if ay > 0.0 { hh - ly } else { ly + hh } / p0.scale;
        let side = if ay > 0.0 { 3 } else { 1 };
        let d = d
            .max(-g.crop0[side] / per_y)
            .min(limit_in(natural.1 - ct - cb, per_y));
        next[side] = (g.crop0[side] + d * per_y).round();
        height = rh0 - d;
        shift_y = if ay > 0.0 { -d } else { d } / 2.0;
    }
    let (ox, oy) = rotate_by(shift_x * p0.scale, shift_y * p0.scale, p0.rotate);
    let (w, h) = (width.round().max(1.0), height.round().max(1.0));
    let (cx, cy) = (p0.cx + ox, p0.cy + oy);
    let x = (cx - g.origin.0 - g.translate[0] - w / 2.0).round();
    let y = (cy - g.origin.1 - g.translate[1] - h / 2.0).round();
    let [l, t, r, b] = next.map(|v| v.max(0.0) as u64);
    Step {
        fields: json!({
            "rect": { "x": x as i64, "y": y as i64, "width": w as u64, "height": h as u64 },
            "crop": { "left": l, "top": t, "right": r, "bottom": b },
        }),
        guides: Vec::new(),
        readout: format!("Crop {l} {t} {r} {b}"),
        at: (cx - w * p0.scale / 2.0, cy - h * p0.scale / 2.0),
    }
}

/// The handle of `p` at canvas point (x, y), within `reach` canvas pixels:
/// its sides (ax, ay).
fn handle_at(p: &Placement, x: f32, y: f32, reach: f32) -> Option<(f32, f32)> {
    let (lx, ly) = p.local(x, y);
    HANDLES
        .iter()
        .map(|&(_, ax, ay)| {
            (
                ax,
                ay,
                (lx - ax * p.w / 2.0).abs().max((ly - ay * p.h / 2.0).abs()),
            )
        })
        .filter(|(.., d)| *d <= reach)
        .min_by(|a, b| a.2.total_cmp(&b.2))
        .map(|(ax, ay, _)| (ax, ay))
}

/// The resize pointer for the handle on sides (ax, ay) of an item turned
/// `rotate` degrees.
fn resize_cursor(ax: f32, ay: f32, rotate: f32) -> &'static str {
    // The handle's direction from the centre, turned, in eighths of a turn.
    let angle = ay.atan2(ax).to_degrees() + rotate;
    let eighth = ((angle / 45.0).round() as i32).rem_euclid(4);
    match eighth {
        0 => "ew",
        1 => "nwse",
        2 => "ns",
        _ => "nesw",
    }
}

/// The boxes the pointer can be over, bottom to top: the scene's top-level
/// items, then a selected child (with its parents). Hidden items only
/// when selected.
fn hit_boxes<'a>(
    scene: &'a isocast_protocol::Scene,
    selected: Option<&str>,
) -> Vec<(String, Placement, &'a SceneItem)> {
    let mut out = Vec::new();
    for item in &scene.items {
        let path = model::key(item).to_string();
        if !item.visible && selected != Some(path.as_str()) {
            continue;
        }
        out.push((path, Placement::of(item, (0.0, 0.0)), item));
    }
    if let Some(path) = selected
        && path.contains('/')
        && let Some(chain) = model::item_chain(scene, path)
    {
        let item = chain[chain.len() - 1];
        out.push((
            path.to_string(),
            Placement::of(item, origin_of(&chain)),
            item,
        ));
    }
    out
}

/// The topmost item at canvas point (x, y), by path.
fn topmost(
    scene: &isocast_protocol::Scene,
    selected: Option<&str>,
    x: f32,
    y: f32,
) -> Option<String> {
    hit_boxes(scene, selected)
        .into_iter()
        .rev()
        .find(|(_, p, _)| p.contains(x, y))
        .map(|(path, ..)| path)
}

fn same_boxes(model: &VecModel<CanvasBox>, boxes: &[CanvasBox]) -> bool {
    model.row_count() == boxes.len() && model.iter().zip(boxes).all(|(a, b)| a == *b)
}

/// The size of the picture an item's source draws, before cropping: from
/// the live engine, which runs every source, else from its settings.
fn natural_size(app: &Rc<App>, setup: &Setup, item: &SceneItem) -> Option<(f32, f32)> {
    let id = item.source.as_deref()?;
    let s = app.session.borrow();
    if let Some(running) = s.running.get(id)
        && let (Some(w), Some(h)) = (running.width, running.height)
    {
        return Some((w as f32, h as f32));
    }
    let source = serde_json::to_value(model::find_source(setup, id)?).ok()?;
    let field = |name: &str| source.get(name).and_then(Value::as_f64).map(|v| v as f32);
    let declared = (field("width"), field("height"));
    let canvas = (setup.canvas.width as f32, setup.canvas.height as f32);
    match source.get("kind").and_then(Value::as_str)? {
        "test_pattern" => Some((declared.0.unwrap_or(1920.0), declared.1.unwrap_or(1080.0))),
        "browser" => Some((declared.0.unwrap_or(800.0), declared.1.unwrap_or(600.0))),
        "color" => Some(match declared {
            (Some(w), Some(h)) => (w, h),
            _ => canvas,
        }),
        _ => match declared {
            (Some(w), Some(h)) => Some((w, h)),
            _ => None,
        },
    }
}

impl Area for Canvas {
    fn connect(&self, app: &Rc<App>, window: &AppWindow) {
        let canvas = window.global::<CanvasArea>();
        canvas.set_boxes(self.boxes.clone().into());
        canvas.set_guides(self.guides.clone().into());
        let weak = Rc::downgrade(app);
        canvas.on_pointer_down(move |x, y, alt, shift, meta| {
            if let Some(app) = weak.upgrade() {
                app.canvas
                    .pointer_down(&app, x, y, Keys { alt, shift, meta });
            }
        });
        let weak = Rc::downgrade(app);
        canvas.on_pointer_moved(move |x, y, alt, shift, meta| {
            if let Some(app) = weak.upgrade() {
                app.canvas
                    .pointer_moved(&app, x, y, Keys { alt, shift, meta });
            }
        });
        let weak = Rc::downgrade(app);
        canvas.on_pointer_up(move |_, _| {
            if let Some(app) = weak.upgrade() {
                app.canvas.pointer_up(&app);
            }
        });
        let weak = Rc::downgrade(app);
        canvas.on_context_menu(move |x, y, wx, wy| {
            if let Some(app) = weak.upgrade() {
                app.canvas.context_menu(&app, x, y, wx, wy);
            }
        });

        let weak = Rc::downgrade(app);
        window
            .window()
            .set_rendering_notifier(move |state, _| {
                let Some(app) = weak.upgrade() else { return };
                match state {
                    slint::RenderingState::BeforeRendering => {
                        app.canvas.render_started.set(Some(Instant::now()));
                        if let Some(window) = app.window() {
                            app.canvas.draw_picture(&window);
                        }
                        // The window's layout may have moved the monitors.
                        for cell in [&app.canvas.preview, &app.canvas.program] {
                            if let Ok(mut preview) = cell.try_borrow_mut()
                                && let Some(preview) = preview.as_mut()
                            {
                                preview.window_drawn();
                            }
                        }
                    }
                    slint::RenderingState::AfterRendering => {
                        app.canvas.frames.set(app.canvas.frames.get() + 1);
                        if let Some(log) = app.canvas.render_log.borrow_mut().as_mut()
                            && let Some(started) = app.canvas.render_started.take()
                        {
                            log.push((started, Instant::now()));
                        }
                    }
                    _ => {}
                }
            })
            .expect("the renderer takes a rendering notifier");
    }

    fn update(&self, app: &Rc<App>, window: &AppWindow, changed: Changed) {
        // A daemon that comes back numbers its revisions afresh.
        if changed.any(Changed::LINK)
            && app.session.borrow().link != Some(crate::daemon::Link::Connected)
        {
            *self.shown_layout.borrow_mut() = (0, String::new());
            if let Ok(mut preview) = self.preview.try_borrow_mut()
                && let Some(preview) = preview.as_mut()
            {
                preview.forget_layouts();
            }
        }
        if changed.any(Changed::SCENES | Changed::SETUP | Changed::LINK) {
            self.follow_scenes(app);
        }
        if changed.any(Changed::CONFIRMED | Changed::SCENES) {
            self.push_layout(app);
        }
        if changed.any(Changed::SELECTION | Changed::SETUP | Changed::SCENES) {
            self.place_box(app, window);
        }
        if changed.any(Changed::SETUP) {
            self.update_fps(app);
        }
        if changed.any(Changed::SETUP | Changed::SCENES) {
            let canvas = window.global::<CanvasArea>();
            let s = app.session.borrow();
            let name = |id: &Option<String>| {
                id.as_deref()
                    .and_then(|id| model::find_scene(s.setup.as_ref()?, id))
                    .map(|scene| model::scene_name(scene).to_string())
                    .unwrap_or_default()
            };
            let no_scenes = s.setup.as_ref().is_none_or(|x| x.scenes.is_empty());
            let program_empty = if no_scenes {
                "Add a scene with + under Scenes."
            } else if name(&s.scene).is_empty() {
                "Nothing on air."
            } else {
                ""
            };
            canvas.set_program_name(name(&s.scene).into());
            canvas.set_preview_name(name(&s.preview).into());
            canvas.set_program_empty_text(program_empty.into());
            canvas.set_empty_text(
                if s.studio {
                    if name(&s.preview).is_empty() {
                        "Click a scene to put it in preview."
                    } else {
                        ""
                    }
                } else {
                    program_empty
                }
                .into(),
            );
        }
    }

    /// A dialog took the window: the item the pointer was over isn't
    /// outlined any longer (the pointer may never come back over it).
    fn dialog_opened(&self, app: &Rc<App>, _kind: &str, _arg: &str) {
        if let Some(window) = app.window() {
            window.global::<CanvasArea>().set_hover_item("".into());
        }
    }

    /// The viewer line's menus: the zoom ("Fit 51% ⌄": Preview Scaling)
    /// and View (the View menu's toggles).
    fn context_menu(&self, app: &Rc<App>, owner: &str) -> Option<MenuBuilder> {
        let window = app.window()?;
        let ui = window.global::<Ui>();
        match owner {
            "viewer:scale" => {
                let canvas = ui.get_preview_scale() == "canvas";
                Some(
                    MenuBuilder::new()
                        .item("preview_scale_window", "Scale to Window")
                        .checked(!canvas)
                        .item("preview_scale_canvas", "Canvas (100%)")
                        .checked(canvas),
                )
            }
            "viewer:view" => Some(
                MenuBuilder::new()
                    .item("studio_mode", "Studio Mode")
                    .hint("⇧⌘S")
                    .checked(ui.get_studio())
                    .separator()
                    .item("source_toolbar", "Source Toolbar")
                    .checked(ui.get_show_source_toolbar())
                    .item("status_bar", "Status Bar")
                    .checked(ui.get_show_status_bar())
                    .item("docks_pane", "Docks")
                    .checked(ui.get_show_docks())
                    .separator()
                    .item("lock_preview", "Lock Preview")
                    .checked(ui.get_preview_locked())
                    .separator()
                    .item("reset_ui", "Reset UI"),
            ),
            _ => None,
        }
    }

    fn menu_chosen(&self, app: &Rc<App>, owner: &str, id: &str) -> bool {
        if !owner.starts_with("viewer:") {
            return false;
        }
        app.menu(id);
        true
    }

    fn menu(&self, app: &Rc<App>, id: &str) -> bool {
        // An arrow-key nudge (area A moves the item): the monitors keep the
        // canvas's rate a moment, as for a drag.
        if id.starts_with("nudge:") {
            self.worked(app);
        }
        false
    }

    fn state(
        &self,
        app: &Rc<App>,
        window: &AppWindow,
        name: &str,
        arg: &str,
    ) -> Option<anyhow::Result<()>> {
        let numbers = |text: &str| -> Option<Vec<f32>> {
            text.split(',')
                .map(|n| n.trim().parse::<f32>().ok())
                .collect()
        };
        match name {
            // The selection box at X,Y (canvas pixels).
            "box" => Some(match numbers(arg).filter(|n| n.len() == 2) {
                Some(n) => {
                    let canvas = window.global::<CanvasArea>();
                    canvas.set_box_x(n[0]);
                    canvas.set_box_y(n[1]);
                    Ok(())
                }
                None => Err(anyhow::anyhow!("box:X,Y takes two numbers, not {arg:?}")),
            }),
            // The pointer over the item at PATH (its centre).
            "canvas-hover" => Some({
                let centre = self
                    .boxes
                    .iter()
                    .find(|b| b.path == arg)
                    .map(|b| (b.x + b.w / 2.0, b.y + b.h / 2.0));
                match centre {
                    Some((x, y)) => {
                        self.pointer_moved(app, x, y, Keys::default());
                        Ok(())
                    }
                    None => Err(anyhow::anyhow!(
                        "canvas-hover:PATH takes an item of the edited scene, not {arg:?}"
                    )),
                }
            }),
            // A drag in canvas pixels, let go at the end ("drag"), or held
            // there so the guides and readout show ("hold"), with a key
            // held (shift, alt: Option, meta: ⌘).
            "canvas-drag" | "canvas-hold" => Some({
                let (points, key) = match arg.rsplit_once(',') {
                    Some((rest, key)) if key.chars().all(char::is_alphabetic) => (rest, key),
                    _ => (arg, ""),
                };
                let keys = Keys {
                    alt: key == "alt",
                    shift: key == "shift",
                    meta: key == "meta",
                };
                match numbers(points).filter(|n| n.len() == 4) {
                    Some(n) => {
                        self.pointer_down(app, n[0], n[1], keys);
                        for step in 1..=8 {
                            let t = step as f32 / 8.0;
                            self.pointer_moved(
                                app,
                                n[0] + (n[2] - n[0]) * t,
                                n[1] + (n[3] - n[1]) * t,
                                keys,
                            );
                        }
                        if name == "canvas-drag" {
                            self.pointer_up(app);
                        }
                        Ok(())
                    }
                    None => Err(anyhow::anyhow!(
                        "{name}:X1,Y1,X2,Y2[,shift|alt|meta] takes four numbers (canvas pixels), not {arg:?}"
                    )),
                }
            }),
            "preview-scale" => Some(match arg {
                "window" | "canvas" => {
                    window.global::<Ui>().set_preview_scale(arg.into());
                    Ok(())
                }
                _ => Err(anyhow::anyhow!(
                    "preview-scale takes window or canvas, not {arg:?}"
                )),
            }),
            _ => None,
        }
    }
}

/// Every item's box in scene `scene`, by path.
fn layout(setup: &Setup, scene: &str) -> HashMap<String, BoxShape> {
    fn walk(
        items: &[SceneItem],
        prefix: &str,
        origin: (f32, f32),
        out: &mut HashMap<String, BoxShape>,
    ) {
        for item in items {
            let path = if prefix.is_empty() {
                model::key(item).to_string()
            } else {
                format!("{prefix}/{}", model::key(item))
            };
            out.insert(path.clone(), Placement::of(item, origin).shape());
            let t = &item.style.transform;
            let rect = model::rect_of(item);
            let inner = (
                origin.0 + rect.x as f32 + t.translate[0],
                origin.1 + rect.y as f32 + t.translate[1],
            );
            walk(&item.children, &path, inner, out);
        }
    }
    let mut out = HashMap::new();
    if let Some(scene) = model::find_scene(setup, scene) {
        walk(&scene.items, "", (0.0, 0.0), &mut out);
    }
    out
}
