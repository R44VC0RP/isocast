//! Area I: the setup wizard (#46). On the first launch, once the
//! permissions step is done, OBS's scene collections are looked for
//! (`obs_discover`) and offered, each with its profile's settings, to
//! import (`library_import_obs`, the stream key only if asked). Then, and
//! from Tools › Auto-Configuration Wizard… or after "Start with this Mac",
//! the goal: recording, streaming (with an upload test, `speed_test`) or
//! both; `optimize` picks the settings and benchmarks this Mac; its one
//! line and changes are applied as one undoable edit.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use isocast_protocol::api::{Capacity, Method};
use isocast_protocol::change::Difference;
use isocast_protocol::library::LibraryImportObsParams;
use isocast_protocol::obs::Discovery;
use isocast_protocol::optimize::{
    Goal, ObsDiscoverParams, OptimizeParams, Optimized, SpeedTest, SpeedTestParams, mbps,
};
use isocast_protocol::stream::StreamService;
use serde_json::Value;
use slint::ComponentHandle;

use super::{Area, Changed};
use crate::app::App;
use crate::daemon::Link;
use crate::kit::model;
use crate::{AppWindow, ChangeRow, ObsCollectionRow, ToastKind, WizardArea};

/// This area's snapshot states (see src/areas/mod.rs).
pub const STATES: &[&str] = &[
    "wizard:obs|goal|summary",
    "wizard-set:FIELD=VALUE",
    "wizard-sample:optimizing|error|import-error|OPTIMIZED.json",
    "wizard-speed:SPEED_TEST.json",
    "wizard-first-launch",
    "wizard-act:import|test-upload|apply|close",
];

#[derive(Default)]
struct State {
    discovery: Option<Discovery>,
    selected: Vec<bool>,
    move_key: bool,
    step: &'static str,
    with_obs: bool,
    goal: Option<Goal>,
    service: Option<StreamService>,
    server: String,
    speed: Option<SpeedTest>,
    speed_error: String,
    optimized: Option<Optimized>,
    error: String,
    import_error: String,
    discovering: bool,
    importing: bool,
    testing: bool,
    optimizing: bool,
    applying: bool,
    shown: bool,
}

#[derive(Default)]
pub struct Wizard {
    state: RefCell<State>,
    /// Looked for OBS at the first launch already (once per run).
    offered: Cell<bool>,
}

impl Area for Wizard {
    fn connect(&self, app: &Rc<App>, window: &AppWindow) {
        let area = window.global::<WizardArea>();
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
        on!(on_toggle, |app, index| app
            .wizard
            .toggle(&app, index as usize));
        on!(on_set, |app, field, value| app
            .wizard
            .set(&app, &field, &value));
        on!(on_import, |app| app.wizard.import(&app));
        on!(on_skip_obs, |app| app.wizard.go(&app, "goal"));
        on!(on_back, |app| {
            let back = match app.wizard.state.borrow().step {
                "summary" => "goal",
                _ => "obs",
            };
            app.wizard.go(&app, back);
        });
        on!(on_test_upload, |app| app.wizard.test_upload(&app));
        on!(on_optimize, |app| app.wizard.optimize(&app));
        on!(on_apply, |app| app.wizard.apply(&app));
        on!(on_close, |app| app.wizard.close(&app));
    }

    fn update(&self, app: &Rc<App>, _window: &AppWindow, changed: Changed) {
        if app.mode.offscreen || self.offered.get() || !changed.any(Changed::SETUP | Changed::LINK)
        {
            return;
        }
        let (ready, first) = {
            let s = app.session.borrow();
            (
                s.loaded && s.link == Some(Link::Connected),
                s.setup.is_none(),
            )
        };
        if !ready {
            return;
        }
        self.offered.set(true);
        // The first launch: no setup yet.
        if first {
            self.discover(app, true);
            if crate::engine::os() == isocast_protocol::hello::EngineOs::Linux {
                self.linux_first_run(app);
            }
        }
    }

    fn menu(&self, app: &Rc<App>, id: &str) -> bool {
        match id {
            "auto_config" => self.open_goal(app),
            _ => return false,
        }
        true
    }

    fn state(
        &self,
        app: &Rc<App>,
        _window: &AppWindow,
        name: &str,
        arg: &str,
    ) -> Option<anyhow::Result<()>> {
        match name {
            "wizard" => match arg {
                "obs" => self.open_obs(app),
                "goal" => self.open_goal(app),
                "summary" => {
                    self.open_goal(app);
                    self.go(app, "summary");
                }
                _ => return Some(Err(anyhow::anyhow!("wizard:obs|goal|summary"))),
            },
            "wizard-set" => match arg.split_once('=') {
                Some(("toggle", index)) => self.toggle(app, index.parse().unwrap_or(0)),
                Some((field, value)) => self.set(app, field, value),
                None => return Some(Err(anyhow::anyhow!("wizard-set:FIELD=VALUE"))),
            },
            "wizard-sample" => {
                {
                    let mut s = self.state.borrow_mut();
                    match arg {
                        "optimizing" => {
                            s.step = "summary";
                            s.optimizing = true;
                        }
                        "error" => {
                            s.step = "summary";
                            s.error = "not while recording or streaming: the benchmark would \
                                       compete for the encoders; stop first"
                                .into();
                        }
                        "import-error" => {
                            s.step = "obs";
                            s.import_error = "collection: \"/Users/me/Library/Application \
                                              Support/obs-studio/basic/scenes/Recording.json\" \
                                              is not JSON: EOF while parsing an object at line \
                                              1 column 33"
                                .into();
                        }
                        file => {
                            let read = std::fs::read_to_string(file)
                                .map_err(anyhow::Error::from)
                                .and_then(|text| Ok(serde_json::from_str::<Optimized>(&text)?));
                            match read {
                                Ok(optimized) => {
                                    s.step = "summary";
                                    s.goal = Some(optimized.goal);
                                    s.optimized = Some(optimized);
                                }
                                Err(err) => return Some(Err(err)),
                            }
                        }
                    }
                }
                self.show(app);
            }
            "wizard-speed" => {
                let read = std::fs::read_to_string(arg)
                    .map_err(anyhow::Error::from)
                    .and_then(|text| Ok(serde_json::from_str::<SpeedTest>(&text)?));
                match read {
                    Ok(speed) => {
                        let mut s = self.state.borrow_mut();
                        s.service = Some(speed.service);
                        if let Some(server) = &speed.server
                            && speed.service == StreamService::Custom
                        {
                            s.server = server.clone();
                        }
                        s.speed = Some(speed);
                    }
                    Err(err) => return Some(Err(err)),
                }
                self.show(app);
            }
            "wizard-first-launch" => self.discover(app, true),
            "wizard-act" => match arg {
                "import" => self.import(app),
                "test-upload" => self.test_upload(app),
                "apply" => self.apply(app),
                "close" => self.close(app),
                _ => {
                    return Some(Err(anyhow::anyhow!(
                        "wizard-act:import|test-upload|apply|close"
                    )));
                }
            },
            _ => return None,
        }
        Some(Ok(()))
    }
}

#[cfg(target_os = "linux")]
fn offer_codec(app: &Rc<App>) {
    use crate::toasts::Toast;
    app.show_toast(Toast::new(ToastKind::Info, "Enable software H.264 and live previews?",
        "OpenH264 Video Codec provided by Cisco Systems, Inc. Download from Cisco for your user; no sudo. Remove later with isocast-openh264-install --remove.")
        .action2("Not now", |_| {})
        .action("Download", download_codec));
}

#[cfg(target_os = "linux")]
fn download_codec(app: &Rc<App>) {
    use crate::toasts::Toast;
    const KEY: &str = "openh264-download";
    let mut progress = Toast::new(
        ToastKind::Info,
        "Downloading OpenH264",
        "Fetching and verifying Cisco’s codec…",
    )
    .key(KEY);
    progress.persistent = true;
    app.show_toast(progress);
    std::thread::spawn(|| {
        // Never execute a PATH lookup or a command supplied by an engine.
        let result = std::process::Command::new("/usr/bin/isocast-openh264-install").output();
        let _ = slint::invoke_from_event_loop(move || {
            crate::window::with_app(|app| {
                let notice = match result {
                    Ok(o) if o.status.success() => {
                        if let Some(window) = app.window() {
                            window
                                .global::<crate::SettingsArea>()
                                .set_ob_codec_hint("".into());
                        }
                        Toast::new(
                            ToastKind::Success,
                            "OpenH264 installed",
                            "Software H.264 and live previews are available.",
                        )
                    }
                    Ok(o) => {
                        let stderr = String::from_utf8_lossy(&o.stderr);
                        let reason = stderr
                            .lines()
                            .rev()
                            .map(str::trim)
                            .find(|s| !s.is_empty())
                            .map(str::to_owned)
                            .unwrap_or_else(|| format!("Installer exited with {}.", o.status));
                        Toast::new(ToastKind::Error, "OpenH264 wasn’t installed", reason)
                    }
                    Err(error) => Toast::new(
                        ToastKind::Error,
                        "Couldn’t start isocast-openh264-install",
                        error.to_string(),
                    ),
                };
                app.show_toast(notice.key(KEY));
            })
        });
    });
}

impl Wizard {
    /// Use the engine's doctor report, including for a remote Linux engine.
    /// Remediation shown by another machine is text, never a command to execute.
    fn linux_first_run(&self, app: &Rc<App>) {
        use isocast_protocol::api::{CheckStatus, Doctor, DoctorParams};
        let params = DoctorParams {
            setup: None,
            folder: None,
        };
        app.call_as::<Doctor>(Method::Doctor(Box::new(params)), |app, result| {
            let Some(window) = app.window() else { return };
            let area = window.global::<crate::SettingsArea>();
            let Ok(doctor) = result else {
                area.set_ob_linux_hint(
                    "Couldn’t check capture support. Run isocast doctor on the engine’s computer."
                        .into(),
                );
                return;
            };
            for check in doctor.checks {
                if check.status == CheckStatus::Ok {
                    continue;
                }
                match (check.name.as_str(), check.fix) {
                    ("ScreenCast portal", Some(fix)) => {
                        area.set_ob_linux_hint(format!("No screen picker: {fix}").into())
                    }
                    ("codec: OpenH264", Some(fix)) => {
                        let where_to_run = if crate::engine::is_local() {
                            "as your user"
                        } else {
                            "on the engine’s computer as its desktop user"
                        };
                        area.set_ob_codec_hint(
                            format!(
                                "For software H.264 and live previews, run {fix} {where_to_run}."
                            )
                            .into(),
                        );
                        #[cfg(target_os = "linux")]
                        if crate::engine::is_local()
                            && fix == "/usr/bin/isocast-openh264-install"
                            && std::env::var_os(isocast_openh264::ENV).is_none()
                        {
                            offer_codec(app);
                        }
                    }
                    _ => {}
                }
            }
        });
    }

    /// Whether OBS collections were found (Welcome's Import from OBS… then
    /// opens them instead of a file picker).
    pub fn has_found(&self) -> bool {
        self.state
            .borrow()
            .discovery
            .as_ref()
            .is_some_and(|d| !d.collections.is_empty())
    }

    /// Look for OBS; at the first launch, offer what was found.
    fn discover(&self, app: &Rc<App>, first_launch: bool) {
        self.state.borrow_mut().discovering = true;
        self.show(app);
        app.call_as::<Discovery>(
            Method::ObsDiscover(Some(ObsDiscoverParams::default())),
            move |app, result| {
                let found = {
                    let mut s = app.wizard.state.borrow_mut();
                    s.discovering = false;
                    match result {
                        Ok(discovery) => {
                            // OBS's current collection is chosen to start.
                            s.selected = discovery
                                .collections
                                .iter()
                                .enumerate()
                                .map(|(i, c)| c.current || i == 0)
                                .collect();
                            if let Some(first) = s.selected.iter().position(|on| *on) {
                                s.selected = (0..s.selected.len()).map(|i| i == first).collect();
                            }
                            let found = !discovery.collections.is_empty();
                            s.discovery = Some(discovery);
                            found
                        }
                        Err(error) => {
                            s.import_error = error.message;
                            false
                        }
                    }
                };
                if first_launch && found && !app.wizard.state.borrow().shown {
                    app.wizard.open_obs(app);
                }
                app.wizard.show(app);
            },
        );
    }

    /// Open on From OBS (looking again if nothing was found yet).
    pub fn open_obs(&self, app: &Rc<App>) {
        {
            let mut s = self.state.borrow_mut();
            s.shown = true;
            s.with_obs = true;
            s.step = "obs";
            s.import_error.clear();
        }
        if self.state.borrow().discovery.is_none() {
            self.discover(app, false);
        }
        self.show(app);
    }

    /// Open on Goal (Tools › Auto-Configuration Wizard…, or after the first
    /// run made a setup).
    pub fn open_goal(&self, app: &Rc<App>) {
        {
            let mut s = self.state.borrow_mut();
            s.shown = true;
            s.with_obs = false;
            s.step = "goal";
            s.optimized = None;
            s.error.clear();
            if s.goal.is_none() {
                s.goal = Some(Goal::Recording);
            }
            let stream = app
                .session
                .borrow()
                .setup
                .as_ref()
                .and_then(|s| s.stream.clone());
            if let Some(stream) = stream {
                s.service = Some(stream.service);
                s.server = stream.server.unwrap_or_default();
                if s.goal == Some(Goal::Recording) {
                    s.goal = Some(Goal::Both);
                }
            }
        }
        self.show(app);
    }

    fn close(&self, app: &Rc<App>) {
        self.state.borrow_mut().shown = false;
        self.show(app);
    }

    fn go(&self, app: &Rc<App>, step: &'static str) {
        {
            let mut s = self.state.borrow_mut();
            s.step = step;
            if step == "summary" && s.optimized.is_none() && !s.optimizing {
                s.error.clear();
            }
        }
        // Not Now / Continue with nothing from OBS and no setup: Welcome
        // has the other ways to start.
        let no_setup = app.session.borrow().setup.is_none();
        if step == "goal" && no_setup {
            self.close(app);
            return;
        }
        self.show(app);
    }

    fn toggle(&self, app: &Rc<App>, index: usize) {
        if let Some(on) = self.state.borrow_mut().selected.get_mut(index) {
            *on = !*on;
        }
        self.show(app);
    }

    fn set(&self, app: &Rc<App>, field: &str, value: &str) {
        {
            let mut s = self.state.borrow_mut();
            match field {
                "goal" => {
                    s.goal = match value {
                        "streaming" => Some(Goal::Streaming),
                        "both" => Some(Goal::Both),
                        _ => Some(Goal::Recording),
                    }
                }
                "service" => {
                    let service = StreamService::from_id(value).ok();
                    if service != s.service {
                        s.speed = None;
                        s.speed_error.clear();
                    }
                    s.service = service;
                }
                "server" => {
                    s.server = value.trim().to_string();
                    s.speed = None;
                }
                "move_key" => s.move_key = value == "true",
                _ => {}
            }
        }
        self.show(app);
    }

    /// Import the chosen collections, each with its profile; the first
    /// becomes the one in use. Then on to Goal.
    fn import(&self, app: &Rc<App>) {
        let jobs: Vec<LibraryImportObsParams> = {
            let mut s = self.state.borrow_mut();
            if s.importing {
                return;
            }
            let Some(discovery) = &s.discovery else {
                return;
            };
            let jobs: Vec<LibraryImportObsParams> = discovery
                .collections
                .iter()
                .zip(&s.selected)
                .filter(|(_, on)| **on)
                .enumerate()
                .map(|(i, (c, _))| {
                    let key = discovery
                        .profiles
                        .iter()
                        .find(|p| Some(&p.path) == c.profile.as_ref())
                        .and_then(|p| p.stream.as_ref())
                        .is_some_and(|stream| stream.key_in_file);
                    LibraryImportObsParams {
                        collection: Some(c.file.clone()),
                        profile: c.profile.clone(),
                        switch: i == 0,
                        stream_key: key && s.move_key,
                        ..LibraryImportObsParams::default()
                    }
                })
                .collect();
            s.importing = !jobs.is_empty();
            s.import_error.clear();
            jobs
        };
        self.show(app);
        self.import_next(app, jobs, Vec::new());
    }

    fn import_next(
        &self,
        app: &Rc<App>,
        mut jobs: Vec<LibraryImportObsParams>,
        mut notes: Vec<String>,
    ) {
        if jobs.is_empty() {
            self.state.borrow_mut().importing = false;
            app.reload();
            app.refresh_status(false);
            if !notes.is_empty() {
                app.toast(
                    ToastKind::Warning,
                    "Imported from OBS, with notes",
                    notes.join("\n"),
                );
            }
            self.open_goal(app);
            // Still showing From OBS in the header: it was part of this run.
            self.state.borrow_mut().with_obs = true;
            self.show(app);
            return;
        }
        let job = jobs.remove(0);
        let name = job.collection.clone().unwrap_or_default();
        app.call(
            Method::LibraryImportObs(Box::new(job)),
            move |app, result| match result {
                Ok(change) => {
                    let unsupported = change["unsupported"].as_array().map_or(0, Vec::len);
                    if unsupported > 0 {
                        notes.push(format!(
                            "{}: {unsupported} thing(s) from OBS had no Isocast equivalent",
                            std::path::Path::new(&name)
                                .file_stem()
                                .and_then(|s| s.to_str())
                                .unwrap_or(&name)
                        ));
                    }
                    notes.extend(
                        change["notes"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .filter(|n| n.contains("stream key"))
                            .map(str::to_string),
                    );
                    app.wizard.import_next(app, jobs, notes);
                }
                Err(error) => {
                    {
                        let mut s = app.wizard.state.borrow_mut();
                        s.importing = false;
                        s.import_error = error.message;
                    }
                    app.reload();
                    app.wizard.show(app);
                }
            },
        );
    }

    fn test_upload(&self, app: &Rc<App>) {
        let params = {
            let mut s = self.state.borrow_mut();
            if s.testing {
                return;
            }
            s.testing = true;
            s.speed = None;
            s.speed_error.clear();
            SpeedTestParams {
                service: Some(s.service.unwrap_or(StreamService::Twitch)),
                server: (!s.server.is_empty()).then(|| s.server.clone()),
                seconds: None,
            }
        };
        self.show(app);
        app.call_as::<SpeedTest>(Method::SpeedTest(Box::new(params)), |app, result| {
            {
                let mut s = app.wizard.state.borrow_mut();
                s.testing = false;
                match result {
                    Ok(speed) => s.speed = Some(speed),
                    Err(error) => s.speed_error = error.message,
                }
            }
            app.wizard.show(app);
        });
    }

    fn optimize(&self, app: &Rc<App>) {
        let params = {
            let mut s = self.state.borrow_mut();
            if s.optimizing {
                return;
            }
            let goal = s.goal.unwrap_or(Goal::Recording);
            s.optimizing = true;
            s.optimized = None;
            s.error.clear();
            s.step = "summary";
            let streams = goal.streams();
            OptimizeParams {
                goal,
                service: streams.then(|| s.service.unwrap_or(StreamService::Twitch)),
                server: (streams && !s.server.is_empty()).then(|| s.server.clone()),
                // A measured upload isn't measured again (else the engine
                // tests, or says why it can't).
                upload_kbps: s
                    .speed
                    .as_ref()
                    .filter(|_| streams)
                    .and_then(|t| t.upload_kbps),
                folder: None,
                apply: false,
            }
        };
        self.show(app);
        app.call_as::<Optimized>(Method::Optimize(Box::new(params)), |app, result| {
            {
                let mut s = app.wizard.state.borrow_mut();
                s.optimizing = false;
                match result {
                    Ok(optimized) => s.optimized = Some(optimized),
                    Err(error) => s.error = error.message,
                }
            }
            app.wizard.show(app);
        });
    }

    /// Make the optimized setup live, as one undoable edit.
    fn apply(&self, app: &Rc<App>) {
        let (setup, summary) = {
            let mut s = self.state.borrow_mut();
            match &s.optimized {
                Some(o) if !o.differences.is_empty() && s.error.is_empty() => {
                    let out = (serde_json::to_value(&o.setup), o.summary.clone());
                    s.applying = true;
                    out
                }
                _ => {
                    drop(s);
                    self.close(app);
                    return;
                }
            }
        };
        let Ok(setup) = setup else {
            self.state.borrow_mut().applying = false;
            return;
        };
        self.show(app);
        app.replace_setup_then(setup, true, move |app, applied| {
            app.wizard.state.borrow_mut().applying = false;
            if applied {
                app.wizard.close(app);
                app.refresh_status(false);
                app.toast(ToastKind::Success, "Optimized", summary.clone());
            } else {
                app.wizard.show(app);
            }
        });
    }

    fn show(&self, app: &Rc<App>) {
        let Some(window) = app.window() else { return };
        let area = window.global::<WizardArea>();
        let s = self.state.borrow();
        area.set_shown(s.shown);
        area.set_step(s.step.into());
        area.set_with_obs(s.with_obs);
        // Closing with no setup leads to Welcome.
        area.set_can_close(true);
        area.set_discovering(s.discovering);
        let empty = Discovery::default();
        let discovery = s.discovery.as_ref().unwrap_or(&empty);
        area.set_obs_folder(discovery.folder.as_str().into());
        let rows: Vec<ObsCollectionRow> = discovery
            .collections
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let profile = discovery
                    .profiles
                    .iter()
                    .find(|p| Some(&p.path) == c.profile.as_ref());
                let plural =
                    |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
                let mut meta = format!(
                    "{} · {}",
                    plural(c.scenes, "scene"),
                    plural(c.sources, "source")
                );
                if c.current {
                    meta.push_str(" · in use in OBS");
                }
                ObsCollectionRow {
                    name: c.name.as_str().into(),
                    meta: meta.into(),
                    profile: profile
                        .map(|p| match p.name == c.name {
                            true => p.summary.clone(),
                            false => format!("Profile “{}”: {}", p.name, p.summary),
                        })
                        .unwrap_or_default()
                        .into(),
                    selected: s.selected.get(i).copied().unwrap_or(false),
                    key_in_file: profile
                        .and_then(|p| p.stream.as_ref())
                        .is_some_and(|stream| stream.key_in_file),
                }
            })
            .collect();
        let selected = rows.iter().filter(|r| r.selected).count();
        area.set_any_key(rows.iter().any(|r| r.selected && r.key_in_file));
        area.set_selected_count(selected as i32);
        area.set_collections(model(rows));
        area.set_obs_errors(
            discovery
                .errors
                .iter()
                .map(|e| {
                    // The file and its folder ("Streaming/basic.ini").
                    let path = std::path::Path::new(&e.file);
                    let parts: Vec<&str> = path
                        .iter()
                        .rev()
                        .take(2)
                        .filter_map(|p| p.to_str())
                        .collect();
                    let file = match parts.as_slice() {
                        [name, folder] if *name == "basic.ini" => format!("{folder}/{name}"),
                        [name, ..] => name.to_string(),
                        [] => e.file.clone(),
                    };
                    format!("{file}: {}", e.message)
                })
                .collect::<Vec<_>>()
                .join("\n")
                .into(),
        );
        area.set_move_key(s.move_key);
        area.set_importing(s.importing);
        area.set_import_error(s.import_error.as_str().into());

        let goal = s.goal.unwrap_or(Goal::Recording);
        area.set_goal(goal.id().into());
        let service = s.service.unwrap_or(StreamService::Twitch);
        area.set_service(service.id().into());
        area.set_server(s.server.as_str().into());
        area.set_needs_server(matches!(
            service,
            StreamService::Custom | StreamService::Kick
        ));
        area.set_testing(s.testing);
        let (speed, note) = match &s.speed {
            Some(t) => match t.upload_kbps {
                Some(kbps) => (
                    format!(
                        "{}{} up · stream at {}",
                        if t.capped { "At least " } else { "" },
                        mbps(kbps),
                        mbps(t.recommended_kbps)
                    ),
                    String::new(),
                ),
                None => (
                    format!("Not measured · stream at {}", mbps(t.recommended_kbps)),
                    t.reason.clone().unwrap_or_default(),
                ),
            },
            None => (String::new(), s.speed_error.clone()),
        };
        area.set_speed(speed.into());
        area.set_speed_note(note.into());

        area.set_optimizing(s.optimizing);
        area.set_applying(s.applying);
        area.set_error(s.error.as_str().into());
        match &s.optimized {
            Some(o) => {
                area.set_summary(o.summary.as_str().into());
                area.set_verdict(verdict(o).into());
                area.set_changes(model(changes(&o.differences)));
                let notes: Vec<String> = o
                    .notes
                    .iter()
                    .map(|n| {
                        let mut c = n.chars();
                        c.next()
                            .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                            .unwrap_or_default()
                    })
                    .collect();
                area.set_notes(notes.join("\n").into());
            }
            None => {
                area.set_summary("".into());
                area.set_verdict("".into());
                area.set_changes(model(Vec::new()));
                area.set_notes("".into());
            }
        }
    }
}

/// What the benchmark found, in a line.
fn verdict(o: &Optimized) -> String {
    let t = &o.tuning;
    let speed = if t.realtime_factor > 0.0 {
        format!(" ({:.1}× real time on hard content)", t.realtime_factor)
    } else {
        String::new()
    };
    let machine = crate::engine::words().machine;
    let base = match t.capacity {
        Some(Capacity::Fits) => format!("This {machine} keeps up with room to spare"),
        Some(Capacity::NoMargin) => format!("This {machine} keeps up, with nothing to spare"),
        Some(Capacity::CannotKeepUp) => format!("This {machine} can't keep up with all of it"),
        None => "Not benchmarked".into(),
    };
    match (&t.recommendation, &t.change) {
        // The settings it picked didn't fit; what was applied does.
        (Some(recommendation), Some(_)) => {
            format!(
                "Stepped down to fit this {}: {recommendation}.",
                crate::engine::words().machine
            )
        }
        _ => format!("{base}{speed}."),
    }
}

/// The differences as rows a person reads.
fn changes(differences: &[Difference]) -> Vec<ChangeRow> {
    let text = |v: &Option<Value>, path: &str| -> String {
        let Some(v) = v else { return "—".into() };
        match (path, v) {
            (p, Value::Number(n)) if p.ends_with("video_bitrate_kbps") => {
                mbps(n.as_u64().unwrap_or(0) as u32)
            }
            ("canvas.fps", Value::Number(n)) => format!("{n} fps"),
            (_, Value::Bool(b)) => if *b { "On" } else { "Off" }.into(),
            (_, Value::String(s)) => match s.as_str() {
                "hevc" => "HEVC".into(),
                "h264" => "H.264".into(),
                "very_high" => "Very High".into(),
                s => {
                    let mut c = s.chars();
                    c.next()
                        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                        .unwrap_or_default()
                }
            },
            (_, Value::Array(items)) => items
                .iter()
                .map(|i| {
                    i.as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| i.to_string())
                })
                .collect::<Vec<_>>()
                .join(", "),
            (_, Value::Object(o)) => {
                let service = o.get("service").and_then(Value::as_str).unwrap_or("stream");
                let kbps = o
                    .get("video_bitrate_kbps")
                    .and_then(Value::as_u64)
                    .map(|k| format!(", {}", mbps(k as u32)))
                    .unwrap_or_default();
                let size = match (
                    o.get("width").and_then(Value::as_u64),
                    o.get("height").and_then(Value::as_u64),
                ) {
                    (Some(w), Some(h)) => format!(", {w}×{h}"),
                    _ => String::new(),
                };
                format!("{}{kbps}{size}", text_service(service))
            }
            (_, Value::Null) => "—".into(),
            (_, other) => other.to_string(),
        }
    };
    // Width and height changes as one size.
    let mut merged: Vec<Difference> = Vec::new();
    for d in differences {
        let side = |path: &str| {
            path.strip_suffix(".width")
                .map(|p| (p.to_string(), true))
                .or_else(|| path.strip_suffix(".height").map(|p| (p.to_string(), false)))
        };
        if let Some((prefix, is_width)) = side(&d.path)
            && let Some(other) = merged
                .iter_mut()
                .find(|m| side(&m.path).is_some_and(|(p, w)| p == prefix && w != is_width))
        {
            let (w, h) = if is_width { (d, &*other) } else { (&*other, d) };
            let size = |w: &Option<Value>, h: &Option<Value>| match (w, h) {
                (Some(w), Some(h)) => Some(Value::String(format!("{w}×{h}"))),
                _ => None,
            };
            let (from, to) = (size(&w.from, &h.from), size(&w.to, &h.to));
            other.path = format!("{prefix}.size");
            other.from = from;
            other.to = to;
            continue;
        }
        merged.push(d.clone());
    }
    let differences = merged;
    differences
        .iter()
        .map(|d| {
            let label = match d.path.as_str() {
                "canvas.fps" => "Frame rate",
                "recording.program.enabled" => "Program file",
                "recording.program.encoder.codec" => "Program codec",
                "recording.program.encoder.quality" => "Program quality",
                "recording.program.encoder.bit_depth" => "Program bit depth",
                "recording.program.size" => "Program size",
                "recording.program.width" => "Program width",
                "recording.program.height" => "Program height",
                "recording.iso.enabled" => "ISO files",
                "recording.iso.sources" => "ISO sources",
                "recording.iso.encoder.codec" => "ISO codec",
                "recording.iso.encoder.quality" => "ISO quality",
                "recording.iso.encoder.bit_depth" => "ISO bit depth",
                "stream" => "Stream",
                "stream.service" => "Service",
                "stream.server" => "Server",
                "stream.video_bitrate_kbps" => "Stream bitrate",
                "stream.size" => "Stream size",
                "stream.width" => "Stream width",
                "stream.height" => "Stream height",
                other => other,
            };
            ChangeRow {
                label: label.into(),
                from: text(&d.from, &d.path).into(),
                to: text(&d.to, &d.path).into(),
            }
        })
        .collect()
}

fn text_service(id: &str) -> String {
    match id {
        "twitch" => "Twitch".into(),
        "youtube" => "YouTube".into(),
        "kick" => "Kick".into(),
        "facebook" => "Facebook".into(),
        "custom" => "Custom server".into(),
        other => other.into(),
    }
}
