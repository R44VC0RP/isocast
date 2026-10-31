//! Area C: the Controls dock, the Scene Transitions dock, Studio Mode's
//! column, the status bar, the REC and LIVE details, and the toasts of
//! recording and streaming (ui/areas/controls/). The web app's
//! controls-dock.js, transitions.js, stage.js (the studio column),
//! statusbar.js and lib/session.js (outputs, dropped frames) are the
//! behaviour to match.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use isocast_protocol::api::{Capacity, EngineState, Event, LiveFile, Method, Overload};
use isocast_protocol::hello::EngineOs;
use isocast_protocol::platform_text as wording;
use isocast_protocol::replay::{ReplaySaved, ReplayState, ReplayStatus};
use isocast_protocol::stream::{StreamService, StreamState, StreamStatus};
use isocast_protocol::virtual_camera::{
    SIZES as VCAM_SIZES, Target as VcamTarget, VirtualCameraState, VirtualCameraStatus,
};
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
    "stream:live|reconnecting|connecting|short|key-problem",
    "vertical-stream:live|reconnecting|connecting|failed|off",
    "vertical-stopped[:WHY]",
    "details:rec|live",
    "toasts",
    "toast-drops",
    "toast-overload[:fits|EVENT.json]",
    "toast-audio-stops:REPORT.json",
    "transition:KIND",
    "vcam[:missing|approval|running|failed|removable]",
    "vcam-target:program|preview|scene|source|vertical|canvas:ID (as its Shows menu picks it)",
    "replay[:running|failed|saved]",
    "pause",
    "resume",
    "toast-saved (the toast a stopped recording shows)",
];

/// The transition kinds, in the web app's order and words.
const TRANSITIONS: &[(&str, &str)] = &[
    ("cut", "Cut"),
    ("fade", "Fade"),
    ("swipe", "Swipe"),
    ("slide", "Slide"),
    ("fade_to_color", "Fade to Color"),
    ("luma_wipe", "Luma Wipe"),
    ("stinger", "Stinger"),
    ("move", "Move"),
];

/// A kind's label ("Fade to Color"), or the kind itself.
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
        let at = crate::clock::now()
            .checked_sub(Duration::from_secs_f64(elapsed.max(0.0)))
            .unwrap_or_else(crate::clock::now);
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
        self.0
            .get()
            .map_or(0, |at| crate::clock::since(at).as_secs())
    }

    fn pause(&self, elapsed: f64) {
        self.0.set(None);
        self.1.set(Some(elapsed));
    }

    /// Until just after its next whole second.
    fn next_tick(&self) -> Option<Duration> {
        let at = self.0.get()?;
        let ms = crate::clock::since(at).as_millis() as u64 % 1000;
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
    /// This take's pauses, from `recording_paused` events: where in the
    /// take each began, and how long it lasted (`None`: still paused).
    pauses: RefCell<Vec<(f64, Option<f64>)>>,
    /// How long the take has been paused in all, by this computer's clock
    /// while paused (from the daemon's `paused_seconds`).
    paused_for: Since,
    /// A pause or resume is on its way.
    busy_pause: Cell<bool>,
    /// This take's encoder overload (#66), once one came: its notice shows
    /// once, and the drops toast stays quiet for the rest of the take.
    overload: RefCell<Option<Overload>>,
    /// A fix chosen while recording, applied when the recording stops:
    /// its description and patch.
    pending_fix: RefCell<Option<(String, Value)>>,
    /// Overload notices hidden for good this run (and in the prefs).
    hidden: RefCell<Vec<String>>,
    /// Why the vertical canvas stopped, once told (#96): its notice shows
    /// once per failure.
    vertical_stopped: RefCell<Option<String>>,
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
        area.on_toggle_pause(move || {
            if let Some(app) = weak.upgrade() {
                toggle_pause(&app);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_toggle_vcam(move || {
            if let Some(app) = weak.upgrade() {
                toggle_vcam(&app);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_toggle_replay(move || {
            if let Some(app) = weak.upgrade() {
                toggle_replay(&app, isocast_protocol::CanvasId::Main);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_save_replay(move || {
            if let Some(app) = weak.upgrade() {
                save_replay(&app, isocast_protocol::CanvasId::Main);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_remove_vcam(move || {
            if let Some(app) = weak.upgrade() {
                remove_vcam(&app);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_set_vcam(move |field, value| {
            if let Some(app) = weak.upgrade() {
                set_vcam(&app, &field, &value);
            }
        });
        area.on_copy_text(|text| crate::platform::copy_text(&text));
        area.on_open_vcam_settings(|| {
            crate::platform::open_url(
                "x-apple.systempreferences:com.apple.LoginItems-Settings.extension",
            )
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
        if changed.any(Changed::STATUS) {
            self.vertical_came_or_went(app);
        }
        if changed.any(Changed::STREAM | Changed::BUSY) {
            self.show_stream(app, &area);
        }
        if changed.any(Changed::VCAM | Changed::BUSY | Changed::SETUP | Changed::STATUS) {
            show_vcam(app, &area);
        }
        if changed.any(Changed::REPLAY | Changed::BUSY | Changed::SETUP | Changed::STATUS) {
            show_replay(app, &area);
        }
        if changed.any(Changed::SETUP) {
            show_transition(app, &area);
            self.show_dialog(app, &area);
        }
        if changed.any(Changed::SETUP | Changed::SCENES) {
            area.set_can_take(app.session.borrow().staged().is_some());
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
            Event::Started { .. } => {
                self.drops.borrow_mut().files.clear();
                self.pauses.borrow_mut().clear();
                // Once per take.
                self.overload.replace(None);
            }
            Event::EncoderOverload { overload } => self.overload_came(app, overload),
            Event::RecordingPaused { recording, .. } => {
                {
                    let mut pauses = self.pauses.borrow_mut();
                    let open = pauses.last().is_some_and(|(_, d)| d.is_none());
                    if recording.paused && !open {
                        pauses.push((recording.elapsed_seconds, None));
                    } else if !recording.paused && open {
                        let before: f64 = pauses.iter().filter_map(|(_, d)| *d).sum();
                        if let Some(last) = pauses.last_mut() {
                            last.1 = Some((recording.paused_seconds - before).max(0.0));
                        }
                    }
                }
                if let Some(window) = app.window() {
                    self.show_pauses(&window.global::<ControlsArea>());
                }
            }
            Event::Status { live } => self.note_drops(app, Some(&live.files), None),
            // The main canvas's stream only: the vertical canvas's has
            // its own status (#86).
            Event::Stream { stream, .. } if stream.canvas.is_main() => {
                let on = matches!(
                    stream.state,
                    StreamState::Live | StreamState::Connecting | StreamState::Reconnecting
                );
                if !self.stream_on.get() {
                    self.drops.borrow_mut().stream = 0;
                }
                self.stream_on.set(on);
            }
            Event::Stopped {
                report: Some(report),
                ..
            } => {
                self.drops_stopped(app);
                self.apply_pending_fix(app);
                saved_toast(app, report);
            }
            Event::Stopped { .. } => {
                self.drops_stopped(app);
                self.apply_pending_fix(app);
            }
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
                let streams = match sample_streams(arg) {
                    Ok(streams) => streams,
                    Err(error) => return Some(Err(error)),
                };
                let mut s = app.session.borrow_mut();
                s.set_streams(None, streams);
                self.stream_on
                    .set(s.stream.as_ref().is_some_and(|x| x.is_active()));
                drop(s);
                app.changed(Changed::STREAM | Changed::STATUS);
            }
            // The vertical canvas's stream (#96), as a status shows it.
            "vertical-stream" => {
                let streams = match arg {
                    "off" => Vec::new(),
                    which => {
                        let sample = if which == "failed" {
                            "connecting"
                        } else {
                            which
                        };
                        let mut status = match sample_stream(sample) {
                            Ok(status) => status,
                            Err(error) => return Some(Err(error)),
                        };
                        status.canvas = isocast_protocol::CanvasId::Vertical;
                        status.service = StreamService::Custom;
                        status.server = "rtmp://127.0.0.1/vertical".into();
                        if which == "failed" {
                            status.state = StreamState::Failed;
                            status.last_error = Some("the server refused the stream key".into());
                        }
                        vec![status]
                    }
                };
                app.session.borrow_mut().vertical_streams = streams;
                app.changed(Changed::STREAM | Changed::STATUS);
            }
            // A failure contained to the vertical canvas (#96), as a
            // status reports it (a real one: developer mode's
            // ISOCAST_COMPOSITOR_FAIL_AFTER=vertical=SECONDS).
            "vertical-stopped" => {
                let why = if arg.is_empty() {
                    "vertical compositor panicked: injected compositor failure"
                } else {
                    arg
                };
                {
                    let mut s = app.session.borrow_mut();
                    let Some(vertical) = s
                        .status
                        .as_mut()
                        .and_then(|x| x.engine.as_mut())
                        .and_then(|e| e.vertical.as_mut())
                    else {
                        return Some(Err(anyhow::anyhow!(
                            "vertical-stopped needs a running engine with a vertical canvas \
                             (vertical-canvas first, then wait)"
                        )));
                    };
                    vertical.stopped = Some(why.to_string());
                    vertical.fps = 0.0;
                }
                app.changed(Changed::STATUS);
            }
            "details" => {
                app.flush();
                open_details(app, arg);
            }
            "toast-saved" => {
                let report: isocast_protocol::api::Report = match serde_json::from_value(json!({
                    "folder": wording::select("/Users/you/Movies/Isocast/2026-10-17 19-27-03 Untitled", r"C:\Users\you\Videos\Isocast\2026-10-17 19-27-03 Untitled"),
                    "manifest": wording::select("/Users/you/Movies/Isocast/2026-10-17 19-27-03 Untitled/2026-10-17 19-27-03.session.json", r"C:\Users\you\Videos\Isocast\2026-10-17 19-27-03 Untitled\2026-10-17 19-27-03.session.json"),
                    "disk": {"file_system": "apfs", "network": false, "free_bytes": 500_000_000_000u64,
                             "write_bytes_per_second": 2.0e9, "estimated_bytes_per_second": 4.0e6},
                    "takes": [],
                    "duration_seconds": 12.0,
                    "files": [],
                    "captures": [],
                    "warnings": [],
                    "errors": [],
                })) {
                    Ok(report) => report,
                    Err(error) => return Some(Err(error.into())),
                };
                saved_toast(app, &report);
            }
            // Pause or resume the recording through the Controls dock.
            "pause" | "resume" => {
                let paused = app
                    .session
                    .borrow()
                    .status
                    .as_ref()
                    .and_then(|s| s.live.as_ref())
                    .is_some_and(|l| l.paused);
                if paused == (name == "resume") {
                    toggle_pause(app);
                }
            }
            // The virtual camera's popover; with an argument, as the
            // daemon reports that state (no extension to start offscreen).
            "vcam" => {
                if !arg.is_empty() {
                    let status = match sample_vcam(app, arg) {
                        Ok(status) => status,
                        Err(error) => return Some(Err(error)),
                    };
                    app.session.borrow_mut().vcam = Some(status);
                    app.changed(Changed::VCAM);
                }
                app.flush();
                if let Some(window) = app.window() {
                    let area = window.global::<ControlsArea>();
                    area.set_vcam_details_serial(area.get_vcam_details_serial() + 1);
                }
            }
            // The camera's Shows choice, as its popover's menu makes it.
            "vcam-target" => {
                set_vcam(app, "target", arg);
                app.flush();
            }
            // Start the replay buffer through the Controls dock; with an
            // argument, as the daemon reports that state.
            "replay" => match arg {
                "" => {
                    let on = app
                        .session
                        .borrow()
                        .replay
                        .as_ref()
                        .is_some_and(|r| r.state == ReplayState::Running);
                    if !on {
                        toggle_replay(app, isocast_protocol::CanvasId::Main);
                    }
                }
                "running" | "failed" => {
                    let mut status = app.session.borrow().replay.clone().unwrap_or_default();
                    status.seconds = status.seconds.max(30);
                    if arg == "running" {
                        status.state = ReplayState::Running;
                        status.buffered_seconds = 33.0;
                        status.memory_bytes = 61_400_000;
                        (status.width, status.height, status.fps) = (1920, 1080, 60);
                    } else {
                        status.state = ReplayState::Failed;
                        status.last_error =
                            Some("the live engine stopped, and the replay buffer with it".into());
                    }
                    app.session.borrow_mut().replay = Some(status);
                    app.changed(Changed::REPLAY);
                }
                "saved" => replay_saved(
                    app,
                    &ReplaySaved {
                        path: wording::select(
                            "~/Movies/Isocast/Replay 2026-10-15 14-32-18.mov",
                            "~/Videos/Isocast/Replay 2026-10-15 14-32-18.mov",
                        )
                        .into(),
                        seconds: 30.4,
                        frames: 1824,
                        bytes: 46_100_000,
                        ..Default::default()
                    },
                ),
                other => {
                    return Some(Err(anyhow::anyhow!(
                        "replay takes running, failed or saved, not {other:?}"
                    )));
                }
            },
            "toasts" => {
                // The web app's 42-toasts: four kinds; the oldest goes.
                app.toast(
                    ToastKind::Error,
                    "Couldn’t start recording",
                    wording::select_linux(
                        "Not enough free space on Macintosh HD: 3.2 GB free, 20 GB needed.",
                        "Not enough free space on C: 3.2 GB free, 20 GB needed.",
                        "Not enough free space on /home: 3.2 GB free, 20 GB needed.",
                    ),
                );
                app.toast(
                    ToastKind::Warning,
                    "Imported, with notes",
                    "Browser source “Chat” has no Isocast equivalent; left out.",
                );
                app.toast(
                    ToastKind::Success,
                    "Recording saved",
                    wording::select("3 files in ~/Movies/Isocast", "3 files in ~/Videos/Isocast"),
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
            // Audio sources that stopped while recording (#101), from a
            // report `isocast stop --json` printed.
            "toast-audio-stops" => {
                match std::fs::read_to_string(arg)
                    .map_err(anyhow::Error::from)
                    .and_then(|text| Ok(serde_json::from_str(&text)?))
                {
                    Ok(report) => app.audio_stops(&report),
                    Err(error) => return Some(Err(error.context(format!("{arg}: not a report")))),
                }
            }
            // The encoder overload notice (#66), as the soak's base M2
            // sends it; "fits" when the chip table says it should fit; or
            // an `encoder_overload` event saved by `isocast watch --json`.
            "toast-overload" => {
                let overload = if arg.ends_with(".json") {
                    match std::fs::read_to_string(arg)
                        .map_err(anyhow::Error::from)
                        .and_then(|text| Ok(serde_json::from_str::<Event>(&text)?))
                    {
                        Ok(Event::EncoderOverload { overload }) => *overload,
                        Ok(_) => {
                            return Some(Err(anyhow::anyhow!(
                                "{arg}: not an encoder_overload event"
                            )));
                        }
                        Err(error) => return Some(Err(error)),
                    }
                } else {
                    sample_overload(arg == "fits")
                };
                self.overload_came(app, &overload);
            }
            _ => return None,
        }
        Some(Ok(()))
    }
}

/// The overload a base M2 reports recording a 4K60 and a 1080p60 ISO and
/// a 1080p60 program while streaming (the hardware soak), for snapshots.
fn sample_overload(fits: bool) -> Overload {
    use isocast_protocol::api::{CapacityEstimate, CapacitySuggestion, OverloadedFile};
    let fix = CapacitySuggestion {
        description: "capture screen at 2560×1440".into(),
        change: Some(json!({ "sources": [{ "id": "screen", "width": 2560, "height": 1440 }] })),
        method: None,
        load: 0.74,
        capacity: Capacity::Fits,
        after_take: true,
    };
    Overload {
        take: 1,
        elapsed_seconds: 3.0,
        dropping: vec![
            OverloadedFile {
                output: "screen".into(),
                dropped_frames: 74,
                percent: 41.0,
            },
            OverloadedFile {
                output: "facecam".into(),
                dropped_frames: 73,
                percent: 40.5,
            },
        ],
        protected_dropping: false,
        reason: String::new(),
        estimate: Some(CapacityEstimate {
            chip: wording::select_linux("Apple M2", "Sample Windows PC", "Sample Linux computer")
                .into(),
            encode_engines: 1,
            engine_pixels_per_second: 8e8,
            basis: "measured".into(),
            outputs: Vec::new(),
            resources: Vec::new(),
            load: if fits { 0.6 } else { 1.09 },
            capacity: if fits {
                Capacity::Fits
            } else {
                Capacity::CannotKeepUp
            },
            streaming: true,
            iso_drop_percent: 13.0,
            summary: String::new(),
            suggestions: if fits { Vec::new() } else { vec![fix] },
            admission: None,
            measured: None,
            workload_key: "sample".into(),
        }),
        workload_key: "sample".into(),
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
            Outcome::Nothing | Outcome::Pick => return,
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
                // Finishing its files after a stop (even one asked while
                // it was still starting).
                (Some(_), _) if s.finalizing() => "stopping",
                (Some(EngineState::Starting), _) => "starting",
                (Some(EngineState::Recording), true) => "stopping",
                (Some(EngineState::Recording), false) => "recording",
                _ => "idle",
            }
            .into(),
        );
        area.set_busy_record(s.record_busy());
        let live = s.status.as_ref().and_then(|x| x.live.as_ref());
        self.recording
            .set(recording.then(|| live.map_or(0.0, |l| l.elapsed_seconds)));
        let paused = recording && live.is_some_and(|l| l.paused);
        if let Some(live) = live.filter(|_| paused) {
            self.recording.pause(live.elapsed_seconds);
        }
        // A window that opened mid-pause learns of it from the status (a
        // status read can be older than the events, so the list otherwise
        // follows `recording_paused` events only).
        if let Some(live) = live.filter(|l| recording && l.paused) {
            let mut pauses = self.pauses.borrow_mut();
            if pauses.is_empty() {
                pauses.push((live.elapsed_seconds, None));
            }
        }
        // The pause's own clock: all pauses so far, less the closed ones.
        self.paused_for.set(paused.then(|| {
            let closed: f64 = self.pauses.borrow().iter().filter_map(|(_, d)| *d).sum();
            live.map_or(0.0, |l| (l.paused_seconds - closed).max(0.0))
        }));
        if !recording {
            self.pauses.borrow_mut().clear();
        }
        area.set_paused(paused);
        area.set_busy_pause(self.busy_pause.get());
        self.show_pauses(area);
        let files: &[LiveFile] = live.map_or(&[], |l| &l.files);
        let dropped: u64 = files.iter().map(|f| f.dropped_frames).sum();
        let gaps: u64 = files.iter().map(|f| f.audio_dropouts).sum();
        area.set_dropped(if dropped > 0 {
            count(dropped).into()
        } else {
            SharedString::new()
        });
        let pending = metadata_pending(s.status.as_ref().and_then(|x| x.diagnostics.as_ref()));
        area.set_rec_tooltip(
            if s.finalizing() && !pending.is_empty() {
                format!(
                    "Finishing recording files. Final metadata is still pending:\n{}",
                    pending.join("\n")
                )
            } else if s.finalizing() || (recording && s.busy_record) {
                "Finishing recording files. Waiting for the completed outcome.".to_string()
            } else if paused {
                format!(
                    "Paused: the take stands at this time, its {} file{} still open. Resume to carry on in the same files.",
                    files.len(),
                    if files.len() == 1 { "" } else { "s" }
                )
            } else if recording {
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

    /// The take's pauses in the REC details: where each began and how
    /// long it lasted, the open one counting.
    fn show_pauses(&self, area: &ControlsArea) {
        let pauses = self.pauses.borrow();
        area.set_pauses(model(
            pauses
                .iter()
                .map(|(at, length)| ToastRow {
                    label: match length {
                        Some(_) => format!("Paused at {}", clock(*at as u64)),
                        None => format!("Paused at {} · now", clock(*at as u64)),
                    }
                    .into(),
                    value: clock(length.map_or(self.paused_for.seconds(), |l| l.round() as u64))
                        .into(),
                    tone: if length.is_none() { "warn" } else { "" }.into(),
                })
                .collect::<Vec<_>>(),
        ));
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
        let level = streams_health(&s.streams, stream);
        area.set_stream_health(level.into());
        let reconnecting = s
            .streams
            .iter()
            .filter(|x| x.state == StreamState::Reconnecting)
            .count();
        let word = match level {
            "ok" | "off" => String::new(),
            // One of several destinations reconnecting: the others are live.
            _ if s.streams.len() > 1 && reconnecting > 0 && stream.state == StreamState::Live => {
                format!("{reconnecting} reconnecting")
            }
            _ => match stream.state {
                StreamState::Reconnecting => "Reconnecting",
                StreamState::Connecting => "Connecting",
                StreamState::Failed => "Failed",
                _ if level == "bad" => "Poor connection",
                _ => "Unstable",
            }
            .to_string(),
        };
        area.set_stream_health_word(word.into());
        area.set_stream_rows(model(if s.streams.len() > 1 {
            destination_rows(&s.streams)
        } else {
            stream_rows(stream)
        }));
    }

    /// The REC and LIVE timers (the LCD's, the details'), and the status
    /// bar's line, which says how long the stream has run.
    fn show_times(&self, app: &Rc<App>, window: &AppWindow) {
        let area = window.global::<ControlsArea>();
        area.set_rec_time(clock(self.recording.seconds()).into());
        area.set_live_time(clock(self.streaming.seconds()).into());
        area.set_rec_seconds(self.recording.seconds().min(i32::MAX as u64) as i32);
        area.set_pause_time(clock(self.paused_for.seconds()).into());
        if self.paused_for.0.get().is_some() {
            self.show_pauses(&area);
        }
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
        let health = stream.map_or("off", |x| streams_health(&s.streams, x));
        let connection = match (stream.map(|x| x.state), health) {
            (Some(StreamState::Reconnecting), _) => "reconnecting",
            (_, "bad") => "poor connection",
            (_, "warn") => "unstable connection",
            _ => "connection stable",
        };
        let link = s.link_error.as_str();
        let load_warning = s.status.as_ref().and_then(|s| s.load_warning.as_ref());
        // Why a stopped take is still finishing: its final metadata
        // couldn't be written yet (the engine retries; the reason says
        // what to close or fix).
        let pending = metadata_pending(s.status.as_ref().and_then(|x| x.diagnostics.as_ref()));
        let pending = pending.first().filter(|_| s.finalizing()).cloned();
        let mut line = if !link.is_empty() && s.status.is_none() {
            link.to_string()
        } else if let Some(reason) = &pending {
            format!("Finishing recording files · {reason}")
        } else if s.finalizing() || (recording && s.busy_record) {
            format!(
                "Finishing recording files{isos}{}",
                if streaming {
                    " · the stream carries on"
                } else {
                    ""
                }
            )
        } else if recording && live.is_some_and(|l| l.paused) {
            format!(
                "Paused for {} · the take’s files stay open in {folder}{}",
                clock(self.paused_for.seconds()),
                if streaming {
                    " · the stream carries on"
                } else {
                    ""
                }
            )
        } else if recording && streaming {
            format!(
                "Recording to {folder}{isos} · streaming for {}",
                clock(self.streaming.seconds())
            )
        } else if recording {
            format!("Recording to {folder}{isos}")
        } else if state == Some(EngineState::Starting) {
            "Starting the recording…".to_string()
        } else if streaming && s.streams.len() > 1 {
            let live = s
                .streams
                .iter()
                .filter(|x| x.state == StreamState::Live)
                .count();
            format!(
                "Streaming to {live} of {} destinations for {} · {connection} · not recording",
                s.streams
                    .iter()
                    .filter(|x| x.enabled || x.is_active())
                    .count(),
                clock(self.streaming.seconds())
            )
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
        // Keep the recording/pause and connection state ahead of a load
        // warning, including when a narrow status bar has to elide its tail.
        if recording && streaming && health != "ok" {
            line = format!("{connection} · {line}");
        }
        // The vertical canvas's own state (#96): its streams, or that it
        // stopped.
        let (vertical, vertical_bad) = vertical_words(&s);
        let mut line = match (vertical, line.is_empty()) {
            (Some(words), false) => format!("{line} · {words}"),
            (Some(words), true) => words,
            (None, _) => line,
        };
        if let Some(warning) = load_warning {
            line.push_str(" · ");
            line.push_str(&warning.message);
        }
        // Automatic quality (T.2), after the states and #90's warning, only
        // while it lowers something or is held back.
        let governor = s.status.as_ref().and_then(|s| s.governor.as_ref());
        if let Some(words) = governor.and_then(governor_words) {
            if !line.is_empty() {
                line.push_str(" · ");
            }
            line.push_str(&words);
        }
        let held = governor.is_some_and(|g| g.error.is_some() || g.enabled && g.limited_by_pins);
        area.set_status_line(line.into());
        area.set_status_warn(
            load_warning.is_some()
                || (streaming && health != "ok")
                || vertical_bad
                || held
                || pending.is_some(),
        );

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

    /// Tell once when the vertical canvas stopped after a contained failure
    /// (#96), with what brings it back (poison needs an engine restart).
    fn vertical_came_or_went(&self, app: &Rc<App>) {
        let stopped = {
            let s = app.session.borrow();
            s.status
                .as_ref()
                .and_then(|x| x.engine.as_ref())
                .and_then(|e| e.vertical.as_ref())
                .and_then(|v| v.stopped.clone())
        };
        if *self.vertical_stopped.borrow() == stopped {
            return;
        }
        self.vertical_stopped.replace(stopped.clone());
        let Some(why) = stopped else { return };
        let toast = Toast::new(
            ToastKind::Error,
            "The vertical canvas stopped",
            format!(
                "{why}. The main canvas, its recording and its streams keep going. {}",
                crate::model::vertical_recovery(&why)
            ),
        )
        .key("vertical-stopped");
        // Restarting is for someone at the engine's own computer.
        let toast = if crate::engine::is_local() {
            toast.action("Restart Engine", restart_engine)
        } else {
            toast
        };
        app.show_toast(toast);
    }

    /// Tick just after each whole second of whichever timer runs.
    fn arm_ticker(&self, app: &Rc<App>) {
        let next = [
            self.recording.next_tick(),
            self.streaming.next_tick(),
            self.paused_for.next_tick(),
        ]
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
            let quiet = self.overload.borrow().is_some();
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
            if quiet {
                drop(drops);
                // The overload notice says why; it updates while it shows.
                if app.toasts.has_key("drops") {
                    self.show_overload(app);
                }
                return;
            }
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
        if self.overload.borrow().is_some() {
            // Its fix can apply now.
            if app.toasts.has_key("drops") {
                self.show_overload(app);
            }
            return;
        }
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

impl Controls {
    /// The engine says the encoders can't keep up (once per take): show
    /// the notice, unless it was hidden for this setup.
    fn overload_came(&self, app: &Rc<App>, overload: &Overload) {
        let hidden =
            !overload.workload_key.is_empty() && hidden_notice(app, &overload.workload_key);
        self.overload.replace(Some(overload.clone()));
        if !hidden {
            self.show_overload(app);
        }
    }

    /// The overload notice: what's dropping, why, and one fix.
    fn show_overload(&self, app: &Rc<App>) {
        let Some(overload) = self.overload.borrow().clone() else {
            return;
        };
        let recording = app
            .session
            .borrow()
            .status
            .as_ref()
            .is_some_and(|s| s.state == EngineState::Recording);
        let setup = app.session.borrow().setup.clone();
        // The take's counts so far, else what the engine reported.
        let mut rows = self.drops.borrow().rows.clone().unwrap_or_default();
        rows.retain(|r| r.2 == "bad");
        if rows.is_empty() {
            rows = overload
                .dropping
                .iter()
                .map(|f| {
                    let label = if f.output == "stream" {
                        "Stream".to_string()
                    } else {
                        file_label(setup.as_deref(), &f.output)
                    };
                    (label, format!("{} dropped", f.dropped_frames), "bad")
                })
                .collect();
        }
        let estimate = overload.estimate.as_ref();
        let at_capacity = estimate.is_none_or(|e| e.capacity != Capacity::Fits);
        let fix = estimate.and_then(|e| e.suggestions.first()).cloned();
        let machine_title = format!(
            "This {} can’t keep up with every file",
            crate::engine::words().machine
        );
        let (title, mut message) = if at_capacity {
            (
                machine_title.as_str(),
                if overload.protected_dropping {
                    "Its video encoders are full, so files are dropping frames.".to_string()
                } else {
                    "Its video encoders are full, so ISO files drop frames to keep the program \
                     and stream whole."
                        .to_string()
                },
            )
        } else {
            (
                "The encoders fell behind",
                "ISO files are dropping frames to keep the program and stream whole, though \
                 this setup should fit. Another app using the encoders can do this."
                    .to_string(),
            )
        };
        if let Some(fix) = &fix {
            message.push_str(&format!(" To fit, {}.", fix.description));
        }
        let mut toast = Toast::new(ToastKind::Warning, title, message)
            .key("drops")
            .rows(rows);
        toast = match fix {
            Some(fix) if fix.method.as_deref() == Some("stream_stop") => {
                toast.action("Stop Stream", |app| {
                    // Only ever stops: it may have stopped meanwhile.
                    let live = app
                        .session
                        .borrow()
                        .stream
                        .as_ref()
                        .is_some_and(StreamStatus::is_active);
                    if live {
                        toggle_stream(app);
                    }
                })
            }
            Some(fix) if fix.change.is_some() => {
                let change = fix.change.clone().unwrap_or_default();
                let description = fix.description.clone();
                if recording {
                    toast.action("Fix After This Take", move |app| {
                        app.controls
                            .set_pending_fix(app, description.clone(), change.clone());
                    })
                } else {
                    toast.action("Apply Fix", move |app| {
                        apply_fix(app, &description, change.clone());
                    })
                }
            }
            _ => toast.action("Details", move |app| {
                open_details(app, if recording { "rec" } else { "live" });
            }),
        };
        // Without a key (the estimate failed) there's no setup to name, and
        // hiding "" would hide every other key-less notice.
        let key = overload.workload_key.clone();
        if !key.is_empty() {
            toast = toast.action2("Don’t Show Again for This Setup", move |app| {
                hide_notice(app, &key);
            });
        }
        app.show_toast(toast);
    }

    /// Keep `change` to apply when the recording stops.
    pub fn set_pending_fix(&self, app: &Rc<App>, description: String, change: Value) {
        app.toast(
            ToastKind::Info,
            "The fix applies when this take ends",
            crate::app::sentence(&description),
        );
        self.pending_fix.replace(Some((description, change)));
    }

    fn apply_pending_fix(&self, app: &Rc<App>) {
        if let Some((description, change)) = self.pending_fix.take() {
            apply_fix(app, &description, change);
        }
    }
}

/// Apply a capacity fix to the live setup (undoable), and say so.
fn apply_fix(app: &Rc<App>, description: &str, change: Value) {
    let description = description.to_string();
    app.edit_then(change, EditOptions::default(), move |app, applied| {
        if applied {
            app.toast(
                ToastKind::Success,
                "Fixed for the next take",
                crate::app::sentence(&description),
            );
        }
    });
}

/// Whether the viewer hid the overload notice for this workload.
fn hidden_notice(app: &Rc<App>, key: &str) -> bool {
    app.controls.hidden.borrow().iter().any(|k| k == key)
        || (!app.mode.offscreen && crate::prefs::notice_hidden(key))
}

fn hide_notice(app: &Rc<App>, key: &str) {
    app.controls.hidden.borrow_mut().push(key.to_string());
    if !app.mode.offscreen {
        crate::prefs::hide_notice(key);
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

/// The final metadata the engine is still retrying to write for a
/// stopped take (`status.diagnostics.metadata_pending`): each says the
/// file, the error and what to do. Empty when the daemon omits it.
fn metadata_pending(diagnostics: Option<&Value>) -> Vec<String> {
    diagnostics
        .and_then(|d| d.get("metadata_pending"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

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

/// 3725 s → "01:02:05".
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
    crate::platform::abbreviate_home(folder)
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

/// The worst health of the destinations streaming (or trying to): one
/// reconnecting while the others are live makes the stream as one
/// unstable, not fine.
fn streams_health(streams: &[StreamStatus], summary: &StreamStatus) -> &'static str {
    if streams.len() <= 1 {
        return health(summary);
    }
    let rank = |level: &str| match level {
        "bad" => 3,
        "warn" => 2,
        "ok" => 1,
        _ => 0,
    };
    streams
        .iter()
        .filter(|x| x.is_active())
        .map(health)
        .max_by_key(|level| rank(level))
        .unwrap_or_else(|| health(summary))
}

/// What the LIVE details call a destination: its service, or its id when
/// that says more (two on one service, a custom server).
fn destination_label(stream: &StreamStatus, all: &[StreamStatus]) -> String {
    let shared = all.iter().filter(|x| x.service == stream.service).count() > 1;
    let plain = (stream.id == isocast_protocol::stream::MAIN_DESTINATION
        && stream.service != StreamService::Custom)
        || stream.id == stream.service.id();
    if plain && !shared {
        service_name(stream.service).into()
    } else {
        stream.id.clone()
    }
}

/// The LIVE details with several destinations: one row each, its state,
/// rate and drops.
fn destination_rows(streams: &[StreamStatus]) -> Vec<ToastRow> {
    streams
        .iter()
        .filter(|x| x.enabled || x.state != StreamState::Offline)
        .map(|x| {
            let dropped = if x.dropped_frames > 0 {
                format!(" · {} dropped", count(x.dropped_frames))
            } else {
                String::new()
            };
            let (value, tone) = match x.state {
                StreamState::Live => (
                    format!("Live · {}{dropped}", kbps(x.total_kbps)),
                    if health(x) == "ok" { "" } else { "warn" },
                ),
                StreamState::Reconnecting => (format!("Reconnecting{dropped}"), "warn"),
                StreamState::Connecting => ("Connecting…".to_string(), "warn"),
                StreamState::Failed => (
                    format!(
                        "Failed: {}",
                        x.last_error.as_deref().unwrap_or("it couldn’t connect")
                    ),
                    "bad",
                ),
                StreamState::Offline => ("Not streaming".to_string(), ""),
            };
            ToastRow {
                label: destination_label(x, streams).into(),
                value: value.into(),
                tone: tone.into(),
            }
        })
        .collect()
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
    if app.session.borrow().record_busy() {
        return;
    }
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

/// "Show in Finder": the Finder here, or the engine file browser for
/// another computer's engine (area J's "Show on <engine>").
fn show_file(app: &Rc<App>, path: &str) {
    if crate::areas::remote::is_remote(app) {
        crate::areas::remote::reveal(app, path);
    } else {
        reveal(path);
    }
}

/// Show a file or folder in the Finder (File Explorer on Windows).
#[cfg(windows)]
fn reveal(path: &str) {
    crate::platform::reveal(path);
}

/// Show a file or folder in the Finder.
#[cfg(not(windows))]
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
        if s.record_busy() {
            return;
        }
        s.busy_record = true;
        // Status reads asked before this say nothing newer.
        s.record_epoch += 1;
        let recording = s
            .status
            .as_ref()
            .is_some_and(|x| matches!(x.state, EngineState::Recording | EngineState::Starting));
        if recording {
            // Shown as finalizing at once; only a status read or the
            // `stopped` event ends it, never the stop's acknowledgement.
            if let Some(status) = s.status.as_mut() {
                status.finalizing = true;
            }
        } else {
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
        let epoch = app.session.borrow().record_epoch;
        let failed = move |app: &Rc<App>, error: &crate::daemon::CallError| {
            {
                let mut session = app.session.borrow_mut();
                if session.record_epoch == epoch
                    && let Some(status) = session.status.as_mut()
                {
                    status.finalizing = false;
                }
            }
            app.failed("Couldn’t stop recording", error);
        };
        // Answered at once: the files are still being finished (the
        // status's `finalizing`) until the `stopped` event.
        app.call(Method::RequestStop(None), move |app, result| {
            match result {
                // The `stopped` event's toast says where it went (#114),
                // for this window's stop and any other alike.
                Ok(_) => {}
                Err(error) if error.is_unknown_method() => {
                    // Updates can leave an older engine recording. Only its
                    // structured capability refusal permits this fallback;
                    // keep busy/finishing until the completed Report arrives.
                    app.call_as::<isocast_protocol::api::Report>(
                        Method::Stop(None),
                        move |app, result| {
                            match result {
                                Ok(report) if !report.errors.is_empty() => {
                                    saved_toast(app, &report);
                                }
                                Ok(_) => {}
                                Err(error) => failed(app, &error),
                            }
                            done(app);
                        },
                    );
                    return;
                }
                Err(error) => failed(app, &error),
            }
            done(app);
        });
    } else {
        app.call(Method::Start(Box::default()), move |app, result| {
            if let Err(error) = result {
                // Keyed as the engine's `stopped` toast with the same
                // words, so a refused start shows one toast, not two.
                app.show_toast(
                    Toast::new(
                        ToastKind::Error,
                        "Couldn’t start recording",
                        error.message.clone(),
                    )
                    .key(format!("rec-error:{}", error.message)),
                );
            }
            done(app);
        });
    }
}

/// Pause the recording (its files stay open, a stream carries on) or
/// resume it.
fn toggle_pause(app: &Rc<App>) {
    if app.controls.busy_pause.get() || app.session.borrow().record_busy() {
        return;
    }
    let paused = app
        .session
        .borrow()
        .status
        .as_ref()
        .and_then(|s| s.live.as_ref())
        .is_some_and(|l| l.paused);
    app.controls.busy_pause.set(true);
    app.changed(Changed::BUSY);
    let method = if paused {
        Method::Resume
    } else {
        Method::Pause
    };
    app.call(method, move |app, result| {
        app.controls.busy_pause.set(false);
        if let Err(error) = result {
            app.failed(
                if paused {
                    "Couldn’t resume the recording"
                } else {
                    "Couldn’t pause the recording"
                },
                &error,
            );
        }
        app.refresh_status(true);
        app.changed(Changed::BUSY);
    });
}

/// The start that found no camera extension says so in these words
/// (isocast-platform-macos virtual_camera/process.rs).
fn vcam_missing(error: &str) -> bool {
    error.contains("not installed")
}

/// A release build's first start asked macOS to activate the camera
/// extension, and macOS waits for the user's approval (isocast-daemon
/// virtual_camera.rs); the engine starts the camera when it comes.
fn vcam_approval(error: &str) -> bool {
    error.contains("Login Items & Extensions")
}

/// The virtual camera's button and popover: its state, its output (the
/// setup's `virtual_camera`) and size, and why it couldn't start.
fn show_vcam(app: &Rc<App>, area: &ControlsArea) {
    let s = app.session.borrow();
    let status = s.vcam.as_ref();
    area.set_vcam_state(
        match status.map(|x| x.state) {
            Some(VirtualCameraState::Starting) => "starting",
            Some(VirtualCameraState::Running) => "running",
            Some(VirtualCameraState::Stopping) => "stopping",
            Some(VirtualCameraState::Failed) => "failed",
            _ => "stopped",
        }
        .into(),
    );
    area.set_busy_vcam(s.busy_vcam);
    area.set_vcam_removable(status.is_some_and(|x| x.removable));
    area.set_vcam_removing(s.removing_vcam);
    let error = status
        .and_then(|x| x.last_error.clone())
        .unwrap_or_default();
    // The extension's install and approval words are macOS's (a macOS
    // engine's, whatever this window runs on). Windows has no camera yet;
    // Linux's engine says why its v4l2loopback device can't be written.
    let os = crate::engine::os();
    let mac = os == EngineOs::Macos;
    let unavailable = match os {
        EngineOs::Macos => String::new(),
        EngineOs::Windows => "Not available on Windows yet".into(),
        _ => status
            .and_then(|x| x.unavailable_reason.clone())
            .unwrap_or_default(),
    };
    let missing = mac && vcam_missing(&error);
    let approval = mac && vcam_approval(&error);
    area.set_vcam_missing(missing);
    area.set_vcam_approval(approval);
    area.set_vcam_unavailable(unavailable.as_str().into());
    area.set_vcam_error(
        if missing || approval || (!unavailable.is_empty() && error.contains(&unavailable)) {
            String::new()
        } else {
            error
        }
        .into(),
    );
    let destination = status
        .filter(|camera| camera.state == VirtualCameraState::Running)
        .and_then(|camera| camera.destination.as_deref())
        .map(|name| format!("Sending to {name}"));
    let governor = match status.and_then(|x| x.governor_fps) {
        Some(0) if status.is_some_and(|x| x.frames_sent == 0) => {
            "Waiting for the first picture: Automatic Quality has paused live frames.".to_string()
        }
        Some(0) => {
            "Held on its last picture by Automatic Quality to protect the recording.".to_string()
        }
        Some(fps) => {
            format!("Reduced to {fps} fps by Automatic Quality to protect the recording.")
        }
        None => String::new(),
    };
    area.set_vcam_note(
        [
            destination.as_deref().unwrap_or_default(),
            governor.as_str(),
        ]
        .into_iter()
        .filter(|x| !x.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
        .into(),
    );
    let setup = s.setup.as_deref();
    let settings = setup.map(|x| x.virtual_camera.clone()).unwrap_or_default();
    let mut targets = vec![
        kit::option("program", "Program", ""),
        kit::option("preview", "Preview", "Studio Mode"),
        kit::option("scene", "A scene", ""),
        kit::option("source", "A source", ""),
    ];
    // Offered with a vertical canvas (and kept while chosen, so the
    // engine's validation says what's missing).
    if setup.is_some_and(|x| x.vertical.is_some()) || settings.target == VcamTarget::Vertical {
        targets.push(kit::option("vertical", "Vertical program", ""));
    }
    // Every other extra canvas's program (#120): `canvas:ID`.
    let chosen_canvas = (settings.target == VcamTarget::Canvas)
        .then(|| settings.canvas.clone().unwrap_or_default());
    let mut offered = false;
    for canvas in s.extra_canvases().into_iter().filter(|c| !c.is_vertical()) {
        offered |= chosen_canvas.as_deref() == Some(canvas.as_str());
        targets.push(kit::option(
            &format!("canvas:{canvas}"),
            &format!("{} program", s.canvas_label(canvas)),
            "",
        ));
    }
    if let Some(id) = chosen_canvas.as_deref().filter(|_| !offered) {
        targets.push(kit::option(
            &format!("canvas:{id}"),
            &format!("{id} program"),
            "",
        ));
    }
    area.set_vcam_targets(model(targets));
    area.set_vcam_target(
        match settings.target {
            VcamTarget::Program => "program".to_string(),
            VcamTarget::Preview => "preview".into(),
            VcamTarget::Scene => "scene".into(),
            VcamTarget::Source => "source".into(),
            VcamTarget::Vertical => "vertical".into(),
            VcamTarget::Canvas => format!("canvas:{}", settings.canvas.as_deref().unwrap_or("")),
        }
        .as_str()
        .into(),
    );
    area.set_vcam_scenes(model(setup.map_or(Vec::new(), |x| {
        x.scenes
            .iter()
            .map(|scene| kit::option(&scene.id, crate::model::scene_name(scene), ""))
            .collect()
    })));
    area.set_vcam_scene(settings.scene.clone().unwrap_or_default().into());
    area.set_vcam_sources(model(setup.map_or(Vec::new(), |x| {
        x.sources
            .iter()
            .filter(|src| src.kind.has_video())
            .map(|src| kit::option(&src.id, src.name.as_deref().unwrap_or(&src.id), ""))
            .collect()
    })));
    area.set_vcam_source(settings.source.clone().unwrap_or_default().into());
    // The size with none set: the canvas the camera shows.
    let canvas = setup.map(|x| {
        let mut canvas = settings.clone();
        (canvas.width, canvas.height) = (None, None);
        canvas.size(x)
    });
    let mut sizes = vec![kit::option(
        "",
        "Canvas",
        &canvas.map_or(String::new(), |(w, h)| format!("{w}×{h}")),
    )];
    sizes.extend(
        VCAM_SIZES
            .iter()
            .map(|(w, h)| kit::option(&format!("{w}x{h}"), &format!("{w}×{h}"), "")),
    );
    area.set_vcam_sizes(model(sizes));
    area.set_vcam_size(
        match (settings.width, settings.height) {
            (Some(w), Some(h)) => format!("{w}x{h}"),
            _ => String::new(),
        }
        .into(),
    );
    // What it sends (running), or would send (the next start's: a
    // stopped camera's status still has the last start's size).
    let (w, h, fps) = match status {
        Some(x)
            if x.width > 0
                && matches!(
                    x.state,
                    VirtualCameraState::Running | VirtualCameraState::Starting
                ) =>
        {
            (x.width, x.height, x.fps)
        }
        _ => {
            let (w, h) = setup.map_or((0, 0), |x| settings.size(x));
            (w, h, setup.map_or(0, |x| settings.fps(x)))
        }
    };
    area.set_vcam_format(format!("{w}×{h} at {fps} fps").into());
    area.set_vcam_counts(
        match status {
            Some(x) if x.state == VirtualCameraState::Running && x.transport == "v4l2loopback" => {
                let mut text = format!("{} frames sent", count(x.frames_sent));
                if x.frames_repeated > 0 {
                    text.push_str(&format!(" · {} repeated", count(x.frames_repeated)));
                }
                if x.frames_dropped > 0 {
                    text.push_str(&format!(" · {} dropped", count(x.frames_dropped)));
                }
                text
            }
            Some(x) if x.state == VirtualCameraState::Running => format!(
                "{} frames sent · {} dropped",
                count(x.frames_sent),
                count(x.frames_dropped)
            ),
            _ => String::new(),
        }
        .into(),
    );
}

/// Start or stop the virtual camera. A development engine never installs
/// anything: without its extension the start fails, and the popover says how
/// to install it. A release engine asks macOS to activate the extension it
/// carries; the popover then says how to allow it.
fn toggle_vcam(app: &Rc<App>) {
    let on = {
        let mut s = app.session.borrow_mut();
        if s.busy_vcam {
            return;
        }
        s.busy_vcam = true;
        let on = s.vcam.as_ref().is_some_and(|x| {
            matches!(
                x.state,
                VirtualCameraState::Running | VirtualCameraState::Starting
            )
        });
        let status = s.vcam.get_or_insert_with(VirtualCameraStatus::default);
        status.state = if on {
            VirtualCameraState::Stopping
        } else {
            VirtualCameraState::Starting
        };
        on
    };
    app.changed(Changed::BUSY | Changed::VCAM);
    let method = if on {
        Method::VirtualCameraStop
    } else {
        Method::VirtualCameraStart
    };
    app.call_as::<VirtualCameraStatus>(method, move |app, result| {
        app.session.borrow_mut().busy_vcam = false;
        match result {
            Ok(status) => app.session.borrow_mut().vcam = Some(status),
            Err(error) => {
                let missing = vcam_missing(&error.message) || vcam_approval(&error.message);
                {
                    let mut s = app.session.borrow_mut();
                    let status = s.vcam.get_or_insert_with(VirtualCameraStatus::default);
                    status.state = if on {
                        VirtualCameraState::Running
                    } else {
                        VirtualCameraState::Failed
                    };
                    status.last_error = Some(error.message.clone());
                }
                if missing {
                    // Say how to install it, where the button is.
                    if let Some(window) = app.window() {
                        let area = window.global::<ControlsArea>();
                        area.set_vcam_details_serial(area.get_vcam_details_serial() + 1);
                    }
                } else {
                    app.failed(
                        if on {
                            "Couldn’t stop the virtual camera"
                        } else {
                            "Couldn’t start the virtual camera"
                        },
                        &error,
                    );
                }
                app.refresh_status(true);
            }
        }
        app.changed(Changed::BUSY | Changed::VCAM);
    });
}

/// Remove the camera extension from macOS (#69). The engine waits for
/// macOS's password prompt (minutes, perhaps), so the call goes on its own
/// connection and thread: the app's other calls don't queue behind it.
fn remove_vcam(app: &Rc<App>) {
    {
        let mut s = app.session.borrow_mut();
        if s.busy_vcam {
            return;
        }
        s.busy_vcam = true;
        s.removing_vcam = true;
    }
    app.changed(Changed::BUSY | Changed::VCAM);
    std::thread::spawn(|| {
        let result = crate::daemon::connect()
            .and_then(|mut client| {
                client.call::<isocast_protocol::virtual_camera::VirtualCameraRemoval>(
                    Method::VirtualCameraRemove,
                )
            })
            .map_err(|error| {
                error
                    .downcast_ref::<isocast_client::DaemonError>()
                    .map_or_else(|| format!("{error:#}"), |e| e.0.message.clone())
            });
        let _ = slint::invoke_from_event_loop(move || {
            crate::window::with_app(|app| {
                {
                    let mut s = app.session.borrow_mut();
                    s.busy_vcam = false;
                    s.removing_vcam = false;
                }
                match result {
                    Ok(removal) => {
                        app.toast(
                            ToastKind::Info,
                            if removal.restart_needed {
                                "Restart to finish removing the virtual camera"
                            } else {
                                "Virtual camera removed"
                            },
                            removal.message,
                        );
                    }
                    Err(message) => {
                        app.toast(
                            ToastKind::Error,
                            "Couldn’t remove the virtual camera",
                            message,
                        );
                    }
                }
                app.refresh_status(true);
                app.changed(Changed::BUSY | Changed::VCAM);
            })
        });
    });
}

/// Change the camera's output in the live setup (`virtual_camera`): the
/// engine takes it while the camera is off.
fn set_vcam(app: &Rc<App>, field: &str, value: &str) {
    let first = |ids: Vec<String>| ids.into_iter().next();
    let patch = {
        let s = app.session.borrow();
        let Some(setup) = s.setup.as_deref() else {
            return;
        };
        let current = &setup.virtual_camera;
        // Leaving a canvas target: its size (picked for that canvas's
        // shape, #120) goes too, so the new picture isn't letterboxed into
        // it.
        let unsize = |mut patch: Value| {
            if current.target == VcamTarget::Canvas {
                patch["width"] = Value::Null;
                patch["height"] = Value::Null;
            }
            patch
        };
        match field {
            "target" => match value {
                // A scene or source is needed with them: the one chosen
                // before, else the first.
                "scene" => unsize(json!({ "target": "scene", "source": null, "canvas": null,
                    "scene": current.scene.clone().or_else(|| first(setup.scenes.iter().map(|x| x.id.clone()).collect())) })),
                "source" => unsize(json!({ "target": "source", "scene": null, "canvas": null,
                    "source": current.source.clone().or_else(|| first(setup.sources.iter().filter(|x| x.kind.has_video()).map(|x| x.id.clone()).collect())) })),
                other => match other.strip_prefix("canvas:") {
                    // An extra canvas (#120): its size if the camera
                    // offers it, else the offered size of its shape.
                    Some(id) => {
                        let size = isocast_protocol::CanvasId::parse(id)
                            .ok()
                            .and_then(|c| setup.extra_canvas(c))
                            .map(|c| c.canvas(&setup.canvas))
                            .map(|c| (c.width, c.height));
                        let (width, height) = match size {
                            Some(size) if VCAM_SIZES.contains(&size) => (json!(null), json!(null)),
                            Some((w, h)) if h > w => (json!(1080), json!(1920)),
                            _ => (json!(1920), json!(1080)),
                        };
                        json!({ "target": "canvas", "canvas": id, "scene": null, "source": null,
                            "width": width, "height": height })
                    }
                    None => unsize(
                        json!({ "target": other, "scene": null, "source": null, "canvas": null }),
                    ),
                },
            },
            "scene" => json!({ "scene": value }),
            "source" => json!({ "source": value }),
            "size" => match value.split_once('x') {
                Some((w, h)) => match (w.parse::<u32>(), h.parse::<u32>()) {
                    (Ok(w), Ok(h)) => json!({ "width": w, "height": h }),
                    _ => return,
                },
                None => json!({ "width": null, "height": null }),
            },
            _ => return,
        }
    };
    app.edit(json!({ "virtual_camera": patch }), EditOptions::default());
}

/// The camera's status for snapshots: `missing` (a start found no
/// extension), `running` (an hour of frames), `failed` (another reason),
/// `removable` (a release engine's, with Remove Virtual Camera).
fn sample_vcam(app: &Rc<App>, which: &str) -> anyhow::Result<VirtualCameraStatus> {
    let s = app.session.borrow();
    let mut status = s.vcam.clone().unwrap_or_default();
    // As each engine reports it: Linux's says why its v4l2loopback device
    // can't be written (before any start); Windows has no camera to start.
    let os = crate::engine::os();
    let linux = !matches!(os, EngineOs::Macos | EngineOs::Windows);
    if linux {
        status.transport = "v4l2loopback".into();
        status.unavailable_reason = None;
        status.governor_fps = None;
    }
    match which {
        "missing" | "approval" if os == EngineOs::Windows => {
            status.state = VirtualCameraState::Stopped;
            status.last_error = None;
        }
        "missing" if linux => {
            status.state = VirtualCameraState::Stopped;
            status.last_error = None;
            status.unavailable_reason = Some("No v4l2loopback camera device. Install v4l2loopback for your kernel (Debian/Ubuntu: v4l2loopback-dkms and matching kernel headers), then run: sudo modprobe v4l2loopback exclusive_caps=1 card_label=\"Isocast Camera\"".into());
        }
        "approval" if linux => {
            status.state = VirtualCameraState::Stopped;
            status.last_error = None;
            status.unavailable_reason = Some("/dev/video10 can’t be opened for writing: add yourself to the video group, then sign in again".into());
        }
        "missing" => {
            status.state = VirtualCameraState::Failed;
            status.last_error = Some("virtual_camera: Isocast Virtual Camera is not installed/approved; run scripts/vcam-install.sh (a development engine never installs it)".into());
        }
        "approval" => {
            status.state = VirtualCameraState::Failed;
            status.last_error = Some("virtual_camera: allow Isocast Virtual Camera in System Settings › General › Login Items & Extensions › Camera Extensions (macOS asks once); it starts as soon as you do".into());
        }
        "running" => {
            status.state = VirtualCameraState::Running;
            status.frames_sent = 108_000;
            status.frames_dropped = 0;
            status.last_error = None;
        }
        "failed" => {
            status.state = VirtualCameraState::Failed;
            status.last_error = Some("virtual_camera.width: give an advertised size: 1920x1080, 1280x720, 2560x1440, 3840x2160, 1080x1920, 720x1280".into());
        }
        // A release engine (it carries the extension): Remove is offered.
        "removable" => {
            status.state = VirtualCameraState::Stopped;
            status.last_error = None;
            status.removable = true;
        }
        other => anyhow::bail!(
            "vcam takes missing, approval, running, failed or removable, not {other:?}"
        ),
    }
    Ok(status)
}

/// The replay buffer's button and Save Replay: its state, its length and
/// what it holds.
fn show_replay(app: &Rc<App>, area: &ControlsArea) {
    let s = app.session.borrow();
    let status = s.replay.as_ref();
    area.set_replay_state(
        match status.map(|x| x.state) {
            _ if s.busy_replay => {
                if status.is_some_and(|x| x.state == ReplayState::Running) {
                    "stopping"
                } else {
                    "starting"
                }
            }
            Some(ReplayState::Running) => "running",
            Some(ReplayState::Failed) => "failed",
            _ => "off",
        }
        .into(),
    );
    area.set_busy_replay(s.busy_replay);
    area.set_saving_replay(s.saving_replay);
    let seconds = status
        .filter(|x| x.state == ReplayState::Running)
        .map(|x| x.seconds)
        .or_else(|| s.setup.as_ref().map(|x| x.recording.replay.seconds))
        .unwrap_or(30);
    area.set_replay_length(format!("the last {}", duration_words(seconds)).into());
    area.set_replay_counts(
        status
            .filter(|x| x.state == ReplayState::Running)
            .map(|x| {
                format!(
                    "{:.0} s held · {}",
                    x.buffered_seconds.min(f64::from(x.seconds)),
                    megabytes(x.memory_bytes)
                )
            })
            .unwrap_or_default()
            .into(),
    );
}

/// "30 s", "2 min", "1 min 30 s".
fn duration_words(seconds: u32) -> String {
    match (seconds / 60, seconds % 60) {
        (0, s) => format!("{s} s"),
        (m, 0) => format!("{m} min"),
        (m, s) => format!("{m} min {s} s"),
    }
}

/// "61 MB", "1.2 GB".
fn megabytes(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.1} GB", bytes as f64 / 1e9)
    } else {
        format!("{:.0} MB", bytes as f64 / 1e6)
    }
}

/// Canvas `canvas`'s replay buffer, as last told (#120: each canvas has
/// its own).
pub(crate) fn replay_of(
    s: &crate::session::Session,
    canvas: isocast_protocol::CanvasId,
) -> Option<&ReplayStatus> {
    if canvas.is_main() {
        s.replay.as_ref()
    } else {
        s.canvases
            .iter()
            .find(|c| c.id == canvas)
            .and_then(|c| c.replay.as_ref())
    }
}

/// Start or stop canvas `canvas`'s replay buffer (the Controls dock's is
/// the main canvas's; the canvas monitor's menu has the others', #120).
pub(crate) fn toggle_replay(app: &Rc<App>, canvas: isocast_protocol::CanvasId) {
    let on = {
        let mut s = app.session.borrow_mut();
        if s.busy_replay {
            return;
        }
        s.busy_replay = true;
        replay_of(&s, canvas).is_some_and(|x| x.state == ReplayState::Running)
    };
    app.changed(Changed::BUSY | Changed::REPLAY);
    let params = isocast_protocol::replay::ReplayParams::of(canvas);
    let method = if on {
        Method::ReplayStop(params)
    } else {
        Method::ReplayStart(params)
    };
    app.call_as::<ReplayStatus>(method, move |app, result| {
        app.session.borrow_mut().busy_replay = false;
        match result {
            Ok(status) => app.session.borrow_mut().set_replay(status),
            Err(error) => {
                app.failed(
                    if on {
                        "Couldn’t stop the replay buffer"
                    } else {
                        "Couldn’t start the replay buffer"
                    },
                    &error,
                );
                app.refresh_status(true);
            }
        }
        app.changed(Changed::BUSY | Changed::REPLAY);
    });
}

/// Save Replay: canvas `canvas`'s buffer's last seconds to a file; a
/// toast says where.
pub(crate) fn save_replay(app: &Rc<App>, canvas: isocast_protocol::CanvasId) {
    {
        let mut s = app.session.borrow_mut();
        if s.saving_replay {
            return;
        }
        s.saving_replay = true;
    }
    app.changed(Changed::REPLAY);
    let params = isocast_protocol::replay::ReplayParams::of(canvas);
    app.call_as::<ReplaySaved>(Method::ReplaySave(params), |app, result| {
        app.session.borrow_mut().saving_replay = false;
        match result {
            Ok(saved) => replay_saved(app, &saved),
            Err(error) => app.failed("Couldn’t save the replay", &error),
        }
        app.changed(Changed::REPLAY);
    });
}

/// The toast for a saved replay (this window's, a hotkey's or another
/// client's).
pub fn replay_saved(app: &Rc<App>, saved: &ReplaySaved) {
    let name = std::path::Path::new(&saved.path)
        .file_name()
        .map_or(saved.path.clone(), |n| n.to_string_lossy().into_owned());
    let mut message = format!(
        "{name}: {:.1} s, {}.",
        saved.seconds,
        megabytes(saved.bytes)
    );
    if saved.short {
        message.push_str(" Shorter than asked: the buffer hadn’t filled yet.");
    }
    if saved.dropped_frames > 0 {
        message.push_str(&format!(
            " {} frame(s) missed show the picture before.",
            saved.dropped_frames
        ));
    }
    let shown = saved.path.clone();
    app.show_toast(
        Toast::new(ToastKind::Success, "Replay saved", message)
            .action(crate::areas::remote::reveal_label(app), move |app| {
                show_file(app, &shown)
            }),
    );
}

/// The vertical canvas's streams (#96): whether one streams (or tries
/// to), whether it is set up with a key for its main destination, and why
/// its key can't be used.
fn vertical_stream_state(s: &crate::session::Session) -> (bool, bool, Option<&'static str>) {
    let active = s.vertical_streams.iter().any(StreamStatus::is_active);
    let configured = s
        .setup
        .as_ref()
        .and_then(|x| x.vertical.as_ref())
        .is_some_and(|v| v.stream.is_some());
    let enabled: Vec<&StreamStatus> = s.vertical_streams.iter().filter(|x| x.enabled).collect();
    // Every destination turned off on purpose: the vertical canvas simply
    // doesn't go live, without a warning.
    let all_off = enabled.is_empty() && !s.vertical_streams.is_empty();
    let problem = if !configured || all_off {
        None
    } else if enabled.is_empty() || enabled.iter().any(|x| !x.key_set) {
        Some(crate::engine::pick(
            "Add the vertical canvas’s stream key. It’s kept in the Keychain, apart from the main canvas’s.",
            "Add the vertical canvas’s stream key. It’s kept in the Credential Manager, apart from the main canvas’s.",
        ))
    } else if enabled.iter().any(|x| x.key_problem.is_some()) {
        Some(
            "The vertical canvas’s saved stream key can’t be read. Settings › Stream says how to fix it.",
        )
    } else {
        None
    };
    (active, configured && !all_off && problem.is_none(), problem)
}

/// Start or stop the vertical canvas's streams alongside the main one's
/// (Start Streaming starts both once both are set up, #96).
fn toggle_vertical_alongside(app: &Rc<App>, start: bool) {
    let params = isocast_protocol::api::StreamStartParams {
        canvas: isocast_protocol::CanvasId::Vertical,
        ..Default::default()
    };
    app.session.borrow_mut().busy_canvas_stream = true;
    app.changed(Changed::BUSY);
    let method = if start {
        Method::StreamStart(Some(params))
    } else {
        Method::StreamStop(Some(params))
    };
    app.call_as::<isocast_protocol::api::Streaming>(method, move |app, result| {
        match result {
            Ok(status) => {
                app.session.borrow_mut().vertical_streams = if status.destinations.is_empty() {
                    vec![status.stream]
                } else {
                    status.destinations
                };
            }
            Err(error) => {
                app.failed(
                    if start {
                        "The vertical canvas didn’t go live"
                    } else {
                        "Couldn’t stop the vertical stream"
                    },
                    &error,
                );
                app.refresh_status(true);
            }
        }
        app.session.borrow_mut().busy_canvas_stream = false;
        app.changed(Changed::BUSY | Changed::STREAM);
    });
}

/// Start or stop the `[[canvases]]`' streams alongside the main one's
/// (#84): on start, each canvas whose enabled destinations all have keys
/// and none streams; on stop, each that streams. The vertical canvas has
/// its own path (`toggle_vertical_alongside`).
fn toggle_canvases_alongside(app: &Rc<App>, start: bool) {
    let canvases: Vec<isocast_protocol::CanvasId> = app
        .session
        .borrow()
        .canvases
        .iter()
        .filter(|c| !c.id.is_vertical())
        .filter(|c| {
            let active = c.streams.iter().any(StreamStatus::is_active);
            let enabled: Vec<&StreamStatus> = c.streams.iter().filter(|x| x.enabled).collect();
            if start {
                !active
                    && !enabled.is_empty()
                    && enabled.iter().all(|x| x.key_set && x.key_problem.is_none())
            } else {
                active
            }
        })
        .map(|c| c.id)
        .collect();
    for canvas in canvases {
        let params = isocast_protocol::api::StreamStartParams {
            canvas,
            ..Default::default()
        };
        let method = if start {
            Method::StreamStart(Some(params))
        } else {
            Method::StreamStop(Some(params))
        };
        app.call_as::<isocast_protocol::api::Streaming>(method, move |app, result| {
            if let Err(error) = result {
                app.failed(
                    &if start {
                        format!("Canvas {canvas} didn’t go live")
                    } else {
                        format!("Couldn’t stop canvas {canvas}’s stream")
                    },
                    &error,
                );
            }
            app.refresh_status(true);
        });
    }
}

fn toggle_stream(app: &Rc<App>) {
    let (vertical_on, vertical_ready, vertical_problem) =
        vertical_stream_state(&app.session.borrow());
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
            Some(crate::engine::pick(
                "Add your stream key first. It’s kept in the Keychain, never in the setup.",
                "Add your stream key first. It’s kept in the Credential Manager, never in the setup.",
            ))
        } else if s.stream.as_ref().is_some_and(|x| x.key_problem.is_some()) {
            Some("The saved stream key can’t be read. Settings › Stream says how to fix it.")
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
    // The other canvases' streams go with the main one's (#84).
    toggle_canvases_alongside(app, !on);
    // The vertical canvas's streams go with the main one's: started when
    // they are set up (else why not, with a way there), stopped with it.
    if on && vertical_on {
        toggle_vertical_alongside(app, false);
    } else if !on && !vertical_on {
        if vertical_ready {
            toggle_vertical_alongside(app, true);
        } else if let Some(problem) = vertical_problem {
            let toast = Toast::new(
                ToastKind::Warning,
                "Only the main canvas is going live",
                problem,
            )
            .action("Open Settings", |app| {
                app.open_dialog("settings", "stream-vertical");
            });
            app.show_toast(toast);
        }
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
        Method::StreamStop(None)
    } else {
        Method::StreamStart(None)
    };
    app.call_as::<isocast_protocol::api::Streaming>(method, move |app, result| {
        match result {
            Ok(status) => app
                .session
                .borrow_mut()
                .set_streams(Some(status.stream), status.destinations),
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

/// Restart the live engine (the vertical canvas's Restart Engine, #96):
/// isocastd refuses, atomically, while anything records, streams or
/// exports; the window's link starts it again.
fn restart_engine(app: &Rc<App>) {
    app.call(Method::ShutdownIfIdle, |app, result| match result {
        Ok(_) => {
            app.toast(
                ToastKind::Info,
                "Restarting the engine",
                "The vertical canvas comes back with it.",
            );
        }
        Err(error) => app.failed(
            "The engine can’t restart yet: stop recording and streaming first",
            &error,
        ),
    });
}

/// Automatic quality's words (T.2) for the status bar and Settings, while
/// it is doing something: lowering a rate, finding a bottleneck, or held
/// back by what the user keeps (pins). None while everything runs as set
/// (or it is off). The engine's own reason, else what is lowered.
pub(crate) fn governor_words(
    governor: &isocast_protocol::governor::GovernorStatus,
) -> Option<String> {
    if let Some(error) = &governor.error {
        return Some(format!("Automatic quality: {error}"));
    }
    let lowered = |percent: u32| percent < 100;
    let active = governor.bottleneck.is_some()
        || governor.limited_by_pins
        || governor.preview_fps.is_some()
        || lowered(governor.iso_percent)
        || lowered(governor.vertical_percent)
        || lowered(governor.stream_bitrate_percent);
    if !governor.enabled || !active {
        return None;
    }
    let reason = governor.reason.trim();
    if !reason.is_empty() {
        return Some(reason.to_string());
    }
    let mut parts = Vec::new();
    if let Some(fps) = governor.preview_fps {
        parts.push(format!("preview at {fps} fps"));
    }
    for (what, percent) in [
        ("ISO video", governor.iso_percent),
        ("vertical video", governor.vertical_percent),
        ("stream bitrate", governor.stream_bitrate_percent),
    ] {
        if lowered(percent) {
            parts.push(format!("{what} at {percent}%"));
        }
    }
    if governor.limited_by_pins {
        parts.push("kept settings stop it lowering more".to_string());
    }
    Some(if parts.is_empty() {
        format!(
            "Automatic quality is watching this {}’s load",
            crate::engine::words().machine
        )
    } else {
        format!("Automatic quality: {}", parts.join(", "))
    })
}

/// The status bar's words for the vertical canvas (#96), and whether they
/// warn: that it stopped, or how its streams are doing while they run.
fn vertical_words(s: &crate::session::Session) -> (Option<String>, bool) {
    let stopped = s
        .status
        .as_ref()
        .and_then(|x| x.engine.as_ref())
        .and_then(|e| e.vertical.as_ref())
        .and_then(|v| v.stopped.as_deref());
    if let Some(why) = stopped
        && s.setup.as_ref().is_some_and(|x| x.vertical.is_some())
    {
        return (
            Some(format!(
                "vertical canvas stopped. {}",
                crate::model::vertical_recovery(why)
            )),
            true,
        );
    }
    // A failed one is news while the broadcast runs, not after.
    let broadcasting = s.streams.iter().any(StreamStatus::is_active)
        || s.vertical_streams.iter().any(StreamStatus::is_active);
    let shown: Vec<&StreamStatus> = s
        .vertical_streams
        .iter()
        .filter(|x| x.is_active() || (broadcasting && x.state == StreamState::Failed))
        .collect();
    if shown.is_empty() {
        return (None, false);
    }
    let live = shown
        .iter()
        .filter(|x| x.state == StreamState::Live)
        .count();
    let any = |state: StreamState| shown.iter().any(|x| x.state == state);
    if live == shown.len() {
        let elapsed = shown
            .iter()
            .map(|x| x.elapsed_seconds)
            .fold(0.0f64, f64::max);
        let words = if live > 1 {
            format!("vertical live to {live} for {}", clock(elapsed as u64))
        } else {
            format!("vertical live for {}", clock(elapsed as u64))
        };
        (Some(words), false)
    } else if live > 0 {
        (
            Some(format!("vertical live to {live} of {}", shown.len())),
            true,
        )
    } else if any(StreamState::Reconnecting) {
        (Some("vertical reconnecting".into()), true)
    } else if any(StreamState::Connecting) {
        (Some("connecting the vertical stream…".into()), false)
    } else {
        (Some("vertical stream failed".into()), true)
    }
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
        // Offline, with a saved key this build may not read (#63).
        "key-problem" => json!({
            "state": "offline", "service": "twitch", "server": "live.twitch.tv",
            "key_set": true,
            "key_problem": wording::select_linux("The saved key was stored by a differently signed copy of this \
                Isocast, and macOS lets only that copy use or replace it. Open Keychain Access, \
                delete the “dev.isocast.release.stream” item, then paste the key and press Save Key.",
                "The saved key could not be read from Credential Manager. Open Credential Manager › Windows Credentials and check the Isocast entry, then paste the key and press Save Key.",
        "The saved key could not be read from Isocast's credentials file. Paste the key again to replace it.")
        }),
        other => anyhow::bail!(
            "no sample stream {other:?}; one of live, reconnecting, connecting, short, \
             key-problem, multi"
        ),
    };
    Ok(serde_json::from_value(value)?)
}

/// Every destination's status for a snapshot: one for the samples above,
/// three for `multi` (YouTube and a backup server live, Twitch
/// reconnecting).
fn sample_streams(which: &str) -> anyhow::Result<Vec<StreamStatus>> {
    if which != "multi" {
        return Ok(vec![sample_stream(which)?]);
    }
    let values = [
        json!({
            "id": "main", "state": "live", "service": "youtube",
            "server": "rtmps://a.rtmps.youtube.com/live2", "key_set": true,
            "elapsed_seconds": 754, "width": 1920, "height": 1080, "fps": 30,
            "video_kbps": 5980, "target_video_kbps": 6000, "audio_kbps": 160,
            "total_kbps": 6140, "frames_sent": 22620, "dropped_frames": 0,
            "congestion": 0.04, "reconnects": 0
        }),
        json!({
            "id": "twitch", "state": "reconnecting", "service": "twitch",
            "server": "rtmps://ingest.global-contribute.live-video.net/app", "key_set": true,
            "elapsed_seconds": 754, "width": 1920, "height": 1080, "fps": 30,
            "target_video_kbps": 6000, "frames_sent": 21900, "dropped_frames": 720,
            "congestion": 0.0, "reconnects": 1,
            "last_error": "the connection dropped: Connection reset by peer"
        }),
        json!({
            "id": "backup", "state": "live", "service": "custom",
            "server": "rtmp://relay.local/live", "key_set": true,
            "elapsed_seconds": 754, "width": 1280, "height": 720, "fps": 30,
            "video_kbps": 2990, "target_video_kbps": 3000, "audio_kbps": 160,
            "total_kbps": 3150, "frames_sent": 22620, "dropped_frames": 0,
            "congestion": 0.02, "reconnects": 0
        }),
    ];
    values
        .into_iter()
        .map(|value| Ok(serde_json::from_value(value)?))
        .collect()
}

/// The toast a stopped recording shows: its files, and where they are.
fn saved_toast(app: &Rc<App>, report: &isocast_protocol::api::Report) {
    let files = report.files.len();
    let folder = home_short(&report.folder.display().to_string());
    let files = format!(
        "{files} file{} in {folder}",
        if files == 1 { "" } else { "s" }
    );
    // Problems while recording: not "saved" (as `isocast stop` fails on
    // them). The files it lists were still finished; say where they are.
    if let Some(first) = report.errors.first() {
        let more = match report.errors.len() - 1 {
            0 => String::new(),
            n => format!(" (and {n} more)"),
        };
        let shown = report.manifest.display().to_string();
        app.show_toast(
            Toast::new(
                ToastKind::Error,
                "The recording had errors",
                format!("{}{more} · {files} kept", crate::app::sentence(first)),
            )
            .key("recording-saved")
            .action(crate::areas::remote::reveal_label(app), move |app| {
                show_file(app, &shown)
            }),
        );
        return;
    }
    let shown = report.manifest.display().to_string();
    app.show_toast(
        Toast::new(ToastKind::Success, "Recording saved", files)
            .key("recording-saved")
            .action(crate::areas::remote::reveal_label(app), move |app| {
                show_file(app, &shown)
            }),
    );
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
    fn pending_metadata_is_read_from_diagnostics() {
        let reason = "C:\\rec\\take.isocast.json: final metadata publication is pending: denied";
        assert_eq!(
            metadata_pending(Some(&json!({ "metadata_pending": [reason] }))),
            vec![reason.to_string()]
        );
        assert!(metadata_pending(Some(&json!({ "metadata_pending": [] }))).is_empty());
        assert!(metadata_pending(Some(&json!({ "frame_budget": null }))).is_empty());
        assert!(metadata_pending(None).is_empty());
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
