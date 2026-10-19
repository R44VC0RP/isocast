//! Settings → Hotkeys (area D, docs/hotkeys.md): the profile's bindings
//! by action group, as OBS lists them (each scene's switch, each audio
//! source's mute and push keys, each item's visibility…), from the
//! daemon's catalogue (`hotkeys_actions`) and bindings (`hotkeys_get`).
//! A row is recorded by clicking it and typing the chord (src/keys.rs
//! reads the physical keys), and cleared with ✕; each change is one
//! `hotkeys_set` patch at the revision shown, applied at once, and the
//! API's refusal (a duplicate chord, a refused system shortcut) shows on
//! the row, as do the OS's registration warnings. Bindings that match no
//! row (made by the CLI or MCP with other arguments) are listed under
//! "Other" so they can be changed or cleared too.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use isocast_protocol::Setup;
use isocast_protocol::api::Method;
use isocast_protocol::hotkeys::{Binding, Catalogue, Hotkeys as Current, SetParams};
use serde_json::{Value, json};
use slint::{ComponentHandle, SharedString};

use crate::app::App;
use crate::kit::model;
use crate::{BindingGroup, BindingRow, SettingsArea};

/// A `hotkeys_set` patch: binding ids to their new binding (or none).
type Patch = BTreeMap<String, Option<Binding>>;

/// A row before its binding is found: its label, action and arguments.
type Template = (String, &'static str, Value);

/// A row: what it binds (action and arguments) and to which binding.
struct Row {
    key: String,
    label: String,
    action: String,
    args: Value,
    /// The binding's id and chord, if it has one.
    binding: Option<(String, String)>,
}

#[derive(Default)]
pub struct HotkeysPage {
    catalogue: RefCell<Option<Catalogue>>,
    current: RefCell<Option<Current>>,
    /// The last change's refusal, by row.
    errors: RefCell<HashMap<String, String>>,
    /// The row being recorded.
    recording: RefCell<Option<String>>,
    busy: Cell<bool>,
    /// Why the bindings couldn't be read ("" when they were).
    problem: RefCell<String>,
    /// The page is showing.
    open: Cell<bool>,
    /// Snapshots show sample bindings: the daemon's aren't read over them.
    sampled: Cell<bool>,
}

impl HotkeysPage {
    pub fn connect(app: &Rc<App>, area: &SettingsArea) {
        let weak = Rc::downgrade(app);
        area.on_record_binding(move |key| {
            if let Some(app) = weak.upgrade() {
                app.settings.hotkeys.record(&app, &key);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_clear_binding(move |key| {
            if let Some(app) = weak.upgrade() {
                app.settings.hotkeys.clear(&app, &key);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_cancel_binding(move || {
            if let Some(app) = weak.upgrade() {
                app.settings.hotkeys.stop_recording(&app);
            }
        });
    }

    /// The page came up (or its profile may have changed): read the
    /// catalogue once and the bindings each time.
    pub fn load(&self, app: &Rc<App>) {
        self.open.set(true);
        if self.catalogue.borrow().is_none() {
            app.call_as::<Catalogue>(Method::HotkeysActions, |app, result| {
                let page = &app.settings.hotkeys;
                match result {
                    Ok(catalogue) => *page.catalogue.borrow_mut() = Some(catalogue),
                    Err(error) => *page.problem.borrow_mut() = error.message,
                }
                page.show(app);
            });
        }
        self.reload(app);
        self.show(app);
    }

    /// Read the bindings again (another client, a profile switch).
    pub fn reload(&self, app: &Rc<App>) {
        if !self.open.get() || self.sampled.get() {
            return;
        }
        app.call_as::<Current>(Method::HotkeysGet, |app, result| {
            let page = &app.settings.hotkeys;
            if page.sampled.get() {
                return;
            }
            match result {
                Ok(current) => {
                    *page.current.borrow_mut() = Some(current);
                    page.problem.borrow_mut().clear();
                }
                Err(error) => *page.problem.borrow_mut() = error.message,
            }
            page.show(app);
        });
    }

    /// The page went away (another page, Settings closed).
    pub fn closed(&self, app: &Rc<App>) {
        self.open.set(false);
        self.stop_recording(app);
        self.errors.borrow_mut().clear();
    }

    fn record(&self, app: &Rc<App>, key: &str) {
        if self.busy.get() {
            return;
        }
        *self.recording.borrow_mut() = Some(key.to_string());
        let weak = Rc::downgrade(app);
        let key = key.to_string();
        crate::keys::capture(Some(Box::new(move |chord| {
            if let Some(app) = weak.upgrade() {
                let page = &app.settings.hotkeys;
                *page.recording.borrow_mut() = None;
                match chord {
                    Some(chord) => page.bind(&app, &key, Some(&chord)),
                    None => page.show(&app),
                }
            }
        })));
        self.show(app);
    }

    fn stop_recording(&self, app: &Rc<App>) {
        if self.recording.borrow_mut().take().is_some() {
            crate::keys::capture(None);
            self.show(app);
        }
    }

    fn clear(&self, app: &Rc<App>, key: &str) {
        self.stop_recording(app);
        self.bind(app, key, None);
    }

    /// Give row `key` the chord (or none): one `hotkeys_set` patch.
    fn bind(&self, app: &Rc<App>, key: &str, chord: Option<&str>) {
        let Some((patch, revision)) = self.patch(app, key, chord) else {
            self.show(app);
            return;
        };
        self.send(app, key.to_string(), patch, revision, true);
    }

    fn send(&self, app: &Rc<App>, key: String, patch: Patch, revision: Option<u64>, retry: bool) {
        self.busy.set(true);
        self.show(app);
        let params = SetParams {
            bindings: None,
            patch: Some(patch.clone()),
            revision,
        };
        app.call_as::<Current>(Method::HotkeysSet(Box::new(params)), move |app, result| {
            let page = &app.settings.hotkeys;
            page.busy.set(false);
            match result {
                Ok(current) => {
                    page.errors.borrow_mut().remove(&key);
                    *page.current.borrow_mut() = Some(current);
                }
                // The setup moved on since: read it and try once more.
                Err(error) if retry && error.message.contains("revision") => {
                    app.call_as::<Current>(Method::HotkeysGet, move |app, result| {
                        let page = &app.settings.hotkeys;
                        match result {
                            Ok(current) => {
                                let revision = current.revision;
                                *page.current.borrow_mut() = Some(current);
                                page.send(app, key, patch, Some(revision), false);
                            }
                            Err(error) => {
                                page.errors.borrow_mut().insert(key, error.message);
                                page.show(app);
                            }
                        }
                    });
                    return;
                }
                Err(error) => {
                    page.errors.borrow_mut().insert(key, error.message);
                }
            }
            page.show(app);
        });
    }

    /// The patch for row `key` and the revision it is made at.
    fn patch(&self, app: &Rc<App>, key: &str, chord: Option<&str>) -> Option<(Patch, Option<u64>)> {
        let current = self.current.borrow();
        let current = current.as_ref()?;
        let rows = {
            let s = app.session.borrow();
            rows(s.setup.as_deref(), current)
        };
        let row = rows.iter().flat_map(|(_, r)| r).find(|r| r.key == key)?;
        let mut patch = BTreeMap::new();
        match (chord, &row.binding) {
            (None, None) => return None,
            (None, Some((id, _))) => {
                patch.insert(id.clone(), None);
            }
            (Some(chord), binding) => {
                let id = binding
                    .as_ref()
                    .map(|(id, _)| id.clone())
                    .unwrap_or_else(|| new_id(&row.action, &row.args, current));
                patch.insert(
                    id,
                    Some(Binding {
                        keys: chord.to_string(),
                        action: row.action.clone(),
                        args: row.args.clone(),
                    }),
                );
            }
        }
        Some((patch, Some(current.revision)))
    }

    /// Snapshots: bindings as the daemon reports them, with a refusal and
    /// an OS warning, without changing any profile (`hotkeys-sample`);
    /// and row `index` waiting for its keys (`hotkey-record:INDEX`).
    pub fn sample(&self, app: &Rc<App>) {
        let binding = |keys: &str, action: &str, args: Value| Binding {
            keys: keys.into(),
            action: action.into(),
            args,
        };
        let scene = app
            .session
            .borrow()
            .setup
            .as_ref()
            .and_then(|s| s.scenes.first().map(|x| x.id.clone()))
            .unwrap_or_else(|| "main".into());
        let current = Current {
            profile: Some("default".into()),
            revision: app.session.borrow().revision,
            bindings: BTreeMap::from([
                (
                    "recording-toggle".into(),
                    binding("ctrl+alt+cmd+r", "recording.toggle", json!({})),
                ),
                (
                    "recording-marker".into(),
                    binding("ctrl+alt+cmd+m", "recording.marker", json!({"label": "Marker"})),
                ),
                (
                    "scene-switch".into(),
                    binding("f13", "scene.switch", json!({"scene": scene})),
                ),
                (
                    "intro".into(),
                    binding("ctrl+shift+1", "scene.switch_index", json!({"index": 1})),
                ),
            ]),
            configured: Default::default(),
            warnings: vec![
                isocast_protocol::platform_text::select("hotkeys.media-play: media keys need Input Monitoring for Isocast’s engine (System Settings › Privacy & Security › Input Monitoring); they work while Isocast is in front meanwhile", "hotkeys.media-play: this Windows build cannot register this media key globally; choose another shortcut").into(),
            ],
            pending: false,
            registrations: vec![isocast_protocol::hotkeys::Registration {
                binding: "scene-switch".into(),
                global: false,
                warning: Some("F13 is taken by another app; it works while Isocast is in front".into()),
            }],
            held_sources: BTreeMap::new(),
            studio_mode: false,
        };
        *self.current.borrow_mut() = Some(current);
        self.sampled.set(true);
        self.errors.borrow_mut().insert(
            "streaming.toggle|{}".into(),
            "patch.streaming-toggle.keys: cmd+s is a common app shortcut (Save); choose another, like ctrl+alt+cmd+s".into(),
        );
        self.open.set(true);
        self.show(app);
    }

    pub fn sample_record(&self, app: &Rc<App>, index: usize) {
        let key = {
            let current = self.current.borrow();
            let Some(current) = current.as_ref() else {
                return;
            };
            let s = app.session.borrow();
            rows(s.setup.as_deref(), current)
                .into_iter()
                .flat_map(|(_, r)| r)
                .nth(index)
                .map(|r| r.key)
        };
        if let Some(key) = key {
            *self.recording.borrow_mut() = Some(key);
            self.show(app);
        }
    }

    /// Put the rows on the page.
    pub fn show(&self, app: &Rc<App>) {
        let Some(window) = app.window() else { return };
        let area = window.global::<SettingsArea>();
        area.set_bindings_busy(self.busy.get());
        let current = self.current.borrow();
        let problem = self.problem.borrow();
        let Some(current) = current.as_ref() else {
            area.set_bindings(model(Vec::<BindingGroup>::new()));
            area.set_bindings_warnings(model(Vec::<SharedString>::new()));
            area.set_bindings_note(
                if problem.is_empty() {
                    "Reading this profile’s hotkeys…".to_string()
                } else {
                    format!("Couldn’t read the hotkeys: {problem}")
                }
                .into(),
            );
            return;
        };
        area.set_bindings_note(if problem.is_empty() {
            SharedString::new()
        } else {
            format!("Couldn’t read the hotkeys: {problem}").into()
        });
        let errors = self.errors.borrow();
        let recording = self.recording.borrow();
        let groups = {
            let s = app.session.borrow();
            rows(s.setup.as_deref(), current)
        };
        let labels = self.catalogue.borrow();
        // The engine's warnings name the binding's field (for agents); the
        // banner names its row instead (#112).
        let label_of = |id: &str| {
            groups
                .iter()
                .flat_map(|(_, rows)| rows)
                .find(|row| row.binding.as_ref().is_some_and(|(b, _)| b == id))
                .map(|row| {
                    if row.label.is_empty() {
                        other_label(labels.as_ref(), &row.action, &row.args)
                    } else {
                        row.label.clone()
                    }
                })
        };
        let mut warnings: Vec<SharedString> = current
            .warnings
            .iter()
            .map(|w| banner_warning(w, &label_of).into())
            .collect();
        if current.pending {
            warnings.push(
                isocast_protocol::platform_text::select(
                    "macOS is still taking the new keys; they work in a moment.",
                    "Windows is still registering the new shortcuts; they work in a moment.",
                )
                .into(),
            );
        }
        area.set_bindings_warnings(model(warnings));
        area.set_bindings(model(groups.into_iter().map(|(title, rows)| {
            BindingGroup {
                title: title.into(),
                rows: model(rows.into_iter().map(|row| {
                    let chord = row.binding.as_ref().map(|(_, c)| c.clone());
                    let warning = row
                        .binding
                        .as_ref()
                        .and_then(|(id, _)| {
                            current
                                .registrations
                                .iter()
                                .find(|r| &r.binding == id)
                                .and_then(|r| r.warning.clone())
                        })
                        .unwrap_or_default();
                    let label = if row.label.is_empty() {
                        other_label(labels.as_ref(), &row.action, &row.args)
                    } else {
                        row.label.clone()
                    };
                    BindingRow {
                        key: row.key.as_str().into(),
                        label: label.into(),
                        chord: chord.clone().unwrap_or_default().into(),
                        caps: model(
                            chord
                                .map(|c| crate::keys::chord_caps(&c))
                                .unwrap_or_default()
                                .into_iter()
                                .map(SharedString::from),
                        ),
                        error: errors.get(&row.key).cloned().unwrap_or_default().into(),
                        warning: warning.into(),
                        recording: recording.as_deref() == Some(row.key.as_str()),
                    }
                })),
            }
        })));
    }
}

/// The page's rows in OBS's groups: recording, streaming, scenes, Studio
/// Mode, sources, timeline, then bindings that match none of them.
fn rows(setup: Option<&Setup>, current: &Current) -> Vec<(String, Vec<Row>)> {
    let empty = json!({});
    let mut groups: Vec<(String, Vec<Template>)> = vec![
        (
            "Recording".into(),
            vec![
                ("Start recording".into(), "recording.start", empty.clone()),
                ("Stop recording".into(), "recording.stop", empty.clone()),
                (
                    "Start or stop recording".into(),
                    "recording.toggle",
                    empty.clone(),
                ),
                ("Pause recording".into(), "recording.pause", empty.clone()),
                ("Resume recording".into(), "recording.resume", empty.clone()),
                (
                    "Pause or resume recording".into(),
                    "recording.pause_toggle",
                    empty.clone(),
                ),
                ("Split the take".into(), "recording.split", empty.clone()),
                (
                    "Add a marker".into(),
                    "recording.marker",
                    json!({"label": "Marker"}),
                ),
            ],
        ),
        (
            "Streaming".into(),
            vec![
                ("Start streaming".into(), "streaming.start", empty.clone()),
                ("Stop streaming".into(), "streaming.stop", empty.clone()),
                (
                    "Start or stop streaming".into(),
                    "streaming.toggle",
                    empty.clone(),
                ),
            ],
        ),
    ];
    let mut scenes = Vec::new();
    let mut sources = Vec::new();
    if let Some(setup) = setup {
        for scene in &setup.scenes {
            scenes.push((
                format!("Switch to “{}”", crate::model::scene_name(scene)),
                "scene.switch",
                json!({"scene": scene.id}),
            ));
        }
        for source in setup.sources.iter().filter(|s| s.kind.has_audio()) {
            let name = source.name.clone().unwrap_or_else(|| source.id.clone());
            for (action, words) in [
                ("audio.mute", "Mute"),
                ("audio.unmute", "Unmute"),
                ("audio.push_to_talk", "Push to talk:"),
                ("audio.push_to_mute", "Push to mute:"),
            ] {
                sources.push((
                    format!("{words} “{name}”"),
                    action,
                    json!({"source": source.id}),
                ));
            }
        }
        for scene in &setup.scenes {
            for item in &scene.items {
                let Some(id) = item.id.clone().or_else(|| item.source.clone()) else {
                    continue;
                };
                sources.push((
                    format!(
                        "Show or hide “{}” in “{}”",
                        crate::model::item_label(setup, item),
                        crate::model::scene_name(scene)
                    ),
                    "item.toggle",
                    json!({"scene": scene.id, "item": id}),
                ));
            }
        }
    }
    groups.push(("Scenes".into(), scenes));
    groups.push((
        "Studio Mode".into(),
        vec![
            (
                "Studio Mode on or off".into(),
                "studio.toggle",
                empty.clone(),
            ),
            (
                "Transition (preview to program)".into(),
                "transition.take",
                empty.clone(),
            ),
        ],
    ));
    groups.push(("Sources".into(), sources));
    groups.push((
        "Timeline".into(),
        vec![
            ("Retake".into(), "timeline.retake", empty.clone()),
            ("New timeline".into(), "timeline.new", empty.clone()),
            ("Close the timeline".into(), "timeline.close", empty.clone()),
            (
                "Export the timeline".into(),
                "timeline.export",
                empty.clone(),
            ),
        ],
    ));

    // Bindings by (action, arguments); each row takes the first.
    let mut unused: Vec<(&String, &Binding)> = current.bindings.iter().collect();
    let mut out = Vec::new();
    for (title, templates) in groups {
        let mut rows = Vec::new();
        for (label, action, args) in templates {
            let at = unused
                .iter()
                .position(|(_, b)| b.action == action && same_args(&b.args, &args));
            let binding = at.map(|at| {
                let (id, b) = unused.remove(at);
                (id.clone(), b.keys.clone())
            });
            rows.push(Row {
                key: format!("{action}|{args}"),
                label,
                action: action.to_string(),
                args,
                binding,
            });
        }
        if !rows.is_empty() {
            out.push((title, rows));
        }
    }
    if !unused.is_empty() {
        out.push((
            "Other".into(),
            unused
                .into_iter()
                .map(|(id, b)| Row {
                    key: format!("id:{id}"),
                    label: String::new(),
                    action: b.action.clone(),
                    args: b.args.clone(),
                    binding: Some((id.clone(), b.keys.clone())),
                })
                .collect(),
        ));
    }
    out
}

fn same_args(a: &Value, b: &Value) -> bool {
    let empty = |v: &Value| v.is_null() || v.as_object().is_some_and(|o| o.is_empty());
    a == b || (empty(a) && empty(b))
}

/// A binding no row shows: the catalogue's label and its arguments.
/// A warning about binding `hotkeys.ID[.FIELD]: …` as the banner says it:
/// the binding's row label (`label_of`) in place of its field path, else
/// just the message.
fn banner_warning(warning: &str, label_of: &dyn Fn(&str) -> Option<String>) -> String {
    let Some((path, message)) = warning
        .strip_prefix("hotkeys.")
        .and_then(|rest| rest.split_once(": "))
    else {
        return warning.to_string();
    };
    let id = path.split(['.', ' ']).next().unwrap_or(path);
    match label_of(id) {
        Some(label) => format!("{label}: {message}"),
        None => {
            let mut chars = message.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect()
            })
        }
    }
}

fn other_label(catalogue: Option<&Catalogue>, action: &str, args: &Value) -> String {
    let label = catalogue
        .and_then(|c| c.actions.iter().find(|a| a.id == action))
        .map_or(action.to_string(), |a| a.label.clone());
    match args.as_object() {
        Some(map) if !map.is_empty() => {
            let parts: Vec<String> = map
                .iter()
                .map(|(k, v)| match v {
                    Value::String(s) => format!("{k} {s}"),
                    other => format!("{k} {other}"),
                })
                .collect();
            format!("{label} ({})", parts.join(", "))
        }
        _ => label,
    }
}

/// A binding id for a new row: lowercase letters, digits and dashes, at
/// most 64, not taken ("scene-switch-intro", "recording-toggle-2").
fn new_id(action: &str, args: &Value, current: &Current) -> String {
    let mut base: String = std::iter::once(action.to_string())
        .chain(
            args.as_object()
                .into_iter()
                .flat_map(|m| m.values())
                .filter_map(|v| v.as_str().map(str::to_string)),
        )
        .collect::<Vec<_>>()
        .join("-")
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    while base.contains("--") {
        base = base.replace("--", "-");
    }
    let base = base.trim_matches('-');
    let base: String = base.chars().take(58).collect();
    let taken = |id: &str| {
        current.bindings.contains_key(id)
            || current
                .configured
                .value
                .as_ref()
                .and_then(Value::as_object)
                .is_some_and(|m| m.contains_key(id))
    };
    let mut id = base.clone();
    let mut n = 2;
    while taken(&id) {
        id = format!("{base}-{n}");
        n += 1;
    }
    id
}
