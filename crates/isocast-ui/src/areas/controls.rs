//! Area C: the Controls dock, the Scene Transitions dock, Studio Mode's
//! column, the status bar, the REC and LIVE details, and the toasts of
//! recording and streaming (ui/areas/controls/). The web app's
//! controls-dock.js, transitions.js, stage.js (the studio column),
//! statusbar.js and lib/session.js (outputs, dropped frames) are the
//! behaviour to match.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use isocast_protocol::api::{EngineState, Event, LiveFile, Method};
use isocast_protocol::stream::{StreamService, StreamState, StreamStatus};
use serde_json::{Map, Value, json};
use slint::{ComponentHandle, SharedString};

use super::{Area, Changed};
use crate::app::App;
use crate::form::{self, Context, FormEvent, FormModel, Outcome, Target};
use crate::kit::{self, model};
use crate::session::EditOptions;
use crate::toasts::Toast;
use crate::{AppWindow, ControlsArea, FileRow, ToastKind, ToastRow};

/// Snapshot states (`--state NAME[:ARG]`).
pub const STATES: &[&str] = &[
    "studio",
    "record",
    "stop-record",
    "go-live",
    "stop-stream",
    "stream:live|reconnecting|connecting|short",
    "details:rec|live",
    "toasts",
    "toast-drops",
    "transition:KIND",
];

/// The transition kinds, in the web app's order and words.
const TRANSITIONS: &[(&str, &str)] = &[
    ("cut", "Cut"),
    ("fade", "Fade"),
    ("swipe", "Swipe"),
    ("slide", "Slide"),
    ("fade_to_color", "Fade to Colour"),
    ("luma_wipe", "Luma Wipe"),
    ("stinger", "Stinger"),
    ("move", "Move"),
];

/// A kind's label ("Fade to Colour"), or the kind itself.
fn transition_label(kind: &str) -> String {
    TRANSITIONS
        .iter()
        .find(|(k, _)| *k == kind)
        .map_or(kind.to_string(), |(_, label)| (*label).to_string())
}

/// The open Transition Properties dialog.
struct TransitionDialog {
    /// The setup's transition when it opened (Cancel goes back to it).
    original: Value,
    /// A kind that can't be chosen without its settings (a stinger needs
    /// its video): edited here and applied on OK.
    draft: Option<Value>,
}

/// When something started, by this computer's clock, from the daemon's
/// elapsed seconds: kept between readings (unless it moves by more than
/// 1.5 s), so the timer ticks on its own and doesn't jump.
#[derive(Default)]
struct Since(Cell<Option<Instant>>, Cell<Option<f64>>);

impl Since {
    fn set(&self, elapsed: Option<f64>) {
        self.1.set(None);
        let Some(elapsed) = elapsed else {
            self.0.set(None);
            return;
        };
        let at = Instant::now()
            .checked_sub(Duration::from_secs_f64(elapsed.max(0.0)))
            .unwrap_or_else(Instant::now);
        let keep = self.0.get().is_some_and(|old| {
            let off = if old > at { old - at } else { at - old };
            off <= Duration::from_millis(1500)
        });
        if !keep {
            self.0.set(Some(at));
        }
    }

    fn seconds(&self) -> u64 {
        if let Some(seconds) = self.1.get() {
            return seconds.max(0.0) as u64;
        }
        self.0.get().map_or(0, |at| at.elapsed().as_secs())
    }

    fn pause(&self, elapsed: f64) {
        self.0.set(None);
        self.1.set(Some(elapsed));
    }

    /// Until just after its next whole second.
    fn next_tick(&self) -> Option<Duration> {
        let at = self.0.get()?;
        let ms = at.elapsed().as_millis() as u64 % 1000;
        Some(Duration::from_millis(1000 - ms + 5))
    }
}

/// Frames dropped and audio gaps so far, per file of the take and for the
/// stream: the drops toast's rows (lib/session.js noteDrops).
#[derive(Default)]
struct Drops {
    /// (source, frames dropped, audio dropouts), in the order first seen.
    files: Vec<(String, u64, u64)>,
    stream: u64,
    rows: Option<Vec<(String, String, &'static str)>>,
}

#[derive(Default)]
pub struct Controls {
    recording: Since,
    streaming: Since,
    ticker: slint::Timer,
    drops: RefCell<Drops>,
    /// The stream was on at the last stream event (a new one counts its
    /// drops afresh).
    stream_on: Cell<bool>,
    dialog: RefCell<Option<TransitionDialog>>,
    /// Transition Properties' rows (changed in place, so a number being
    /// scrubbed isn't rebuilt under the pointer).
    form: FormModel,
}

impl Area for Controls {
    fn connect(&self, app: &Rc<App>, window: &AppWindow) {
        let area = window.global::<ControlsArea>();
        let weak = Rc::downgrade(app);
        area.on_toggle_record(move || {
            if let Some(app) = weak.upgrade() {
                toggle_recording(&app);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_toggle_stream(move || {
            if let Some(app) = weak.upgrade() {
                toggle_stream(&app);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_marker(move |label| {
            if let Some(app) = weak.upgrade() {
                add_marker(&app, label.trim());
            }
        });
        let weak = Rc::downgrade(app);
        area.on_split(move || {
            if let Some(app) = weak.upgrade() {
                split(&app);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_choose_transition(move |kind| {
            if let Some(app) = weak.upgrade() {
                choose_transition(&app, &kind);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_set_duration(move |ms| {
            if let Some(app) = weak.upgrade() {
                app.edit(
                    json!({ "transition": { "duration_ms": ms.clamp(0, 10_000) } }),
                    EditOptions::live("transition-ms"),
                );
            }
        });
        let weak = Rc::downgrade(app);
        area.on_dialog_accept(move || {
            if let Some(app) = weak.upgrade() {
                app.controls.accept_dialog(&app);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_dialog_cancel(move || {
            if let Some(app) = weak.upgrade() {
                app.controls.cancel_dialog(&app);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_dialog_defaults(move || {
            if let Some(app) = weak.upgrade() {
                app.controls.dialog_defaults(&app);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_take(move |index| {
            if let Some(app) = weak.upgrade() {
                let transition = if index < 0 {
                    None
                } else {
                    quick_transitions(&app)
                        .into_iter()
                        .nth(index as usize)
                        .and_then(|(_, _, t)| serde_json::from_value(t).ok())
                };
                app.take(transition);
            }
        });
    }

    fn update(&self, app: &Rc<App>, window: &AppWindow, changed: Changed) {
        let area = window.global::<ControlsArea>();
        if changed.any(Changed::STATUS | Changed::BUSY | Changed::SETUP) {
            self.show_recording(app, &area);
        }
        if changed.any(Changed::STREAM | Changed::BUSY) {
            self.show_stream(app, &area);
        }
        if changed.any(Changed::SETUP) {
            show_transition(app, &area);
            self.show_dialog(app, &area);
        }
        if changed.any(Changed::SETUP | Changed::SCENES) {
            area.set_can_take(app.session.borrow().preview.is_some());
        }
        self.show_times(app, window);
        self.arm_ticker(app);
    }

    fn menu(&self, app: &Rc<App>, id: &str) -> bool {
        match id {
            "studio_mode" => {
                let on = app.session.borrow().studio;
                app.set_studio(!on);
            }
            "take" => app.take(None),
            _ => return false,
        }
        true
    }

    fn dialog_opened(&self, app: &Rc<App>, kind: &str, arg: &str) {
        if kind != "transition" {
            self.dialog.replace(None);
            return;
        }
        let original = current_transition(app);
        let duration = original["duration_ms"].as_u64().unwrap_or(300);
        let draft = (!arg.is_empty() && original["kind"].as_str() != Some(arg))
            .then(|| json!({ "kind": arg, "duration_ms": duration }));
        self.dialog
            .replace(Some(TransitionDialog { original, draft }));
        self.form.clear();
        if let Some(window) = app.window() {
            self.show_dialog(app, &window.global::<ControlsArea>());
        }
    }

    fn event(&self, app: &Rc<App>, event: &Event) {
        match event {
            Event::Started { .. } => self.drops.borrow_mut().files.clear(),
            Event::Status { live } => self.note_drops(app, Some(&live.files), None),
            Event::Stream { stream } => {
                let on = matches!(
                    stream.state,
                    StreamState::Live | StreamState::Connecting | StreamState::Reconnecting
                );
                if !self.stream_on.get() {
                    self.drops.borrow_mut().stream = 0;
                }
                self.stream_on.set(on);
            }
            Event::Stopped { .. } => self.drops_stopped(app),
            _ => {}
        }
    }

    fn warning(&self, app: &Rc<App>, message: &str) -> bool {
        // Drops are counted from `status` (see note_drops); the stream's
        // total comes in its warning.
        if is_file_drop(message) {
            return true;
        }
        if let Some(total) = stream_drops(message) {
            self.note_drops(app, None, Some(total));
            return true;
        }
        false
    }

    fn state(
        &self,
        app: &Rc<App>,
        _window: &AppWindow,
        name: &str,
        arg: &str,
    ) -> Option<anyhow::Result<()>> {
        match name {
            "studio" => app.set_studio(true),
            // Choose a transition in the Scene Transitions dock.
            "transition" => choose_transition(app, arg),
            // Start or stop a real recording through the Controls dock.
            "record" => {
                let recording = app
                    .session
                    .borrow()
                    .status
                    .as_ref()
                    .is_some_and(|s| s.state == EngineState::Recording);
                if !recording {
                    toggle_recording(app);
                }
            }
            "stop-record" => toggle_recording(app),
            // Start or stop the setup's stream through the Controls dock.
            "go-live" | "stop-stream" => {
                let on = app.session.borrow().stream.as_ref().is_some_and(|x| {
                    matches!(
                        x.state,
                        StreamState::Live | StreamState::Connecting | StreamState::Reconnecting
                    )
                });
                if on == (name == "stop-stream") {
                    toggle_stream(app);
                }
            }
            // The web app's snapshots set a stream's status the same way
            // (there is no server to stream to).
            "stream" => {
                let stream = match sample_stream(arg) {
                    Ok(stream) => stream,
                    Err(error) => return Some(Err(error)),
                };
                self.stream_on.set(true);
                app.session.borrow_mut().stream = Some(stream);
                app.changed(Changed::STREAM | Changed::STATUS);
            }
            "details" => {
                app.flush();
                open_details(app, arg);
            }
            "toasts" => {
                // The web app's 42-toasts: four kinds; the oldest goes.
                app.toast(
                    ToastKind::Error,
                    "Couldn’t start recording",
                    "Not enough free space on Macintosh HD: 3.2 GB free, 20 GB needed.",
                );
                app.toast(
                    ToastKind::Warning,
                    "Imported, with notes",
                    "Browser source “Chat” has no Isocast equivalent; left out.",
                );
                app.toast(
                    ToastKind::Success,
                    "Recording saved",
                    "3 files in ~/Movies/Isocast",
                );
                app.show_toast(
                    Toast::new(ToastKind::Info, "Removed “Intro Clip”", "").action("Undo", |_| {}),
                );
            }
            "toast-drops" => {
                // The web app's 81-toast-error.
                app.toast(
                    ToastKind::Error,
                    "Couldn’t start recording",
                    "Not enough free space on Macintosh HD: 3.2 GB free, 20 GB needed.",
                );
                app.show_toast(
                    Toast::new(
                        ToastKind::Warning,
                        "The recording dropped frames",
                        "In the files it just finished:",
                    )
                    .key("drops")
                    .rows(vec![
                        ("Program".into(), "90 (61%)".into(), ""),
                        ("Intro Clip ISO".into(), "4 (0.2%)".into(), ""),
                    ]),
                );
            }
            _ => return None,
        }
        Some(Ok(()))
    }
}

impl Controls {
    /// The transition the dialog shows: its draft, else the setup's.
    fn dialog_value(&self, app: &Rc<App>) -> Option<Value> {
        let dialog = self.dialog.borrow();
        let dialog = dialog.as_ref()?;
        Some(
            dialog
                .draft
                .clone()
                .unwrap_or_else(|| current_transition(app)),
        )
    }

    /// Transition Properties: its title, its settings' rows and buttons.
    fn show_dialog(&self, app: &Rc<App>, area: &ControlsArea) {
        let Some(value) = self.dialog_value(app) else {
            return;
        };
        let draft = self
            .dialog
            .borrow()
            .as_ref()
            .is_some_and(|d| d.draft.is_some());
        let kind = value["kind"].as_str().unwrap_or("cut").to_string();
        let fields = form::fields("Transition", &kind);
        area.set_dialog_title(format!("Properties for “{}”", transition_label(&kind)).into());
        area.set_dialog_name(transition_label(&kind).into());
        let groups = {
            let s = app.session.borrow();
            let ctx = Context {
                kind: &kind,
                source: None,
                setup: s.setup.as_deref(),
                devices: None,
                running: None,
                // Its kind and duration are the Transitions dock's.
                skip: &["kind", "duration_ms"],
                refreshing: false,
                typing: false,
            };
            form::build(&ctx, &fields, value.as_object().unwrap_or(&Map::new()))
        };
        self.form.show(groups);
        area.set_form_groups(self.form.model());
        area.set_dialog_draft(draft);
        area.set_dialog_empty(!draft && fields.is_empty());
        area.set_dialog_has_fields(!fields.is_empty());
    }

    /// A setting changed in the dialog (form "transition", from
    /// `form::dispatch`): shown at once (live while a number is dragged),
    /// or kept in the draft until OK.
    pub fn form_event(&self, app: &Rc<App>, key: &str, event: FormEvent) {
        let Some(value) = self.dialog_value(app) else {
            return;
        };
        let kind = value["kind"].as_str().unwrap_or("cut").to_string();
        let fields = form::fields("Transition", &kind);
        let typing = Cell::new(false);
        let settings = value.as_object().cloned().unwrap_or_default();
        let target = Target {
            kind: &kind,
            fields: &fields,
            value: &settings,
            devices: None,
            typing: &typing,
        };
        let (patch, live) = match form::change(app, &target, key, event) {
            Outcome::Set(patch, live) => (patch, live.is_some()),
            Outcome::Reshow => {
                if let Some(window) = app.window() {
                    self.show_dialog(app, &window.global::<ControlsArea>());
                }
                return;
            }
            Outcome::Nothing => return,
        };
        let drafting = {
            let mut dialog = self.dialog.borrow_mut();
            match dialog.as_mut().and_then(|d| d.draft.as_mut()) {
                Some(draft) => {
                    *draft =
                        isocast_protocol::change::merge_patch(draft, &Value::Object(patch.clone()));
                    true
                }
                None => false,
            }
        };
        if drafting {
            if let Some(window) = app.window() {
                self.show_dialog(app, &window.global::<ControlsArea>());
            }
            return;
        }
        let options = if live {
            let name = patch.keys().next().cloned().unwrap_or_default();
            EditOptions {
                key: Some(format!("transition:{name}")),
                interval: Duration::from_millis(80),
                ..EditOptions::default()
            }
        } else {
            EditOptions::default()
        };
        app.edit(json!({ "transition": patch }), options);
    }

    /// OK: a draft is applied (and the dialog stays if the daemon refuses
    /// it); otherwise the changes are already made.
    fn accept_dialog(&self, app: &Rc<App>) {
        let draft = self.dialog.borrow().as_ref().and_then(|d| d.draft.clone());
        let Some(draft) = draft else {
            self.close_dialog(app);
            return;
        };
        let patch = form::object_patch(&current_transition(app), &draft);
        app.edit_then(
            json!({ "transition": patch }),
            EditOptions::default(),
            |app, applied| {
                if applied {
                    app.controls.close_dialog(app);
                }
            },
        );
    }

    /// Cancel: the transition goes back to what it was when the dialog
    /// opened.
    fn cancel_dialog(&self, app: &Rc<App>) {
        let back = self
            .dialog
            .borrow()
            .as_ref()
            .filter(|d| d.draft.is_none())
            .map(|d| form::object_patch(&current_transition(app), &d.original));
        if let Some(patch) = back
            && !patch.is_empty()
        {
            app.edit(json!({ "transition": patch }), EditOptions::default());
        }
        self.close_dialog(app);
    }

    /// Defaults: every optional setting (but the duration) back to its
    /// default.
    fn dialog_defaults(&self, app: &Rc<App>) {
        let Some(value) = self.dialog_value(app) else {
            return;
        };
        let kind = value["kind"].as_str().unwrap_or("cut").to_string();
        let reset: Map<String, Value> = form::fields("Transition", &kind)
            .into_iter()
            .filter(|f| f.optional && f.name != "duration_ms")
            .map(|f| (f.name, Value::Null))
            .collect();
        let drafting = {
            let mut dialog = self.dialog.borrow_mut();
            match dialog.as_mut().and_then(|d| d.draft.as_mut()) {
                Some(draft) => {
                    *draft = json!({ "kind": draft["kind"], "duration_ms": draft["duration_ms"] });
                    true
                }
                None => false,
            }
        };
        if drafting {
            if let Some(window) = app.window() {
                self.show_dialog(app, &window.global::<ControlsArea>());
            }
        } else if !reset.is_empty() {
            app.edit(json!({ "transition": reset }), EditOptions::default());
        }
    }

    fn close_dialog(&self, app: &Rc<App>) {
        self.dialog.replace(None);
        app.close_dialog();
    }

    /// REC, CPU and FPS in the status bar, the REC details' files, and the
    /// Controls dock's recording state.
    fn show_recording(&self, app: &Rc<App>, area: &ControlsArea) {
        let s = app.session.borrow();
        let state = s.status.as_ref().map(|x| x.state);
        let recording = state == Some(EngineState::Recording);
        area.set_record_state(
            match (state, s.busy_record) {
                (Some(EngineState::Starting), _) => "starting",
                (Some(EngineState::Recording), true) => "stopping",
                (Some(EngineState::Recording), false) => "recording",
                _ => "idle",
            }
            .into(),
        );
        area.set_busy_record(s.busy_record);
        let live = s.status.as_ref().and_then(|x| x.live.as_ref());
        self.recording
            .set(recording.then(|| live.map_or(0.0, |l| l.elapsed_seconds)));
        if recording && let Some(live) = live.filter(|l| l.paused) {
            self.recording.pause(live.elapsed_seconds);
        }
        let files: &[LiveFile] = live.map_or(&[], |l| &l.files);
        let dropped: u64 = files.iter().map(|f| f.dropped_frames).sum();
        let gaps: u64 = files.iter().map(|f| f.audio_dropouts).sum();
        area.set_dropped(if dropped > 0 {
            count(dropped).into()
        } else {
            SharedString::new()
        });
        area.set_rec_tooltip(
            if recording {
                format!(
                    "Recording {} file{}: {dropped} frames dropped, {gaps} audio gaps. Click for details, markers and splits.",
                    files.len(),
                    if files.len() == 1 { "" } else { "s" }
                )
            } else {
                "Not recording".to_string()
            }
            .into(),
        );
        let setup = s.setup.as_deref();
        area.set_files(model(
            files
                .iter()
                .map(|f| FileRow {
                    name: file_label(setup, &f.source).into(),
                    frames: format!("{} frames", count(f.frames)).into(),
                    dropped: format!("{} dropped", count(f.dropped_frames)).into(),
                    dropped_bad: f.dropped_frames > 0,
                    gaps: format!(
                        "{} gap{}",
                        f.audio_dropouts,
                        if f.audio_dropouts == 1 { "" } else { "s" }
                    )
                    .into(),
                    gaps_warn: f.audio_dropouts > 0,
                })
                .collect::<Vec<_>>(),
        ));

        // CPU and frame rate.
        let cpu = s.status.as_ref().and_then(|x| x.cpu_percent);
        area.set_cpu_text(cpu.map_or("—".to_string(), |c| format!("{c:.1}%")).into());
        let engine = s.status.as_ref().and_then(|x| x.engine.as_ref());
        let target = engine
            .map(|e| f64::from(e.canvas.fps))
            .or_else(|| setup.map(|x| f64::from(x.canvas.fps)));
        let fps = engine.map(|e| e.fps);
        let dash = |v: Option<f64>| v.map_or("—".to_string(), |v| format!("{v:.2}"));
        area.set_fps_text(format!("{} / {} FPS", dash(fps), dash(target)).into());
        area.set_fps_lagging(match (fps, target) {
            (Some(fps), Some(target)) => fps > 0.0 && fps < target * 0.95,
            _ => false,
        });
        area.set_fps_tooltip(
            match engine {
                Some(e) => format!(
                    "Program frames drawn per second, of the canvas’s {}. {} missed since the engine started.",
                    target.map_or("—".to_string(), |t| format!("{t}")),
                    e.missed_frames
                ),
                None => "The engine isn’t running".to_string(),
            }
            .into(),
        );
    }

    /// The stream's items in the status bar, its details and the Controls
    /// dock's stream state.
    fn show_stream(&self, app: &Rc<App>, area: &ControlsArea) {
        let s = app.session.borrow();
        let stream = s.stream.as_ref();
        let state = stream.map(|x| x.state);
        area.set_stream_state(
            match state {
                Some(StreamState::Connecting) => "connecting",
                Some(StreamState::Live) => "live",
                Some(StreamState::Reconnecting) => "reconnecting",
                Some(StreamState::Failed) => "failed",
                _ => "offline",
            }
            .into(),
        );
        area.set_busy_stream(s.busy_stream);
        let streaming = matches!(state, Some(StreamState::Live | StreamState::Reconnecting));
        self.streaming
            .set(streaming.then(|| stream.map_or(0.0, |x| x.elapsed_seconds)));
        let Some(stream) = stream else {
            area.set_stream_health("off".into());
            area.set_stream_health_word(SharedString::new());
            area.set_stream_rows(model(Vec::<ToastRow>::new()));
            return;
        };
        let total = stream.dropped_frames + stream.frames_sent;
        let percent = if total > 0 {
            stream.dropped_frames as f64 / total as f64 * 100.0
        } else {
            0.0
        };
        area.set_stream_dropped(format!("{} ({percent:.1}%)", count(stream.dropped_frames)).into());
        area.set_stream_dropped_bad(stream.dropped_frames > 0);
        area.set_stream_kbps(kbps(stream.total_kbps).into());
        let level = health(stream);
        area.set_stream_health(level.into());
        let word = match level {
            "ok" | "off" => "",
            _ => match stream.state {
                StreamState::Reconnecting => "Reconnecting",
                StreamState::Connecting => "Connecting",
                StreamState::Failed => "Failed",
                _ if level == "bad" => "Poor connection",
                _ => "Unstable",
            },
        };
        area.set_stream_health_word(word.into());
        area.set_stream_rows(model(stream_rows(stream)));
    }

    /// The REC and LIVE timers (the LCD's, the details'), and the status
    /// bar's line, which says how long the stream has run.
    fn show_times(&self, app: &Rc<App>, window: &AppWindow) {
        let area = window.global::<ControlsArea>();
        area.set_rec_time(clock(self.recording.seconds()).into());
        area.set_live_time(clock(self.streaming.seconds()).into());
        area.set_rec_seconds(self.recording.seconds().min(i32::MAX as u64) as i32);
        area.set_live_seconds(self.streaming.seconds().min(i32::MAX as u64) as i32);
        self.show_status_line(app, &area);
    }

    /// The LCD's numbers and the status bar's words and readouts (Paper's
    /// Main window and Studio Mode boards): READY, REC or LIVE with what
    /// is happening, then frames dropped, the data rate and the free space.
    fn show_status_line(&self, app: &Rc<App>, area: &ControlsArea) {
        let s = app.session.borrow();
        let state = s.status.as_ref().map(|x| x.state);
        let recording = state == Some(EngineState::Recording);
        let stream = s.stream.as_ref();
        let streaming = stream
            .is_some_and(|x| matches!(x.state, StreamState::Live | StreamState::Reconnecting));
        let live = s.status.as_ref().and_then(|x| x.live.as_ref());
        let take_dropped: u64 = live.map_or(0, |l| l.files.iter().map(|f| f.dropped_frames).sum());
        let stream_dropped = stream.map_or(0, |x| x.dropped_frames);
        let stream_percent = stream.map_or(0.0, |x| {
            let total = x.dropped_frames + x.frames_sent;
            if total > 0 {
                x.dropped_frames as f64 / total as f64 * 100.0
            } else {
                0.0
            }
        });

        // The LCD: zeros (dimmed) for what isn't running or measured
        // yet; the take's drops while recording, else the stream's.
        let engine = s.status.as_ref().and_then(|x| x.engine.as_ref());
        area.set_engine_running(engine.is_some());
        let (lead, fps) = lcd_fps(engine.map(|e| e.fps));
        area.set_lcd_fps_lead(lead.into());
        area.set_lcd_fps(fps.into());
        let cpu = s.status.as_ref().and_then(|x| x.cpu_percent);
        area.set_lcd_cpu_known(cpu.is_some());
        area.set_lcd_cpu(format!("{:.1}%", cpu.unwrap_or(0.0)).into());
        let dropped = if recording {
            take_dropped
        } else if streaming {
            stream_dropped
        } else {
            0
        };
        area.set_lcd_dropped(count(dropped).into());
        area.set_lcd_dropping(dropped > 0);

        // The status bar's line.
        let setup = s.setup.as_deref();
        let folder = setup.map_or(String::new(), |x| home_short(&x.recording.folder));
        let isos = setup.map_or(0, |x| {
            if x.recording.iso.enabled {
                x.recording.iso.sources.len()
            } else {
                0
            }
        });
        let isos = match isos {
            0 => String::new(),
            1 => " · 1 ISO file".to_string(),
            n => format!(" · {n} ISO files"),
        };
        let health = stream.map_or("off", health);
        let connection = match (stream.map(|x| x.state), health) {
            (Some(StreamState::Reconnecting), _) => "reconnecting",
            (_, "bad") => "poor connection",
            (_, "warn") => "unstable connection",
            _ => "connection stable",
        };
        let link = s.link_error.as_str();
        let line = if !link.is_empty() && s.status.is_none() {
            link.to_string()
        } else if recording && streaming {
            format!(
                "Recording to {folder}{isos} · streaming for {}",
                clock(self.streaming.seconds())
            )
        } else if recording {
            format!("Recording to {folder}{isos}")
        } else if state == Some(EngineState::Starting) {
            "Starting the recording…".to_string()
        } else if streaming {
            format!(
                "Streaming for {} · {connection} · not recording",
                clock(self.streaming.seconds())
            )
        } else if stream.is_some_and(|x| x.state == StreamState::Connecting) {
            "Connecting the stream…".to_string()
        } else if setup.is_some() {
            format!("Records to {folder}{isos}")
        } else if s.status.is_some() {
            // Paper's Welcome board, beside the IDLE badge.
            "No setup loaded".to_string()
        } else {
            String::new()
        };
        area.set_status_line(line.into());
        area.set_status_warn(streaming && health != "ok");

        // The readouts: the take's drops, else the stream's (with its
        // share); the stream's rate, else the disk's while recording.
        area.set_readout_dropped(
            if recording || !streaming {
                format!("{} dropped", count(take_dropped))
            } else {
                format!("{} dropped ({stream_percent:.1}%)", count(stream_dropped))
            }
            .into(),
        );
        area.set_readout_dropped_bad(if recording {
            take_dropped > 0
        } else {
            streaming && stream_dropped > 0
        });
        let megabits = if streaming {
            stream.map_or(0.0, |x| x.total_kbps / 1000.0)
        } else if recording {
            live.and_then(|l| l.disk.as_ref())
                .map_or(0.0, |d| d.write_bytes_per_second * 8.0 / 1e6)
        } else {
            0.0
        };
        area.set_readout_rate(format!("{megabits:.1} Mb/s").into());
        let free = s.status.as_ref().and_then(|x| x.recording_free_bytes);
        area.set_readout_free(
            free.map_or(String::new(), crate::model::gigabytes_free)
                .into(),
        );
    }

    /// Tick just after each whole second of whichever timer runs.
    fn arm_ticker(&self, app: &Rc<App>) {
        let next = [self.recording.next_tick(), self.streaming.next_tick()]
            .into_iter()
            .flatten()
            .min();
        let Some(next) = next else {
            self.ticker.stop();
            return;
        };
        let weak = Rc::downgrade(app);
        self.ticker
            .start(slint::TimerMode::SingleShot, next, move || {
                if let Some(app) = weak.upgrade()
                    && let Some(window) = app.window()
                {
                    app.controls.show_times(&app, &window);
                    app.controls.arm_ticker(&app);
                }
            });
    }

    /// Counts from a recording's status (`files`) or the stream's total so
    /// far: the drops toast shows (or updates) when any of them went up.
    fn note_drops(&self, app: &Rc<App>, files: Option<&[LiveFile]>, stream_total: Option<u64>) {
        let rows = {
            let mut drops = self.drops.borrow_mut();
            let mut grew = false;
            for f in files.unwrap_or_default() {
                let now = (f.dropped_frames, f.audio_dropouts);
                match drops
                    .files
                    .iter_mut()
                    .find(|(source, ..)| *source == f.source)
                {
                    Some(seen) => {
                        if now.0 > seen.1 || now.1 > seen.2 {
                            grew = true;
                        }
                        (seen.1, seen.2) = now;
                    }
                    None => {
                        if now.0 > 0 || now.1 > 0 {
                            grew = true;
                        }
                        drops.files.push((f.source.clone(), now.0, now.1));
                    }
                }
            }
            if let Some(total) = stream_total {
                if total > drops.stream {
                    grew = true;
                }
                drops.stream = total;
            }
            if !grew {
                return;
            }
            let setup = app.session.borrow().setup.clone();
            let mut rows = Vec::new();
            let mut frames = drops.stream;
            for (source, dropped, audio) in &drops.files {
                frames += dropped;
                let label = file_label(setup.as_deref(), source);
                if *dropped > 0 {
                    rows.push((label.clone(), format!("{dropped} dropped"), "bad"));
                }
                if *audio > 0 {
                    rows.push((
                        format!("{label} audio"),
                        format!("{audio} gap{}", if *audio == 1 { "" } else { "s" }),
                        "warn",
                    ));
                }
            }
            if drops.stream > 0 {
                rows.push(("Stream".into(), format!("{} dropped", drops.stream), "bad"));
            }
            drops.rows = Some(rows.clone());
            (frames, rows)
        };
        let (frames, rows) = rows;
        let recording = app
            .session
            .borrow()
            .status
            .as_ref()
            .is_some_and(|s| s.state == EngineState::Recording);
        app.show_toast(
            Toast::new(
                ToastKind::Warning,
                if frames > 0 {
                    "Dropping frames"
                } else {
                    "Audio dropouts"
                },
                if recording {
                    "The recording carries on."
                } else {
                    "The stream carries on; recordings keep every frame."
                },
            )
            .action("Details", move |app| {
                open_details(app, if recording { "rec" } else { "live" });
            })
            .key("drops")
            .rows(rows),
        );
    }

    /// The recording stopped: a drops toast still up says so.
    fn drops_stopped(&self, app: &Rc<App>) {
        let rows = self.drops.borrow().rows.clone();
        if let Some(rows) = rows
            && app.toasts.has_key("drops")
        {
            app.show_toast(
                Toast::new(
                    ToastKind::Warning,
                    "The recording dropped frames",
                    "In the files it just finished:",
                )
                .key("drops")
                .rows(rows),
            );
        }
    }
}

/// Open the REC ("rec") or LIVE ("live") details over the status bar.
fn open_details(app: &Rc<App>, which: &str) {
    if let Some(window) = app.window() {
        let area = window.global::<ControlsArea>();
        area.set_details_which(which.into());
        area.set_details_serial(area.get_details_serial() + 1);
    }
}

/// A file's name in the details and the drops toast: "Program", or its
/// source's name.
fn file_label(setup: Option<&isocast_protocol::Setup>, source: &str) -> String {
    if source == "program" {
        return "Program".into();
    }
    setup
        .and_then(|s| crate::model::find_source(s, source))
        .map(|s| crate::model::source_name(s).to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| source.to_string())
}

/// The transition dock: the kind, its duration and whether it has
/// properties; Studio Mode's quick transitions.
fn show_transition(app: &Rc<App>, area: &ControlsArea) {
    let (quick, details): (Vec<SharedString>, Vec<SharedString>) = quick_transitions(app)
        .into_iter()
        .map(|(name, detail, _)| (name.into(), detail.into()))
        .unzip();
    let s = app.session.borrow();
    let transition = s.setup_json().map(|x| &x["transition"]);
    let kind = transition
        .and_then(|t| t["kind"].as_str())
        .unwrap_or("cut")
        .to_string();
    let duration = transition
        .and_then(|t| t["duration_ms"].as_i64())
        .unwrap_or(300);
    let mut options: Vec<_> = TRANSITIONS
        .iter()
        .map(|(k, label)| kit::option(k, label, ""))
        .collect();
    if !TRANSITIONS.iter().any(|(k, _)| *k == kind) {
        options.push(kit::option(&kind, &kind, ""));
    }
    area.set_transitions(model(options));
    area.set_transition(kind.as_str().into());
    area.set_duration_ms(duration as i32);
    area.set_has_duration(kind != "cut" && kind != "stinger");
    area.set_has_properties(kind != "cut" && kind != "fade");
    area.set_duration_tooltip(
        if kind == "stinger" {
            "A stinger lasts as long as its video"
        } else {
            "How long the transition takes"
        }
        .into(),
    );
    area.set_quick_transitions(model(quick));
    area.set_quick_details(model(details));
}

/// The transition scene switches use: a kind that needs settings first
/// (a stinger's video) opens its properties; others keep the duration.
fn choose_transition(app: &Rc<App>, kind: &str) {
    let current = current_transition(app);
    if current["kind"].as_str().unwrap_or("cut") == kind {
        return;
    }
    if kind == "stinger" {
        app.open_dialog("transition", kind);
        return;
    }
    let target = json!({
        "kind": kind,
        "duration_ms": current["duration_ms"].as_u64().unwrap_or(300),
    });
    app.edit(
        json!({ "transition": form::object_patch(&current, &target) }),
        EditOptions::default(),
    );
}

/// The setup's transition as shown (a cut if it has none).
fn current_transition(app: &Rc<App>) -> Value {
    app.session
        .borrow()
        .setup_json()
        .map(|x| x["transition"].clone())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({ "kind": "cut" }))
}

/// 3725 s → "01:02:05".
/// The LCD's frame rate in two whole digits at least, the unused leading
/// zero apart to be dimmed: ("0", "7.71"), ("", "29.93"); dimmed "00.00"
/// while the engine isn't running.
fn lcd_fps(fps: Option<f64>) -> (String, String) {
    match fps {
        None => (String::new(), "00.00".to_string()),
        Some(fps) => {
            let text = format!("{fps:.2}");
            if text.len() < 5 {
                ("0".to_string(), text)
            } else {
                (String::new(), text)
            }
        }
    }
}

fn clock(seconds: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

/// A folder as people write it: `~` for the home folder.
fn home_short(folder: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    match folder.strip_prefix(&home) {
        Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => {
            format!("~{rest}")
        }
        _ => folder.to_string(),
    }
}

/// A count with digit grouping (22,620).
fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// "6.1 Mb/s", "160 kb/s".
fn kbps(kbps: f64) -> String {
    if kbps <= 0.0 {
        "0 kb/s".into()
    } else if kbps >= 1000.0 {
        format!("{:.1} Mb/s", kbps / 1000.0)
    } else {
        format!("{} kb/s", kbps.round())
    }
}

/// The stream's health, as OBS colours it: "ok" green, "warn" amber while
/// it backs up or reconnects, "bad" red when it fails or drops a lot.
fn health(stream: &StreamStatus) -> &'static str {
    match stream.state {
        StreamState::Offline => return "off",
        StreamState::Failed => return "bad",
        StreamState::Reconnecting | StreamState::Connecting => return "warn",
        StreamState::Live => {}
    }
    let sent = stream.frames_sent + stream.dropped_frames;
    let rate = if sent > 0 {
        stream.dropped_frames as f64 / sent as f64
    } else {
        0.0
    };
    if rate > 0.05 || stream.congestion > 0.8 {
        "bad"
    } else if rate > 0.01 || stream.congestion > 0.5 {
        "warn"
    } else {
        "ok"
    }
}

fn service_name(service: StreamService) -> &'static str {
    match service {
        StreamService::Twitch => "Twitch",
        StreamService::Youtube => "YouTube",
        StreamService::Kick => "Kick",
        StreamService::Facebook => "Facebook",
        StreamService::Custom => "Custom server",
    }
}

/// The LIVE details: where to, and how it is keeping up.
fn stream_rows(stream: &StreamStatus) -> Vec<ToastRow> {
    let row = |label: &str, value: String, tone: &str| ToastRow {
        label: label.into(),
        value: value.into(),
        tone: tone.into(),
    };
    let congestion = (stream.congestion * 100.0).round() as i64;
    let mut rows = vec![
        row(
            "To",
            format!("{} · {}", service_name(stream.service), stream.server),
            "",
        ),
        row(
            "Picture",
            if stream.width > 0 {
                format!("{}×{} · {} fps", stream.width, stream.height, stream.fps)
            } else {
                "—".into()
            },
            "",
        ),
        row(
            "Video",
            format!(
                "{} of {}",
                kbps(stream.video_kbps),
                kbps(f64::from(stream.target_video_kbps))
            ),
            "",
        ),
        row("Audio", kbps(stream.audio_kbps), ""),
        row("Frames sent", count(stream.frames_sent), ""),
        row(
            "Dropped",
            count(stream.dropped_frames),
            if stream.dropped_frames > 0 { "bad" } else { "" },
        ),
        row(
            "Backed up",
            format!("{congestion}%"),
            if congestion > 50 { "warn" } else { "" },
        ),
        row(
            "Reconnects",
            stream.reconnects.to_string(),
            if stream.reconnects > 0 { "warn" } else { "" },
        ),
    ];
    if let Some(error) = &stream.last_error {
        rows.push(row("Last error", error.clone(), "bad"));
    }
    rows
}

/// "abc dropped 3 frame(s) at 12 s (40 so far)" or "abc had 2 audio
/// dropout(s) at 12 s (5 so far)": a file's drops, which `status` counts.
fn is_file_drop(message: &str) -> bool {
    fn digits(s: &str) -> Option<&str> {
        let rest = s.trim_start_matches(|c: char| c.is_ascii_digit());
        (rest.len() < s.len()).then_some(rest)
    }
    let Some((source, rest)) = message.split_once(' ') else {
        return false;
    };
    if source.is_empty() {
        return false;
    }
    let rest = if let Some(r) = rest.strip_prefix("dropped ") {
        digits(r).and_then(|r| r.strip_prefix(" frame(s)"))
    } else if let Some(r) = rest.strip_prefix("had ") {
        digits(r).and_then(|r| r.strip_prefix(" audio dropout(s)"))
    } else {
        None
    };
    rest.and_then(|r| r.strip_prefix(" at "))
        .and_then(digits)
        .and_then(|r| r.strip_prefix(" s ("))
        .and_then(digits)
        .is_some_and(|r| r == " so far)")
}

/// "the stream dropped 3 frame(s) (40 so far…": the stream's total.
fn stream_drops(message: &str) -> Option<u64> {
    let rest = message.strip_prefix("the stream dropped ")?;
    let rest = rest.trim_start_matches(|c: char| c.is_ascii_digit());
    let rest = rest.strip_prefix(" frame(s) (")?;
    let end = rest.find(|c: char| !c.is_ascii_digit())?;
    if !rest[end..].starts_with(" so far") {
        return None;
    }
    rest[..end].parse().ok()
}

/// Studio Mode's quick transitions: a cut, the setup's own duration as a
/// fade, and a fade through black (label, transition).
/// Studio Mode's quick transitions: each one's name, its length ("300
/// ms", or "" for a cut) and the transition.
fn quick_transitions(app: &Rc<App>) -> Vec<(String, String, Value)> {
    let ms = app
        .session
        .borrow()
        .setup_json()
        .and_then(|x| x["transition"]["duration_ms"].as_u64())
        .unwrap_or(300);
    vec![
        ("Cut".into(), String::new(), json!({ "kind": "cut" })),
        (
            "Fade".into(),
            format!("{ms} ms"),
            json!({ "kind": "fade", "duration_ms": ms }),
        ),
        (
            "Fade to Black".into(),
            format!("{ms} ms"),
            json!({ "kind": "fade_to_color", "color": "#000000", "duration_ms": ms }),
        ),
    ]
}

fn add_marker(app: &Rc<App>, label: &str) {
    let label = if label.is_empty() { "Marker" } else { label };
    app.call(
        Method::Marker {
            label: label.into(),
        },
        |app, result| match result {
            Ok(marked) => {
                let take = marked["take"].as_u64().unwrap_or_default();
                let at = marked["time_seconds"].as_f64().unwrap_or_default();
                let label = marked["label"].as_str().unwrap_or("Marker");
                app.toast(
                    ToastKind::Success,
                    &format!("Marked “{label}”"),
                    format!("Take {take} at {at:.1} s"),
                );
            }
            Err(error) => app.failed("Couldn’t add the marker", &error),
        },
    );
}

fn split(app: &Rc<App>) {
    app.call(Method::Split, |app, result| match result {
        Ok(started) => {
            let files = started["files"].as_array().map_or(0, Vec::len);
            app.toast(
                ToastKind::Success,
                "Split into a new take",
                format!("{files} new files"),
            );
        }
        Err(error) => app.failed("Couldn’t split the recording", &error),
    });
}

/// Show a file or folder in the Finder.
fn reveal(path: &str) {
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("/usr/bin/open")
        .args(["-R", path])
        .spawn();
    #[cfg(not(target_os = "macos"))]
    let result = std::process::Command::new("xdg-open")
        .arg(
            std::path::Path::new(path)
                .parent()
                .unwrap_or(std::path::Path::new(path)),
        )
        .spawn();
    if let Err(error) = result {
        isocast_diagnostics::diagnostic!("error"; "couldn’t show {path}: {error}");
    }
}

fn toggle_recording(app: &Rc<App>) {
    let recording = {
        let mut s = app.session.borrow_mut();
        if s.busy_record {
            return;
        }
        s.busy_record = true;
        let recording = s
            .status
            .as_ref()
            .is_some_and(|x| matches!(x.state, EngineState::Recording | EngineState::Starting));
        if !recording {
            // Shown as starting at once (the engine may take a moment).
            match s.status.as_mut() {
                Some(status) => status.state = EngineState::Starting,
                None => {
                    s.status = serde_json::from_value(json!({ "state": "starting" })).ok();
                }
            }
        }
        recording
    };
    app.changed(Changed::BUSY | Changed::STATUS);
    let done = |app: &Rc<App>| {
        app.session.borrow_mut().busy_record = false;
        app.changed(Changed::BUSY | Changed::STATUS);
        app.refresh_status(true);
    };
    if recording {
        app.call(Method::Stop, move |app, result| {
            match result {
                Ok(report) => {
                    let files = report["files"].as_array().map_or(0, Vec::len);
                    let folder = report["folder"].as_str().unwrap_or_default().to_string();
                    let shown = report["manifest"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| folder.clone());
                    app.show_toast(
                        Toast::new(
                            ToastKind::Success,
                            "Recording saved",
                            format!(
                                "{files} file{} in {folder}",
                                if files == 1 { "" } else { "s" }
                            ),
                        )
                        .action("Show in Finder", move |_| reveal(&shown)),
                    );
                }
                Err(error) => app.failed("Couldn’t stop recording", &error),
            }
            done(app);
        });
    } else {
        app.call(
            Method::Start(Box::new(isocast_protocol::api::StartParams {
                setup: None,
                folder: None,
                seconds: None,
            })),
            move |app, result| {
                if let Err(error) = result {
                    app.failed("Couldn’t start recording", &error);
                }
                done(app);
            },
        );
    }
}

fn toggle_stream(app: &Rc<App>) {
    let (on, missing) = {
        let s = app.session.borrow();
        if s.busy_stream {
            return;
        }
        let on = s.stream.as_ref().is_some_and(|x| {
            matches!(
                x.state,
                StreamState::Live | StreamState::Connecting | StreamState::Reconnecting
            )
        });
        let missing = if s.setup.as_ref().is_some_and(|x| x.stream.is_none()) {
            Some("Choose where to stream first.")
        } else if s.stream.as_ref().is_some_and(|x| !x.key_set) {
            Some("Add your stream key first. It’s kept in the Keychain, never in the setup.")
        } else {
            None
        };
        (on, missing)
    };
    if !on && let Some(missing) = missing {
        // Settings → Stream, saying why.
        app.open_dialog("settings", &format!("stream:{missing}"));
        return;
    }
    {
        let mut s = app.session.borrow_mut();
        s.busy_stream = true;
        if !on && let Some(stream) = s.stream.as_mut() {
            stream.state = StreamState::Connecting;
        }
    }
    app.changed(Changed::BUSY | Changed::STREAM);
    let method = if on {
        Method::StreamStop
    } else {
        Method::StreamStart
    };
    app.call_as::<StreamStatus>(method, move |app, result| {
        match result {
            Ok(status) => app.session.borrow_mut().stream = Some(status),
            Err(error) => {
                if error.is_unknown_method() {
                    app.toast(
                        ToastKind::Info,
                        "Streaming isn’t in this engine yet",
                        "Go live arrives with the streaming update to isocastd.",
                    );
                } else {
                    app.failed(
                        if on {
                            "Couldn’t stop the stream"
                        } else {
                            "Couldn’t go live"
                        },
                        &error,
                    );
                }
                app.refresh_status(true);
            }
        }
        app.session.borrow_mut().busy_stream = false;
        app.changed(Changed::BUSY | Changed::STREAM);
    });
}

/// A stream's status for snapshots, as the web app's snapshots set it:
/// `live` (12 minutes in, 14 dropped), `reconnecting` (an hour in, 1,400
/// dropped), `connecting`, `short` (a minute in, as in 63-studio-live).
fn sample_stream(which: &str) -> anyhow::Result<StreamStatus> {
    let value = match which {
        "live" | "" => json!({
            "state": "live", "service": "youtube", "server": "a.rtmp.youtube.com",
            "key_set": true, "elapsed_seconds": 754, "width": 1920, "height": 1080,
            "fps": 30, "video_kbps": 5980, "target_video_kbps": 6000, "audio_kbps": 160,
            "total_kbps": 6140, "frames_sent": 22620, "dropped_frames": 14,
            "congestion": 0.12, "reconnects": 0
        }),
        "reconnecting" => json!({
            "state": "reconnecting", "service": "twitch", "server": "live.twitch.tv",
            "key_set": true, "elapsed_seconds": 3754, "total_kbps": 0,
            "frames_sent": 22620, "dropped_frames": 1400, "congestion": 0.9,
            "reconnects": 2
        }),
        "connecting" => json!({
            "state": "connecting", "service": "youtube", "server": "a.rtmp.youtube.com",
            "key_set": true
        }),
        "short" => json!({
            "state": "live", "service": "youtube", "server": "a.rtmp.youtube.com",
            "key_set": true, "elapsed_seconds": 61, "total_kbps": 6100,
            "frames_sent": 1800, "dropped_frames": 0, "congestion": 0.02, "reconnects": 0
        }),
        other => anyhow::bail!(
            "no sample stream {other:?}; one of live, reconnecting, connecting, short"
        ),
    };
    Ok(serde_json::from_value(value)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_warnings_are_recognised() {
        assert!(is_file_drop("cam-1 dropped 3 frame(s) at 12 s (40 so far)"));
        assert!(is_file_drop(
            "program had 2 audio dropout(s) at 5 s (5 so far)"
        ));
        assert!(!is_file_drop("cam-1 dropped frames at 12 s"));
        assert!(!is_file_drop("the disk is nearly full"));
        assert_eq!(
            stream_drops("the stream dropped 3 frame(s) (40 so far, 0.2%)"),
            Some(40)
        );
        assert_eq!(stream_drops("the stream reconnected"), None);
    }

    #[test]
    fn numbers_read_as_in_the_web_app() {
        assert_eq!(count(22620), "22,620");
        assert_eq!(count(14), "14");
        assert_eq!(count(1_234_567), "1,234,567");
        assert_eq!(kbps(6140.0), "6.1 Mb/s");
        assert_eq!(kbps(160.0), "160 kb/s");
        assert_eq!(kbps(0.0), "0 kb/s");
        assert_eq!(clock(3725), "01:02:05");
    }

    #[test]
    fn health_follows_drops_and_backlog() {
        let mut stream = sample_stream("live").unwrap();
        assert_eq!(health(&stream), "ok");
        stream.congestion = 0.6;
        assert_eq!(health(&stream), "warn");
        stream.dropped_frames = 5000;
        assert_eq!(health(&stream), "bad");
        let reconnecting = sample_stream("reconnecting").unwrap();
        assert_eq!(health(&reconnecting), "warn");
    }
}
