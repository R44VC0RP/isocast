//! The window's copy of the engine's state (the web app's lib/session.js
//! and lib/store.js): the live setup with this window's edits shown before
//! the daemon confirms them, the confirmed setups of recent revisions (the
//! canvas draws its boxes at the layout each frame was drawn from), the
//! engine's status, the stream, devices, audio levels, the scenes on air
//! and in preview, Studio Mode and the selection.
//!
//! This is plain data; `App` (app.rs) changes it, sends the calls and tells
//! the areas what changed ([`Changed`]).

use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};

use isocast_protocol::api::{AudioLevel, SourceStatus, Status};
use isocast_protocol::change::merge_patch;
use isocast_protocol::devices::Devices;
use isocast_protocol::stream::StreamStatus;
use isocast_protocol::{CanvasId, Scene, SceneItem, Setup};
use serde_json::Value;

use crate::app::App;
use crate::daemon::Link;
use crate::model;

/// What changed, so each area redraws only what it shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Changed(u32);

impl Changed {
    pub const NONE: Self = Self(0);
    /// The setup as shown (any edit, confirmed or not).
    pub const SETUP: Self = Self(1);
    /// A new confirmed revision (the canvas's layouts).
    pub const CONFIRMED: Self = Self(1 << 1);
    /// The engine's status (recording, CPU, FPS, sources' health).
    pub const STATUS: Self = Self(1 << 2);
    /// The scene on air or in preview, or Studio Mode.
    pub const SCENES: Self = Self(1 << 3);
    pub const SELECTION: Self = Self(1 << 4);
    pub const DEVICES: Self = Self(1 << 5);
    /// Audio levels (about 20 times a second).
    pub const LEVELS: Self = Self(1 << 6);
    /// A source of the live engine opened, runs or failed.
    pub const RUNNING: Self = Self(1 << 7);
    pub const STREAM: Self = Self(1 << 8);
    /// The link to isocastd.
    pub const LINK: Self = Self(1 << 9);
    /// Recording or streaming is starting or stopping.
    pub const BUSY: Self = Self(1 << 10);
    /// The virtual camera's state (`virtual_camera` events, Status).
    pub const VCAM: Self = Self(1 << 11);
    /// The replay buffer's state (`replay` events, Status).
    pub const REPLAY: Self = Self(1 << 12);
    pub const ALL: Self = Self(u32::MAX);

    /// Whether any of `other`'s changes are in this.
    pub fn any(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for Changed {
    type Output = Self;
    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl std::ops::BitOrAssign for Changed {
    fn bitor_assign(&mut self, other: Self) {
        self.0 |= other.0;
    }
}

/// The selected scene item: its scene and its path of keys ("group/clip").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub scene: String,
    pub path: String,
}

/// How an edit goes to the daemon.
#[derive(Debug, Clone, Default)]
pub struct EditOptions {
    /// Edits with the same key in a row (a drag, a slider) merge into one,
    /// go as `live` changes (applied at once, saved when the gesture
    /// pauses) and count once for undo.
    pub key: Option<String>,
    /// At most one such edit every `interval`.
    pub interval: Duration,
    /// Not a step to undo (an undo itself).
    pub not_undoable: bool,
}

impl EditOptions {
    /// One step of a gesture named `key` (see [`EditOptions::key`]).
    pub fn live(key: impl Into<String>) -> Self {
        Self {
            key: Some(key.into()),
            ..Self::default()
        }
    }
}

pub(crate) enum EditChange {
    Patch(Value),
    Setup(Value),
}

/// Called with whether the daemon applied the edit.
pub type EditDone = Box<dyn FnOnce(&Rc<App>, bool)>;

pub(crate) struct Edit {
    pub change: EditChange,
    pub key: Option<String>,
    pub interval: Duration,
    pub retried: bool,
    pub done: Vec<EditDone>,
}

impl Edit {
    /// A step of a gesture: an absolute merge patch with a key, which may
    /// go to the daemon while earlier steps are still in flight (and is
    /// superseded by a later step of the same gesture).
    pub fn pipelines(&self) -> bool {
        self.key.is_some() && matches!(self.change, EditChange::Patch(_))
    }
}

/// How many confirmed setups are kept for frames still on their way.
const HISTORY: usize = 240;

#[derive(Default)]
pub struct Session {
    pub link: Option<Link>,
    pub link_error: String,
    /// The daemon's words for the last edit it refused (named for people),
    /// for a sheet whose toast would sit behind it (Settings).
    pub last_refusal: Option<String>,
    /// The live setup has been read at least once.
    pub loaded: bool,
    /// The setup as shown: the daemon's, plus edits not yet confirmed.
    pub setup: Option<Rc<Setup>>,
    pub(crate) shown: Option<Value>,
    /// The daemon's own setup and revision.
    pub(crate) confirmed: Option<Value>,
    pub revision: u64,
    history: VecDeque<(u64, Rc<Setup>)>,
    pub(crate) queue: VecDeque<Edit>,
    /// Edits sent, oldest first, each with the revision it was planned
    /// against: one at a time, or several steps of a gesture (pipelined,
    /// each planned against the revision the one before will make).
    pub(crate) inflight: VecDeque<(Edit, u64)>,
    /// Gesture steps the daemon refused as stale while others were in
    /// flight: sent again, on top of the daemon's setup, once those are
    /// back.
    pub(crate) retry: VecDeque<Edit>,
    /// Applies sent, the most in flight at once, and how many the daemon
    /// refused as stale (measurements: `--bench`, `--soak`).
    pub applies_sent: u64,
    pub most_in_flight: usize,
    pub applies_stale: u64,
    pub(crate) last_sent: Option<Instant>,
    pub(crate) waiting: bool,
    undo: Vec<Option<Value>>,
    redo: Vec<Option<Value>>,
    last_undo: Option<(String, Instant)>,
    pub status: Option<Status>,
    /// The stream as one: the only destination's status, or every
    /// destination's together (see [`summarize`]).
    pub stream: Option<StreamStatus>,
    /// Each stream destination's own status, the main one first (#73).
    pub streams: Vec<StreamStatus>,
    /// The vertical canvas's stream destinations (#86), kept apart so the
    /// main canvas's stream shows only its own.
    pub vertical_streams: Vec<StreamStatus>,
    /// Every canvas besides the main one (#84), as status has them; their
    /// streams kept up to date by stream events.
    pub canvases: Vec<isocast_protocol::api::CanvasStatus>,
    /// Starting or stopping the vertical canvas's stream.
    pub busy_canvas_stream: bool,
    pub devices: Option<Devices>,
    /// The live engine's sources by id.
    pub running: HashMap<String, SourceStatus>,
    /// The newest audio levels by source id ("program" for the mix).
    pub levels: HashMap<String, AudioLevel>,
    pub scene: Option<String>,
    /// The scene on air on each extra canvas (Status and
    /// `canvas_scene_changed`; the main one's while it follows the main
    /// canvas).
    pub canvas_scenes: HashMap<CanvasId, String>,
    /// The scene in each extra canvas's own preview (Studio Mode).
    pub canvas_previews: HashMap<CanvasId, String>,
    pub preview: Option<String>,
    pub studio: bool,
    pub selection: Option<Selection>,
    /// The extra canvas chosen for the canvas monitor beside the others
    /// and for editing (#120); the first extra canvas while it is absent
    /// or the setup hasn't it (see [`Session::extra_canvas`]).
    pub extra_choice: Option<CanvasId>,
    /// The canvas editor edits the chosen extra canvas (#86, #120), while
    /// the setup has one (else the main canvas, as without this).
    pub extra_edit: bool,
    /// The setup shown as the chosen extra canvas (`Setup::for_canvas`):
    /// its size and each scene's own arrangement for it (or the main
    /// items it falls back to). None without an extra canvas.
    pub extra: Option<Rc<Setup>>,
    /// `shown` as the chosen extra canvas edits it (see `canvas_json`).
    pub(crate) extra_shown: Option<Value>,
    pub busy_record: bool,
    pub busy_stream: bool,
    /// The virtual camera, from Status and `virtual_camera` events.
    pub vcam: Option<isocast_protocol::virtual_camera::VirtualCameraStatus>,
    /// Its start or stop is on its way.
    pub busy_vcam: bool,
    /// `virtual_camera_remove` waits for macOS's password prompt.
    pub removing_vcam: bool,
    /// The replay buffer, from Status and `replay` events.
    pub replay: Option<isocast_protocol::replay::ReplayStatus>,
    /// Its start or stop is on its way.
    pub busy_replay: bool,
    /// A save is on its way.
    pub saving_replay: bool,
    /// When this window last switched scenes itself (a status read just
    /// after may predate the switch).
    pub(crate) switched_at: Option<Instant>,
    /// Counts this window's record requests and the engine's `started`
    /// and `stopped` events: a status read asked before the latest says
    /// nothing newer about the recording (see `App::refresh_status`).
    pub(crate) record_epoch: u64,
    /// Request time of the newest applied status reply. Concurrent reads can
    /// finish out of order even within one recording epoch.
    pub(crate) status_applied_from: Option<Instant>,
}

impl Session {
    /// The recording is finishing its files after a stop (from Status, or
    /// this window's own stop until its status says otherwise).
    pub fn finalizing(&self) -> bool {
        self.status.as_ref().is_some_and(|x| x.finalizing)
    }

    /// Recording can't be started, stopped, paused, split or retaken
    /// just now: a record request is on its way or the files are being
    /// finished.
    pub fn record_busy(&self) -> bool {
        self.busy_record || self.finalizing()
    }

    /// Whether edits are waiting to reach the daemon.
    pub fn editing(&self) -> bool {
        !self.inflight.is_empty() || !self.retry.is_empty() || !self.queue.is_empty()
    }

    /// The setup shown, as the daemon's JSON (fields the typed setup
    /// fills with defaults are absent here, as they are in the web app).
    pub fn setup_json(&self) -> Option<&Value> {
        self.shown.as_ref()
    }

    /// The newest confirmed setup.
    pub fn confirmed_setup(&self) -> Option<Rc<Setup>> {
        self.history.back().map(|(_, setup)| setup.clone())
    }

    /// The scene whose items are edited: the one in preview in Studio
    /// Mode, else the one on air (the first scene if neither exists).
    pub fn editing_scene(&self) -> Option<String> {
        let setup = self.setup.as_ref()?;
        let wanted = if self.studio {
            self.staged().or(self.on_air())
        } else {
            self.on_air()
        };
        match wanted {
            Some(id) if model::find_scene(setup, id).is_some() => Some(id.clone()),
            _ => setup.scenes.first().map(|s| s.id.clone()),
        }
    }

    /// The extra canvas the canvas monitor shows and Edit edits (#120):
    /// the one chosen, if the setup has it, else its first (the vertical
    /// one first). None without an extra canvas.
    pub fn extra_canvas(&self) -> Option<CanvasId> {
        let setup = self.setup.as_ref()?;
        self.extra_choice
            .filter(|c| setup.extra_canvas(*c).is_some())
            .or_else(|| setup.extra_canvas_ids().first().copied())
    }

    /// Every extra canvas of the setup shown, the vertical one first.
    pub fn extra_canvases(&self) -> Vec<CanvasId> {
        self.setup
            .as_ref()
            .map(|s| s.extra_canvas_ids())
            .unwrap_or_default()
    }

    /// An extra canvas's name ("Vertical", or its `name`).
    pub fn canvas_label(&self, canvas: CanvasId) -> String {
        match &self.setup {
            Some(setup) => model::canvas_label(setup, canvas),
            None => canvas.as_str().to_string(),
        }
    }

    /// Whether the canvas editor edits an extra canvas (the chosen one).
    pub fn editing_extra(&self) -> bool {
        self.extra_edit && self.extra.is_some()
    }

    /// The canvas whose items edits change (see `App::edit`).
    pub fn edit_canvas(&self) -> CanvasId {
        match self.extra_canvas() {
            Some(canvas) if self.editing_extra() => canvas,
            _ => CanvasId::Main,
        }
    }

    /// Extra canvas `canvas` has scenes of its own on air (it doesn't
    /// follow the main canvas's).
    pub fn independent(&self, canvas: CanvasId) -> bool {
        self.setup
            .as_ref()
            .and_then(|x| x.extra_canvas(canvas))
            .is_some_and(|v| !v.follow_main)
    }

    /// The canvas whose scenes the Scenes dock and Studio Mode switch:
    /// the extra one being edited if it has scenes of its own, else the
    /// main one (which the others follow).
    pub fn scene_canvas(&self) -> CanvasId {
        let canvas = self.edit_canvas();
        if !canvas.is_main() && self.independent(canvas) {
            canvas
        } else {
            CanvasId::Main
        }
    }

    /// The scene on air on canvas `canvas` (an extra canvas that follows
    /// the main one: the main one's, whatever it was switched to while it
    /// didn't: the daemon drops that without an event when it starts
    /// following, so the setup shown decides).
    pub fn scene_on(&self, canvas: CanvasId) -> Option<&String> {
        match canvas {
            CanvasId::Main => self.scene.as_ref(),
            other if self.independent(other) => {
                self.canvas_scenes.get(&other).or(self.scene.as_ref())
            }
            _ => self.scene.as_ref(),
        }
    }

    /// The scene in canvas `canvas`'s preview (Studio Mode): an extra
    /// canvas's own while it picks its own scenes, else the main one's.
    pub fn preview_on(&self, canvas: CanvasId) -> Option<&String> {
        match canvas {
            CanvasId::Main => self.preview.as_ref(),
            other if self.independent(other) => {
                self.canvas_previews.get(&other).or(self.preview.as_ref())
            }
            _ => self.preview.as_ref(),
        }
    }

    /// The scene on air on the canvas whose scenes are switched.
    pub fn on_air(&self) -> Option<&String> {
        match self.scene_canvas() {
            CanvasId::Main => self.scene.as_ref(),
            other => self.canvas_scenes.get(&other),
        }
    }

    /// The scene in its preview (Studio Mode).
    pub fn staged(&self) -> Option<&String> {
        match self.scene_canvas() {
            CanvasId::Main => self.preview.as_ref(),
            other => self.canvas_previews.get(&other),
        }
    }

    /// Take each extra canvas's scene and preview from a status read.
    pub fn set_canvas_scenes(&mut self, canvases: &[isocast_protocol::api::CanvasStatus]) {
        self.canvas_scenes = canvases
            .iter()
            .filter_map(|c| Some((c.id, c.scene.clone()?)))
            .collect();
        self.canvas_previews = canvases
            .iter()
            .filter_map(|c| Some((c.id, c.preview.clone()?)))
            .collect();
    }

    /// The setup shown as JSON as the canvas editor and the Sources dock
    /// edit it: while editing the vertical canvas each scene's vertical
    /// items (the main ones where it has none of its own) and the vertical
    /// canvas's size; else the setup shown. Only for reading items: whole
    /// setups written back come from [`Session::setup_json`].
    pub fn canvas_json(&self) -> Option<&Value> {
        if self.editing_extra() {
            self.extra_shown.as_ref()
        } else {
            self.shown.as_ref()
        }
    }

    /// The setup the canvas editor shows and edits: the vertical canvas's
    /// while editing it, else the setup shown.
    pub fn edited_setup(&self) -> Option<&Rc<Setup>> {
        if self.editing_extra() {
            self.extra.as_ref()
        } else {
            self.setup.as_ref()
        }
    }

    /// The newest confirmed setup as the canvas editor edits it (its
    /// vertical canvas while editing that).
    pub fn confirmed_edited(&self) -> Option<Rc<Setup>> {
        let setup = self.confirmed_setup()?;
        if self.editing_extra() {
            setup.for_canvas(self.edit_canvas()).map(Rc::new)
        } else {
            Some(setup)
        }
    }

    /// A patch changing `fields` of the item at the end of `chain` in scene
    /// `scene`. Sent with `App::edit`, it changes the canvas being edited
    /// (see [`model::route_canvas`]).
    pub fn canvas_item_patch(&self, scene: &str, chain: &[&SceneItem], fields: Value) -> Value {
        model::item_patch(scene, chain, fields)
    }

    /// The selected item's scene and chain (top level down) on the canvas
    /// being edited (the vertical items while editing the vertical canvas),
    /// if it still exists in the setup shown.
    pub fn selected<R>(&self, f: impl FnOnce(&Scene, &[&SceneItem]) -> R) -> Option<R> {
        let selection = self.selection.as_ref()?;
        let setup = self.edited_setup()?;
        let scene = model::find_scene(setup, &selection.scene)?;
        let chain = model::item_chain(scene, &selection.path)?;
        Some(f(scene, &chain))
    }

    /// The daemon's setup at `revision`.
    pub(crate) fn confirm(&mut self, setup: Option<Value>, revision: u64) {
        if let Some(value) = &setup
            && let Ok(typed) = serde_json::from_value::<Setup>(value.clone())
        {
            self.history.push_back((revision, Rc::new(typed)));
            while self.history.len() > HISTORY {
                self.history.pop_front();
            }
        }
        self.confirmed = setup;
        self.revision = revision;
    }

    /// Forget what an earlier daemon said (one that restarted numbers its
    /// revisions afresh).
    pub(crate) fn forget(&mut self) {
        self.history.clear();
        self.queue.clear();
        self.inflight.clear();
        self.retry.clear();
        self.waiting = false;
    }

    /// The setup shown: the confirmed one with the edits in flight and
    /// queued applied in order.
    pub(crate) fn recompute(&mut self) {
        let mut value = self.confirmed.clone();
        let sent = self
            .retry
            .iter()
            .chain(self.inflight.iter().map(|(edit, _)| edit));
        for edit in sent.chain(self.queue.iter()) {
            value = match (&edit.change, value) {
                (EditChange::Setup(setup), _) => Some(setup.clone()),
                (EditChange::Patch(patch), Some(doc)) => Some(merge_patch(&doc, patch)),
                (EditChange::Patch(_), None) => None,
            };
        }
        if value == self.shown {
            return;
        }
        self.setup = match &value {
            Some(v) => match serde_json::from_value::<Setup>(v.clone()) {
                Ok(setup) => Some(Rc::new(setup)),
                // An edit the setup type can't hold (the daemon will say
                // why); show what was there.
                Err(_) => self.setup.clone(),
            },
            None => None,
        };
        self.shown = value;
        self.refresh_extra();
    }

    /// The chosen extra canvas's view of the setup shown (after the setup
    /// or the choice changed).
    pub(crate) fn refresh_extra(&mut self) {
        // The canvas being edited went: back to the main canvas, rather
        // than editing another one unasked.
        if self.extra_edit
            && let (Some(setup), Some(chosen)) = (&self.setup, self.extra_choice)
            && !setup.has_canvas(chosen)
        {
            self.extra_edit = false;
            self.selection = None;
        }
        let canvas = self.extra_canvas();
        self.extra = canvas
            .zip(self.setup.as_ref())
            .and_then(|(canvas, setup)| setup.for_canvas(canvas))
            .map(Rc::new);
        self.extra_shown = canvas
            .zip(self.shown.as_ref())
            .filter(|_| self.extra.is_some())
            .map(|(canvas, shown)| model::canvas_view(shown, canvas));
    }

    /// Before a change: keep the setup as it was, for undo (a stream of
    /// edits with the same key within 1.5 s counts once).
    pub(crate) fn remember(&mut self, key: Option<&str>) {
        let now = crate::clock::now();
        let same = match (key, &self.last_undo) {
            (Some(key), Some((last, at))) => key == last && now - *at < Duration::from_millis(1500),
            _ => false,
        };
        self.last_undo = key.map(|k| (k.to_string(), now));
        if same || self.shown.is_none() {
            return;
        }
        self.undo.push(self.shown.clone());
        if self.undo.len() > 100 {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    /// Forget the undo history (the live setup became another scene
    /// collection or profile).
    pub(crate) fn forget_undo(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.last_undo = None;
    }

    /// The setup to go back to, if any (and remember where it came from).
    pub(crate) fn take_undo(&mut self) -> Option<Value> {
        let previous = self.undo.pop()??;
        self.redo.push(self.shown.clone());
        self.last_undo = None;
        Some(previous)
    }

    pub(crate) fn take_redo(&mut self) -> Option<Value> {
        let next = self.redo.pop()??;
        self.undo.push(self.shown.clone());
        self.last_undo = None;
        Some(next)
    }
}

impl Session {
    /// Take every destination's status (from `status`, or a start or
    /// stop), and the stream as one from them.
    pub fn set_streams(&mut self, main: Option<StreamStatus>, streams: Vec<StreamStatus>) {
        self.streams = if streams.is_empty() {
            main.into_iter().collect()
        } else {
            streams
        };
        self.stream = summarize(&self.streams);
    }

    /// A replay buffer's status (a `replay` event, or a start or stop):
    /// the main canvas's, or an extra canvas's (#120).
    pub fn set_replay(&mut self, status: isocast_protocol::replay::ReplayStatus) {
        if status.canvas.is_main() {
            self.replay = Some(status);
        } else if let Some(canvas) = self.canvases.iter_mut().find(|c| c.id == status.canvas) {
            canvas.replay = Some(status);
        }
    }

    /// One destination's status changed (a `stream` event).
    pub fn set_stream(&mut self, status: StreamStatus) {
        if !status.canvas.is_main() {
            if let Some(canvas) = self.canvases.iter_mut().find(|c| c.id == status.canvas) {
                match canvas.streams.iter_mut().find(|s| s.id == status.id) {
                    Some(slot) => *slot = status.clone(),
                    None => canvas.streams.push(status.clone()),
                }
            }
            if !status.canvas.is_vertical() {
                return;
            }
            match self.vertical_streams.iter_mut().find(|s| s.id == status.id) {
                Some(slot) => *slot = status,
                None => self.vertical_streams.push(status),
            }
            return;
        }
        match self.streams.iter_mut().find(|s| s.id == status.id) {
            Some(slot) => *slot = status,
            None => self.streams.push(status),
        }
        self.stream = summarize(&self.streams);
    }
}

/// The stream as one, for the Controls dock, the LCD and the status bar:
/// the only destination as it is; for several, live when any is (else
/// reconnecting, connecting, failed, offline), with their rates, frames
/// and drops added up and the main one's service and server. Its key is
/// set only when every enabled destination has one.
pub fn summarize(streams: &[StreamStatus]) -> Option<StreamStatus> {
    use isocast_protocol::stream::StreamState;
    match streams {
        [] => None,
        [only] => Some(only.clone()),
        all => {
            let mut out = all[0].clone();
            let rank = |state: StreamState| match state {
                StreamState::Live => 4,
                StreamState::Reconnecting => 3,
                StreamState::Connecting => 2,
                StreamState::Failed => 1,
                StreamState::Offline => 0,
            };
            out.state = all
                .iter()
                .map(|s| s.state)
                .max_by_key(|state| rank(*state))
                .unwrap_or(StreamState::Offline);
            let active: Vec<&StreamStatus> = all.iter().filter(|s| s.is_active()).collect();
            let counted: Vec<&StreamStatus> = if active.is_empty() {
                all.iter().collect()
            } else {
                active
            };
            out.elapsed_seconds = counted
                .iter()
                .map(|s| s.elapsed_seconds)
                .fold(0.0, f64::max);
            out.video_kbps = counted.iter().map(|s| s.video_kbps).sum();
            out.audio_kbps = counted.iter().map(|s| s.audio_kbps).sum();
            out.total_kbps = counted.iter().map(|s| s.total_kbps).sum();
            out.frames_sent = counted.iter().map(|s| s.frames_sent).sum();
            out.dropped_frames = counted.iter().map(|s| s.dropped_frames).sum();
            out.audio_dropouts = counted.iter().map(|s| s.audio_dropouts).sum();
            out.reconnects = counted.iter().map(|s| s.reconnects).sum();
            out.bytes_sent = counted.iter().map(|s| s.bytes_sent).sum();
            out.congestion = counted.iter().map(|s| s.congestion).fold(0.0, f64::max);
            out.key_set = all.iter().filter(|s| s.enabled).all(|s| s.key_set);
            out.key_problem = all.iter().find_map(|s| s.key_problem.clone());
            out.bitrate_warning = counted.iter().find_map(|s| s.bitrate_warning.clone());
            out.last_error = all
                .iter()
                .filter(|s| s.state == StreamState::Failed || s.state == StreamState::Reconnecting)
                .find_map(|s| s.last_error.clone())
                .or(out.last_error);
            Some(out)
        }
    }
}
