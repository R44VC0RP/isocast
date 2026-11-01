//! Area D: Settings (every tab), the first run, Welcome, the connecting
//! and failed states, and About (ui/areas/settings/). The web app's
//! settings.js, onboarding.js and states.js are the behaviour matched.
//!
//! Settings edits a draft: the live setup's name, canvas, recording, audio
//! and stream as JSON, and this viewer's snapping and preview rate. Each
//! control names the field it changes (`set-text`, `set-number`,
//! `set-flag`); Apply (or OK) sends what differs from the live setup as
//! one merge patch, an undoable edit. The stream key goes straight to the
//! Keychain (`stream_key_set`) and is never shown again.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use isocast_protocol::api::{
    ApplyParams, ImportObsParams, InitParams, Method, StreamKeyParams, StreamKeyTarget,
};
use isocast_protocol::change::{Change, merge_patch};
use isocast_protocol::devices::{CameraKind, Devices, Permission};
use isocast_protocol::governor::GovernorSettings;
use isocast_protocol::init::{InitOptions, Layout};
use isocast_protocol::platform_text as wording;
use isocast_protocol::stream::{MAIN_DESTINATION, StreamService, StreamState, StreamStatus};
use isocast_protocol::{CanvasId, EncoderQuality, QualityPreset, Setup};
use serde_json::{Map, Value, json};
use slint::{ComponentHandle, SharedString};

use super::{Area, Changed};
use crate::app::App;
use crate::daemon::{Link, Update};
use crate::kit::{self, model};
use crate::platform::{self, Choose};
use crate::session::EditOptions;
use crate::{
    AppWindow, DestinationRow, DeviceChoice, DropdownOption, HotkeyGroup, HotkeyRow, IsoChoice,
    IsoTrack, MicRow, NavTab, ServiceChoice, SettingsArea, ToastKind, Ui,
};

/// Snapshot states (`--state NAME[:ARG]`).
pub const STATES: &[&str] = &[
    "about",
    "settings[:TAB]",
    "notice:TEXT",
    "set-text:FIELD=VALUE",
    "set-number:FIELD=VALUE",
    "set-flag:FIELD=VALUE",
    "choose-service:ID",
    "destination:ID",
    "add-destination",
    "iso-source:ID",
    "advanced-quality",
    "connecting",
    "reconnecting",
    "failed[:WHY]",
    "first-run[:STEP]",
    "first-run-next",
    "first-run-set:FIELD=VALUE",
    "first-run-create",
    "open-setup:PATH",
    "import-obs:PATH",
    "hotkeys-sample",
    "bug-report",
    "bug-report-describe:TEXT",
    "bug-report-create",
    "bug-report-send",
    "hotkey-record:INDEX",
];

/// Settings' pages in the sidebar's order and groups (Paper's Settings
/// sheet: Recording, Broadcast, System), in OBS's words: id, name, glyph
/// (by name), group, and the line under the page's name.
const TABS: &[(&str, &str, &str, &str, &str)] = &[
    (
        "output",
        "Recording",
        "drive",
        "Recording",
        "Where takes are written, how good they look, and which files each take makes.",
    ),
    (
        "video",
        "Video",
        "monitor",
        "Recording",
        "The canvas every scene is drawn on, and how many frames a second it makes.",
    ),
    (
        "audio",
        "Audio",
        "volume",
        "Recording",
        "The microphones your sources use, and where monitored sources play.",
    ),
    (
        "stream",
        "Stream",
        "sheet-broadcast",
        "Broadcast",
        "Where the program goes live, and how it is sent.",
    ),
    (
        "general",
        "General",
        "sun",
        "System",
        "This setup's name, snapping in the preview, how often the preview draws, app updates and permissions.",
    ),
    (
        "hotkeys",
        "Hotkeys",
        "keys",
        "System",
        "Keys for recording, streaming, scenes and sources, and the window’s own.",
    ),
    (
        "advanced",
        "Advanced",
        "sheet-sliders",
        "System",
        "Automatic quality, what a stream does when its connection drops, and developer mode.",
    ),
    (
        "remote",
        "Remote",
        "link",
        "System",
        "Control other computers running Isocast, or let them control this one, on the local network.",
    ),
];

/// Streaming services, and where each one's key (and for Kick, server)
/// comes from.
const SERVICES: &[(&str, &str, &str)] = &[
    ("twitch", "Twitch", "Key from the Creator Dashboard"),
    ("youtube", "YouTube", "Key from YouTube Studio"),
    ("kick", "Kick", "Server and key from the Creator Dashboard"),
    ("facebook", "Facebook", "Key from Live Producer"),
    ("custom", "Custom server", "Any RTMP or RTMPS server"),
];

/// The server a service uses when the setup names none.
fn preset_server(service: &str) -> Option<&'static str> {
    match service {
        "twitch" => Some("rtmps://ingest.global-contribute.live-video.net/app"),
        "youtube" => Some("rtmps://a.rtmps.youtube.com/live2"),
        "facebook" => Some("rtmps://rtmp-api.facebook.com/rtmp"),
        _ => None,
    }
}

const CODECS: &[(&str, &str)] = &[
    ("hevc", "HEVC (H.265)"),
    ("h264", "H.264"),
    ("prores422_proxy", "ProRes 422 Proxy"),
    ("prores422_lt", "ProRes 422 LT"),
    ("prores422", "ProRes 422"),
    ("prores422_hq", "ProRes 422 HQ"),
    ("prores4444", "ProRes 4444"),
];

const CANVASES: &[(&str, &str)] = &[
    ("1920x1080", "1920×1080"),
    ("2560x1440", "2560×1440"),
    ("3840x2160", "3840×2160"),
    ("1280x720", "1280×720"),
    ("1080x1920", "1080×1920 (vertical)"),
];

const FPS: &[u32] = &[24, 25, 30, 50, 60];

const PAN_LAWS: &[(&str, &str)] = &[
    ("sine", "Sine (−5.3 dB at half)"),
    ("square", "Constant power (−3 dB at half)"),
    ("linear", "Linear (−6 dB at half)"),
];

/// Recording quality as OBS's simple output mode names it: five stops the
/// setup's `encoder.quality` takes by name. Numbers (1–100) work too;
/// each name stands for the engine's own number.
const QUALITIES: &[(&str, QualityPreset)] = &[
    ("low", QualityPreset::Low),
    ("medium", QualityPreset::Medium),
    ("high", QualityPreset::High),
    ("very_high", QualityPreset::VeryHigh),
    ("incredible", QualityPreset::Incredible),
];

/// Every keyboard shortcut, as Settings → Hotkeys lists them (Help →
/// Keyboard Shortcuts opens that page): the web app's shortcuts.js.
/// A shortcut: its keys ("–" between two is a range) and what it does.
type Shortcut = (&'static [&'static str], &'static str);

const SHORTCUTS: &[(&str, &[Shortcut])] = &[
    (
        "Scenes and Studio Mode",
        &[
            (
                &["1", "–", "9"],
                "Switch to that scene (in Studio Mode, put it in preview)",
            ),
            (
                &["Return"],
                "Transition: put the preview on air (Studio Mode; Space too)",
            ),
            (&["⇧", "⌘", "S"], "Studio Mode"),
            (&["⇧", "⌘", "N"], "Add a scene"),
        ],
    ),
    (
        "Sources",
        &[
            (&["⇧", "⌘", "A"], "Add a source"),
            (&["⌘", "E"], "Edit Transform"),
            (&["⌘", "R"], "Reset Transform"),
            (&["⌘", "F"], "Fit to Screen"),
            (&["⌘", "S"], "Stretch to Screen"),
            (&["⌘", "D"], "Center to Screen"),
            (
                &["⌘", "C"],
                wording::select(
                    "Copy the selected source (⌘V: paste it as a reference)",
                    "Copy the selected source (Ctrl+V: paste it as a reference)",
                ),
            ),
            (
                &["←", "↑", "→", "↓"],
                wording::select(
                    "Nudge the selected source 1 px (with ⇧, 10 px)",
                    "Nudge the selected source 1 px (with Shift, 10 px)",
                ),
            ),
            (&["⌫"], "Remove the selected source from the scene"),
            (&["Esc"], "Deselect"),
        ],
    ),
    (
        "On the preview",
        &[
            (
                &["⇧", "Drag"],
                "Move along one axis; resize keeping the shape",
            ),
            (&["⌥", "Drag"], "Crop from an edge or corner"),
            (&["⌘", "Drag"], "Place freely, without snapping"),
            (
                &["⌘", "Z"],
                wording::select("Undo (⇧⌘Z: redo)", "Undo (Ctrl+Y: redo)"),
            ),
        ],
    ),
    (
        "Fields and the mixer",
        &[
            (
                &["Drag"],
                wording::select(
                    "A number’s label: scrub (with ⇧, ×10; with ⌥, ×0.1)",
                    "A number’s label: scrub (with Shift, ×10; with Alt, ×0.1)",
                ),
            ),
            (
                &["↑", "↓"],
                wording::select(
                    "In a number: step (with ⇧, ×10)",
                    "In a number: step (with Shift, ×10)",
                ),
            ),
            (&["Return"], "Apply what you typed (Esc: put it back)"),
            (&["Double-click"], "A fader: back to 0 dB"),
        ],
    ),
    (
        "From the keyboard",
        &[
            (
                &["Tab"],
                "Move between controls (in a dialog, it stays in the dialog)",
            ),
            (
                &["↑", "↓"],
                "In the Scenes or Sources list (reached with Tab): the next one",
            ),
            (&["F2"], "In the Scenes or Sources list: rename"),
            (
                &["←", "→"],
                wording::select(
                    "On a slider: move it (with ⇧, ×10; Home and End: its ends)",
                    "On a slider: move it (with Shift, ×10; Home and End: its ends)",
                ),
            ),
            (
                &["↑", "↓"],
                "In a menu: the next item (→ opens a submenu, ← leaves it)",
            ),
        ],
    ),
    (
        "Window",
        // Off macOS: full screen is F11, and Alt+F4 exits (the File menu's).
        if !cfg!(target_os = "macos") {
            &[
                (&["⌘", ","], "Settings"),
                (&["F11"], "Full screen"),
                (&["⌘", "/"], "These shortcuts"),
                (&["⌥", "F4"], "Exit"),
            ]
        } else {
            &[
                (&["⌘", ","], "Settings"),
                (&["⌃", "⌘", "F"], "Full screen"),
                (&["⌘", "/"], "These shortcuts"),
            ]
        },
    ),
];

/// This viewer's settings on General (not the setup's).
#[derive(Debug, Clone, PartialEq)]
struct Prefs {
    snapping: bool,
    distance: i32,
    preview_rate: String,
    /// Check for app updates about once a day (Sparkle keeps it).
    auto_updates: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            snapping: true,
            distance: 8,
            preview_rate: "canvas".into(),
            auto_updates: true,
        }
    }
}

impl Prefs {
    fn read(ui: &Ui) -> Self {
        Self {
            snapping: ui.get_snap_enabled(),
            distance: ui.get_snap_distance().round() as i32,
            preview_rate: ui.get_preview_rate().into(),
            auto_updates: crate::update::automatic(),
        }
    }
}

/// The Settings dialog's draft, while it is open.
#[derive(Default)]
struct Draft {
    /// The setup's parts Settings changes, as they were at the last Apply
    /// (or when it opened), and as changed since.
    base: Value,
    draft: Value,
    /// Microphone sources' devices (Audio → Audio Devices), by source id.
    mics: BTreeMap<String, String>,
    prefs_base: Prefs,
    prefs: Prefs,
    /// Why it opened (Stream), until the stream is ready.
    notice: String,
    /// Custom and Kick ask for a server before streaming is set up.
    needs_server: String,
    /// The stream destination open in Stream (`main` or an id in
    /// `stream.destinations`), and whether one is being added (#73).
    destination: String,
    adding: bool,
    /// Which canvas's streams Stream shows (#96): `[stream]`, or the
    /// vertical canvas's `[vertical.stream]` (its own destinations and
    /// Keychain keys).
    canvas: CanvasId,
    /// The canvas besides the main one Video › Canvases edits (#84), if
    /// one was picked; else the first.
    edit_canvas: Option<CanvasId>,
    /// Every canvas row is closed (its open one was clicked).
    canvas_closed: bool,
    applying: bool,
    /// Why the daemon refused the last Apply/OK ("" if it didn't).
    refused: String,
    key_busy: bool,
}

/// The first run's choices.
#[derive(Debug, Clone)]
struct FirstRun {
    step: i32,
    /// The first step (permissions) is shown. On Windows only when a
    /// privacy switch blocks the camera or microphone: nothing else there
    /// needs allowing. Once shown in a run, it stays.
    access: bool,
    display: String,
    /// None: the best camera (the daemon picks).
    camera: Option<String>,
    microphone: String,
    system_audio: bool,
    layout: String,
    canvas: String,
    fps: u32,
    folder: String,
    name: String,
    creating: bool,
}

impl FirstRun {
    fn first_step(&self) -> i32 {
        if self.access { 0 } else { 1 }
    }
}

impl Default for FirstRun {
    fn default() -> Self {
        Self {
            step: 0,
            access: true,
            display: "main".into(),
            camera: None,
            microphone: "default".into(),
            system_audio: true,
            layout: "bottom_right".into(),
            canvas: "1920x1080".into(),
            fps: 60,
            folder: isocast_protocol::init::DEFAULT_FOLDER.into(),
            name: "My setup".into(),
            creating: false,
        }
    }
}

#[derive(Default)]
pub struct Settings {
    draft: RefCell<Option<Draft>>,
    /// The draft kept while the permissions step stands in for Settings
    /// (its Review…), for Settings to open with again.
    parked: RefCell<Option<Draft>>,
    first_run: RefCell<FirstRun>,
    /// Looking for devices again (Audio, the first run).
    looking: std::cell::Cell<bool>,
    /// Welcome's choice that is working ("obs", "file").
    welcome_busy: RefCell<String>,
    /// The Hotkeys page (src/areas/hotkeys.rs).
    pub hotkeys: super::hotkeys::HotkeysPage,
    /// Help › Report a Bug… (src/areas/report.rs).
    pub report: super::report::Report,
}

impl Area for Settings {
    fn connect(&self, app: &Rc<App>, window: &AppWindow) {
        let area = window.global::<SettingsArea>();
        area.set_tabs(model(TABS.iter().map(|(id, label, icon, group, _)| {
            NavTab {
                id: (*id).into(),
                label: (*label).into(),
                icon: (*icon).into(),
                group: (*group).into(),
            }
        })));
        area.set_services(model(SERVICES.iter().map(|(id, label, hint)| {
            ServiceChoice {
                id: (*id).into(),
                label: (*label).into(),
                hint: (*hint).into(),
            }
        })));
        // What makes the files (shown after the codec: Paper's Encoder row).
        // The engine machine's encoder (decision 22).
        let linux = crate::engine::os() == isocast_protocol::hello::EngineOs::Linux;
        let engine = if engine_is_mac() {
            "Apple VideoToolbox"
        } else if linux {
            "OpenH264 (software)"
        } else {
            ""
        };
        // A Linux engine records H.264 only (its software encoder).
        area.set_codecs(model(
            CODECS
                .iter()
                .filter(|(value, _)| !linux || *value == "h264")
                .map(|(value, label)| kit::option(value, label, engine)),
        ));
        area.set_pan_laws(model(
            PAN_LAWS
                .iter()
                .map(|(value, label)| kit::option(value, label, "")),
        ));
        area.set_hotkeys(model(SHORTCUTS.iter().map(|(title, rows)| HotkeyGroup {
            title: (*title).into(),
            rows: model(rows.iter().map(|(keys, what)| {
                HotkeyRow {
                    what: (*what).into(),
                    keys: model(
                        keys.iter()
                            .map(|k| SharedString::from(crate::keys::ui_key_cap(k))),
                    ),
                }
            })),
        })));
        area.on_server_valid(|text| server_valid(&text));
        super::hotkeys::HotkeysPage::connect(app, &area);
        super::report::Report::connect(app, &area);

        macro_rules! on {
            ($setter:ident, | $app:ident $(, $arg:ident)* | $body:expr) => {{
                let weak = Rc::downgrade(app);
                area.$setter(move |$($arg),*| {
                    if let Some($app) = weak.upgrade() {
                        $body;
                    }
                });
            }};
        }

        on!(on_ok, |app| app.settings.apply(&app, true));
        on!(on_apply, |app| app.settings.apply(&app, false));
        on!(on_cancel, |app| {
            app.settings.close(&app);
        });
        on!(on_tab_chosen, |app, tab| app.settings.tab_shown(&app, &tab));
        on!(on_revert_group, |app, group| app
            .settings
            .revert_group(&app, &group));
        on!(on_set_text, |app, field, value| app
            .settings
            .set_text(&app, &field, &value));
        on!(on_set_number, |app, field, value| app
            .settings
            .set_number(&app, &field, value));
        on!(on_set_flag, |app, field, value| app
            .settings
            .set_flag(&app, &field, value));
        on!(on_choose_quality, |app, stop| {
            if let Some((name, _)) = QUALITIES.get(stop.max(0) as usize) {
                app.settings.change(
                    &app,
                    json!({"recording": {
                        "program": {"encoder": {"quality": name}},
                        "iso": {"encoder": {"quality": name}},
                    }}),
                );
            }
        });
        on!(on_set_iso_source, |app, id, on| app
            .settings
            .set_iso_source(&app, &id, on));
        on!(on_set_iso_audio, |app, id, audio| app
            .settings
            .set_iso_audio(&app, &id, &audio));
        on!(on_set_mic, |app, id, device| {
            if let Some(d) = app.settings.draft.borrow_mut().as_mut() {
                d.mics.insert(id.into(), device.into());
            }
            app.settings.show(&app);
        });
        on!(on_choose_service, |app, id| app
            .settings
            .choose_service(&app, &id));
        on!(on_server_chosen, |app, server| {
            let server = server.trim().to_string();
            if server_valid(&server) {
                let service = app
                    .settings
                    .draft
                    .borrow_mut()
                    .as_mut()
                    .map(|d| std::mem::take(&mut d.needs_server));
                if let Some(service) = service {
                    let adding = app
                        .settings
                        .draft
                        .borrow()
                        .as_ref()
                        .is_some_and(|d| d.adding);
                    if adding {
                        app.settings.add_destination(&app, &service, Some(&server));
                    } else {
                        let patch = app
                            .settings
                            .stream_patch(json!({"service": service, "server": server}));
                        app.settings.change(&app, patch);
                    }
                }
            }
        });
        on!(on_server_back, |app| {
            if let Some(d) = app.settings.draft.borrow_mut().as_mut() {
                if d.needs_server.is_empty() {
                    d.adding = false;
                }
                d.needs_server.clear();
            }
            app.settings.show(&app);
        });
        on!(on_select_destination, |app, id| {
            if let Some(d) = app.settings.draft.borrow_mut().as_mut() {
                d.destination = id.into();
            }
            app.settings.show(&app);
        });
        on!(on_add_destination, |app| {
            if let Some(d) = app.settings.draft.borrow_mut().as_mut() {
                d.adding = true;
                d.needs_server.clear();
            }
            app.settings.show(&app);
        });
        on!(on_remove_destination, |app| app
            .settings
            .remove_destination(&app));
        on!(on_set_destination_enabled, |app, id, on| app
            .settings
            .set_destination_enabled(&app, &id, on));
        on!(on_save_key, |app, key| app.settings.save_key(&app, &key));
        on!(on_clear_key, |app| app.settings.clear_key(&app));
        on!(on_browse_folder, |app| {
            choose_folder(&app, |app, folder| {
                app.settings
                    .change(app, json!({"recording": {"folder": folder}}));
            });
        });
        on!(on_refresh_devices, |app| app.settings.look_again(&app));

        // Welcome and the failed state.
        on!(on_retry, |app| {
            app.reload();
            app.refresh_status(false);
        });
        on!(on_start_with_this_mac, |app| app
            .settings
            .start_first_run(&app));
        on!(on_open_setup_file, |app| app.settings.open_setup_file(&app));
        on!(on_import_obs, |app| app.settings.import_obs(&app));

        // The first run.
        on!(on_ob_close, |app| app.settings.close_first_run(&app));
        on!(on_ob_back, |app| {
            let back = {
                let run = app.settings.first_run.borrow();
                run.step > run.first_step()
            };
            if back {
                app.settings.first_run.borrow_mut().step -= 1;
                app.settings.show_first_run(&app);
            } else {
                app.settings.close_first_run(&app);
            }
        });
        on!(on_ob_next, |app| {
            {
                let mut run = app.settings.first_run.borrow_mut();
                run.step = (run.step + 1).min(2);
            }
            app.settings.show_first_run(&app);
        });
        on!(on_ob_create, |app| app.settings.create_setup(&app));
        on!(on_ob_set, |app, field, value| app
            .settings
            .first_run_set(&app, &field, &value));
        on!(on_ob_browse, |app| {
            choose_folder(&app, |app, folder| {
                app.settings.first_run.borrow_mut().folder = folder;
                app.settings.show_first_run(app);
            });
        });
    }

    fn update(&self, app: &Rc<App>, window: &AppWindow, changed: Changed) {
        let open = self.draft.borrow().is_some();
        if open
            && changed.any(Changed::SETUP | Changed::STATUS | Changed::STREAM | Changed::DEVICES)
        {
            self.show(app);
        }
        // Another client, a profile switch or our own change: the
        // bindings at the new revision.
        if open && changed.any(Changed::CONFIRMED) {
            self.hotkeys.reload(app);
        }
        let ui = window.global::<Ui>();
        if ui.get_onboarding() && changed.any(Changed::DEVICES | Changed::SETUP) {
            self.show_first_run(app);
        }
    }

    fn menu(&self, app: &Rc<App>, id: &str) -> bool {
        match id {
            "settings" => app.open_dialog("settings", ""),
            "hotkeys" => app.open_dialog("settings", "hotkeys"),
            "report_bug" => app.open_dialog("bug-report", ""),
            "check_updates" => crate::update::check_now(),
            "machine_calibrate" | "machine_cancel" => {
                let method = if id == "machine_calibrate" {
                    Method::MachineCalibrate
                } else {
                    Method::MachineCancel
                };
                app.call(method, |app, result| {
                    if let Err(error) = result {
                        app.toast(ToastKind::Error, "Machine calibration", &error.message);
                    }
                });
            }
            "show_recordings" => {
                let folder = app
                    .session
                    .borrow()
                    .setup
                    .as_ref()
                    .map(|s| s.recording.folder.clone());
                if let Some(folder) = folder {
                    platform::open_folder(&folder);
                }
            }
            "start_over" => self.start_first_run(app),
            _ => return false,
        }
        true
    }

    fn dialog_opened(&self, app: &Rc<App>, kind: &str, arg: &str) {
        if kind != "settings" {
            // Another dialog replaced Settings: its draft goes.
            *self.draft.borrow_mut() = None;
            self.hotkeys.closed(app);
            if kind == "bug-report" {
                self.report.opened(app);
            }
            return;
        }
        let Some(window) = app.window() else { return };
        let area = window.global::<SettingsArea>();
        // "stream:Add your stream key first." opens Stream saying why (or
        // the notice set just before, as Start Streaming does).
        let (tab, notice) = arg.split_once(':').unwrap_or((arg, ""));
        // "stream-vertical:…": Stream showing the vertical canvas's (#96);
        // "stream@ID", "video@ID": Stream showing extra canvas ID's, Video
        // with its row open (#120).
        let (tab, canvas) = match tab.split_once('@') {
            Some((tab, id)) => (tab, CanvasId::parse(id).unwrap_or(CanvasId::Main)),
            None if tab == "stream-vertical" => ("stream", CanvasId::Vertical),
            None => (tab, CanvasId::Main),
        };
        let notice = match (notice, tab) {
            ("", "stream") => area.get_notice().to_string(),
            (notice, _) => notice.to_string(),
        };
        let tab = if TABS.iter().any(|(id, ..)| *id == tab) {
            tab
        } else {
            TABS[0].0
        };
        let parked = self.parked.borrow_mut().take();
        if let Some(parked) = parked {
            // Back from the permissions step: its edits as they were.
            *self.draft.borrow_mut() = Some(parked);
        } else {
            let prefs = Prefs::read(&window.global::<Ui>());
            let base = app.session.borrow().setup_json().map(pick);
            *self.draft.borrow_mut() = base.map(|base| Draft {
                draft: base.clone(),
                base,
                prefs_base: prefs.clone(),
                prefs,
                notice,
                canvas,
                ..Draft::default()
            });
        }
        if !canvas.is_main()
            && let Some(d) = self.draft.borrow_mut().as_mut()
        {
            if tab == "video" {
                d.edit_canvas = Some(canvas);
                d.canvas_closed = false;
            } else {
                d.canvas = canvas;
            }
        }
        area.set_tab(tab.into());
        area.set_quality_advanced(false);
        self.tab_shown(app, tab);
        self.show(app);
    }

    fn state(
        &self,
        app: &Rc<App>,
        window: &AppWindow,
        name: &str,
        arg: &str,
    ) -> Option<anyhow::Result<()>> {
        match name {
            "about" => app.open_dialog("about", ""),
            // Help › Report a Bug… (a preview: nothing is written).
            "bug-report" => app.open_dialog("bug-report", ""),
            // ...with what happened, then written (a real zip, beside an
            // isolated engine's logs).
            "bug-report-describe" => {
                window
                    .global::<SettingsArea>()
                    .set_report_describe(arg.into());
                self.report.refresh(app);
            }
            "bug-report-create" => self.report.write(app),
            // ...or sent, as Send Report does (to ISOCAST_REPORT_ENDPOINT).
            "bug-report-send" => self.report.send(app),
            "settings" => app.open_dialog("settings", arg),
            // Settings → Stream as Start Streaming opens it without a key.
            "notice" => app.open_dialog("settings", &format!("stream:{arg}")),
            // Settings' controls, by the field they change (as clicking
            // and typing in them do).
            "set-text" | "set-number" | "set-flag" => {
                let Some((field, value)) = arg.split_once('=') else {
                    return Some(Err(anyhow::anyhow!("{name}:FIELD=VALUE")));
                };
                match name {
                    "set-text" => self.set_text(app, field, value),
                    "set-number" => match value.parse() {
                        Ok(number) => self.set_number(app, field, number),
                        Err(_) => return Some(Err(anyhow::anyhow!("{value:?} isn't a number"))),
                    },
                    _ => self.set_flag(app, field, value == "true"),
                }
            }
            "choose-service" => self.choose_service(app, arg),
            // Stream's destination `arg` open, or the services to add one.
            "destination" | "add-destination" => {
                if let Some(d) = self.draft.borrow_mut().as_mut() {
                    if name == "destination" {
                        d.destination = arg.to_string();
                    } else {
                        d.adding = true;
                    }
                }
                self.show(app);
            }
            // Settings → Hotkeys with sample bindings (no profile changes),
            // and a row waiting for its keys.
            "hotkeys-sample" => {
                app.open_dialog("settings", "hotkeys");
                app.flush();
                self.hotkeys.sample(app);
            }
            "hotkey-record" => self.hotkeys.sample_record(app, arg.parse().unwrap_or(0)),
            "iso-source" => self.set_iso_source(app, arg, true),
            // Output's quality numbers, under Advanced.
            "advanced-quality" => {
                app.open_dialog("settings", "output");
                window.global::<SettingsArea>().set_quality_advanced(true);
            }
            // The link to isocastd as the window shows it while it isn't
            // connected (they pass in moments otherwise).
            "connecting" | "reconnecting" | "failed" => {
                let link = match name {
                    "connecting" => Link::Connecting,
                    "reconnecting" => Link::Disconnected,
                    _ => Link::Failed,
                };
                let why = if arg.is_empty() && link == Link::Failed {
                    wording::select_linux(
                        "connect /tmp/isocast.sock: No such file or directory (os error 2)",
                        "connect to isocastd.exe: the recording engine is not available",
                        "connect to isocastd: the recording engine is not available",
                    )
                } else {
                    arg
                };
                app.session.borrow_mut().loaded = false;
                app.apply(Update::Link(link, why.into()));
            }
            // What Welcome's choices do once a file is chosen (the open
            // panel can't be driven offscreen).
            "open-setup" => self.open_setup_path(app, arg),
            "import-obs" => self.import_obs_path(app, arg),
            // The first run's buttons.
            "first-run-next" => {
                let next = (self.first_run.borrow().step + 1).min(2);
                self.first_run.borrow_mut().step = next;
                self.show_first_run(app);
            }
            "first-run-set" => match arg.split_once('=') {
                Some((field, value)) => self.first_run_set(app, field, value),
                None => return Some(Err(anyhow::anyhow!("first-run-set:FIELD=VALUE"))),
            },
            "first-run-create" => self.create_setup(app),
            "first-run" => {
                self.start_first_run(app);
                let step: i32 = arg.parse().unwrap_or(1);
                let mut run = self.first_run.borrow_mut();
                run.step = (step - 1).clamp(run.first_step(), 2);
                drop(run);
                self.show_first_run(app);
            }
            _ => return None,
        }
        Some(Ok(()))
    }
}

impl Settings {
    // ---------- The Settings dialog ----------

    /// Keep the open draft, unsaved edits and all, for the next time
    /// Settings opens (the permissions step stands in for it a while).
    pub fn park(&self) {
        *self.parked.borrow_mut() = self.draft.borrow_mut().take();
    }

    /// Close Settings, dropping its draft and why it opened.
    fn close(&self, app: &Rc<App>) {
        *self.draft.borrow_mut() = None;
        self.hotkeys.closed(app);
        if let Some(window) = app.window() {
            window
                .global::<SettingsArea>()
                .set_notice(SharedString::new());
        }
        app.close_dialog();
    }

    /// A page came up: Audio asks for the devices again.
    fn tab_shown(&self, app: &Rc<App>, tab: &str) {
        if let (Some(window), Some((_, title, icon, _, about))) =
            (app.window(), TABS.iter().find(|(id, ..)| *id == tab))
        {
            let area = window.global::<SettingsArea>();
            area.set_page_title((*title).into());
            area.set_page_about((*about).into());
            area.set_page_icon((*icon).into());
        }
        if tab == "audio" {
            app.load_devices(true);
        }
        if tab == "hotkeys" {
            self.hotkeys.load(app);
        } else {
            self.hotkeys.closed(app);
        }
        if tab == "remote" {
            app.remote.page_shown(app);
        }
    }

    /// Change the draft by a merge patch.
    fn change(&self, app: &Rc<App>, patch: Value) {
        if let Some(d) = self.draft.borrow_mut().as_mut() {
            d.draft = merge_patch(&d.draft, &patch);
        }
        self.show(app);
    }

    fn set_prefs(&self, app: &Rc<App>, change: impl FnOnce(&mut Prefs)) {
        if let Some(d) = self.draft.borrow_mut().as_mut() {
            change(&mut d.prefs);
        }
        self.show(app);
    }

    fn set_text(&self, app: &Rc<App>, field: &str, value: &str) {
        let trimmed = value.trim();
        let patch = match field {
            "name" if !trimmed.is_empty() => json!({"name": trimmed}),
            "preview_rate" => {
                let rate = value.to_string();
                return self.set_prefs(app, |p| p.preview_rate = rate);
            }
            "service" => {
                let open = self.open_destination();
                let same = open.get("service").is_some_and(|s| s == value);
                let server = if same {
                    open.get("server").cloned().unwrap_or(Value::Null)
                } else {
                    Value::Null
                };
                return self.change_destination(app, json!({"service": value, "server": server}));
            }
            "server" => {
                let server = if trimmed.is_empty() {
                    Value::Null
                } else {
                    json!(trimmed)
                };
                return self.change_destination(app, json!({"server": server}));
            }
            "destination_bitrate" => {
                let kbps = if value == "own" {
                    json!(
                        self.stream_value(&["video_bitrate_kbps"])
                            .and_then(|v| v.as_u64())
                            .unwrap_or(6000)
                    )
                } else {
                    Value::Null
                };
                return self.change_destination(app, json!({"video_bitrate_kbps": kbps}));
            }
            "destination_size" if value == "custom" => {
                return self.change_destination(app, json!({"width": 1280, "height": 720}));
            }
            "destination_size" => {
                return self.change_destination(app, json!({"width": null, "height": null}));
            }
            "canvas_pick" => {
                if let Some(d) = self.draft.borrow_mut().as_mut() {
                    // "" closes the open one.
                    d.edit_canvas = CanvasId::parse(value).ok();
                    d.canvas_closed = d.edit_canvas.is_none();
                }
                return self.show(app);
            }
            "canvas_add" => {
                let Some((patch, canvas)) = self.add_canvas(value) else {
                    return;
                };
                if let Some(d) = self.draft.borrow_mut().as_mut() {
                    d.edit_canvas = Some(canvas);
                    d.canvas_closed = false;
                }
                patch
            }
            "canvas_remove" => {
                let Ok(canvas) = CanvasId::parse(value) else {
                    return;
                };
                if canvas.is_vertical() {
                    return self.set_flag(app, "vertical", false);
                }
                // Never drops scenes' own layouts for it (the setup would
                // refuse that): say which.
                let own: Vec<String> = app
                    .session
                    .borrow()
                    .setup
                    .as_ref()
                    .map(|setup| {
                        setup
                            .scenes
                            .iter()
                            .filter(|scene| scene.arrangement(canvas).is_some())
                            .map(|scene| format!("“{}”", crate::model::scene_name(scene)))
                            .collect()
                    })
                    .unwrap_or_default();
                if !own.is_empty() {
                    if let Some(d) = self.draft.borrow_mut().as_mut() {
                        d.refused = format!(
                            "The canvas stays: {} {} a layout of {} own for it. Remove it with \
                             `isocast canvas remove {canvas}`, which removes those too.",
                            own.join(", "),
                            if own.len() == 1 { "has" } else { "have" },
                            if own.len() == 1 { "its" } else { "their" },
                        );
                    }
                    return self.show(app);
                }
                if let Some(d) = self.draft.borrow_mut().as_mut() {
                    d.refused.clear();
                    d.edit_canvas = None;
                }
                json!({"canvases": [{"id": canvas.as_str(), "remove": true}]})
            }
            "canvas_fps" => {
                let fps = value.parse::<u32>().ok().map_or(Value::Null, Value::from);
                self.edit_patch(json!({"fps": fps}))
            }
            "stream_canvas" => {
                let canvas = CanvasId::parse(value).unwrap_or(CanvasId::Main);
                if let Some(d) = self.draft.borrow_mut().as_mut()
                    && d.canvas != canvas
                {
                    d.canvas = canvas;
                    d.destination = MAIN_DESTINATION.into();
                    d.adding = false;
                    d.needs_server.clear();
                }
                return self.show(app);
            }
            "rate_control" => self.stream_patch(json!({"rate_control": value})),
            "audio_bitrate" => match value.parse::<u32>() {
                Ok(kbps) => self.stream_patch(json!({"audio_bitrate_kbps": kbps})),
                Err(_) => return,
            },
            "stream_size" if value == "canvas" => {
                self.stream_patch(json!({"width": null, "height": null}))
            }
            "stream_size" => {
                // The vertical canvas's own shape, smaller.
                let (width, height) = if self.stream_canvas() == CanvasId::Vertical {
                    (720, 1280)
                } else {
                    (1280, 720)
                };
                self.stream_patch(json!({"width": width, "height": height}))
            }
            "folder" if !trimmed.is_empty() => json!({"recording": {"folder": trimmed}}),
            "program_codec" | "iso_codec" => {
                let which = field.trim_end_matches("_codec");
                let mut encoder = json!({"codec": value});
                // H.264 is 8-bit only; an ISO depth left out stays out
                // (it is 8), and a set 10 becomes 8.
                let depth = self.draft_value(&["recording", which, "encoder", "bit_depth"]);
                if value == "h264" && (which == "program" || depth.is_some_and(|d| d != 8)) {
                    encoder["bit_depth"] = json!(8);
                }
                // A codec MP4 can't hold (ProRes) takes MOV with it, as the
                // engine's encoder options list says (MP4 greys out).
                if !mp4_ok(value) {
                    encoder["container"] = json!("mov");
                }
                json!({"recording": {which: {"encoder": encoder}}})
            }
            "program_container" | "iso_container" => {
                let which = field.trim_end_matches("_container");
                let codec = self
                    .draft_value(&["recording", which, "encoder", "codec"])
                    .and_then(|v| v.as_str().map(str::to_string));
                if value == "mp4" && codec.as_deref().is_some_and(|c| !mp4_ok(c)) {
                    // Put the control back on MOV.
                    return self.show(app);
                }
                json!({"recording": {which: {"encoder": {"container": value}}}})
            }
            // Colour Depth's greyed 10 (arrow keys reach it): not for
            // H.264 or an engine that captures screens in 8 bits.
            "iso_depth"
                if value == "10"
                    && (!engine_is_mac()
                        || self.draft_value(&["recording", "iso", "encoder", "codec"])
                            == Some(json!("h264"))) =>
            {
                return self.show(app);
            }
            "program_depth" | "iso_depth" => {
                let which = field.trim_end_matches("_depth");
                let depth: u32 = value.parse().unwrap_or(8);
                json!({"recording": {which: {"encoder": {"bit_depth": depth}}}})
            }
            "monitoring_device" => json!({"audio": {"monitoring_device": value}}),
            "pan_law" => json!({"audio": {"pan_law": value}}),
            "canvas_size" => match value.split_once('x') {
                Some((w, h)) => match (w.parse::<u32>(), h.parse::<u32>()) {
                    (Ok(w), Ok(h)) => json!({"canvas": {"width": w, "height": h}}),
                    _ => return self.show(app),
                },
                // "Custom": the numbers beside it say what.
                None => return self.show(app),
            },
            "fps" => match value.parse::<u32>() {
                Ok(fps) => json!({"canvas": {"fps": fps}}),
                Err(_) => return,
            },
            _ => return self.show(app),
        };
        self.change(app, patch);
    }

    fn set_number(&self, app: &Rc<App>, field: &str, value: f32) {
        let whole = value.round() as i64;
        let patch = match field {
            "snap_distance" => {
                return self.set_prefs(app, |p| p.distance = whole.clamp(1, 40) as i32);
            }
            "video_bitrate" => self.stream_patch(json!({"video_bitrate_kbps": whole})),
            "keyframe_interval" => self.stream_patch(json!({"keyframe_interval_seconds": whole})),
            "destination_bitrate" => {
                return self.change_destination(app, json!({"video_bitrate_kbps": whole}));
            }
            "destination_width" => return self.change_destination(app, json!({"width": whole})),
            "destination_height" => {
                return self.change_destination(app, json!({"height": whole}));
            }
            "stream_width" => self.stream_patch(json!({"width": whole})),
            "stream_height" => self.stream_patch(json!({"height": whole})),
            "program_quality" => json!({"recording": {"program": {"encoder": {"quality": whole}}}}),
            "iso_quality" => json!({"recording": {"iso": {"encoder": {"quality": whole}}}}),
            "keep_free" => json!({"recording": {"keep_free_gb": value as f64}}),
            "replay_seconds" => json!({"recording": {"replay": {"seconds": whole.clamp(
                i64::from(isocast_protocol::replay::MIN_SECONDS),
                i64::from(isocast_protocol::replay::MAX_SECONDS),
            )}}}),
            "replay_memory" => {
                json!({"recording": {"replay": {"max_memory_mb": whole.clamp(16, 8192)}}})
            }
            "canvas_width" => json!({"canvas": {"width": whole}}),
            "canvas_height" => json!({"canvas": {"height": whole}}),
            "vertical_width" => self.edit_patch(json!({"width": whole})),
            "vertical_height" => self.edit_patch(json!({"height": whole})),
            "reconnect_delay" => json!({"stream": {"reconnect": {"delay_seconds": whole}}}),
            "reconnect_retries" => json!({"stream": {"reconnect": {"max_retries": whole}}}),
            _ => return,
        };
        self.change(app, patch);
    }

    fn set_flag(&self, app: &Rc<App>, field: &str, on: bool) {
        let patch = match field {
            "snapping" => return self.set_prefs(app, |p| p.snapping = on),
            "auto_updates" => return self.set_prefs(app, |p| p.auto_updates = on),
            // Turn off streaming (the shown canvas's).
            "stream" if !on => self.stream_patch(Value::Null),
            "program" => json!({"recording": {"program": {"enabled": on}}}),
            "iso" => json!({"recording": {"iso": {"enabled": on}}}),
            "iso_filtered" => json!({"recording": {"iso": {"filtered": on}}}),
            "reconnect" => json!({"stream": {"reconnect": {"enabled": on}}}),
            "developer" => json!({"developer": {"enabled": on}}),
            // Automatic quality (T.2), and what it must keep ("pin:PATH").
            "governor" => return self.change_governor(app, |g| g.enabled = on),
            // The user's own choice to let it skip ISO pictures (off by default).
            "iso_thinning" => return self.change_governor(app, |g| g.allow_iso_thinning = on),
            _ if field.starts_with("pin:") => {
                let pin = field["pin:".len()..].to_string();
                return self.change_governor(app, |g| {
                    let pins = &mut g.pins;
                    pins.retain(|p| *p != pin);
                    if on {
                        pins.push(pin);
                    }
                });
            }
            // The vertical canvas (#86): back as saved, else 1080×1920
            // following the main canvas's scenes; or none.
            "vertical" if on => {
                let saved = self
                    .draft
                    .borrow()
                    .as_ref()
                    .map(|d| d.base["vertical"].clone())
                    .filter(Value::is_object);
                json!({"vertical": saved.unwrap_or_else(|| json!({
                    "width": 1080, "height": 1920, "follow_main": true,
                }))})
            }
            // Never drops scenes' own vertical layouts with it (the engine
            // refuses that too): say which, and how to put them back on
            // the main layout first.
            "vertical" => {
                let own: Vec<String> = app
                    .session
                    .borrow()
                    .setup
                    .as_ref()
                    .map(|setup| {
                        setup
                            .scenes
                            .iter()
                            .filter(|scene| scene.vertical.is_some())
                            .map(|scene| format!("“{}”", crate::model::scene_name(scene)))
                            .collect()
                    })
                    .unwrap_or_default();
                if !own.is_empty() {
                    if let Some(d) = self.draft.borrow_mut().as_mut() {
                        d.refused = format!(
                            "The vertical canvas stays on: {} {} a vertical layout of {} own. \
                             To keep nothing you’d lose, put each back on the main layout \
                             first (View › Canvases › Use Main Layout for This Scene, \
                             while editing the vertical canvas).",
                            own.join(", "),
                            if own.len() == 1 { "has" } else { "have" },
                            if own.len() == 1 { "its" } else { "their" },
                        );
                    }
                    return self.show(app);
                }
                if let Some(d) = self.draft.borrow_mut().as_mut() {
                    d.refused.clear();
                }
                json!({"vertical": null})
            }
            "vertical_follow" => self.edit_patch(json!({"follow_main": on})),
            // Kept when off, with its file name and encoder.
            "vertical_record" => {
                let canvas = self.edit_canvas();
                let filename = isocast_protocol::canvas_filename(canvas);
                let has = self
                    .draft
                    .borrow()
                    .as_ref()
                    .is_some_and(|d| draft_canvas(&d.draft, canvas)["recording"].is_object());
                if has || canvas.is_vertical() {
                    self.edit_patch(json!({"recording": {"enabled": on}}))
                } else {
                    self.edit_patch(json!({"recording": {"enabled": on, "filename": filename}}))
                }
            }
            // Back on: the hours it had (a custom `auto_off_hours` too).
            "developer_auto_off" => json!({"developer": {"auto_off_hours": if on {
                self.auto_off_hours()
            } else {
                0
            }}}),
            _ => return,
        };
        self.change(app, patch);
    }

    /// Change the draft's automatic quality (`governor`, T.2): whether it
    /// is on, whether the user allows ISO frame thinning, and the field
    /// paths it must never change (its pins). Back at its default (on, no
    /// ISO thinning, nothing kept) while the setup has none, it is left
    /// out again, as the setup omits it, so Apply has nothing to send.
    fn change_governor(&self, app: &Rc<App>, change: impl FnOnce(&mut GovernorSettings)) {
        if let Some(d) = self.draft.borrow_mut().as_mut() {
            let mut governor = governor_settings(&d.draft);
            change(&mut governor);
            let default = governor.is_default() && d.base["governor"].is_null();
            if let Some(map) = d.draft.as_object_mut() {
                if default {
                    map.remove("governor");
                } else {
                    // As the setup serializes it, so an untouched draft
                    // matches its base.
                    let value = serde_json::to_value(&governor).unwrap_or_default();
                    map.insert("governor".into(), value);
                }
            }
        }
        self.show(app);
    }

    /// Developer mode's hours until it turns itself off when the switch is
    /// on: the draft's, else the applied setting's, else the default (#55).
    fn auto_off_hours(&self) -> u64 {
        let guard = self.draft.borrow();
        let d = guard.as_ref();
        [d.map(|d| &d.draft), d.map(|d| &d.base)]
            .into_iter()
            .flatten()
            .filter_map(|v| v["developer"]["auto_off_hours"].as_u64())
            .find(|&hours| hours > 0)
            .unwrap_or(isocast_protocol::developer::DEFAULT_AUTO_OFF_HOURS.into())
    }

    /// The live setup with the draft's ISO settings, for the ISO audio
    /// menus.
    fn drafted(&self, app: &Rc<App>) -> Option<Setup> {
        let iso = self.draft_value(&["recording", "iso"])?;
        draft_setup(app.session.borrow().setup.as_deref(), &iso)
    }

    /// Video source `source`'s ISO file: back to the other files' audio
    /// (`default`), or with audio source `audio` added or taken out.
    fn set_iso_audio(&self, app: &Rc<App>, source: &str, audio: &str) {
        if let Some(setup) = self.drafted(app) {
            let list = super::sources::items::toggled_iso_audio(&setup, source, audio);
            self.change(
                app,
                json!({"recording": {"iso": {"source_audio": {source: list}}}}),
            );
        }
    }

    fn set_iso_source(&self, app: &Rc<App>, id: &str, on: bool) {
        let mut sources: Vec<Value> = self
            .draft_value(&["recording", "iso", "sources"])
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        sources.retain(|s| s != id);
        if on {
            sources.push(json!(id));
        }
        self.change(app, json!({"recording": {"iso": {"sources": sources}}}));
    }

    fn choose_service(&self, app: &Rc<App>, id: &str) {
        let adding = self.draft.borrow().as_ref().is_some_and(|d| d.adding);
        if id == "custom" || id == "kick" {
            if let Some(d) = self.draft.borrow_mut().as_mut() {
                d.needs_server = id.to_string();
            }
            self.show(app);
        } else if adding {
            self.add_destination(app, id, None);
        } else {
            let patch = self.stream_patch(json!({"service": id}));
            self.change(app, patch);
        }
    }

    /// The canvas whose streams Stream shows: the vertical one only while
    /// the draft has a vertical canvas.
    /// The canvas Video › Canvases edits: the one picked, if the draft has
    /// it, else the first extra canvas (main if there is none).
    fn edit_canvas(&self) -> CanvasId {
        let guard = self.draft.borrow();
        let Some(d) = guard.as_ref() else {
            return CanvasId::Main;
        };
        d.edit_canvas
            .filter(|c| draft_canvas(&d.draft, *c).is_object())
            .or_else(|| {
                draft_canvases(&d.draft)
                    .first()
                    .and_then(|(id, _)| CanvasId::parse(id).ok())
            })
            .unwrap_or(CanvasId::Main)
    }

    /// A patch making `change` to the edited canvas.
    fn edit_patch(&self, change: Value) -> Value {
        let canvas = self.edit_canvas();
        if canvas.is_main() {
            return json!({});
        }
        canvas_patch(&Value::Null, canvas, change)
    }

    /// The patch adding a canvas of preset `preset` (`WxH`), and its id:
    /// the vertical canvas for the first upright one, else a
    /// `[[canvases]]` entry named after its shape.
    fn add_canvas(&self, preset: &str) -> Option<(Value, CanvasId)> {
        let (w, h) = preset.split_once('x')?;
        let (width, height): (u32, u32) = (w.parse().ok()?, h.parse().ok()?);
        let guard = self.draft.borrow();
        let draft = &guard.as_ref()?.draft;
        if height > width && !draft["vertical"].is_object() {
            let saved = guard.as_ref()?.base["vertical"].clone();
            let mut vertical = if saved.is_object() {
                saved
            } else {
                json!({"follow_main": true})
            };
            vertical["width"] = width.into();
            vertical["height"] = height.into();
            return Some((json!({"vertical": vertical}), CanvasId::Vertical));
        }
        let (base, name) = match (width, height) {
            (w, h) if w == h => ("square", "Square"),
            (w, h) if h > w && h * 4 == w * 5 => ("portrait", "Portrait 4:5"),
            (w, h) if h > w => ("shorts", "Vertical"),
            _ => ("landscape", "Landscape"),
        };
        let taken: Vec<String> = draft_canvases(draft)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let id = (1..)
            .map(|n| {
                if n == 1 {
                    base.to_string()
                } else {
                    format!("{base}-{n}")
                }
            })
            .find(|id| !taken.contains(id))?;
        let canvas = CanvasId::parse(&id).ok()?;
        Some((
            json!({"canvases": [{"id": id, "name": name, "width": width, "height": height}]}),
            canvas,
        ))
    }

    fn stream_canvas(&self) -> CanvasId {
        let guard = self.draft.borrow();
        match guard.as_ref() {
            Some(d) if draft_canvas(&d.draft, d.canvas).is_object() => d.canvas,
            _ => CanvasId::Main,
        }
    }

    /// A merge patch setting `fields` in the shown canvas's stream
    /// settings: `[stream]` or `[vertical.stream]`.
    fn stream_patch(&self, fields: Value) -> Value {
        let canvas = self.stream_canvas();
        if canvas.is_main() {
            return json!({ "stream": fields });
        }
        let guard = self.draft.borrow();
        let draft = guard.as_ref().map_or(&Value::Null, |d| &d.draft);
        canvas_patch(draft, canvas, json!({ "stream": fields }))
    }

    /// The shown canvas's stream settings in the draft, at `path`.
    fn stream_value(&self, path: &[&str]) -> Option<Value> {
        let canvas = self.stream_canvas();
        let guard = self.draft.borrow();
        let draft = &guard.as_ref()?.draft;
        let mut at = if canvas.is_main() {
            &draft["stream"]
        } else {
            &draft_canvas(draft, canvas)["stream"]
        };
        for key in path {
            at = at.get(key)?;
        }
        (!at.is_null()).then(|| at.clone())
    }

    /// The open destination, as the draft has it: `[stream]` itself for
    /// the main one, else its entry in `stream.destinations`.
    fn open_destination(&self) -> Map<String, Value> {
        let id = self.open_id();
        let value = if id == MAIN_DESTINATION {
            self.stream_value(&[])
        } else {
            self.stream_value(&["destinations"]).and_then(|list| {
                list.as_array()?
                    .iter()
                    .find(|d| d["id"] == id.as_str())
                    .cloned()
            })
        };
        value
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default()
    }

    /// The open destination's id, if the draft still has it (else main).
    fn open_id(&self) -> String {
        let destinations = self.stream_value(&["destinations"]);
        let guard = self.draft.borrow();
        let Some(d) = guard.as_ref() else {
            return MAIN_DESTINATION.into();
        };
        let known = destinations
            .as_ref()
            .and_then(Value::as_array)
            .is_some_and(|list| list.iter().any(|x| x["id"] == d.destination.as_str()));
        if known {
            d.destination.clone()
        } else {
            MAIN_DESTINATION.into()
        }
    }

    /// Change the open destination's `fields` (a null removes one).
    fn change_destination(&self, app: &Rc<App>, fields: Value) {
        let id = self.open_id();
        if id == MAIN_DESTINATION {
            let patch = self.stream_patch(fields);
            return self.change(app, patch);
        }
        self.change_extra(app, &id, fields);
    }

    /// Change destination `id` (not main) in `stream.destinations`: the
    /// list goes whole, as a merge patch replaces arrays.
    fn change_extra(&self, app: &Rc<App>, id: &str, fields: Value) {
        let mut list = self
            .stream_value(&["destinations"])
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        if let Some(entry) = list
            .iter_mut()
            .find(|d| d["id"] == id)
            .and_then(Value::as_object_mut)
            && let Some(fields) = fields.as_object()
        {
            for (key, value) in fields {
                if value.is_null() {
                    entry.remove(key);
                } else {
                    entry.insert(key.clone(), value.clone());
                }
            }
        }
        let patch = self.stream_patch(json!({ "destinations": list }));
        self.change(app, patch);
    }

    /// Add a destination of `service` to the draft and open it.
    fn add_destination(&self, app: &Rc<App>, service: &str, server: Option<&str>) {
        let mut list = self
            .stream_value(&["destinations"])
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        let taken = |id: &str| id == MAIN_DESTINATION || list.iter().any(|d| d["id"] == id);
        let id = (1..)
            .map(|n| {
                if n == 1 {
                    service.to_string()
                } else {
                    format!("{service}-{n}")
                }
            })
            .find(|id| !taken(id))
            .expect("a free id");
        let mut entry = json!({"id": id, "service": service});
        if let Some(server) = server {
            entry["server"] = json!(server);
        }
        list.push(entry);
        if let Some(d) = self.draft.borrow_mut().as_mut() {
            d.adding = false;
            d.needs_server.clear();
            d.destination = id;
        }
        let patch = self.stream_patch(json!({ "destinations": list }));
        self.change(app, patch);
    }

    fn remove_destination(&self, app: &Rc<App>) {
        let id = self.open_id();
        if id == MAIN_DESTINATION {
            return;
        }
        let mut list = self
            .stream_value(&["destinations"])
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        list.retain(|d| d["id"] != id.as_str());
        if let Some(d) = self.draft.borrow_mut().as_mut() {
            d.destination = MAIN_DESTINATION.into();
        }
        let patch = self.stream_patch(json!({ "destinations": list }));
        self.change(app, patch);
    }

    fn set_destination_enabled(&self, app: &Rc<App>, id: &str, on: bool) {
        if id == MAIN_DESTINATION {
            let patch = self.stream_patch(json!({ "enabled": on }));
            self.change(app, patch);
        } else {
            // On is the default: left out of the setup.
            self.change_extra(
                app,
                id,
                json!({"enabled": if on { Value::Null } else { json!(false) }}),
            );
        }
    }

    /// The stream key goes to the Keychain at once, for the draft's
    /// service (and a custom server).
    fn save_key(&self, app: &Rc<App>, key: &str) {
        let key = key.trim();
        if key.is_empty() {
            return;
        }
        let Some(target) = self.key_target() else {
            return;
        };
        let params = StreamKeyParams {
            key: key.to_string(),
            target,
        };
        self.set_key_busy(app, true);
        app.call(Method::StreamKeySet(Box::new(params)), |app, result| {
            app.settings.set_key_busy(app, false);
            match result {
                Ok(_) => {
                    app.toast(
                        ToastKind::Success,
                        "Stream key saved",
                        format!(
                            "It’s in your {}; Isocast never shows it again.",
                            crate::engine::words().credential_store
                        ),
                    );
                    app.refresh_status(true);
                }
                Err(error) => app.failed("Couldn’t save the stream key", &error),
            }
        });
    }

    fn clear_key(&self, app: &Rc<App>) {
        let Some(target) = self.key_target() else {
            return;
        };
        app.call(
            Method::StreamKeyClear(Box::new(target)),
            |app, result| match result {
                Ok(_) => {
                    app.toast(
                        ToastKind::Info,
                        &format!(
                            "Stream key removed from the {}",
                            crate::engine::words().credential_store
                        ),
                        "",
                    );
                    app.refresh_status(true);
                }
                Err(error) => app.failed("Couldn’t remove the stream key", &error),
            },
        );
    }

    /// Which key: the open destination's service, and its server for a
    /// custom one; an added destination's own (its id with them).
    fn key_target(&self) -> Option<StreamKeyTarget> {
        let id = self.open_id();
        let open = self.open_destination();
        let service: StreamService = serde_json::from_value(open.get("service")?.clone()).ok()?;
        let server = (service == StreamService::Custom)
            .then(|| open.get("server").cloned())
            .flatten()
            .and_then(|v| v.as_str().map(str::to_string))
            .filter(|s| !s.is_empty());
        Some(StreamKeyTarget {
            destination: (id != MAIN_DESTINATION).then_some(id),
            service: Some(service),
            server,
            canvas: self.stream_canvas(),
            revision: None,
        })
    }

    fn set_key_busy(&self, app: &Rc<App>, busy: bool) {
        // Parked meanwhile (Review…): the parked draft's, so its buttons
        // aren't left disabled on return (#58).
        if let Some(d) = self.draft.borrow_mut().as_mut() {
            d.key_busy = busy;
        } else if let Some(d) = self.parked.borrow_mut().as_mut() {
            d.key_busy = busy;
        }
        self.show(app);
    }

    /// Ask for the devices again (the refresh button), showing it turn
    /// until they come.
    fn look_again(&self, app: &Rc<App>) {
        self.looking.set(true);
        self.show(app);
        self.show_first_run(app);
        app.call_as::<Devices>(Method::Devices, |app, result| {
            app.settings.looking.set(false);
            match result {
                Ok(devices) => {
                    app.session.borrow_mut().devices = Some(devices);
                    app.changed(Changed::DEVICES);
                }
                Err(error) => app.failed(
                    crate::engine::pick(
                        "Couldn’t list this Mac’s devices",
                        "Couldn’t list this PC’s devices",
                    ),
                    &error,
                ),
            }
            app.settings.show(app);
        });
    }

    /// Apply: this viewer's settings now, and the setup's changes as one
    /// edit; OK (`close`) closes once it is applied.
    fn apply(&self, app: &Rc<App>, close: bool) {
        let Some(window) = app.window() else { return };
        let patch = {
            let mut guard = self.draft.borrow_mut();
            let Some(d) = guard.as_mut() else {
                drop(guard);
                return app.close_dialog();
            };
            if d.applying {
                return;
            }
            d.refused.clear();
            if d.prefs != d.prefs_base {
                let ui = window.global::<Ui>();
                ui.set_snap_enabled(d.prefs.snapping);
                ui.set_snap_distance(d.prefs.distance as f32);
                ui.set_preview_rate(d.prefs.preview_rate.as_str().into());
                if !app.mode.offscreen {
                    crate::prefs::save(&window);
                }
                if d.prefs.auto_updates != d.prefs_base.auto_updates {
                    crate::update::set_automatic(d.prefs.auto_updates);
                }
                d.prefs_base = d.prefs.clone();
            }
            let s = app.session.borrow();
            let patch = self.patch(d, &s);
            if patch.is_empty() {
                None
            } else {
                d.applying = true;
                Some(Value::Object(patch))
            }
        };
        let Some(patch) = patch else {
            if close {
                self.close(app);
            } else {
                self.show(app);
            }
            return;
        };
        self.show(app);
        app.session.borrow_mut().last_refusal = None;
        app.edit_then(patch, EditOptions::default(), move |app, ok| {
            let settings = &app.settings;
            let refusal = app.session.borrow_mut().last_refusal.take();
            // Parked meanwhile (Review… opened the permissions step): the
            // parked draft takes the answer, and that step stays open.
            let parked = settings.draft.borrow().is_none() && settings.parked.borrow().is_some();
            let mut draft = settings.draft.borrow_mut();
            let mut parked_draft = settings.parked.borrow_mut();
            if let Some(d) = draft.as_mut().or(parked_draft.as_mut()) {
                d.applying = false;
                if ok {
                    d.base = d.draft.clone();
                    d.mics.clear();
                } else {
                    // In the sheet: the toast sits behind it.
                    d.refused = refusal.unwrap_or_else(|| {
                        "isocastd refused the change; nothing was applied.".into()
                    });
                }
            }
            drop((draft, parked_draft));
            if ok {
                app.refresh_status(true);
            }
            if parked {
                return;
            }
            if ok && close {
                settings.close(app);
                return;
            }
            settings.show(app);
        });
    }

    /// What Apply sends: the draft's differences from the setup it was
    /// made from, and the microphones whose device changed.
    fn patch(&self, d: &Draft, s: &crate::session::Session) -> Map<String, Value> {
        let mut patch = object_patch(&d.base, &d.draft);
        let sources = s
            .setup_json()
            .and_then(|setup| setup.get("sources"))
            .and_then(Value::as_array);
        let changed: Vec<Value> = d
            .mics
            .iter()
            .filter(|(id, device)| {
                let now = sources
                    .and_then(|list| list.iter().find(|x| x["id"] == id.as_str()))
                    .and_then(|x| x.get("device"))
                    .and_then(Value::as_str);
                now != Some(device.as_str())
            })
            .map(|(id, device)| json!({"id": id, "device": device}))
            .collect();
        if !changed.is_empty() {
            patch.insert("sources".into(), Value::Array(changed));
        }
        patch
    }

    fn draft_value(&self, path: &[&str]) -> Option<Value> {
        let guard = self.draft.borrow();
        let mut at = &guard.as_ref()?.draft;
        for key in path {
            at = at.get(key)?;
        }
        (!at.is_null()).then(|| at.clone())
    }

    /// Show the draft.
    fn show(&self, app: &Rc<App>) {
        let Some(window) = app.window() else { return };
        let area = window.global::<SettingsArea>();
        area.set_looking(self.looking.get());
        let guard = self.draft.borrow();
        let Some(d) = guard.as_ref() else {
            area.set_has_draft(false);
            return;
        };
        let s = app.session.borrow();
        let draft = &d.draft;
        let text = |v: &Value| v.as_str().unwrap_or_default().to_string();
        let number = |v: &Value, default: f64| v.as_f64().unwrap_or(default) as f32;
        let patch = self.patch(d, &s);

        area.set_has_draft(true);
        area.set_dirty(!patch.is_empty() || d.prefs != d.prefs_base);
        area.set_applying(d.applying);

        // General
        let machine = s
            .status
            .as_ref()
            .and_then(|status| status.machine_profile.as_ref());
        area.set_machine_summary(
            machine
                .map_or("Not calibrated; using the per-chip capacity table", |m| {
                    m.summary.as_str()
                })
                .into(),
        );
        area.set_machine_calibrating(machine.is_some_and(|m| m.running));
        area.set_name(text(&draft["name"]).into());
        area.set_setup_file(
            s.status
                .as_ref()
                .and_then(|x| x.setup_file.as_ref())
                .map(|f| f.path.display().to_string())
                .unwrap_or_default()
                .into(),
        );
        area.set_snapping(d.prefs.snapping);
        area.set_snap_distance(d.prefs.distance as f32);
        area.set_preview_rate(d.prefs.preview_rate.as_str().into());
        area.set_updates_available(crate::update::available());
        area.set_automatic_updates_available(crate::update::automatic_available());
        area.set_auto_updates(d.prefs.auto_updates);

        // Stream: the destinations, and the open one's service, server
        // and key (#73), of the canvas chosen (#96).
        let has_vertical = draft["vertical"].is_object();
        let canvas = if draft_canvas(draft, d.canvas).is_object() {
            d.canvas
        } else {
            CanvasId::Main
        };
        // Every canvas besides the main one (#84), for the Streams of
        // choice.
        let extras = draft_canvases(draft);
        area.set_stream_canvas_choice(has_vertical || !extras.is_empty());
        area.set_stream_canvas(canvas.as_str().into());
        area.set_stream_canvas_options(slint::ModelRc::new(slint::VecModel::from(
            std::iter::once(("main".to_string(), "Main Canvas".to_string()))
                .chain(extras.iter().cloned())
                .map(|(id, label)| crate::Segment {
                    value: id.into(),
                    label: label.into(),
                    disabled: false,
                })
                .collect::<Vec<_>>(),
        )));
        let stream = if canvas.is_main() {
            &draft["stream"]
        } else {
            &draft_canvas(draft, canvas)["stream"]
        };
        let has_stream = stream.is_object();
        let canvas_streams: Vec<StreamStatus> = s
            .canvases
            .iter()
            .find(|c| c.id == canvas)
            .map(|c| c.streams.clone())
            .unwrap_or_default();
        let statuses = match canvas {
            CanvasId::Main => &s.streams,
            CanvasId::Vertical => &s.vertical_streams,
            _ => &canvas_streams,
        };
        // The canvas's stream as one: its main destination's status.
        let summary = match canvas {
            CanvasId::Main => s.stream.as_ref(),
            _ => statuses
                .iter()
                .find(|x| x.id == MAIN_DESTINATION)
                .or(statuses.first()),
        };
        let open_id = {
            let known = stream["destinations"]
                .as_array()
                .is_some_and(|list| list.iter().any(|x| x["id"] == d.destination.as_str()));
            if known {
                d.destination.clone()
            } else {
                MAIN_DESTINATION.to_string()
            }
        };
        let status_of = |id: &str| -> Option<&StreamStatus> {
            statuses
                .iter()
                .find(|x| x.id == id)
                .or_else(|| summary.filter(|x| x.id == id))
        };
        let mut rows = Vec::new();
        if has_stream {
            let mut entries = vec![(MAIN_DESTINATION.to_string(), stream.clone())];
            for extra in stream["destinations"].as_array().into_iter().flatten() {
                entries.push((text(&extra["id"]), extra.clone()));
            }
            let main_size = (stream["width"].clone(), stream["height"].clone());
            let main_kbps = stream["video_bitrate_kbps"].clone();
            for (id, entry) in &entries {
                let status = status_of(id);
                let service = text(&entry["service"]);
                let label = match (id.as_str(), service_label(&service)) {
                    (MAIN_DESTINATION, name) => name.to_string(),
                    (id, name) if id == service => name.to_string(),
                    (id, name) => format!("{id} · {name}"),
                };
                let own = id != MAIN_DESTINATION
                    && ((!entry["video_bitrate_kbps"].is_null()
                        && entry["video_bitrate_kbps"] != main_kbps)
                        || (!entry["width"].is_null()
                            && (entry["width"].clone(), entry["height"].clone()) != main_size));
                let same_service = status
                    .and_then(|x| serde_json::to_value(x.service).ok())
                    .is_some_and(|v| v == entry["service"]);
                let words = match status.map(|x| x.state) {
                    Some(StreamState::Live) => "Live",
                    Some(StreamState::Connecting) => "Connecting",
                    Some(StreamState::Reconnecting) => "Reconnecting",
                    Some(StreamState::Failed) => "Failed",
                    _ if !same_service => "Not saved yet",
                    _ if !status.is_some_and(|x| x.key_set) => "No key yet",
                    _ if own => "Own encoder",
                    _ => "Key saved",
                };
                rows.push(DestinationRow {
                    id: id.as_str().into(),
                    label: label.into(),
                    status: words.into(),
                    enabled: entry["enabled"].as_bool().unwrap_or(true),
                });
            }
        }
        area.set_destinations(model(rows));
        area.set_destination(open_id.as_str().into());
        area.set_adding_destination(d.adding);
        let open = if open_id == MAIN_DESTINATION {
            stream.clone()
        } else {
            stream["destinations"]
                .as_array()
                .and_then(|list| list.iter().find(|x| x["id"] == open_id.as_str()))
                .cloned()
                .unwrap_or(Value::Null)
        };
        area.set_destination_live(status_of(&open_id).is_some_and(|x| x.is_active()));
        area.set_destination_own_bitrate(!open["video_bitrate_kbps"].is_null());
        area.set_destination_bitrate(number(
            &open["video_bitrate_kbps"],
            number(&stream["video_bitrate_kbps"], 6000.0).into(),
        ));
        area.set_destination_own_size(!open["width"].is_null());
        area.set_destination_width(number(&open["width"], 1280.0));
        area.set_destination_height(number(&open["height"], 720.0));
        let status = status_of(&open_id);
        let service = text(&open["service"]);
        let status_service = status
            .and_then(|x| serde_json::to_value(x.service).ok())
            .and_then(|v| v.as_str().map(str::to_string));
        let key_set = status.is_some_and(|x| x.key_set);
        let ready = has_stream && summary.is_some_and(|x| x.key_set);
        area.set_notice(if ready { "" } else { d.notice.as_str() }.into());
        area.set_refused(d.refused.as_str().into());
        area.set_has_stream(has_stream);
        area.set_needs_server(d.needs_server.as_str().into());
        area.set_stream_live(match canvas {
            CanvasId::Main => s.stream.as_ref().is_some_and(|x| {
                matches!(
                    x.state,
                    StreamState::Live | StreamState::Connecting | StreamState::Reconnecting
                )
            }),
            _ => statuses.iter().any(StreamStatus::is_active),
        });
        let preset = preset_server(&service);
        area.set_service(service.as_str().into());
        area.set_server(text(&open["server"]).into());
        area.set_server_placeholder(preset.unwrap_or("rtmps://…").into());
        area.set_server_hint(
            if preset.is_some() {
                "Leave empty for the service’s recommended server."
            } else if service == "kick" {
                "Your channel’s Stream URL, from Kick’s Creator Dashboard."
            } else {
                "Like rtmp://host/app or rtmps://host/app (without the key)."
            }
            .into(),
        );
        let same_service = status_service.as_deref() == Some(service.as_str());
        area.set_key_saved(same_service && key_set);
        area.set_key_problem(
            status
                .filter(|_| same_service)
                .and_then(|x| x.key_problem.clone())
                .unwrap_or_default()
                .into(),
        );
        area.set_key_busy(d.key_busy);
        area.set_video_bitrate(number(&stream["video_bitrate_kbps"], 6000.0));
        area.set_rate_control(stream["rate_control"].as_str().unwrap_or("constant").into());
        area.set_audio_bitrate(
            stream["audio_bitrate_kbps"]
                .as_u64()
                .unwrap_or(160)
                .to_string()
                .into(),
        );
        area.set_keyframe_interval(number(&stream["keyframe_interval_seconds"], 2.0));
        area.set_stream_custom_size(!stream["width"].is_null());
        area.set_stream_width(number(&stream["width"], 1280.0));
        area.set_stream_height(number(&stream["height"], 720.0));

        // Output
        let rec = &draft["recording"];
        let program = &rec["program"];
        let iso = &rec["iso"];
        area.set_folder(text(&rec["folder"]).into());
        area.set_folder_short(middle_short(&text(&rec["folder"])).into());
        area.set_keep_free(number(&rec["keep_free_gb"], 1.0));
        area.set_replay_seconds(number(&rec["replay"]["seconds"], 30.0));
        area.set_replay_memory(number(&rec["replay"]["max_memory_mb"], 1024.0));
        let program_on = program["enabled"] != json!(false);
        let iso_on = iso["enabled"] == json!(true);
        let codec = |encoder: &Value| encoder["codec"].as_str().unwrap_or("hevc").to_string();
        let prores_codec = |c: &str| c.starts_with("prores");
        let encoders: Vec<&Value> = [(program_on, &program["encoder"]), (iso_on, &iso["encoder"])]
            .into_iter()
            .filter(|(on, e)| *on && e.is_object())
            .map(|(_, e)| e)
            .collect();
        let prores = !encoders.is_empty() && encoders.iter().all(|e| prores_codec(&codec(e)));
        let pq = &program["encoder"]["quality"];
        let iq = &iso["encoder"]["quality"];
        let same = quality_number(pq) == quality_number(iq) || !iso_on || !program_on;
        let shown = if iso_on && !program_on { iq } else { pq };
        let exact = if same { quality_stop(shown) } else { -1 };
        area.set_quality_stop(exact);
        area.set_quality_position(quality_position(quality_number(shown)));
        area.set_quality_disabled(prores);
        area.set_quality_value(
            if prores {
                "Fixed".to_string()
            } else if same {
                quality_number(shown).to_string()
            } else {
                format!("{} · ISO {}", quality_number(pq), quality_number(iq))
            }
            .into(),
        );
        area.set_program_quality(quality_number(pq) as f32);
        area.set_iso_quality(quality_number(iq) as f32);
        area.set_program_on(program_on);
        area.set_iso_on(iso_on);
        let depth = |encoder: &Value| encoder["bit_depth"].as_u64().unwrap_or(8).to_string();
        let shows_depth = |c: &str| !prores_codec(c) && c != "h264";
        let program_codec = codec(&program["encoder"]);
        area.set_program_codec(program_codec.as_str().into());
        area.set_program_container(
            program["encoder"]["container"]
                .as_str()
                .unwrap_or("mov")
                .into(),
        );
        area.set_program_depth(depth(&program["encoder"]).into());
        area.set_program_mp4_ok(mp4_ok(&program_codec));
        area.set_program_shows_depth(shows_depth(&program_codec));
        let iso_codec = codec(&iso["encoder"]);
        area.set_iso_codec(iso_codec.as_str().into());
        area.set_iso_container(iso["encoder"]["container"].as_str().unwrap_or("mov").into());
        // Colour Depth: every ISO file's without one of its own. A set
        // depth shows as set; left out it is 8, or 10 at the incredible
        // HEVC preset.
        let iso_depth = iso_depth_of(&iso["encoder"]);
        area.set_iso_depth(iso_depth.to_string().into());
        let ten_ok = iso_codec != "h264" && engine_is_mac();
        area.set_iso_ten_ok(ten_ok);
        area.set_iso_depth_note(
            if iso_codec == "h264" {
                "H.264 files are 8-bit only; HEVC records 10-bit."
            } else if !engine_is_mac() {
                "This engine captures screens in 8 bits only."
            } else {
                ""
            }
            .into(),
        );
        area.set_iso_mp4_ok(mp4_ok(&iso_codec));
        area.set_iso_shows_depth(!prores_codec(&iso_codec));
        area.set_iso_filtered(iso["filtered"] == json!(true));
        let chosen: Vec<&str> = iso["sources"]
            .as_array()
            .map(|list| list.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let drafted = draft_setup(s.setup.as_deref(), &draft["recording"]["iso"]);
        area.set_iso_sources(model(
            drafted
                .iter()
                .flat_map(|setup| setup.sources.iter())
                .filter(|source| source.kind.has_video())
                .map(|source| IsoChoice {
                    id: source.id.as_str().into(),
                    name: crate::model::source_name(source).into(),
                    on: chosen.contains(&source.id.as_str()),
                    shared_audio: drafted.as_ref().is_some_and(|setup| {
                        !setup.recording.iso.source_audio.contains_key(&source.id)
                    }),
                    tracks: model(drafted.iter().flat_map(|setup| {
                        let tracks = setup
                            .recording
                            .iso
                            .audio_for(source, source.kind.has_audio());
                        setup
                            .sources
                            .iter()
                            .filter(|a| a.kind.has_audio())
                            .map(move |a| IsoTrack {
                                id: a.id.as_str().into(),
                                name: crate::model::source_name(a).into(),
                                on: tracks.contains(&a.id),
                            })
                            .collect::<Vec<_>>()
                    })),
                }),
        ));

        // Audio
        let devices = s.devices.as_ref();
        let mic_sources: Vec<&Value> = s
            .setup_json()
            .and_then(|setup| setup["sources"].as_array())
            .map(|list| list.iter().filter(|x| x["kind"] == "microphone").collect())
            .unwrap_or_default();
        area.set_mics(model(mic_sources.iter().map(|source| {
            let id = text(&source["id"]);
            let value = d
                .mics
                .get(&id)
                .cloned()
                .unwrap_or_else(|| text(&source["device"]));
            let name = source["name"]
                .as_str()
                .filter(|n| !n.is_empty())
                .unwrap_or(&id)
                .to_string();
            MicRow {
                options: model(microphone_options(devices, &value)),
                id: id.into(),
                name: name.into(),
                value: value.into(),
            }
        })));
        let audio = &draft["audio"];
        let device = audio["monitoring_device"]
            .as_str()
            .unwrap_or("default")
            .to_string();
        area.set_outputs(model(output_options(devices, &device)));
        area.set_monitoring_device(device.into());
        area.set_pan_law(audio["pan_law"].as_str().unwrap_or("sine").into());

        // Video
        let canvas = &draft["canvas"];
        let (width, height) = (
            canvas["width"].as_u64().unwrap_or(1920),
            canvas["height"].as_u64().unwrap_or(1080),
        );
        let size = format!("{width}x{height}");
        let known = CANVASES.iter().any(|(v, _)| *v == size);
        let mut sizes: Vec<DropdownOption> = CANVASES
            .iter()
            .map(|(v, label)| kit::option(v, label, ""))
            .collect();
        sizes.push(kit::option(
            "custom",
            &format!("Custom ({width}×{height})"),
            "",
        ));
        area.set_canvas_sizes(model(sizes));
        area.set_canvas_size(if known { size.as_str() } else { "custom" }.into());
        area.set_canvas_width(width as f32);
        area.set_canvas_height(height as f32);
        let fps = canvas["fps"].as_u64().unwrap_or(30) as u32;
        let mut rates: Vec<u32> = FPS.to_vec();
        if !rates.contains(&fps) {
            rates.insert(0, fps);
        }
        area.set_fps_options(model(rates.iter().map(|f| {
            let f = f.to_string();
            kit::option(&f, &f, "")
        })));
        area.set_fps(fps.to_string().into());
        // Canvases (#84): every one besides main, the edited one's
        // settings in the vertical-* fields.
        let extras = draft_canvases(draft);
        let edit = d
            .edit_canvas
            .filter(|c| draft_canvas(draft, *c).is_object())
            .or_else(|| extras.first().and_then(|(id, _)| CanvasId::parse(id).ok()))
            .filter(|_| !d.canvas_closed);
        let main_fps = draft["canvas"]["fps"].as_u64().unwrap_or(30);
        area.set_canvas_rows(model(extras.iter().map(|(id, label)| {
            let c = CanvasId::parse(id)
                .map(|c| draft_canvas(draft, c).clone())
                .unwrap_or_default();
            let fps = c["fps"].as_u64().unwrap_or(main_fps);
            let mut detail = format!(
                "{}×{} · {fps} fps · {}",
                c["width"].as_u64().unwrap_or(1080),
                c["height"].as_u64().unwrap_or(1920),
                if c["follow_main"].as_bool() == Some(false) {
                    "own scenes"
                } else {
                    "main scenes"
                }
            );
            if c["recording"].is_object() && c["recording"]["enabled"].as_bool() != Some(false) {
                detail.push_str(" · recorded");
            }
            let streams = c["stream"]["destinations"].as_array().map_or(0, Vec::len)
                + usize::from(c["stream"].is_object());
            if streams > 0 {
                detail.push_str(&format!(
                    " · {streams} stream{}",
                    if streams == 1 { "" } else { "s" }
                ));
            }
            kit::option(id, label, &detail)
        })));
        area.set_edit_canvas(edit.map_or("".into(), |c| c.as_str().into()));
        let vertical = edit.map_or(&Value::Null, |c| draft_canvas(draft, c));
        area.set_canvas_fps(
            vertical["fps"]
                .as_u64()
                .map_or("main".to_string(), |f| f.to_string())
                .into(),
        );
        area.set_canvas_fps_options(model(
            std::iter::once(kit::option("main", &format!("Main ({main_fps})"), "")).chain(
                [24u32, 25, 30, 50, 60].iter().map(|f| {
                    let f = f.to_string();
                    kit::option(&f, &f, "")
                }),
            ),
        ));
        area.set_canvas_presets(model(
            [
                (
                    "1080x1920",
                    "9:16 Vertical",
                    "1080×1920 · Shorts, Reels, TikTok",
                ),
                ("1080x1080", "1:1 Square", "1080×1080"),
                ("1080x1350", "4:5 Portrait", "1080×1350 · feeds"),
                ("1280x720", "16:9 Landscape", "1280×720"),
            ]
            .iter()
            .map(|(v, l, d)| kit::option(v, l, d)),
        ));
        area.set_vertical_on(vertical.is_object());
        area.set_vertical_width(vertical["width"].as_u64().unwrap_or(1080) as f32);
        area.set_vertical_height(vertical["height"].as_u64().unwrap_or(1920) as f32);
        area.set_vertical_follow(vertical["follow_main"].as_bool() != Some(false));
        area.set_vertical_record(
            vertical["recording"].is_object()
                && vertical["recording"]["enabled"].as_bool() != Some(false),
        );
        area.set_recording(s.status.as_ref().is_some_and(|x| {
            matches!(
                x.state,
                isocast_protocol::api::EngineState::Recording
                    | isocast_protocol::api::EngineState::Starting
            )
        }));

        // Advanced (the main canvas's stream)
        let reconnect = &draft["stream"]["reconnect"];
        area.set_reconnect_on(reconnect["enabled"] != json!(false));
        area.set_reconnect_delay(number(&reconnect["delay_seconds"], 2.0));
        area.set_reconnect_retries(number(&reconnect["max_retries"], 25.0));
        let developer = &draft["developer"];
        area.set_developer_on(developer["enabled"] == json!(true));
        area.set_developer_auto_off(developer["auto_off_hours"] != json!(0));
        // Automatic quality (T.2): on unless turned off, its pins, and
        // the ISO files recorded now, each of which can be kept.
        let governor = governor_settings(draft);
        let pinned = |pin: &str| governor.pinned(pin);
        area.set_governor_on(governor.enabled);
        area.set_iso_thinning(governor.allow_iso_thinning);
        area.set_pin_preview(pinned(PIN_PREVIEW));
        area.set_pin_iso(pinned(PIN_ISO));
        area.set_pin_vertical(pinned(PIN_VERTICAL));
        area.set_pin_stream(pinned(PIN_STREAM));
        area.set_pin_iso_sources(model(
            drafted
                .iter()
                .filter(|setup| setup.recording.iso.enabled)
                .flat_map(|setup| setup.sources.iter())
                .filter(|source| source.kind.has_video() && chosen.contains(&source.id.as_str()))
                .map(|source| IsoTrack {
                    id: source.id.as_str().into(),
                    name: crate::model::source_name(source).into(),
                    on: pinned(&iso_pin(source.id.as_str())),
                }),
        ));
        area.set_governor_now(
            s.status
                .as_ref()
                .and_then(|x| x.governor.as_ref())
                .and_then(super::controls::governor_words)
                .unwrap_or_default()
                .into(),
        );
        let hours = self.auto_off_hours();
        area.set_developer_auto_off_label(
            format!(
                "Turn Off After {hours} Hour{}",
                if hours == 1 { "" } else { "s" }
            )
            .into(),
        );

        // The summary: what the draft makes, how fast its program file
        // fills the disk (the estimate the engine's free-space check uses,
        // ProgramRecording::bytes_per_second), and the recording folder's
        // disk (the engine's Status, as the status bar shows it).
        area.set_summary_video(format!("{width}×{height} | {fps} fps").into());
        let rate = serde_json::from_value::<isocast_protocol::ProgramRecording>(program.clone())
            .ok()
            .zip(serde_json::from_value::<isocast_protocol::Canvas>(canvas.clone()).ok())
            .map(|(program, canvas)| program.bytes_per_second(&canvas))
            .filter(|rate| *rate > 0.0);
        area.set_summary_rate(
            rate.map_or(String::new(), |rate| {
                format!("~{:.1} GB/h", rate * 3600.0 / 1e9)
            })
            .into(),
        );
        area.set_summary_free(
            s.status
                .as_ref()
                .and_then(|x| x.recording_free_bytes)
                .map_or(String::new(), crate::model::gigabytes_free)
                .into(),
        );
        let (on, files) = if program_on {
            (true, &program["encoder"])
        } else {
            (iso_on, &iso["encoder"])
        };
        let mix = program["audio"].as_array().is_some_and(|a| !a.is_empty());
        // The engine mixes the program in stereo at 48 kHz.
        area.set_summary_audio(
            if program_on && mix {
                "Stereo 48 kHz"
            } else {
                "No program sound"
            }
            .into(),
        );
        area.set_summary_codec(
            if on {
                // A Linux engine's files are H.264 (its default, and the
                // only codec its writer takes).
                let linux = crate::engine::os() == isocast_protocol::hello::EngineOs::Linux;
                let codec = match files["codec"].as_str() {
                    None if linux => "h264".to_string(),
                    _ => codec(files),
                };
                let label = CODECS
                    .iter()
                    .find(|(v, _)| *v == codec)
                    .map(|(_, label)| *label)
                    .unwrap_or(codec.as_str());
                let container = files["container"].as_str().unwrap_or("mov").to_uppercase();
                if linux && codec != "h264" {
                    // Short, to fit the footer: "HEVC (unsupported) · MOV".
                    let name = label.split(' ').next().unwrap_or(label);
                    format!("{name} (unsupported) · {container}")
                } else {
                    format!("{label} · {container}")
                }
            } else {
                "Nothing recorded".to_string()
            }
            .into(),
        );
    }

    /// Put a group of fields back as they are in the live setup (a group
    /// heading's reset glyph): "recording" (folder, quality, keep free),
    /// "program" and "iso" (all but their quality, which Recording has).
    fn revert_group(&self, app: &Rc<App>, group: &str) {
        let paths: &[&[&str]] = match group {
            "recording" => &[
                &["recording", "folder"],
                &["recording", "keep_free_gb"],
                &["recording", "program", "encoder", "quality"],
                &["recording", "iso", "encoder", "quality"],
            ],
            "program" => &[&["recording", "program"]],
            "iso" => &[&["recording", "iso"]],
            "replay" => &[&["recording", "replay"]],
            _ => return,
        };
        if let Some(d) = self.draft.borrow_mut().as_mut() {
            for path in paths {
                // Recording's quality numbers stay as they are in Program
                // and ISO.
                let kept = (path.len() == 2)
                    .then(|| d.draft["recording"][path[1]]["encoder"]["quality"].clone());
                let saved = path.iter().fold(&d.base, |v, k| &v[*k]).clone();
                let mut slot = &mut d.draft;
                for key in &path[..path.len() - 1] {
                    if !slot[*key].is_object() {
                        slot[*key] = json!({});
                    }
                    slot = &mut slot[*key];
                }
                let last = path[path.len() - 1];
                if let Some(map) = slot.as_object_mut() {
                    if saved.is_null() {
                        map.remove(last);
                    } else {
                        map.insert(last.to_string(), saved);
                    }
                }
                if let Some(quality) = kept.filter(|q| !q.is_null()) {
                    let encoder = &mut d.draft["recording"][path[1]]["encoder"];
                    if encoder.is_object() {
                        encoder["quality"] = quality;
                    }
                }
            }
        }
        self.show(app);
    }

    // ---------- Welcome: opening and importing setups ----------

    /// Ask for a setup file (TOML or JSON) and make it the live setup.
    fn open_setup_file(&self, app: &Rc<App>) {
        choose_file(
            app,
            &["toml", "json"],
            "Open",
            "Apply it from the command line instead: isocast apply FILE --revision 0.",
            |app, path| app.settings.open_setup_path(app, &path),
        );
    }

    /// Make the setup file at `path` the live setup.
    fn open_setup_path(&self, app: &Rc<App>, path: &str) {
        self.set_welcome_busy(app, "file");
        let result = read_setup(path);
        self.set_welcome_busy(app, "");
        match result {
            Ok(setup) => self.use_setup(app, setup, Vec::new()),
            Err(message) => {
                app.toast(ToastKind::Error, "Couldn’t read the setup file", message);
            }
        }
    }

    /// Ask for an OBS scene collection and make it the live setup (or,
    /// when OBS's own were found, offer those: area I).
    fn import_obs(&self, app: &Rc<App>) {
        if app.wizard.has_found() {
            app.wizard.open_obs(app);
            return;
        }
        choose_file(
            app,
            &["json"],
            "Import",
            "Import it from the command line instead: isocast setup import-obs FILE --output \
             setup.toml, then isocast apply setup.toml --revision 0.",
            |app, path| app.settings.import_obs_path(app, &path),
        );
    }

    /// Import the OBS scene collection at `path` as the live setup.
    fn import_obs_path(&self, app: &Rc<App>, path: &str) {
        self.set_welcome_busy(app, "obs");
        let params = ImportObsParams {
            collection: path.to_string(),
            profile: None,
            width: None,
            height: None,
            fps: None,
        };
        app.call(Method::ImportObs(Box::new(params)), |app, result| {
            app.settings.set_welcome_busy(app, "");
            match result {
                Ok(imported) => {
                    let notes = imported["notes"]
                        .as_array()
                        .map(|n| {
                            n.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                    app.settings
                        .use_setup(app, imported["setup"].clone(), notes);
                }
                Err(error) => app.failed("Couldn’t import the scene collection", &error),
            }
        });
    }

    /// Make `setup` the live setup: as an undoable edit when there is one
    /// already, else as the first.
    fn use_setup(&self, app: &Rc<App>, setup: Value, notes: Vec<String>) {
        let has_setup = app.session.borrow().setup.is_some();
        let done = move |app: &Rc<App>| {
            app.refresh_status(false);
            if !notes.is_empty() {
                app.toast(ToastKind::Warning, "Imported, with notes", notes.join("\n"));
            }
        };
        if has_setup {
            // Status and notes once it is applied (the web app awaits it).
            app.replace_setup_then(setup, true, move |app, applied| {
                if applied {
                    done(app);
                }
            });
            return;
        }
        let typed: Setup = match serde_json::from_value(setup) {
            Ok(setup) => setup,
            Err(error) => {
                app.toast(
                    ToastKind::Error,
                    "Couldn’t use that setup",
                    format!("It isn’t a valid setup: {error}"),
                );
                return;
            }
        };
        let revision = app.session.borrow().revision;
        let params = ApplyParams {
            revision,
            change: Change::Setup(Box::new(typed)),
            live: false,
        };
        app.call(
            Method::Apply(Box::new(params)),
            move |app, result| match result {
                Ok(_) => {
                    app.reload();
                    done(app);
                }
                Err(error) => app.failed("Couldn’t use that setup", &error),
            },
        );
    }

    fn set_welcome_busy(&self, app: &Rc<App>, which: &str) {
        *self.welcome_busy.borrow_mut() = which.to_string();
        if let Some(window) = app.window() {
            window
                .global::<SettingsArea>()
                .set_welcome_busy(which.into());
        }
    }

    // ---------- The first run ----------

    fn start_first_run(&self, app: &Rc<App>) {
        let access = app.permissions.access_needed();
        *self.first_run.borrow_mut() = FirstRun {
            access,
            step: if access { 0 } else { 1 },
            ..FirstRun::default()
        };
        if let Some(window) = app.window() {
            window.global::<Ui>().set_onboarding(true);
        }
        // Ask for the devices now, so they're there by the second step.
        app.load_devices(true);
        // The Permissions step draws the permissions step's rows (area H),
        // which read the permissions every second while it shows.
        app.permissions.check(app);
        self.show_first_run(app);
    }

    /// Fresh permissions: a camera or microphone that became blocked
    /// during the first run brings its access step back (it never goes
    /// away while shown, so the step doesn't vanish under you).
    pub fn permissions_changed(&self, app: &Rc<App>) {
        let open = app
            .window()
            .is_some_and(|window| window.global::<Ui>().get_onboarding());
        if open && !self.first_run.borrow().access && app.permissions.access_needed() {
            self.first_run.borrow_mut().access = true;
            self.show_first_run(app);
        }
    }

    fn close_first_run(&self, app: &Rc<App>) {
        if let Some(window) = app.window() {
            window.global::<Ui>().set_onboarding(false);
        }
    }

    fn first_run_set(&self, app: &Rc<App>, field: &str, value: &str) {
        {
            let mut run = self.first_run.borrow_mut();
            match field {
                "display" => run.display = value.to_string(),
                "camera" => run.camera = (!value.is_empty()).then(|| value.to_string()),
                "microphone" => run.microphone = value.to_string(),
                "system_audio" => run.system_audio = value == "true",
                "layout" => run.layout = value.to_string(),
                "canvas" => run.canvas = value.to_string(),
                "fps" => run.fps = value.parse().unwrap_or(60),
                "folder" if !value.trim().is_empty() => run.folder = value.trim().to_string(),
                "name" => run.name = value.to_string(),
                _ => {}
            }
        }
        self.show_first_run(app);
    }

    /// Make the setup from the choices (`init_setup`, applied) and open the
    /// window on it.
    fn create_setup(&self, app: &Rc<App>) {
        let options = {
            let mut run = self.first_run.borrow_mut();
            if run.creating {
                return;
            }
            run.creating = true;
            let (width, height) = run
                .canvas
                .split_once('x')
                .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
                .unwrap_or((1920, 1080));
            let name = run.name.trim();
            InitOptions {
                display: Some(run.display.clone()),
                camera: run.camera.clone(),
                microphone: Some(run.microphone.clone()),
                system_audio: Some(run.system_audio),
                layout: serde_json::from_value::<Layout>(json!(run.layout)).ok(),
                width: Some(width),
                height: Some(height),
                fps: Some(run.fps),
                folder: Some(run.folder.clone()),
                name: Some(if name.is_empty() { "My setup" } else { name }.to_string()),
            }
        };
        self.show_first_run(app);
        let name = options.name.clone().unwrap_or_default();
        let params = InitParams {
            options,
            apply: true,
        };
        app.call(Method::InitSetup(Box::new(params)), move |app, result| {
            app.settings.first_run.borrow_mut().creating = false;
            match result {
                Ok(made) => {
                    app.reload();
                    app.refresh_status(false);
                    app.settings.close_first_run(app);
                    // Then optimize it (area I).
                    app.wizard.open_goal(app);
                    let scenes = made["setup"]["scenes"].as_array().map_or(0, Vec::len);
                    app.toast(
                        ToastKind::Success,
                        &format!("Set up “{name}”"),
                        if scenes > 1 {
                            format!("{scenes} scenes, ready to record. Switch between them with 1–{scenes}.")
                        } else {
                            "Ready to record.".to_string()
                        },
                    );
                }
                Err(error) => {
                    app.failed("Couldn’t make the setup", &error);
                    app.settings.show_first_run(app);
                }
            }
        });
    }

    /// Show the first run's step and choices.
    fn show_first_run(&self, app: &Rc<App>) {
        let Some(window) = app.window() else { return };
        let area = window.global::<SettingsArea>();
        let run = self.first_run.borrow();
        let s = app.session.borrow();
        area.set_step(run.step);
        area.set_ob_access(run.access);
        area.set_can_close(s.setup.is_some());
        area.set_creating(run.creating);

        // Devices
        let devices = s.devices.as_ref();
        area.set_devices_listed(devices.is_some());
        area.set_ob_pipewire_unavailable(devices.is_some_and(|d| {
            d.notes
                .iter()
                .any(|n| n.starts_with("PipeWire audio devices:"))
        }));
        if let Some(devices) = devices {
            area.set_displays(model(devices.displays.iter().map(|d| {
                DeviceChoice {
                    value: if d.main {
                        "main".into()
                    } else {
                        d.name.as_str().into()
                    },
                    title: d.name.as_str().into(),
                    // Linux's portal names a screen only once it's
                    // chosen: no size yet (0×0), so none is shown.
                    meta: match (d.main, d.width > 0 && d.height > 0) {
                        (true, true) => format!("Main display · {}×{}", d.width, d.height),
                        (true, false) => "Main display".into(),
                        (false, true) => format!("{}×{}", d.width, d.height),
                        (false, false) => String::new(),
                    }
                    .into(),
                    disabled: false,
                }
            })));
            area.set_displays_empty(
                if devices.displays.is_empty() {
                    format!(
                        "No displays found{}.",
                        if !matches!(
                            devices.permissions.screen_recording,
                            Permission::Granted | Permission::Unknown
                        ) {
                            " (screen recording isn’t allowed yet)"
                        } else {
                            ""
                        }
                    )
                } else {
                    String::new()
                }
                .into(),
            );
            let cameras: Vec<_> = devices
                .cameras
                .iter()
                .filter(|c| c.kind != CameraKind::DeskView)
                .collect();
            let best = best_camera(&cameras).map(|c| c.name.clone());
            let mut camera_choices = vec![DeviceChoice {
                value: "".into(),
                title: "Automatic".into(),
                meta: best.as_deref().unwrap_or("None found").into(),
                disabled: false,
            }];
            for c in &cameras {
                let twins: Vec<_> = cameras.iter().filter(|o| o.name == c.name).collect();
                let twin = twins.len() > 1;
                let place = twins.iter().position(|o| o.id == c.id).unwrap_or(0) + 1;
                let meta = if twin {
                    format!(
                        "{} · {place} of {}",
                        if c.suspended {
                            "Unavailable"
                        } else {
                            camera_short(c.kind)
                        },
                        twins.len()
                    )
                } else if c.suspended {
                    "Unavailable right now".to_string()
                } else {
                    camera_kind(c.kind).to_string()
                };
                camera_choices.push(DeviceChoice {
                    value: if twin { c.id.as_str() } else { c.name.as_str() }.into(),
                    title: c.name.as_str().into(),
                    meta: meta.into(),
                    disabled: c.suspended,
                });
            }
            camera_choices.push(DeviceChoice {
                value: "none".into(),
                title: "No camera".into(),
                meta: "".into(),
                disabled: false,
            });
            area.set_cameras(model(camera_choices));
            let default_mic = devices.microphones.iter().find(|m| m.default).map_or(
                wording::select_linux(
                    "Follows System Settings",
                    "Follows Windows Settings",
                    "Follows the desktop's settings",
                ),
                |m| m.name.as_str(),
            );
            let mut mic_choices = vec![DeviceChoice {
                value: "default".into(),
                title: "System default".into(),
                meta: default_mic.into(),
                disabled: false,
            }];
            for m in &devices.microphones {
                let twins: Vec<_> = devices
                    .microphones
                    .iter()
                    .filter(|o| o.name == m.name)
                    .collect();
                let twin = twins.len() > 1;
                let place = twins.iter().position(|o| o.id == m.id).unwrap_or(0) + 1;
                mic_choices.push(DeviceChoice {
                    value: if twin { m.id.as_str() } else { m.name.as_str() }.into(),
                    title: m.name.as_str().into(),
                    meta: if twin {
                        format!("{place} of {}", twins.len())
                    } else {
                        String::new()
                    }
                    .into(),
                    disabled: false,
                });
            }
            mic_choices.push(DeviceChoice {
                value: "none".into(),
                title: "No microphone".into(),
                meta: "".into(),
                disabled: false,
            });
            area.set_microphones(model(mic_choices));
            // The main display's row is chosen by "main".
            let display = if run.display != "main"
                && devices
                    .displays
                    .iter()
                    .any(|d| d.main && d.name == run.display)
            {
                "main".to_string()
            } else {
                run.display.clone()
            };
            area.set_display(display.into());
        }
        area.set_camera(run.camera.clone().unwrap_or_default().into());
        area.set_microphone(run.microphone.as_str().into());
        area.set_system_audio(run.system_audio);

        // Layout
        area.set_layout(run.layout.as_str().into());
        area.set_ob_canvas(run.canvas.as_str().into());
        area.set_ob_fps(run.fps.to_string().into());
        area.set_ob_folder(run.folder.as_str().into());
        area.set_ob_name(run.name.as_str().into());
    }
}

/// The parts of a setup Settings changes.
/// `setup` with ISO settings `iso` (the draft's), if they read as such.
fn draft_setup(setup: Option<&Setup>, iso: &Value) -> Option<Setup> {
    let mut setup = setup?.clone();
    setup.recording.iso = serde_json::from_value(iso.clone()).ok()?;
    Some(setup)
}

fn pick(setup: &Value) -> Value {
    let mut out = json!({
        "name": setup["name"],
        "canvas": setup["canvas"],
        "recording": setup["recording"],
        "audio": if setup["audio"].is_object() { setup["audio"].clone() } else { json!({}) },
    });
    if setup["stream"].is_object() {
        out["stream"] = setup["stream"].clone();
    }
    // The vertical canvas (#86); absent without one.
    if setup["vertical"].is_object() {
        out["vertical"] = setup["vertical"].clone();
    }
    // More canvases (#84), as a list (empty without any).
    out["canvases"] = if setup["canvases"].is_array() {
        setup["canvases"].clone()
    } else {
        json!([])
    };
    // Automatic quality (Advanced, T.2); absent at its default.
    if setup["governor"].is_object() {
        out["governor"] = setup["governor"].clone();
    }
    // Developer mode (Advanced), the profile's; absent while it is off.
    if setup["developer"].is_object() {
        out["developer"] = setup["developer"].clone();
    }
    out
}

/// Automatic quality's pins (`governor.pins`): the field paths it must
/// never change.
const PIN_PREVIEW: &str = "preview.fps";
const PIN_ISO: &str = "recording.iso.fps";
const PIN_VERTICAL: &str = "vertical.fps";
const PIN_STREAM: &str = "stream.video_bitrate_kbps";

/// The pin keeping one source's ISO file at its frame rate.
fn iso_pin(source: &str) -> String {
    format!("recording.iso.sources.{source}.fps")
}

/// A setup's (or draft's) `governor`: on unless turned off, ISO frame
/// thinning only when the user allowed it, and its pins.
fn governor_settings(setup: &Value) -> GovernorSettings {
    let governor = &setup["governor"];
    let pins = governor["pins"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    GovernorSettings {
        enabled: governor["enabled"].as_bool() != Some(false),
        allow_iso_thinning: governor["allow_iso_thinning"].as_bool() == Some(true),
        pins,
    }
}

/// A patch turning object `from` into `to`: changed fields set, missing
/// ones nulled (nested objects recursively; lists replaced whole, as
/// Settings only changes lists of names).
fn object_patch(from: &Value, to: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    let (Some(from), Some(to)) = (from.as_object(), to.as_object()) else {
        return out;
    };
    for key in from.keys() {
        if !to.contains_key(key) {
            out.insert(key.clone(), Value::Null);
        }
    }
    for (key, value) in to {
        // `[[canvases]]` (#84): the daemon merges it by id, so each entry
        // changed, added or removed (`{"id": …, "remove": true}`).
        if key == "canvases" {
            let list = canvases_patch(from.get(key).unwrap_or(&Value::Null), value);
            if !list.is_empty() {
                out.insert(key.clone(), Value::Array(list));
            }
            continue;
        }
        match from.get(key) {
            Some(old) if old.is_object() && value.is_object() => {
                let inner = object_patch(old, value);
                if !inner.is_empty() {
                    out.insert(key.clone(), Value::Object(inner));
                }
            }
            old if old != Some(value) => {
                out.insert(key.clone(), value.clone());
            }
            _ => {}
        }
    }
    out
}

/// The `canvases` list patch turning `from` into `to`, entry by id.
fn canvases_patch(from: &Value, to: &Value) -> Vec<Value> {
    let empty = Vec::new();
    let from = from.as_array().unwrap_or(&empty);
    let to = to.as_array().unwrap_or(&empty);
    let mut out = Vec::new();
    for old in from {
        if !to.iter().any(|new| new["id"] == old["id"]) {
            out.push(json!({ "id": old["id"], "remove": true }));
        }
    }
    for new in to {
        match from.iter().find(|old| old["id"] == new["id"]) {
            Some(old) => {
                let mut inner = object_patch(old, new);
                if !inner.is_empty() {
                    inner.insert("id".into(), new["id"].clone());
                    out.push(Value::Object(inner));
                }
            }
            None => out.push(new.clone()),
        }
    }
    out
}

/// A quality (a name, a number or nothing) as the engine's number.
fn quality_number(quality: &Value) -> u8 {
    serde_json::from_value::<EncoderQuality>(quality.clone())
        .map(EncoderQuality::value)
        .unwrap_or(EncoderQuality::Preset(QualityPreset::High).value())
}

/// The stop a quality is exactly, if it is one (-1 if not).
fn quality_stop(quality: &Value) -> i32 {
    QUALITIES
        .iter()
        .position(|(name, preset)| {
            quality == name
                || quality.as_u64() == Some(EncoderQuality::Preset(*preset).value() as u64)
                || (quality.is_null() && *name == "high")
        })
        .map_or(-1, |i| i as i32)
}

/// A path with its middle left out when it has more than three parts,
/// so the folder's own name shows: "/Volumes/…/scratch/rec".
pub(crate) fn middle_short(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() <= 4 {
        return path.to_string();
    }
    let head = if parts[0].is_empty() {
        format!("/{}", parts[1])
    } else {
        parts[0].to_string()
    };
    format!("{head}/…/{}", parts[parts.len() - 2..].join("/"))
}

/// Where a quality number sits among the stops, as Segmented's `position`:
/// the stop's index when it is one, else half way between its neighbours
/// (60: 1.5, between Medium's 50 and High's 70).
fn quality_position(quality: u8) -> f32 {
    let below = QUALITIES
        .iter()
        .filter(|(_, preset)| EncoderQuality::Preset(*preset).value() < quality)
        .count();
    let exact = QUALITIES
        .iter()
        .any(|(_, preset)| EncoderQuality::Preset(*preset).value() == quality);
    if exact {
        below as f32
    } else {
        below as f32 - 0.5
    }
}

/// A server the setup can use: rtmp:// or rtmps://, a host, then an app.
fn server_valid(server: &str) -> bool {
    let server = server.trim();
    let rest = server
        .strip_prefix("rtmps://")
        .or_else(|| server.strip_prefix("rtmp://"));
    rest.and_then(|r| r.split_once('/'))
        .is_some_and(|(host, app)| !host.is_empty() && !app.is_empty())
}

/// The microphones a setting can name (shared with source forms).
fn microphone_options(devices: Option<&Devices>, value: &str) -> Vec<DropdownOption> {
    dropdown(crate::form::devices::microphone_options(devices, value))
}

/// The outputs monitoring can play to.
fn output_options(devices: Option<&Devices>, value: &str) -> Vec<DropdownOption> {
    dropdown(crate::form::devices::output_options(devices, value))
}

fn dropdown(choices: Vec<crate::form::devices::Choice>) -> Vec<DropdownOption> {
    choices
        .iter()
        .map(|(value, label, detail)| kit::option(value, label, detail))
        .collect()
}

/// The camera `init_setup` picks when none is named (the daemon decides;
/// this mirrors its order to show the name): USB, then iPhone, then
/// built-in, then others and virtual ones, available before suspended.
fn best_camera<'a>(
    cameras: &[&'a isocast_protocol::devices::Camera],
) -> Option<&'a isocast_protocol::devices::Camera> {
    let rank = |kind: CameraKind| match kind {
        CameraKind::External => Some(0),
        CameraKind::Continuity => Some(1),
        CameraKind::BuiltIn => Some(2),
        CameraKind::Other => Some(3),
        CameraKind::Virtual => Some(4),
        CameraKind::DeskView => Some(5),
        CameraKind::DeviceScreen => None,
    };
    cameras
        .iter()
        .filter_map(|c| Some((rank(c.kind)? + if c.suspended { 10 } else { 0 }, *c)))
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, c)| c)
}

fn camera_kind(kind: CameraKind) -> &'static str {
    match kind {
        CameraKind::External => "USB or capture card",
        CameraKind::Continuity => "iPhone (Continuity Camera)",
        CameraKind::BuiltIn => "Built-in",
        CameraKind::Virtual => "Virtual camera",
        CameraKind::DeviceScreen => "iPhone or iPad screen",
        CameraKind::DeskView => "Desk View",
        CameraKind::Other => "",
    }
}

fn camera_short(kind: CameraKind) -> &'static str {
    match kind {
        CameraKind::External => "USB",
        CameraKind::Continuity => "Continuity",
        CameraKind::BuiltIn => "Built-in",
        CameraKind::Virtual => "Virtual",
        CameraKind::DeviceScreen => "Device screen",
        CameraKind::DeskView => "Desk View",
        CameraKind::Other => "",
    }
}

/// Ask for a folder (the open panel, or another computer's engine's
/// disk); says so where there is no panel.
fn choose_folder(app: &Rc<App>, done: impl FnOnce(&Rc<App>, String) + 'static) {
    let options = Choose {
        folders: true,
        prompt: "Choose",
        ..Choose::default()
    };
    crate::areas::remote::choose(app, options, move |app, chosen| {
        if let Some(folder) = first_chosen(app, chosen, "Type the path into the field instead.") {
            done(app, folder);
        }
    });
}

/// Ask for a file on this computer (a setup to read here), then `done`
/// with its path; `fallback` says what to do where there is no file panel.
fn choose_file(
    app: &Rc<App>,
    types: &[&str],
    prompt: &str,
    fallback: &'static str,
    done: impl FnOnce(&Rc<App>, String) + 'static,
) {
    let options = Choose {
        types,
        prompt,
        ..Choose::default()
    };
    crate::areas::remote::choose_here(app, options, move |app, chosen| {
        if let Some(path) = first_chosen(app, chosen, fallback) {
            done(app, path);
        }
    });
}

fn first_chosen(app: &Rc<App>, chosen: Option<Vec<String>>, fallback: &str) -> Option<String> {
    match chosen {
        Some(paths) => paths.into_iter().next(),
        None => {
            app.toast(ToastKind::Info, "No file panel here", fallback);
            None
        }
    }
}

/// A setup file (TOML or JSON) as JSON, with relative paths made absolute
/// against its folder, as the CLI reads them (the web app's native
/// `read_setup_file`).
fn read_setup(path: &str) -> Result<Value, String> {
    let path = std::path::Path::new(path);
    let text = std::fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let json = path.extension().is_some_and(|ext| ext == "json");
    let mut setup = if json {
        Setup::from_json(&text)
    } else {
        Setup::from_toml(&text)
    }
    .map_err(|err| format!("{}: {err}", path.display()))?;
    if let Some(folder) = path.parent() {
        setup.resolve_paths(folder);
    }
    serde_json::to_value(setup).map_err(|err| err.to_string())
}

/// Whether the engine machine is a Mac, the one place screens are captured
/// (and their ISO files written) in 10 bits.
pub(crate) fn engine_is_mac() -> bool {
    if crate::engine::is_local() {
        cfg!(target_os = "macos")
    } else {
        crate::engine::os() == isocast_protocol::hello::EngineOs::Macos
    }
}

/// The depth of ISO files without one of their own, from the setup's
/// `recording.iso.encoder`: its `bit_depth` if set (8 stays 8 at the
/// incredible preset); left out, 8, but 10 at the incredible HEVC preset,
/// as the engine has it (`IsoEncoder::depth`).
pub(crate) fn iso_depth_of(encoder: &Value) -> u64 {
    encoder["bit_depth"].as_u64().unwrap_or(
        if encoder["codec"].as_str().unwrap_or("hevc") == "hevc"
            && encoder["quality"] == json!("incredible")
        {
            10
        } else {
            8
        },
    )
}

/// Whether `codec` (a setup name: "h264", "prores422_hq"…) can be
/// recorded in MP4, from the engine's encoder options (`Codec::containers`).
fn mp4_ok(codec: &str) -> bool {
    serde_json::from_value::<isocast_protocol::Codec>(json!(codec))
        .map(|c| c.containers().contains(&isocast_protocol::Container::Mp4))
        .unwrap_or(true)
}

/// A service's name as Settings shows it.
fn service_label(service: &str) -> &'static str {
    match service {
        "twitch" => "Twitch",
        "youtube" => "YouTube",
        "kick" => "Kick",
        "facebook" => "Facebook",
        "custom" => "Custom server",
        _ => "Stream",
    }
}

/// Extra canvas `canvas`'s settings in a setup draft: `vertical`, or its
/// `canvases` entry (#84); null if the draft hasn't it (or it is main).
fn draft_canvas(draft: &Value, canvas: CanvasId) -> &Value {
    match canvas {
        CanvasId::Main => &Value::Null,
        CanvasId::Vertical => &draft["vertical"],
        other => draft["canvases"]
            .as_array()
            .and_then(|list| list.iter().find(|c| c["id"] == other.as_str()))
            .unwrap_or(&Value::Null),
    }
}

/// Every extra canvas in a draft, `(id, label)`: the vertical one first.
fn draft_canvases(draft: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if draft["vertical"].is_object() {
        out.push(("vertical".to_string(), "Vertical Canvas".to_string()));
    }
    for canvas in draft["canvases"].as_array().into_iter().flatten() {
        if let Some(id) = canvas["id"].as_str() {
            let label = canvas["name"].as_str().unwrap_or(id).to_string();
            out.push((id.to_string(), label));
        }
    }
    out
}

/// A merge patch making `change` to extra canvas `canvas`'s settings:
/// under `vertical`, or its `canvases` entry (the daemon merges that list
/// by id).
fn canvas_patch(_draft: &Value, canvas: CanvasId, change: Value) -> Value {
    if canvas.is_vertical() {
        return json!({ "vertical": change });
    }
    let mut entry = change;
    if let Some(object) = entry.as_object_mut() {
        object.insert("id".into(), canvas.as_str().into());
    }
    json!({ "canvases": [entry] })
}
