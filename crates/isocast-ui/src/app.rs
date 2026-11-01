//! The hub between the window and isocastd (the web app's lib/session.js):
//! it owns the [`Session`], sends calls and edits to the daemon in order,
//! handles the daemon's replies and events, keeps the shared `Ui` global
//! up to date, and tells each area (src/areas) what changed.
//!
//! Areas use it through these methods: `call` (any API method, with a
//! closure for the reply), `edit` (a merge patch to the live setup, shown
//! at once and sent in order), `select`, `choose_scene`, `open_dialog`,
//! `toast`/`failed`, `load_devices`, `refresh_status`.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use isocast_protocol::CanvasId;
use isocast_protocol::api::{ApplyParams, EngineState, Event, Live, Method, Status};
use isocast_protocol::change::{Change, merge_patch};
use isocast_protocol::devices::Devices;
use isocast_protocol::stream::StreamState;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use slint::{ComponentHandle, SharedString};

use crate::areas::{self, Area};
use crate::daemon::{CallError, Link, Request, Update};
use crate::kit::{self, MenuBuilder};
use crate::model;
use crate::prefs;
use crate::session::{Changed, Edit, EditChange, EditDone, EditOptions, Selection, Session};
use crate::toasts::{Toast, ToastList};
use crate::{AppWindow, ConnectionState, Menus, ToastKind, Ui};

/// How many steps of a gesture (a drag) may be on their way to the
/// daemon at once (see `App::pump`).
const PIPELINE: usize = 2;

/// What the reply to a call runs.
type Reply = Box<dyn FnOnce(&Rc<App>, Result<Value, CallError>)>;

/// How the app runs.
#[derive(Clone, Copy, Default)]
pub struct Mode {
    /// Offscreen (snapshots, soak): no saved layout, no status polling.
    pub offscreen: bool,
}

pub struct App {
    window: slint::Weak<AppWindow>,
    tx: Sender<Request>,
    next_call: Cell<u64>,
    replies: RefCell<HashMap<u64, Reply>>,
    pub session: RefCell<Session>,
    pub toasts: ToastList,
    pub mode: Mode,
    pending: Cell<Changed>,
    flush_timer: slint::Timer,
    edit_timer: slint::Timer,
    status_timer: slint::Timer,
    // The areas, each in src/areas/<name>.rs.
    pub sources: areas::sources::Sources,
    pub mixer: areas::mixer::Mixer,
    pub controls: areas::controls::Controls,
    pub settings: areas::settings::Settings,
    pub canvas: areas::canvas::Canvas,
    pub library: areas::library::Library,
    pub developer: areas::developer::Developer,
    pub permissions: areas::permissions::PermissionsStep,
    pub wizard: areas::wizard::Wizard,
    /// Projectors and the multiview (G.4): windows of their own.
    pub projectors: areas::projectors::Projectors,
    /// Remote control (R2): Settings → Remote, the engine picker,
    /// presence, the engine file browser.
    pub remote: areas::remote::Remote,
    /// `--drag-test`: how long each flush (the areas showing a change) and
    /// each update from the daemon took on the window's thread.
    pub work_log: RefCell<Option<WorkLog>>,
}

#[derive(Default)]
pub struct WorkLog {
    pub flushes: Vec<Duration>,
    pub updates: Vec<Duration>,
}

/// The engine machine's words in the interface (decision 22): its
/// conventions (Windows' off macOS) and nouns (Mac, PC or computer;
/// Keychain, Credential Manager or keyring; Finder, Explorer or file
/// manager).
fn set_engine_words(words: &crate::PlatformText<'_>) {
    words.set_engine_windows(crate::engine::windows());
    words.set_engine_linux(crate::engine::os() == isocast_protocol::hello::EngineOs::Linux);
    let nouns = crate::engine::words();
    words.set_machine(nouns.machine.into());
    words.set_credential_store(nouns.credential_store.into());
    words.set_file_manager(nouns.file_manager.into());
}

impl App {
    pub fn new(
        window: &AppWindow,
        tx: Sender<Request>,
        mode: Mode,
        canvas: areas::canvas::Canvas,
    ) -> Rc<Self> {
        let words = window.global::<crate::PlatformText>();
        // The app's own conventions (menus, shortcuts): Windows' on Linux
        // too (Ctrl, Exit, Maximize, F11).
        words.set_windows(!cfg!(target_os = "macos"));
        // The engine's words (decision 22): this machine's until a remote
        // engine says what it runs (`Update::Hello`).
        set_engine_words(&words);
        window
            .global::<Ui>()
            .set_engine_remote(!crate::engine::is_local());
        window
            .global::<Ui>()
            .set_engine_name(crate::engine::name().unwrap_or_default().into());
        let app = Rc::new(Self {
            window: window.as_weak(),
            tx,
            next_call: Cell::new(1),
            replies: RefCell::default(),
            session: RefCell::default(),
            toasts: ToastList::default(),
            mode,
            pending: Cell::new(Changed::NONE),
            flush_timer: slint::Timer::default(),
            edit_timer: slint::Timer::default(),
            status_timer: slint::Timer::default(),
            work_log: RefCell::new(None),
            sources: areas::sources::Sources::default(),
            mixer: areas::mixer::Mixer::default(),
            controls: areas::controls::Controls::default(),
            settings: areas::settings::Settings::default(),
            canvas,
            library: areas::library::Library::default(),
            developer: areas::developer::Developer::default(),
            permissions: areas::permissions::PermissionsStep::default(),
            wizard: areas::wizard::Wizard::default(),
            projectors: areas::projectors::Projectors::default(),
            remote: areas::remote::Remote::default(),
        });
        window.set_version(env!("CARGO_PKG_VERSION").into());
        if !mode.offscreen {
            prefs::load(window);
            crate::platform::apply_system_settings(window);
        }
        app.session.borrow_mut().studio = window.global::<Ui>().get_studio();
        // The extra canvas the canvas monitor showed last (#120).
        app.session.borrow_mut().extra_choice =
            isocast_protocol::CanvasId::parse(&window.global::<Ui>().get_extra_choice()).ok();
        app.connect(window);
        for area in app.areas() {
            area.connect(&app, window);
        }
        if !mode.offscreen {
            // The engine's health every few seconds while connected.
            let weak = Rc::downgrade(&app);
            app.status_timer.start(
                slint::TimerMode::Repeated,
                Duration::from_secs(3),
                move || {
                    if let Some(app) = weak.upgrade() {
                        let ready = {
                            let s = app.session.borrow();
                            s.link == Some(Link::Connected) && s.loaded
                        };
                        if ready {
                            app.refresh_status(true);
                        }
                    }
                },
            );
        }
        app.changed(Changed::ALL);
        app
    }

    /// Calls made to the daemon and not answered yet (the motion
    /// harness's lockstep waits for none).
    pub fn calls_pending(&self) -> usize {
        self.replies.borrow().len()
    }

    /// The areas, in the order they update.
    pub fn areas(&self) -> [&dyn Area; 11] {
        [
            &self.canvas,
            &self.sources,
            &self.mixer,
            &self.controls,
            &self.settings,
            &self.library,
            &self.developer,
            &self.permissions,
            &self.wizard,
            &self.projectors,
            &self.remote,
        ]
    }

    pub fn window(&self) -> Option<AppWindow> {
        self.window.upgrade()
    }

    fn connect(self: &Rc<Self>, window: &AppWindow) {
        let ui = window.global::<Ui>();
        let weak = Rc::downgrade(self);
        ui.on_menu(move |id| {
            if let Some(app) = weak.upgrade() {
                app.menu(&id);
            }
        });
        let weak = Rc::downgrade(self);
        ui.on_select(move |path| {
            if let Some(app) = weak.upgrade() {
                app.select(&path);
            }
        });
        let weak = Rc::downgrade(self);
        ui.on_open_dialog(move |kind, arg| {
            if let Some(app) = weak.upgrade() {
                app.open_dialog(&kind, &arg);
            }
        });
        let weak = Rc::downgrade(self);
        ui.on_close_dialog(move || {
            if let Some(app) = weak.upgrade() {
                app.close_dialog();
            }
        });
        let weak = Rc::downgrade(self);
        ui.on_set_studio(move |on| {
            if let Some(app) = weak.upgrade() {
                app.set_studio(on);
            }
        });
        let weak = Rc::downgrade(self);
        ui.on_layout_changed(move || {
            if let Some(app) = weak.upgrade()
                && !app.mode.offscreen
                && let Some(window) = app.window()
            {
                prefs::save(&window);
            }
        });
        crate::quit::connect(self, window);

        let menus = window.global::<Menus>();
        menus.on_step(kit::step);
        let weak = Rc::downgrade(self);
        menus.on_request(move |owner, x, y, w, h| {
            if let Some(app) = weak.upgrade() {
                app.request_menu(&owner, [x, y, w, h]);
            }
        });
        let weak = Rc::downgrade(self);
        menus.on_activated(move |owner, id| Self::menu_activated(&weak, owner, id));
        ToastList::connect(self, window);
        crate::form::connect(self, window);
    }

    // ---------- Calls ----------

    /// Call a method of the API; `reply` runs on this thread with its
    /// result as JSON.
    pub fn call(
        self: &Rc<Self>,
        method: Method,
        reply: impl FnOnce(&Rc<App>, Result<Value, CallError>) + 'static,
    ) {
        // Machine control stays on the machine (decision 4): a window on
        // another computer's engine never asks, as the engine would refuse.
        if !crate::engine::is_local()
            && let Some(reason) = isocast_protocol::caller::local_only_reason(&method)
        {
            let engine = crate::engine::name().unwrap_or_else(|| "the engine's computer".into());
            reply(
                self,
                Err(CallError {
                    message: format!("This only works on {engine} itself: {reason}."),
                    code: Some(isocast_protocol::api::RpcError::NOT_PERMITTED),
                }),
            );
            return;
        }
        let id = self.next_call.get();
        self.next_call.set(id + 1);
        self.replies.borrow_mut().insert(id, Box::new(reply));
        if self.tx.send(Request { id, method }).is_err()
            && let Some(reply) = self.replies.borrow_mut().remove(&id)
        {
            reply(
                self,
                Err(CallError {
                    message: "The link to isocastd has stopped".into(),
                    code: None,
                }),
            );
        }
    }

    /// Call a method and read its result as `T`.
    pub fn call_as<T: DeserializeOwned + 'static>(
        self: &Rc<Self>,
        method: Method,
        reply: impl FnOnce(&Rc<App>, Result<T, CallError>) + 'static,
    ) {
        self.call(method, move |app, result| {
            let typed = result.and_then(|value| {
                serde_json::from_value::<T>(value).map_err(|error| CallError {
                    message: format!("isocastd sent an unexpected reply: {error}"),
                    code: None,
                })
            });
            reply(app, typed);
        });
    }

    /// Show what the daemon's threads delivered.
    pub fn apply(self: &Rc<Self>, update: Update) {
        let started = Instant::now();
        self.apply_update(update);
        if let Some(log) = self.work_log.borrow_mut().as_mut() {
            log.updates.push(started.elapsed());
        }
    }

    fn apply_update(self: &Rc<Self>, update: Update) {
        match update {
            Update::Reply { id, result } => {
                let reply = self.replies.borrow_mut().remove(&id);
                if let Some(reply) = reply {
                    reply(self, result);
                }
            }
            Update::Link(link, why) => {
                {
                    let mut s = self.session.borrow_mut();
                    s.link = Some(link);
                    s.link_error = why;
                }
                self.changed(Changed::LINK);
            }
            Update::Watching(live) => self.watching(*live),
            Update::Event(event) => self.event(*event),
            Update::Hello(hello) => {
                crate::engine::set_hello(*hello);
                if let Some(window) = self.window() {
                    set_engine_words(&window.global::<crate::PlatformText>());
                    window
                        .global::<Ui>()
                        .set_engine_name(crate::engine::name().unwrap_or_default().into());
                }
                self.changed(Changed::LINK);
            }
        }
    }

    fn watching(self: &Rc<Self>, live: Live) {
        {
            let mut s = self.session.borrow_mut();
            // A daemon that started again counts revisions from the start.
            s.forget();
            let setup = live
                .setup
                .as_ref()
                .and_then(|s| serde_json::to_value(s).ok());
            s.confirm(setup, live.revision);
            s.recompute();
            s.loaded = true;
        }
        self.changed(Changed::ALL);
        self.refresh_status(false);
        self.projectors.load(self);
    }

    pub(crate) fn event(self: &Rc<Self>, event: Event) {
        match &event {
            Event::SetupChanged { revision, .. } => {
                let stale = {
                    let s = self.session.borrow();
                    *revision > s.revision && !s.editing()
                };
                if stale {
                    self.reload();
                }
            }
            Event::SceneChanged { scene, .. } => {
                self.session.borrow_mut().scene = Some(scene.clone());
                self.changed(Changed::SCENES);
            }
            Event::CanvasSceneChanged { canvas, scene, .. } => {
                if !canvas.is_main() {
                    self.session
                        .borrow_mut()
                        .canvas_scenes
                        .insert(*canvas, scene.clone());
                    self.changed(Changed::SCENES);
                }
            }
            Event::CanvasPreviewChanged { canvas, scene } => {
                if !canvas.is_main() {
                    {
                        let mut s = self.session.borrow_mut();
                        match scene {
                            Some(scene) => s.canvas_previews.insert(*canvas, scene.clone()),
                            None => s.canvas_previews.remove(canvas),
                        };
                        s.studio = s.staged().is_some();
                    }
                    self.changed(Changed::SCENES);
                }
            }
            Event::PreviewChanged { scene } => {
                {
                    let mut s = self.session.borrow_mut();
                    s.preview = Some(scene.clone());
                    s.studio = s.staged().is_some();
                }
                self.changed(Changed::SCENES);
            }
            Event::PreviewCleared => {
                {
                    let mut s = self.session.borrow_mut();
                    s.preview = None;
                    s.studio = s.staged().is_some();
                }
                self.changed(Changed::SCENES);
            }
            // A scene collection or profile was created, switched, renamed or
            // removed: a switch changes the live setup, so read it again (the
            // Profile and Scene Collection menus read the library, area F).
            Event::LibraryChanged { .. } => self.reload(),
            // No plugins view yet (`isocast plugin list` has them).
            Event::PluginChanged { .. } => {}
            // Data shows in the program's picture; the app has no data view.
            Event::DataChanged { .. } => {}
            // Studio Mode is this window's view of the daemon's preview:
            // follow a hotkey that turned it on or off.
            Event::HotkeyFired { action, error, .. } => {
                if action == "studio.toggle" && error.is_none() {
                    self.call_as::<isocast_protocol::hotkeys::Hotkeys>(
                        Method::HotkeysGet,
                        |app, result| {
                            if let Ok(hotkeys) = result
                                && hotkeys.studio_mode != app.session.borrow().studio
                            {
                                app.set_studio(hotkeys.studio_mode);
                            }
                        },
                    );
                }
            }
            Event::VirtualCamera { camera } => {
                self.session.borrow_mut().vcam = Some(camera.clone());
                self.changed(Changed::VCAM);
            }
            // The Controls dock shows the main canvas's buffer; the canvas
            // monitor's menu the others' (#120).
            Event::Replay { replay } => {
                self.session.borrow_mut().set_replay(replay.clone());
                self.changed(Changed::REPLAY);
            }
            // The window that asked says so itself; a hotkey's or another
            // client's save gets the same toast here.
            Event::ReplaySaved { saved } => {
                if !self.session.borrow().saving_replay {
                    crate::areas::controls::replay_saved(self, saved);
                }
            }
            Event::Started { .. } => {
                self.session.borrow_mut().record_epoch += 1;
                self.refresh_status(true);
            }
            // Plugin-facing context; the app refreshes on `started` and
            // `stopped`.
            Event::TakeFinished { .. }
            | Event::ProjectCreated { .. }
            | Event::ProjectRenamed { .. }
            | Event::DeviceChanged { .. }
            | Event::Projectors { .. } => {}
            Event::RecordingPaused { .. } => self.refresh_status(true),
            Event::Status { live } => {
                {
                    let mut s = self.session.borrow_mut();
                    let status = s
                        .status
                        .get_or_insert_with(|| empty_status(EngineState::Recording));
                    status.state = EngineState::Recording;
                    status.live = Some(live.clone());
                    for level in &live.levels {
                        s.levels.insert(level.source.clone(), level.clone());
                    }
                }
                self.changed(Changed::STATUS | Changed::LEVELS);
            }
            Event::Levels { levels } => {
                {
                    let mut s = self.session.borrow_mut();
                    for level in levels {
                        s.levels.insert(level.source.clone(), level.clone());
                    }
                }
                self.changed(Changed::LEVELS);
            }
            Event::SourceChanged { status } => {
                self.session
                    .borrow_mut()
                    .running
                    .insert(status.source.clone(), status.clone());
                self.changed(Changed::RUNNING);
            }
            Event::Stream { stream, .. } => {
                let (before, several) = {
                    let s = self.session.borrow();
                    let list: &[isocast_protocol::stream::StreamStatus] = if stream.canvas.is_main()
                    {
                        &s.streams
                    } else if stream.canvas.is_vertical() {
                        &s.vertical_streams
                    } else {
                        // A canvas the session doesn't know (yet): no
                        // earlier state.
                        s.canvases
                            .iter()
                            .find(|c| c.id == stream.canvas)
                            .map_or(&[], |c| c.streams.as_slice())
                    };
                    let before = list.iter().find(|x| x.id == stream.id).map(|x| x.state);
                    (before, list.len() > 1)
                };
                self.session.borrow_mut().set_stream(stream.clone());
                // Each destination says so itself when there are several.
                let canvas = if stream.canvas.is_main() {
                    String::new()
                } else {
                    format!(
                        "{} ",
                        self.session
                            .borrow()
                            .canvas_label(stream.canvas)
                            .to_lowercase()
                    )
                };
                let what = if several {
                    format!("the {canvas}stream to {}", stream.id)
                } else {
                    format!("the {canvas}stream")
                };
                if stream.state == StreamState::Failed && before != Some(StreamState::Failed) {
                    let why = stream
                        .last_error
                        .clone()
                        .unwrap_or_else(|| "It couldn’t stay connected.".into());
                    self.toast(
                        ToastKind::Error,
                        &format!("{} stopped", capital(&what)),
                        why,
                    );
                } else if stream.state == StreamState::Reconnecting
                    && before == Some(StreamState::Live)
                {
                    let why = stream
                        .last_error
                        .clone()
                        .unwrap_or_else(|| "The connection dropped.".into());
                    self.toast(ToastKind::Warning, &format!("Reconnecting {what}"), why);
                }
                self.changed(Changed::STREAM);
            }
            Event::EngineChanged { running } => {
                if *running {
                    self.refresh_status(true);
                } else {
                    self.session.borrow_mut().running.clear();
                    self.changed(Changed::RUNNING);
                }
            }
            Event::Stopped { error, report, .. } => {
                // A newer recording may already have started. Only a fresh
                // status can say whether we're idle now.
                self.session.borrow_mut().record_epoch += 1;
                self.refresh_status(true);
                if let Some(report) = report {
                    self.audio_stops(report);
                }
                // A refused start also says so in its own words: one toast
                // for the two (the same key), titled by the start.
                if let Some(error) = error {
                    let key = format!("rec-error:{error}");
                    if !self.toasts.has_key(&key) {
                        self.show_toast(
                            Toast::new(ToastKind::Error, "The recording stopped", error.clone())
                                .key(key),
                        );
                    }
                }
            }
            Event::Warning { message } => {
                // Areas may take warnings they show better (dropped frames).
                let taken = self.areas().iter().any(|a| a.warning(self, message));
                if !taken {
                    self.toast(ToastKind::Warning, "Warning", sentence(message));
                }
            }
            Event::Marker { .. } => {}
            // The Controls area shows its notice.
            Event::EncoderOverload { .. } => {}
            // Automatic quality (T.2): the status bar and Settings show it.
            Event::GovernorChanged { governor } => {
                if let Some(status) = self.session.borrow_mut().status.as_mut() {
                    status.governor = Some(governor.clone());
                }
                self.changed(Changed::STATUS);
            }
            // Shown by the remote-engines UI (R2).
            Event::Refused { .. } => {}
            // Presence/remote settings are presented by the R2 remote UI.
            Event::RemoteClientConnected { .. }
            | Event::RemoteClientDisconnected { .. }
            | Event::RemotePaired { .. }
            | Event::RemoteRevoked { .. }
            | Event::RemoteDisabled
            | Event::RemoteHeartbeat => {}
        }
        for area in self.areas() {
            area.event(self, &event);
        }
    }

    // ---------- What changed ----------

    /// Tell the window what changed; it is shown once this turn of the
    /// event loop is over, however many changes come in it.
    pub fn changed(self: &Rc<Self>, changed: Changed) {
        self.pending.set(self.pending.get() | changed);
        if !self.flush_timer.running() {
            let weak = Rc::downgrade(self);
            self.flush_timer
                .start(slint::TimerMode::SingleShot, Duration::ZERO, move || {
                    if let Some(app) = weak.upgrade() {
                        app.flush();
                    }
                });
        }
    }

    /// Show what changed now (also used by snapshots between steps).
    pub fn flush(self: &Rc<Self>) {
        let changed = self.pending.replace(Changed::NONE);
        if changed.is_empty() {
            return;
        }
        let Some(window) = self.window() else { return };
        let started = Instant::now();
        self.drop_stale_selection();
        self.show_ui(&window, changed);
        for area in self.areas() {
            area.update(self, &window, changed);
        }
        if let Some(log) = self.work_log.borrow_mut().as_mut() {
            log.flushes.push(started.elapsed());
        }
    }

    fn show_ui(&self, window: &AppWindow, changed: Changed) {
        let ui = window.global::<Ui>();
        let s = self.session.borrow();
        if changed.any(Changed::LINK) {
            ui.set_connection(match s.link {
                None | Some(Link::Connecting) => ConnectionState::Connecting,
                Some(Link::Connected) => ConnectionState::Connected,
                Some(Link::Disconnected) => ConnectionState::Disconnected,
                Some(Link::Failed) => ConnectionState::Failed,
                Some(Link::Mismatch) => ConnectionState::Mismatch,
                Some(Link::IdentityChanged) => ConnectionState::IdentityChanged,
            });
            ui.set_connection_error(s.link_error.as_str().into());
            if let Some(hello) = crate::engine::hello() {
                ui.set_engine_versions(
                    format!(
                        "This app: {} ({})\n{}: {} ({})",
                        env!("CARGO_PKG_VERSION"),
                        short_commit(isocast_client::COMMIT),
                        hello.name,
                        hello.build.version,
                        short_commit(&hello.build.commit),
                    )
                    .into(),
                );
            }
        }
        if changed.any(Changed::SETUP | Changed::LINK) {
            ui.set_loaded(s.loaded);
            ui.set_has_setup(s.setup.is_some());
            let name = s.setup.as_ref().map(|x| x.name.clone()).unwrap_or_default();
            // A window on another computer's engine names it (decision 5).
            let title = match (crate::engine::name(), name.is_empty()) {
                (None, true) => "Isocast".to_string(),
                (None, false) => format!("Isocast — {name}"),
                (Some(engine), true) => format!("Isocast — {engine}"),
                (Some(engine), false) => format!("Isocast — {engine} — {name}"),
            };
            window.set_title_text(title.into());
            ui.set_setup_name(name.into());
            // The canvas being edited: an extra one's while editing it.
            if let Some(setup) = s.edited_setup() {
                ui.set_canvas_width(setup.canvas.width as i32);
                ui.set_canvas_height(setup.canvas.height as i32);
                ui.set_canvas_fps(setup.canvas.fps as i32);
            }
            // The extra canvases (#120): the chosen one's monitor and the
            // monitor's canvas switcher.
            ui.set_has_extra(s.extra.is_some());
            ui.set_editing_extra(s.editing_extra());
            if let (Some(extra), Some(canvas)) = (&s.extra, s.extra_canvas()) {
                ui.set_extra_width(extra.canvas.width as i32);
                ui.set_extra_height(extra.canvas.height as i32);
                ui.set_extra_id(canvas.as_str().into());
                ui.set_extra_label(s.canvas_label(canvas).into());
            }
            let canvases: Vec<crate::ExtraCanvas> = s
                .extra_canvases()
                .into_iter()
                .filter_map(|canvas| {
                    let size = s.setup.as_ref()?.extra_canvas(canvas)?;
                    Some(crate::ExtraCanvas {
                        id: canvas.as_str().into(),
                        label: s.canvas_label(canvas).into(),
                        width: size.width as i32,
                        height: size.height as i32,
                    })
                })
                .collect();
            ui.set_extra_canvases(slint::ModelRc::new(slint::VecModel::from(canvases)));
        }
        if changed.any(Changed::SCENES | Changed::SETUP) {
            ui.set_on_air_scene(s.on_air().cloned().unwrap_or_default().into());
            ui.set_preview_scene(s.staged().cloned().unwrap_or_default().into());
            ui.set_editing_scene(s.editing_scene().unwrap_or_default().into());
            ui.set_studio(s.studio);
        }
        if changed.any(Changed::STATUS | Changed::STREAM) {
            // Green is for the program and live: the LIVE badge and the
            // program's green ring follow the stream only (Paper's Studio
            // Mode board); recording alone lights the LCD's red.
            let streaming = s
                .stream
                .as_ref()
                .is_some_and(|x| matches!(x.state, StreamState::Live | StreamState::Reconnecting));
            ui.set_live(streaming);
        }
        if changed.any(Changed::SELECTION | Changed::SETUP | Changed::SCENES) {
            let path = s
                .selection
                .as_ref()
                .map(|x| x.path.clone())
                .unwrap_or_default();
            let (source, name, kind, icon) = s
                .selected(|_, chain| {
                    let item = chain[chain.len() - 1];
                    let setup = s.setup.as_ref().expect("a selection is in a setup");
                    let source = item.source.clone().unwrap_or_default();
                    let kind = item
                        .source
                        .as_deref()
                        .and_then(|id| model::find_source(setup, id))
                        .map(|src| model::kind_info(src.kind.label()).label)
                        .unwrap_or("");
                    let icon = model::item_icon(setup, item);
                    (
                        source,
                        model::item_label(setup, item),
                        kind.to_string(),
                        icon,
                    )
                })
                .unwrap_or_default();
            ui.set_selected_item(path.into());
            ui.set_selected_source(source.into());
            ui.set_selected_name(name.into());
            ui.set_selected_kind(kind.into());
            ui.set_selected_icon(icon.into());
        }
        if changed.any(Changed::SELECTION | Changed::SETUP | Changed::SCENES | Changed::RUNNING) {
            let source = ui.get_selected_source();
            ui.set_selected_meta(
                s.running
                    .get(source.as_str())
                    .map(model::source_format)
                    .unwrap_or_default()
                    .into(),
            );
            let scene = s.editing_scene().and_then(|id| {
                let scene = model::find_scene(s.edited_setup()?, &id)?;
                let n = scene.items.len();
                Some(format!(
                    "{} · {n} source{}",
                    model::scene_name(scene),
                    if n == 1 { "" } else { "s" }
                ))
            });
            // Nothing to edit (no setup, no scenes): Paper's Welcome board.
            ui.set_scene_meta(
                scene
                    .unwrap_or_else(|| "No scene · 0 sources".to_string())
                    .into(),
            );
        }
    }

    /// Drop a selection whose item is gone, or that is in another scene
    /// than the one being edited.
    fn drop_stale_selection(&self) {
        let mut s = self.session.borrow_mut();
        let gone = s.selection.is_some()
            && (s.selected(|_, _| ()).is_none()
                || s.selection.as_ref().map(|x| Some(x.scene.clone())) != Some(s.editing_scene()));
        if gone {
            s.selection = None;
            self.pending.set(self.pending.get() | Changed::SELECTION);
        }
    }

    // ---------- The live setup ----------

    /// Read the live setup from the daemon, keeping the edits queued.
    pub fn reload(self: &Rc<Self>) {
        self.call_as::<Live>(Method::Get(None), |app, result| match result {
            Ok(live) => {
                {
                    let mut s = app.session.borrow_mut();
                    let setup = live
                        .setup
                        .as_ref()
                        .and_then(|x| serde_json::to_value(x).ok());
                    s.confirm(setup, live.revision);
                    s.recompute();
                    s.loaded = true;
                }
                app.changed(Changed::SETUP | Changed::CONFIRMED | Changed::LINK);
                // An apply that removed the scene on air puts another up.
                let missing = {
                    let s = app.session.borrow();
                    let ids: Vec<&str> = s
                        .setup
                        .as_ref()
                        .map(|x| x.scenes.iter().map(|sc| sc.id.as_str()).collect())
                        .unwrap_or_default();
                    let gone =
                        |id: &Option<String>| id.as_deref().is_some_and(|i| !ids.contains(&i));
                    gone(&s.scene) || gone(&s.preview)
                };
                if missing {
                    app.refresh_status(true);
                }
            }
            Err(error) => app.failed("Couldn’t read the live setup", &error),
        });
    }

    /// Change the live setup by a merge patch (isocast_protocol::change's
    /// semantics: lists of items with ids merge by id). Shown at once,
    /// sent in order, one at a time.
    pub fn edit(self: &Rc<Self>, patch: Value, options: EditOptions) {
        self.edit_then(patch, options, |_, _| {});
    }

    /// `edit`, then `done` with whether the daemon applied it. While the
    /// canvas editor edits an extra canvas, what the patch changes in
    /// scenes' items goes to their arrangement for it (#86, #120).
    pub fn edit_then(
        self: &Rc<Self>,
        patch: Value,
        options: EditOptions,
        done: impl FnOnce(&Rc<App>, bool) + 'static,
    ) {
        let canvas = self.session.borrow().edit_canvas();
        self.edit_on_then(patch, options, canvas, done);
    }

    /// `edit` of canvas `canvas`'s items whichever canvas is being edited
    /// now (an Undo made while editing the other one).
    pub fn edit_on(self: &Rc<Self>, patch: Value, options: EditOptions, canvas: CanvasId) {
        self.edit_on_then(patch, options, canvas, |_, _| {});
    }

    fn edit_on_then(
        self: &Rc<Self>,
        patch: Value,
        options: EditOptions,
        canvas: CanvasId,
        done: impl FnOnce(&Rc<App>, bool) + 'static,
    ) {
        {
            let mut s = self.session.borrow_mut();
            let patch = match s.setup_json() {
                Some(shown) if model::canvas_json(shown, canvas).is_object() => {
                    model::route_canvas(shown, canvas, patch)
                }
                _ => patch,
            };
            if !options.not_undoable {
                s.remember(options.key.as_deref());
            }
            let merge = options.key.is_some()
                && s.queue.back().is_some_and(|last| {
                    last.key == options.key && matches!(last.change, EditChange::Patch(_))
                });
            if merge {
                let last = s.queue.back_mut().expect("checked above");
                if let EditChange::Patch(existing) = &mut last.change {
                    *existing = merge_patch(existing, &patch);
                }
                last.done.push(Box::new(done));
            } else {
                s.queue.push_back(Edit {
                    change: EditChange::Patch(patch),
                    key: options.key,
                    interval: options.interval,
                    retried: false,
                    done: vec![Box::new(done) as EditDone],
                });
            }
            s.recompute();
        }
        self.changed(Changed::SETUP);
        self.pump();
    }

    /// Replace the whole live setup (structural changes made here).
    pub fn replace_setup(self: &Rc<Self>, setup: Value, undoable: bool) {
        self.replace_setup_then(setup, undoable, |_, _| {});
    }

    /// `replace_setup`, then `done` with whether the daemon applied it.
    pub fn replace_setup_then(
        self: &Rc<Self>,
        setup: Value,
        undoable: bool,
        done: impl FnOnce(&Rc<App>, bool) + 'static,
    ) {
        {
            let mut s = self.session.borrow_mut();
            if undoable {
                s.remember(None);
            }
            s.queue.push_back(Edit {
                change: EditChange::Setup(setup),
                key: None,
                interval: Duration::ZERO,
                retried: false,
                done: vec![Box::new(done) as EditDone],
            });
            s.recompute();
        }
        self.changed(Changed::SETUP);
        self.pump();
    }

    pub fn undo(self: &Rc<Self>) -> bool {
        self.step_history(|s| s.take_undo())
    }

    pub fn redo(self: &Rc<Self>) -> bool {
        self.step_history(|s| s.take_redo())
    }

    /// Undo or redo one step of the setup's history. Developer mode (#47)
    /// is never part of it: each snapshot keeps the current `developer`
    /// value (so a toggle from Settings, the CLI, MCP or the auto-off
    /// survives), and a step that differed only in it is passed over.
    fn step_history(
        self: &Rc<Self>,
        take: impl Fn(&mut crate::session::Session) -> Option<Value>,
    ) -> bool {
        loop {
            let (step, current) = {
                let mut s = self.session.borrow_mut();
                let step = take(&mut s);
                (step, s.setup_json().cloned())
            };
            let Some(mut setup) = step else {
                return false;
            };
            if let Some(map) = setup.as_object_mut() {
                match current.as_ref().and_then(|c| c.get("developer")) {
                    Some(developer) => {
                        map.insert("developer".into(), developer.clone());
                    }
                    None => {
                        map.remove("developer");
                    }
                }
            }
            if current.as_ref() == Some(&setup) {
                continue;
            }
            self.replace_setup(setup, false);
            return true;
        }
    }

    /// Send the next queued edits: one at a time, except gesture steps
    /// (`Edit::pipelines`), which go while up to [`PIPELINE`] earlier steps
    /// are still in flight, each planned against the revision the step
    /// before it will make. A step then never waits for the reply to the
    /// one before to come back through the window's thread, so a drag
    /// keeps up with the pointer. Each waits out its `interval` first.
    fn pump(self: &Rc<Self>) {
        loop {
            let request = {
                let mut s = self.session.borrow_mut();
                let Some(next) = s.queue.front() else { return };
                let room = s.inflight.is_empty()
                    || (next.pipelines()
                        && s.retry.is_empty()
                        && s.inflight.len() < PIPELINE
                        && s.inflight.iter().all(|(edit, _)| edit.pipelines()));
                if !room || s.waiting {
                    return;
                }
                let interval = next.interval;
                let since = s.last_sent.map_or(Duration::MAX, crate::clock::since);
                if since < interval {
                    s.waiting = true;
                    let weak = Rc::downgrade(self);
                    self.edit_timer.start(
                        slint::TimerMode::SingleShot,
                        interval - since,
                        move || {
                            if let Some(app) = weak.upgrade() {
                                app.session.borrow_mut().waiting = false;
                                app.pump();
                            }
                        },
                    );
                    return;
                }
                let edit = s.queue.pop_front().expect("checked above");
                let change = match &edit.change {
                    EditChange::Patch(patch) => Change::Patch(patch.clone()),
                    EditChange::Setup(setup) => match serde_json::from_value(setup.clone()) {
                        Ok(setup) => Change::Setup(Box::new(setup)),
                        Err(error) => {
                            // Never sent, so `applied` won't see it: settle
                            // it here.
                            s.recompute();
                            drop(s);
                            self.toast(
                                ToastKind::Error,
                                "Couldn’t apply the change",
                                format!("The setup isn’t valid: {error}"),
                            );
                            self.changed(Changed::SETUP);
                            for done in edit.done {
                                done(self, false);
                            }
                            continue;
                        }
                    },
                };
                let live = edit.key.is_some();
                // After the steps in flight, each of which makes a revision.
                let revision = s
                    .inflight
                    .back()
                    .map_or(s.revision, |(_, planned)| planned + 1);
                s.inflight.push_back((edit, revision));
                s.applies_sent += 1;
                s.most_in_flight = s.most_in_flight.max(s.inflight.len());
                s.last_sent = Some(crate::clock::now());
                ApplyParams {
                    revision,
                    change,
                    live,
                }
            };
            self.call_as::<isocast_protocol::api::Applied>(
                Method::Apply(Box::new(request)),
                |app, result| app.applied(result),
            );
        }
    }

    fn applied(self: &Rc<Self>, result: Result<isocast_protocol::api::Applied, CallError>) {
        let Some((edit, _)) = self.session.borrow_mut().inflight.pop_front() else {
            return;
        };
        match result {
            Ok(applied) => {
                let superseded = {
                    let mut s = self.session.borrow_mut();
                    let setup = match &edit.change {
                        EditChange::Setup(setup) => Some(setup.clone()),
                        EditChange::Patch(patch) => {
                            s.confirmed.as_ref().map(|doc| merge_patch(doc, patch))
                        }
                    };
                    s.confirm(setup, applied.revision);
                    // Earlier steps of this gesture waiting to be sent again
                    // are older than this one: it replaces them.
                    let (gone, kept): (VecDeque<Edit>, VecDeque<Edit>) =
                        std::mem::take(&mut s.retry)
                            .into_iter()
                            .partition(|old| old.key.is_some() && old.key == edit.key);
                    s.retry = kept;
                    s.recompute();
                    gone
                };
                self.changed(Changed::SETUP | Changed::CONFIRMED);
                for done in superseded.into_iter().flat_map(|e| e.done) {
                    done(self, true);
                }
                for done in edit.done {
                    done(self, true);
                }
                self.after_edit();
            }
            Err(error) => {
                if error.is_stale() {
                    self.session.borrow_mut().applies_stale += 1;
                }
                let retry = error.is_stale()
                    && matches!(edit.change, EditChange::Patch(_))
                    && !edit.retried;
                if retry {
                    // Changed elsewhere meanwhile, or a pipelined step was
                    // planned against a revision an earlier step didn't
                    // make (it changed nothing): once the steps in flight
                    // are back, take the daemon's setup and make this edit
                    // on top of it.
                    let ready = {
                        let mut s = self.session.borrow_mut();
                        s.retry.push_back(edit);
                        s.inflight.is_empty()
                    };
                    if ready {
                        self.resend_stale();
                    }
                    return;
                }
                let title = if error.is_stale() {
                    "The setup changed elsewhere; your change wasn’t applied"
                } else {
                    "Couldn’t apply the change"
                };
                let message = self
                    .session
                    .borrow()
                    .setup
                    .as_ref()
                    .map_or(error.message.clone(), |setup| {
                        named_paths(&error.message, setup)
                    });
                self.session.borrow_mut().last_refusal = Some(message.clone());
                self.toast(ToastKind::Error, title, message);
                self.session.borrow_mut().recompute();
                self.changed(Changed::SETUP);
                for done in edit.done {
                    done(self, false);
                }
                self.reload();
                self.after_edit();
            }
        }
    }

    /// The edits the daemon refused as stale go back to the front of the
    /// queue (in order, a gesture's steps merged), and are sent again on
    /// top of the daemon's setup as it is now.
    fn resend_stale(self: &Rc<Self>) {
        {
            let mut s = self.session.borrow_mut();
            let mut stale = std::mem::take(&mut s.retry);
            while let Some(mut edit) = stale.pop_back() {
                edit.retried = true;
                let same_gesture = edit.key.is_some()
                    && s.queue
                        .front()
                        .is_some_and(|next| next.key == edit.key && next.pipelines());
                if !same_gesture {
                    s.queue.push_front(edit);
                    continue;
                }
                let next = s.queue.front_mut().expect("checked above");
                if let (EditChange::Patch(older), EditChange::Patch(newer)) =
                    (&edit.change, &mut next.change)
                {
                    *newer = merge_patch(older, newer);
                }
                next.retried = true;
                let mut done = edit.done;
                done.append(&mut next.done);
                next.done = done;
            }
        }
        self.call_as::<Live>(Method::Get(None), |app, result| {
            if let Ok(live) = result {
                let mut s = app.session.borrow_mut();
                let setup = live
                    .setup
                    .as_ref()
                    .and_then(|x| serde_json::to_value(x).ok());
                s.confirm(setup, live.revision);
                s.recompute();
            }
            app.changed(Changed::SETUP | Changed::CONFIRMED);
            app.pump();
        });
    }

    /// The next edit, or, once edits settle, the daemon's own copy (it
    /// fills in defaults).
    fn after_edit(self: &Rc<Self>) {
        let idle = !self.session.borrow().editing();
        if !idle {
            self.pump();
            return;
        }
        let weak = Rc::downgrade(self);
        slint::Timer::single_shot(Duration::from_millis(500), move || {
            if let Some(app) = weak.upgrade()
                && !app.session.borrow().editing()
            {
                app.reload();
            }
        });
    }

    // ---------- The engine ----------

    /// Read the engine's status (`quiet`: no toast if it fails).
    pub fn refresh_status(self: &Rc<Self>, quiet: bool) {
        let asked = crate::clock::now();
        let epoch = self.session.borrow().record_epoch;
        self.call_as::<Status>(Method::Status, move |app, result| match result {
            Ok(mut status) => {
                {
                    let mut s = app.session.borrow_mut();
                    if s.record_epoch != epoch
                        || s.status_applied_from.is_some_and(|newer| asked < newer)
                    {
                        return;
                    }
                    s.status_applied_from = Some(asked);
                    // An older daemon omits finalizing. Its blocking Stop
                    // fallback still owns this window's busy state until the
                    // reply; a queued status read must not erase that intent.
                    if s.busy_record && s.finalizing() {
                        status.finalizing = true;
                    }
                    s.running = status
                        .engine
                        .as_ref()
                        .map(|e| {
                            e.sources
                                .iter()
                                .map(|x| (x.source.clone(), x.clone()))
                                .collect()
                        })
                        .unwrap_or_default();
                    // A read from before this window's own switch is older
                    // than what it shows.
                    let fresh = s
                        .switched_at
                        .is_none_or(|t| asked > t + Duration::from_millis(300));
                    if fresh {
                        s.scene = status.scene.clone();
                        s.preview = status.preview.clone();
                    }
                    if !s.busy_stream {
                        s.set_streams(status.stream.clone(), status.streams.clone());
                    }
                    if !s.busy_canvas_stream {
                        s.vertical_streams = status.vertical_streams.clone();
                        s.canvases = status.canvases.clone();
                    }
                    if fresh {
                        s.set_canvas_scenes(&status.canvases);
                        s.studio = s.staged().is_some();
                    }
                    if !s.busy_vcam {
                        s.vcam = status.virtual_camera.clone();
                    }
                    if !s.busy_replay {
                        s.replay = status.replay.clone();
                    }
                    s.status = Some(status);
                }
                app.changed(
                    Changed::STATUS
                        | Changed::SCENES
                        | Changed::RUNNING
                        | Changed::STREAM
                        | Changed::VCAM
                        | Changed::REPLAY,
                );
            }
            Err(error) => {
                let connected = app.session.borrow().link == Some(Link::Connected);
                // Not while an update, or the permissions step's Relaunch,
                // stops the engine on purpose.
                if !quiet
                    && connected
                    && !crate::update::stopping_engine()
                    && !app.permissions.relaunching()
                {
                    app.failed("Couldn’t read the engine’s status", &error);
                }
            }
        });
    }

    /// The devices on this Mac (cached; `fresh` asks again).
    pub fn load_devices(self: &Rc<Self>, fresh: bool) {
        if self.session.borrow().devices.is_some() && !fresh {
            return;
        }
        self.call_as::<Devices>(Method::Devices, |app, result| match result {
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
        });
    }

    /// Click on a scene: on air, or into preview in Studio Mode. While
    /// editing an extra canvas with scenes of its own, its scene (#86);
    /// one that follows the main canvas switches with it.
    pub fn choose_scene(self: &Rc<Self>, id: &str) {
        let (studio, canvas) = {
            let mut s = self.session.borrow_mut();
            s.switched_at = Some(crate::clock::now());
            s.selection = None;
            let canvas = s.scene_canvas();
            let studio = s.studio;
            match (studio, canvas) {
                (true, CanvasId::Main) => s.preview = Some(id.to_string()),
                (false, CanvasId::Main) => s.scene = Some(id.to_string()),
                (true, other) => {
                    s.canvas_previews.insert(other, id.to_string());
                }
                (false, other) => {
                    s.canvas_scenes.insert(other, id.to_string());
                }
            }
            (studio, canvas)
        };
        self.changed(Changed::SCENES | Changed::SELECTION);
        let scene = id.to_string();
        if studio {
            self.call(Method::Preview { scene, canvas }, |app, result| {
                if let Err(error) = result {
                    app.failed("Couldn’t put the scene in preview", &error);
                    app.refresh_status(true);
                }
            });
        } else {
            self.call(
                Method::SwitchScene {
                    scene,
                    transition: None,
                    canvas,
                },
                |app, result| {
                    if let Err(error) = result {
                        app.failed("Couldn’t switch scenes", &error);
                        app.refresh_status(true);
                    }
                },
            );
        }
    }

    /// Studio Mode: put the preview on air with `transition` (a quick
    /// transition) or the setup's; an extra canvas's own preview while
    /// editing it, if it has scenes of its own (#86).
    pub fn take(self: &Rc<Self>, transition: Option<isocast_protocol::Transition>) {
        let canvas = {
            let mut s = self.session.borrow_mut();
            let canvas = s.scene_canvas();
            let Some(preview) = s.staged().cloned() else {
                return;
            };
            s.switched_at = Some(crate::clock::now());
            match canvas {
                CanvasId::Main => {
                    let on_air = s.scene.replace(preview);
                    s.preview = on_air;
                }
                other => {
                    match s.canvas_scenes.insert(other, preview) {
                        Some(on_air) => s.canvas_previews.insert(other, on_air),
                        None => s.canvas_previews.remove(&other),
                    };
                }
            }
            canvas
        };
        self.changed(Changed::SCENES);
        self.call(Method::Take { transition, canvas }, |app, result| {
            if let Err(error) = result {
                app.failed("Couldn’t take the preview to air", &error);
                app.refresh_status(true);
            }
        });
    }

    pub fn set_studio(self: &Rc<Self>, on: bool) {
        let stage = {
            let mut s = self.session.borrow_mut();
            s.studio = on;
            s.switched_at = Some(crate::clock::now());
            if !on {
                s.preview = None;
            }
            s.selection = None;
            (on && s.preview.is_none())
                .then(|| s.scene.clone())
                .flatten()
        };
        if let Some(window) = self.window() {
            window.global::<Ui>().set_studio(on);
            if !self.mode.offscreen {
                prefs::save(&window);
            }
        }
        self.changed(Changed::SCENES | Changed::SELECTION);
        if let Some(scene) = stage {
            self.session.borrow_mut().preview = Some(scene.clone());
            self.call(
                Method::Preview {
                    scene,
                    canvas: CanvasId::Main,
                },
                |app, result| {
                    if let Err(error) = result {
                        app.failed("Couldn’t start studio mode", &error);
                    }
                },
            );
        } else if !on {
            self.call(Method::ClearPreview, |app, result| {
                if let Err(error) = result {
                    app.failed("Couldn’t leave studio mode", &error);
                }
            });
        }
        self.stage_extra_preview();
    }

    /// Studio Mode while editing an extra canvas with scenes of its own
    /// (#86, #120): its own preview starts with the scene on air on it,
    /// and is cleared when Studio Mode ends.
    pub fn stage_extra_preview(self: &Rc<Self>) {
        let (canvas, action) = {
            let mut s = self.session.borrow_mut();
            let canvas = s.scene_canvas();
            if canvas.is_main() {
                return;
            }
            let action = match (s.studio, s.canvas_previews.contains_key(&canvas)) {
                (true, false) => {
                    let scene = s
                        .canvas_scenes
                        .get(&canvas)
                        .cloned()
                        .or_else(|| s.editing_scene());
                    if let Some(scene) = &scene {
                        s.canvas_previews.insert(canvas, scene.clone());
                    }
                    scene.map(Some)
                }
                (false, true) => {
                    s.canvas_previews.remove(&canvas);
                    Some(None)
                }
                _ => None,
            };
            (canvas, action)
        };
        match action {
            Some(Some(scene)) => {
                self.changed(Changed::SCENES);
                self.call(Method::Preview { scene, canvas }, |app, result| {
                    if let Err(error) = result {
                        app.failed("Couldn’t put the canvas’s scene in preview", &error);
                        app.refresh_status(true);
                    }
                });
            }
            Some(None) => {
                self.changed(Changed::SCENES);
                self.call(Method::ClearCanvasPreview { canvas }, |app, result| {
                    if let Err(error) = result {
                        app.failed("Couldn’t clear the canvas’s preview", &error);
                    }
                });
            }
            None => {}
        }
    }

    /// Select the item at `path` in the scene being edited ("" for none).
    pub fn select(self: &Rc<Self>, path: &str) {
        {
            let mut s = self.session.borrow_mut();
            let scene = s.editing_scene();
            s.selection = match (path.is_empty(), scene) {
                (false, Some(scene)) => Some(Selection {
                    scene,
                    path: path.to_string(),
                }),
                _ => None,
            };
        }
        self.changed(Changed::SELECTION);
    }

    // ---------- Dialogs, menus, toasts ----------

    pub fn open_dialog(self: &Rc<Self>, kind: &str, arg: &str) {
        if let Some(window) = self.window() {
            let ui = window.global::<Ui>();
            ui.set_dialog_arg(arg.into());
            ui.set_dialog(kind.into());
            window.global::<Menus>().invoke_close();
        }
        for area in self.areas() {
            area.dialog_opened(self, kind, arg);
        }
    }

    pub fn close_dialog(self: &Rc<Self>) {
        if let Some(window) = self.window() {
            let ui = window.global::<Ui>();
            ui.set_dialog(SharedString::new());
            ui.set_dialog_arg(SharedString::new());
        }
    }

    /// An item of a menu was chosen.
    fn menu_activated(weak: &std::rc::Weak<App>, owner: SharedString, id: SharedString) {
        // On the next turn of the event loop, once the click that chose
        // it is over and the menu's popup is gone: a dialog opened while
        // the menu still held the pointer ignored later clicks.
        let weak = weak.clone();
        slint::Timer::single_shot(std::time::Duration::ZERO, move || {
            if let Some(app) = weak.upgrade() {
                // Open Projector items come first: they're added to
                // other areas' menus.
                if app.projectors.chosen(&app, &id) {
                    return;
                }
                for area in app.areas() {
                    if area.menu_chosen(&app, &owner, &id) {
                        return;
                    }
                }
            }
        });
    }

    /// `owner`'s menu, built by the area that knows the owner.
    fn menu_for(self: &Rc<Self>, owner: &str) -> Option<MenuBuilder> {
        self.areas()
            .iter()
            .find_map(|area| area.context_menu(self, owner))
            // Open Projector, on the preview's, a scene's and a source's.
            .map(|menu| self.projectors.extend_menu(self, owner, menu))
    }

    /// A menu asked for by `owner` at `anchor` (window points: x, y,
    /// width, height; a point for a context menu): the area that knows
    /// the owner builds it.
    fn request_menu(self: &Rc<Self>, owner: &str, anchor: [f32; 4]) {
        if let Some(menu) = self.menu_for(owner) {
            self.show_menu(owner, menu, anchor);
        }
    }

    /// Show a menu built in Rust.
    pub fn show_menu(&self, owner: &str, menu: MenuBuilder, [x, y, w, h]: [f32; 4]) {
        let Some(window) = self.window() else { return };
        let (list, submenus) = menu.build();
        window
            .global::<Menus>()
            .invoke_show(owner.into(), list, submenus, x, y, w, h);
    }

    pub fn toast(self: &Rc<Self>, kind: ToastKind, title: &str, message: impl Into<String>) -> i32 {
        self.toasts.show(self, Toast::new(kind, title, message))
    }

    pub fn show_toast(self: &Rc<Self>, toast: Toast) -> i32 {
        self.toasts.show(self, toast)
    }

    /// Report a failed call in the daemon's own words.
    pub fn failed(self: &Rc<Self>, title: &str, error: &CallError) {
        self.toast(ToastKind::Error, title, error.message.clone());
    }

    /// A menu command (the menu bar's, or a key's). The window's own
    /// commands are handled here; the rest go to the areas.
    pub fn menu(self: &Rc<Self>, id: &str) {
        let Some(window) = self.window() else { return };
        let ui = window.global::<Ui>();
        let saved = |app: &Rc<App>| {
            if !app.mode.offscreen {
                prefs::save(&window);
            }
        };
        match id {
            "undo" => {
                self.undo();
            }
            "redo" => {
                self.redo();
            }
            "deselect" => self.select(""),
            "source_toolbar" => {
                ui.set_show_source_toolbar(!ui.get_show_source_toolbar());
                saved(self);
            }
            "status_bar" => {
                ui.set_show_status_bar(!ui.get_show_status_bar());
                saved(self);
            }
            "docks_pane" => {
                ui.set_show_docks(!ui.get_show_docks());
                saved(self);
            }
            "lock_preview" => {
                ui.set_preview_locked(!ui.get_preview_locked());
                saved(self);
            }
            "lock_docks" => {
                ui.set_docks_locked(!ui.get_docks_locked());
                saved(self);
            }
            "reset_ui" => {
                prefs::reset(&window, false);
                // The canvas monitor back to the first extra canvas (#120),
                // after editing the main canvas again (the edited canvas
                // never changes under the editor).
                if self.session.borrow().editing_extra() {
                    self.menu("edit_canvas_main");
                }
                {
                    let mut s = self.session.borrow_mut();
                    s.extra_choice = None;
                    s.refresh_extra();
                }
                self.changed(Changed::SETUP);
                saved(self);
            }
            "reset_docks" => {
                prefs::reset(&window, true);
                saved(self);
            }
            "dock_scenes" => {
                ui.set_show_scenes(!ui.get_show_scenes());
                saved(self);
            }
            "dock_sources" => {
                ui.set_show_sources(!ui.get_show_sources());
                saved(self);
            }
            "dock_mixer" => {
                ui.set_show_mixer(!ui.get_show_mixer());
                saved(self);
            }
            "dock_transitions" => {
                ui.set_show_transitions(!ui.get_show_transitions());
                saved(self);
            }
            "dock_controls" => {
                ui.set_show_controls(!ui.get_show_controls());
                saved(self);
            }
            "preview_scale_window" | "preview_scale_canvas" => {
                ui.set_preview_scale(id.trim_start_matches("preview_scale_").into());
                saved(self);
            }
            "reload" => {
                self.reload();
                self.refresh_status(false);
            }
            "minimize" => window.window().set_minimized(true),
            "fullscreen" => {
                let w = window.window();
                let full = !w.is_fullscreen();
                w.set_fullscreen(full);
                // Windows: no menu bar in full screen, as F11 full screen
                // goes there (Windows draws a borderless window's menu bar
                // unthemed); F11 leaves (ui/app.slint's keys).
                #[cfg(windows)]
                if !self.mode.offscreen {
                    window.set_native_menu(!full);
                    crate::chrome_windows::style_windows_soon();
                }
            }
            "zoom" => {
                let w = window.window();
                w.set_maximized(!w.is_maximized());
            }
            "bring_all_to_front" => crate::platform::bring_all_to_front(),
            // Another engine in a window of its own (decision 5): `local`,
            // or a paired engine's id or name. The engine picker (R2 UI)
            // sends this; this window stays on its engine.
            _ if id.starts_with("open_engine:") => {
                let target = &id["open_engine:".len()..];
                let opened = isocast_client::Engine::parse(target)
                    .and_then(|engine| crate::engine::open_window(&engine));
                if let Err(error) = opened {
                    self.toast(
                        ToastKind::Error,
                        "Couldn’t open a window for that engine",
                        format!("{error:#}"),
                    );
                }
            }
            "quit" => ui.invoke_quit(),
            _ => {
                let handled = self.areas().iter().any(|area| area.menu(self, id));
                if !handled {
                    isocast_diagnostics::diagnostic!("warn"; "menu: nothing handles {id:?} yet");
                }
            }
        }
    }
}

/// What a measured run reports of the applies it sent.
pub fn applies_json(app: &App) -> Value {
    let s = app.session.borrow();
    json!({
        "sent": s.applies_sent,
        "most_in_flight": s.most_in_flight,
        "stale": s.applies_stale,
    })
}

/// A status before the daemon's first answer.
fn empty_status(state: EngineState) -> Status {
    serde_json::from_value(json!({ "state": state })).expect("a minimal status")
}

/// An engine message with the setup paths people don't know by number
/// named: `sources[7].width: …` → `“Color 2” width: …`, `scenes[1]` →
/// `scene “Bars”`.
pub fn named_paths(message: &str, setup: &isocast_protocol::Setup) -> String {
    let mut out = String::new();
    let mut rest = message;
    while let Some(at) = rest.find("sources[").or_else(|| rest.find("scenes[")) {
        let (head, tail) = rest.split_at(at);
        out.push_str(head);
        let (list, after) = if let Some(after) = tail.strip_prefix("sources[") {
            ("sources", after)
        } else {
            ("scenes", &tail["scenes[".len()..])
        };
        let Some(close) = after.find(']') else {
            out.push_str(tail);
            return out;
        };
        let name = after[..close].parse::<usize>().ok().and_then(|i| {
            if list == "sources" {
                setup
                    .sources
                    .get(i)
                    .map(|s| format!("“{}”", model::source_name(s)))
            } else {
                setup
                    .scenes
                    .get(i)
                    .map(|s| format!("scene “{}”", model::scene_name(s)))
            }
        });
        match name {
            Some(name) => {
                out.push_str(&name);
                rest = &after[close + 1..];
                // `.width` after a source reads as its field.
                if let Some(field) = rest.strip_prefix('.') {
                    out.push(' ');
                    rest = field;
                }
            }
            None => {
                out.push_str(&tail[..tail.len() - after.len() + close + 1]);
                rest = &after[close + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

impl App {
    /// The take's audio sources that stopped while it recorded (the
    /// files say so too): which, when, and why.
    pub fn audio_stops(self: &Rc<Self>, report: &isocast_protocol::api::Report) {
        let mut seen: Vec<(u32, &str, f64)> = Vec::new();
        let mut rows = Vec::new();
        let mut reasons: Vec<String> = Vec::new();
        let takes = report.takes.len() > 1;
        for file in &report.files {
            for stop in &file.audio_stops {
                // A later take only repeats it ("stopped before this take").
                if stop.reason.starts_with("it stopped before this take began") {
                    continue;
                }
                let key = (file.take, stop.source.as_str(), stop.time_seconds);
                if seen.contains(&key) {
                    continue;
                }
                seen.push(key);
                let name = self
                    .session
                    .borrow()
                    .setup
                    .as_ref()
                    .and_then(|s| {
                        s.source(&stop.source)
                            .map(|s| model::source_name(s).to_string())
                    })
                    .unwrap_or_else(|| stop.source.clone());
                let mut when = format!("from {:.1} s", stop.time_seconds);
                if takes {
                    when = format!("take {}, {when}", file.take);
                }
                if let Some(back) = stop.resumed_seconds {
                    when.push_str(&format!(" to {back:.1} s"));
                }
                rows.push((name.clone(), when, "warn"));
                reasons.push(format!("{name}: {}", stop.reason));
            }
        }
        if rows.is_empty() {
            return;
        }
        let message = format!(
            "Each source's audio is missing from the take's files from the time shown. {}.",
            reasons.join("; ")
        );
        self.show_toast(
            Toast::new(ToastKind::Warning, "Audio stopped while recording", message)
                .key("audio-stops")
                .rows(rows),
        );
    }
}

/// "the stream dropped" → "The stream dropped".
pub fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// `text` with its first letter capitalized ("the stream" → "The stream").
fn capital(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// A commit as people compare it: its first 8 hex digits.
fn short_commit(commit: &str) -> &str {
    commit.get(..8).unwrap_or(commit)
}
