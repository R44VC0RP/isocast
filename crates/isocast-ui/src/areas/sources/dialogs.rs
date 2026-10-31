//! Area A's dialogs (the web app's add-source.js, properties.js,
//! filters.js and transform.js): Create/Select Source, Properties,
//! Filters, Edit Transform and Rename. Each keeps what it needs here and
//! shows itself through `SourcesArea`; changes go to the daemon as edits.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use isocast_protocol::api::{Method, ScreenPickParams, ScreenPicked};
use isocast_protocol::{CanvasId, Codec, Setup};
use serde_json::{Map, Value, json};
use slint::{ComponentHandle, SharedString};

use super::items;
use super::preview::SourcePreview;
use crate::app::App;
use crate::form::schema::{self, humanize};
use crate::form::{self, Context, FormEvent, FormModel, Group, Outcome, Row, Target};
use crate::kit::{self, model};
use crate::model as setup_model;
use crate::session::EditOptions;
use crate::{AppWindow, FilterRow, PickRow, SourcesArea, ToastKind, Ui};

/// A new source waiting for what it can't exist without (a file, an
/// app): Properties edits it as a draft and adds it on OK.
#[derive(Clone)]
pub struct Pending {
    pub kind: String,
    pub scene: String,
    pub visible: bool,
    pub draft: Map<String, Value>,
}

#[derive(Default)]
pub struct Dialogs {
    /// Create/Select Source: the kind ("scene" for a nested scene).
    create_kind: RefCell<String>,
    /// Properties: the source, as it was when the dialog opened (Cancel
    /// puts it back), or a draft.
    props_source: RefCell<String>,
    props_original: RefCell<Option<Value>>,
    pending: RefCell<Option<Pending>>,
    created: Cell<bool>,
    /// The next Properties to open is for a source added just now.
    next_created: Cell<bool>,
    /// "Part of a title…" / "Custom…" chosen in a form.
    props_typing: Cell<bool>,
    filter_typing: Cell<bool>,
    /// Filters: the source and the selected filter (list, key).
    filters_source: RefCell<String>,
    filter: RefCell<Option<(String, String)>>,
    /// Filters copied (a filter's Copy, a source's Copy Filters), by
    /// list ("filters", "audio_filters"), to paste into another source.
    filter_clipboard: RefCell<Vec<(String, Value)>>,
    /// Rename: the source.
    rename_source: RefCell<String>,
    pub props_form: FormModel,
    pub filter_form: FormModel,
    pub preview: SourcePreview,
    /// Looking for devices again (the form's refresh buttons spin).
    refreshing: Cell<bool>,
    /// Properties of a camera: its status every second.
    status_timer: slint::Timer,
}

/// A source's settings: everything but its id, name, kind, audio and
/// filters.
fn settings_of(source: &Value) -> Map<String, Value> {
    source
        .as_object()
        .map(|o| {
            o.iter()
                .filter(|(k, _)| {
                    !matches!(
                        k.as_str(),
                        "id" | "name" | "kind" | "audio" | "filters" | "audio_filters"
                    )
                })
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default()
}

fn source_json(app: &Rc<App>, id: &str) -> Option<Value> {
    app.session
        .borrow()
        .setup_json()?
        .get("sources")?
        .as_array()?
        .iter()
        .find(|s| s["id"] == id)
        .cloned()
}

fn name_or_kind(source: &Value) -> String {
    source["name"]
        .as_str()
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            setup_model::kind_info(source["kind"].as_str().unwrap_or_default())
                .label
                .to_string()
        })
}

/// A kind's name as a sheet's line says it: OBS's words in sentence case
/// ("Media Source" → "Media source").
fn sentence_case(label: &str) -> String {
    let mut words = label.split(' ');
    let first = words.next().unwrap_or_default().to_string();
    words.fold(first, |text, word| {
        format!("{text} {}", word.to_lowercase())
    })
}

/// What the Properties sheet's line says a source is: its kind, and the
/// file, words, colour or page it shows ("Media source · clip.mp4").
fn source_subtitle(source: &Value) -> String {
    let kind = source["kind"].as_str().unwrap_or_default();
    let label = sentence_case(setup_model::kind_info(kind).label);
    let name = |path: &str| path.rsplit('/').next().unwrap_or(path).to_string();
    let detail = match kind {
        "media" | "slideshow" => {
            let files: Vec<&str> = source["files"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            match files.as_slice() {
                [] => String::new(),
                [one] => name(one),
                [first, rest @ ..] => format!("{} and {} more", name(first), rest.len()),
            }
        }
        "image" => source["path"].as_str().map(name).unwrap_or_default(),
        "text" => source["text"]
            .as_str()
            .and_then(|t| t.lines().next())
            .map(|line| {
                let short: String = line.chars().take(40).collect();
                if short.len() < line.len() {
                    format!("“{short}…”")
                } else {
                    format!("“{short}”")
                }
            })
            .or_else(|| source["file"].as_str().map(name))
            .unwrap_or_default(),
        "color" => source["color"].as_str().unwrap_or_default().to_string(),
        "browser" => source["url"]
            .as_str()
            .map(str::to_string)
            .or_else(|| source["file"].as_str().map(name))
            .unwrap_or_default(),
        _ => String::new(),
    };
    if detail.is_empty() {
        label
    } else {
        format!("{label} · {detail}")
    }
}

/// A running source's format, as a viewer line says it ("1920×1080 · 30p ·
/// Stereo"); "" until the engine has opened it.
fn source_format(status: Option<&isocast_protocol::api::SourceStatus>) -> String {
    status.map(setup_model::source_format).unwrap_or_default()
}

fn kind_has_video(kind: &str) -> bool {
    !matches!(
        kind,
        "microphone" | "system_audio" | "application_audio" | "tone"
    )
}

/// Properties' row for a video source's own ISO file depth
/// (`encoder.bit_depth`), which isn't one of its kind's fields.
const ISO_DEPTH: &str = "@iso_depth";

/// The ISO Colour Depth row: follow Settings → Recording's Screen / ISO
/// Colour Depth, or
/// the source's own 8 or 10. 10 is offered where its file and capture can
/// have it (kept, with a warning, if it is set where they can't). None
/// for ProRes ISO files, always 10-bit, unless the source has its own.
fn iso_depth_group(
    kind: &str,
    setup: Option<&Setup>,
    value: &Map<String, Value>,
    lane: bool,
) -> Option<Group> {
    let own = value
        .get("encoder")
        .and_then(|encoder| encoder["bit_depth"].as_u64());
    let iso = setup.map(|setup| &setup.recording.iso.encoder);
    let codec = iso.map_or(Codec::Hevc, |encoder| encoder.codec);
    if !matches!(codec, Codec::Hevc | Codec::H264) && own.is_none() {
        return None;
    }
    let screen = matches!(kind, "display" | "window" | "application");
    let no_ten = if codec == Codec::H264 {
        // A Linux engine has no HEVC to choose.
        Some(
            if crate::engine::os() == isocast_protocol::hello::EngineOs::Linux {
                "H.264 ISO files are 8-bit only."
            } else {
                "H.264 ISO files are 8-bit only; choose HEVC in Settings → Recording for 10-bit."
            },
        )
    } else if screen && !crate::areas::settings::engine_is_mac() {
        Some("This engine captures screens in 8 bits only.")
    } else {
        None
    };
    let follows = if matches!(codec, Codec::H264 | Codec::Hevc) {
        iso.map_or(8, |encoder| encoder.depth())
    } else {
        10 // ProRes files are always 10-bit.
    };
    let mut options = vec![
        (
            String::new(),
            "Follow Recording Settings".to_string(),
            format!("{follows}-bit"),
        ),
        ("8".into(), "8-bit".into(), "Smaller files".into()),
    ];
    if no_ten.is_none() || own == Some(10) {
        options.push(("10".into(), "10-bit".into(), "Smoother gradients".into()));
    }
    Some(Group {
        row: Row {
            key: ISO_DEPTH.into(),
            label: "ISO Colour Depth".into(),
            kind: "choice",
            value: own.map(|depth| depth.to_string()).unwrap_or_default(),
            options,
            hint: format!(
                "Applies when this source records an ISO file. {}",
                no_ten.unwrap_or(
                    "10-bit gives smoother gradients, takes more CPU and makes screen files \
                     about 40% larger.",
                )
            ),
            hint_warn: no_ten.is_some() && own == Some(10),
            tooltip: "This source’s ISO file depth, over Screen / ISO Colour Depth in Settings → \
                      Recording."
                .into(),
            lane,
            ..Row::default()
        },
        children: Vec::new(),
    })
}

fn kind_has_audio(source: &Value) -> bool {
    match source["kind"].as_str().unwrap_or_default() {
        "browser" => source["control_audio"] == true,
        "microphone" | "system_audio" | "application_audio" | "tone" | "media" => true,
        _ => false,
    }
}

/// The settings a new source of `kind` starts with.
pub fn starting(
    kind: &str,
    devices: Option<&isocast_protocol::devices::Devices>,
) -> Map<String, Value> {
    let value = match kind {
        "display" => json!({ "display": devices
            .and_then(|d| d.displays.iter().find(|x| x.main))
            .map(|x| x.id.clone())
            .unwrap_or("main".into()) }),
        "camera" => json!({ "device": devices
            .and_then(|d| d.cameras.iter().find(|c| !c.suspended))
            .map(|c| c.name.clone())
            .unwrap_or_default() }),
        "microphone" => json!({ "device": "default" }),
        "window" | "application" | "application_audio" => json!({ "app": "" }),
        "image" => json!({ "path": "" }),
        "slideshow" | "media" => json!({ "files": [] }),
        "text" => json!({ "text": "Text", "size": 96, "weight": 600, "color": "#eeeae2" }),
        "browser" => json!({ "url": "https://" }),
        "color" => json!({ "color": "#2e3338" }),
        _ => json!({}),
    };
    value.as_object().cloned().unwrap_or_default()
}

/// What has to be filled in before a source of `kind` can be added.
pub fn missing(kind: &str, settings: &Map<String, Value>) -> Option<&'static str> {
    let text = |k: &str| settings.get(k).and_then(Value::as_str).unwrap_or_default();
    let empty_list = |k: &str| {
        settings
            .get(k)
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
    };
    match kind {
        "image" if text("path").is_empty() => Some("Choose an image file."),
        "slideshow" | "media" if empty_list("files") => Some("Choose at least one file."),
        "camera" if text("device").is_empty() => Some("Choose a device."),
        "window" | "application" | "application_audio" if text("app").is_empty() => {
            Some("Choose an app.")
        }
        "text" if text("text").trim().is_empty() && text("file").is_empty() => {
            Some("Write the text.")
        }
        "browser" => {
            let url = text("url");
            let ok = ["http://", "https://", "file://"]
                .iter()
                .any(|p| url.len() > p.len() && url.starts_with(p));
            (!ok && text("file").is_empty()).then_some("Enter the page’s URL.")
        }
        _ => None,
    }
}

/// Name for a new source of `kind`: its type, numbered after the first.
fn default_name(app: &Rc<App>, kind: &str) -> String {
    let base = if kind == "scene" {
        "Scene".to_string()
    } else {
        setup_model::kind_info(kind).label.to_string()
    };
    let s = app.session.borrow();
    let names: Vec<String> = s
        .setup
        .as_ref()
        .map(|x| {
            x.sources
                .iter()
                .map(|s| setup_model::source_name(s).to_string())
                .collect()
        })
        .unwrap_or_default();
    if !names.contains(&base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base} {n}"))
        .find(|n| !names.contains(n))
        .expect("an unused number")
}

/// An item for scene `scene` showing `fields` (a source or a scene), the
/// size of the canvas, with an id of its own if the scene shows that
/// already.
fn new_item(app: &Rc<App>, scene: &str, fields: Value, visible: bool) -> Value {
    let s = app.session.borrow();
    // The canvas being edited: its size, and its items (#86).
    let Some(setup) = s.edited_setup().cloned() else {
        return fields;
    };
    let mut item = fields;
    item["rect"] =
        json!({ "x": 0, "y": 0, "width": setup.canvas.width, "height": setup.canvas.height });
    if !visible {
        item["visible"] = json!(false);
    }
    let keys: Vec<String> = setup_model::find_scene(&setup, scene)
        .map(|sc| {
            sc.items
                .iter()
                .map(|i| setup_model::key(i).to_string())
                .collect()
        })
        .unwrap_or_default();
    let key = item["source"]
        .as_str()
        .or(item["scene"].as_str())
        .unwrap_or_default()
        .to_string();
    if keys.contains(&key) {
        let name = item["source"]
            .as_str()
            .and_then(|id| setup_model::find_source(&setup, id))
            .map(|s| setup_model::source_name(s).to_string())
            .unwrap_or(key);
        let taken: Vec<&str> = keys
            .iter()
            .map(String::as_str)
            .chain(setup_model::taken_ids(&setup))
            .collect();
        item["id"] = json!(setup_model::new_id(&name, taken));
    }
    item
}

/// The setup as canvas `canvas` has it (an extra canvas's items and
/// size, or the main ones).
fn canvas_setup(s: &crate::session::Session, canvas: CanvasId) -> Option<Rc<Setup>> {
    match canvas {
        CanvasId::Main => s.setup.clone(),
        other if s.extra_canvas() == Some(other) => s.extra.clone(),
        other => s.setup.as_ref()?.for_canvas(other).map(Rc::new),
    }
}

fn item_key(item: &Value) -> String {
    item["id"]
        .as_str()
        .or(item["source"].as_str())
        .or(item["scene"].as_str())
        .unwrap_or_default()
        .to_string()
}

/// Size a new item from its source's picture: its own size if it fits on
/// the canvas, else as large as fits; centred. Waits up to 3 s for the
/// live engine to open the source. `canvas`: the canvas it was added to
/// (#86), whichever is edited by then.
fn place_new(app: &Rc<App>, scene: String, key: String, tries: u32, canvas: CanvasId) {
    let source = {
        let s = app.session.borrow();
        let Some(setup) = canvas_setup(&s, canvas) else {
            return;
        };
        setup_model::find_scene(&setup, &scene)
            .and_then(|sc| sc.items.iter().find(|i| setup_model::key(i) == key))
            .and_then(|i| i.source.clone())
    };
    let Some(source) = source else { return };
    let opened = {
        let s = app.session.borrow();
        s.running.get(&source).map(|r| (r.width, r.height, r.state))
    };
    let failed =
        opened.is_some_and(|(_, _, state)| state == isocast_protocol::api::SourceRun::Failed);
    let size = opened.and_then(|(w, h, _)| Some((w? as f64, h? as f64)));
    if size.is_none() && !failed && tries < 30 {
        let weak = Rc::downgrade(app);
        slint::Timer::single_shot(Duration::from_millis(100), move || {
            if let Some(app) = weak.upgrade() {
                place_new(&app, scene, key, tries + 1, canvas);
            }
        });
        return;
    }
    let apply = move |app: &Rc<App>, size: Option<(f64, f64)>| {
        let Some((w, h)) = size.filter(|(w, h)| *w > 0.0 && *h > 0.0) else {
            return;
        };
        let (cw, ch) = canvas_setup(&app.session.borrow(), canvas)
            .map(|s| (s.canvas.width as f64, s.canvas.height as f64))
            .unwrap_or((1920.0, 1080.0));
        let scale = 1f64.min(cw / w).min(ch / h);
        let rect = json!({
            "x": items::int((cw - w) / 2.0), "y": items::int((ch - h) / 2.0),
            "width": items::whole(w), "height": items::whole(h),
        });
        let still = canvas_setup(&app.session.borrow(), canvas)
            .and_then(|s| setup_model::find_scene(&s, &scene).cloned())
            .is_some_and(|sc| sc.items.iter().any(|i| setup_model::key(i) == key));
        if still {
            app.edit_on(
                json!({ "scenes": [{ "id": scene, "items": [{ "id": key, "rect": rect, "fit": "none", "style": { "transform": { "scale": scale } } }] }] }),
                EditOptions::default(),
                canvas,
            );
        }
    };
    match size {
        Some(size) => apply(app, Some(size)),
        None => items::source_size(app, Some(source), apply),
    }
}

/// Add a source of `kind` (and, for video, an item showing it in scene
/// `scene`); `done` gets its id once the daemon has it.
fn add_source(
    app: &Rc<App>,
    scene: &str,
    kind: &str,
    settings: Map<String, Value>,
    name: &str,
    visible: bool,
    done: impl FnOnce(&Rc<App>, String) + 'static,
) {
    let (id, audio) = {
        let s = app.session.borrow();
        let Some(setup) = s.setup.clone() else { return };
        let label = if name.is_empty() {
            setup_model::kind_info(kind).label
        } else {
            name
        };
        (
            setup_model::new_id(label, setup_model::taken_ids(&setup)),
            setup.recording.program.audio.clone(),
        )
    };
    let name = if name.is_empty() {
        setup_model::kind_info(kind).label.to_string()
    } else {
        name.to_string()
    };
    let mut source = json!({ "id": id, "name": name, "kind": kind });
    if let Some(object) = source.as_object_mut() {
        object.extend(settings);
    }
    let mut patch = json!({ "sources": [source] });
    let video = kind_has_video(kind);
    let item = video.then(|| new_item(app, scene, json!({ "source": id }), visible));
    if let Some(item) = &item {
        patch["scenes"] = json!([{ "id": scene, "items": [item] }]);
    }
    if matches!(
        kind,
        "microphone" | "system_audio" | "application_audio" | "tone" | "media"
    ) {
        let mut list = audio;
        list.push(id.clone());
        patch["recording"] = json!({ "program": { "audio": list } });
    }
    let scene = scene.to_string();
    let key = item.as_ref().map(item_key);
    let canvas = app.session.borrow().edit_canvas();
    app.edit_then(patch, EditOptions::default(), move |app, applied| {
        if !applied {
            return;
        }
        if let Some(key) = key {
            app.select(&key);
            place_new(app, scene, key, 0, canvas);
        } else {
            app.toast(
                ToastKind::Success,
                &format!("Added “{name}”"),
                "It’s in the program mix: find it in the Audio Mixer.",
            );
        }
        done(app, id);
    });
}

impl Dialogs {
    /// A dialog is opening: load what it shows.
    pub fn opened(&self, app: &Rc<App>, window: &AppWindow, kind: &str, arg: &str) {
        let area = window.global::<SourcesArea>();
        if kind != "properties" && kind != "filters" {
            self.preview.stop();
        }
        self.status_timer.stop();
        match kind {
            "create-source" => {
                *self.create_kind.borrow_mut() = arg.to_string();
                app.load_devices(false);
                let is_scene = arg == "scene";
                area.set_create_is_scene(is_scene);
                area.set_create_audio_only(!is_scene && !kind_has_video(arg));
                area.set_create_mode(if is_scene { "existing" } else { "new" }.into());
                area.set_create_name(default_name(app, arg).into());
                area.set_create_visible(true);
                area.set_create_empty(
                    if is_scene {
                        "No other scenes can go in this one."
                    } else {
                        "None in this setup yet."
                    }
                    .into(),
                );
                let existing = self.existing(app, arg);
                area.set_create_pick(existing.first().map(|p| p.id.clone()).unwrap_or_default());
                area.set_create_existing(model(existing));
                self.show_create(app, window);
            }
            "properties" => {
                let created = self.next_created.replace(false);
                if arg.is_empty() && self.pending.borrow().is_some() {
                    self.props_source.borrow_mut().clear();
                    *self.props_original.borrow_mut() = None;
                } else {
                    *self.pending.borrow_mut() = None;
                    *self.props_source.borrow_mut() = arg.to_string();
                    *self.props_original.borrow_mut() = source_json(app, arg);
                }
                self.created.set(created);
                self.props_typing.set(false);
                self.props_form.clear();
                app.load_devices(false);
                self.show_props(app, window);
                // A camera's form says what the engine receives, live.
                let camera = source_json(app, arg).is_some_and(|s| s["kind"] == "camera");
                if camera && !app.mode.offscreen {
                    let weak = Rc::downgrade(app);
                    self.status_timer.start(
                        slint::TimerMode::Repeated,
                        Duration::from_secs(1),
                        move || {
                            if let Some(app) = weak.upgrade() {
                                app.refresh_status(true);
                            }
                        },
                    );
                }
            }
            "filters" => {
                *self.filters_source.borrow_mut() = arg.to_string();
                let first = source_json(app, arg).and_then(|source| {
                    let first = |list: &str| {
                        source[list]
                            .as_array()
                            .and_then(|l| l.first())
                            .map(|f| (list.to_string(), filter_key(f)))
                    };
                    let video = kind_has_video(source["kind"].as_str().unwrap_or_default());
                    let audio = kind_has_audio(&source);
                    video
                        .then(|| first("filters"))
                        .flatten()
                        .or_else(|| audio.then(|| first("audio_filters")).flatten())
                });
                *self.filter.borrow_mut() = first;
                self.filter_typing.set(false);
                self.filter_form.clear();
                self.show_filters(app, window);
            }
            "transform" => {
                area.set_transform_anchor("top_left".into());
                self.show_transform(app, window);
            }
            "rename" => {
                *self.rename_source.borrow_mut() = arg.to_string();
                let source = source_json(app, arg);
                let name = source.as_ref().map(name_or_kind).unwrap_or_default();
                let kind = source
                    .as_ref()
                    .and_then(|s| s["kind"].as_str().map(str::to_string))
                    .unwrap_or_default();
                area.set_rename_heading(format!("A new name for “{name}”.").into());
                area.set_rename_icon(setup_model::kind_info(&kind).icon.into());
                area.set_rename_name(name.into());
            }
            _ => {}
        }
    }

    /// The setup changed (or the devices, the selection, the engine's
    /// sources): show it in the open dialog.
    pub fn update(&self, app: &Rc<App>, window: &AppWindow) {
        let open = window.global::<Ui>().get_dialog();
        match open.as_str() {
            "create-source" => self.show_create(app, window),
            "properties" => self.show_props(app, window),
            "filters" => self.show_filters(app, window),
            "transform" => self.show_transform(app, window),
            _ => self.preview.stop(),
        }
    }

    fn close(&self, app: &Rc<App>) {
        self.preview.stop();
        self.status_timer.stop();
        app.close_dialog();
    }

    // ---------- Create/Select Source ----------

    fn existing(&self, app: &Rc<App>, kind: &str) -> Vec<PickRow> {
        let s = app.session.borrow();
        let (Some(setup), Some(editing)) = (s.setup.clone(), s.editing_scene()) else {
            return Vec::new();
        };
        if kind == "scene" {
            return setup
                .scenes
                .iter()
                .filter(|sc| sc.id != editing && !shows_scene(&setup, &sc.id, &editing, 0))
                .map(|sc| PickRow {
                    id: sc.id.as_str().into(),
                    label: setup_model::scene_name(sc).into(),
                })
                .collect();
        }
        if !kind_has_video(kind) {
            return Vec::new();
        }
        setup
            .sources
            .iter()
            .filter(|src| src.kind.label() == kind)
            .map(|src| PickRow {
                id: src.id.as_str().into(),
                label: setup_model::source_name(src).into(),
            })
            .collect()
    }

    pub fn show_create(&self, app: &Rc<App>, window: &AppWindow) {
        let area = window.global::<SourcesArea>();
        let kind = self.create_kind.borrow().clone();
        let s = app.session.borrow();
        let scene = s
            .editing_scene()
            .and_then(|id| setup_model::find_scene(s.setup.as_ref()?, &id).cloned());
        let heading = if kind == "scene" {
            "Scene".to_string()
        } else {
            format!(
                "{} · {}",
                setup_model::kind_info(&kind).label,
                scene.as_ref().map(setup_model::scene_name).unwrap_or("")
            )
        };
        area.set_create_heading(heading.into());
        area.set_create_icon(
            if kind == "scene" {
                "scene-card"
            } else {
                setup_model::kind_info(&kind).icon
            }
            .into(),
        );
        let name = area.get_create_name().trim().to_string();
        let taken = area.get_create_mode() == "new"
            && s.setup.as_ref().is_some_and(|setup| {
                setup
                    .sources
                    .iter()
                    .any(|src| setup_model::source_name(src) == name)
            });
        area.set_create_taken(taken);
    }

    pub fn create_accept(&self, app: &Rc<App>, window: &AppWindow) {
        let area = window.global::<SourcesArea>();
        let kind = self.create_kind.borrow().clone();
        let Some(scene) = app.session.borrow().editing_scene() else {
            return;
        };
        let visible = area.get_create_visible();
        if area.get_create_mode() == "existing" || kind == "scene" {
            let pick = area.get_create_pick().to_string();
            if pick.is_empty() {
                return;
            }
            self.close(app);
            let is_scene = kind == "scene";
            let fields = if is_scene {
                json!({ "scene": pick })
            } else {
                json!({ "source": pick })
            };
            let item = new_item(app, &scene, fields, visible);
            let key = item_key(&item);
            let canvas = app.session.borrow().edit_canvas();
            app.edit_then(
                json!({ "scenes": [{ "id": scene, "items": [item] }] }),
                EditOptions::default(),
                move |app, applied| {
                    if applied {
                        app.select(&key);
                        if !is_scene {
                            place_new(app, scene, key, 0, canvas);
                        }
                    }
                },
            );
            return;
        }
        let name = area.get_create_name().trim().to_string();
        if name.is_empty() || area.get_create_taken() {
            return;
        }
        let mut settings = starting(&kind, app.session.borrow().devices.as_ref());
        if form::devices::PICKED_IN_DIALOG && kind == "window" {
            // The desktop's dialog chooses the window when the source first
            // starts; `app` only names it, after the source.
            settings.insert("app".into(), json!(name));
        }
        if missing(&kind, &settings).is_some() {
            // Not valid yet: Properties collects what it needs, then adds it.
            let mut draft = Map::new();
            draft.insert("id".into(), json!("new"));
            draft.insert("kind".into(), json!(kind));
            draft.insert("name".into(), json!(name));
            draft.extend(settings);
            *self.pending.borrow_mut() = Some(Pending {
                kind,
                scene,
                visible,
                draft,
            });
            app.open_dialog("properties", "");
            return;
        }
        self.close(app);
        // Properties next, saying the source was added.
        add_source(app, &scene, &kind, settings, &name, visible, |app, id| {
            app.sources.dialogs.next_created.set(true);
            app.open_dialog("properties", &id);
        });
    }

    // ---------- Properties ----------

    /// The source Properties shows: the live one, or the draft.
    fn props_value(&self, app: &Rc<App>) -> Option<(Map<String, Value>, bool)> {
        if let Some(pending) = self.pending.borrow().as_ref() {
            return Some((pending.draft.clone(), true));
        }
        let id = self.props_source.borrow().clone();
        let source = source_json(app, &id)?;
        Some((source.as_object().cloned().unwrap_or_default(), false))
    }

    pub fn show_props(&self, app: &Rc<App>, window: &AppWindow) {
        let area = window.global::<SourcesArea>();
        let Some((value, draft)) = self.props_value(app) else {
            // The source went away (removed elsewhere): nothing to edit.
            self.close(app);
            return;
        };
        let kind = value
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let source = Value::Object(value.clone());
        area.set_props_title(format!("Properties for “{}”", name_or_kind(&source)).into());
        area.set_props_name(name_or_kind(&source).into());
        area.set_props_subtitle(source_subtitle(&source).into());
        area.set_props_icon(setup_model::kind_info(&kind).icon.into());
        let fields = form::fields("Source", &kind);
        let s = app.session.borrow();
        let running = (!draft).then(|| s.running.get(&id)).flatten();
        let ctx = Context {
            kind: &kind,
            source: (!draft).then_some(id.as_str()),
            setup: s.setup.as_deref(),
            devices: s.devices.as_ref(),
            running,
            skip: &[],
            refreshing: self.refreshing.get(),
            typing: self.props_typing.get(),
        };
        let mut groups = form::build(&ctx, &fields, &value);
        if kind_has_video(&kind) {
            let lane = groups.iter().any(|group| group.row.lane);
            groups.extend(iso_depth_group(&kind, ctx.setup, &value, lane));
        }
        area.set_preview_format(source_format(running).into());
        let failed = running
            .filter(|r| r.state == isocast_protocol::api::SourceRun::Failed)
            .map(|r| {
                r.error
                    .clone()
                    .unwrap_or_else(|| "It couldn’t start.".into())
            });
        drop(s);
        self.props_form.show(groups);
        area.set_props_form(self.props_form.model());
        let video = kind_has_video(&kind);
        area.set_props_video(video);
        area.set_props_need(
            if draft {
                missing(&kind, &settings_of(&source)).unwrap_or("")
            } else {
                ""
            }
            .into(),
        );
        area.set_props_problem(failed.clone().unwrap_or_default().into());
        area.set_props_created(self.created.get());
        if video {
            if draft {
                self.preview_note(window, "The preview shows once the source is added.");
            } else if failed.is_some() {
                self.preview_note(window, "No picture: the source isn’t running.");
            } else {
                self.start_preview(app, window, &id);
            }
        } else {
            self.preview.stop();
        }
    }

    fn preview_note(&self, window: &AppWindow, note: &str) {
        self.preview.stop();
        let area = window.global::<SourcesArea>();
        area.set_preview_note(note.into());
        area.set_preview_shown(false);
    }

    fn start_preview(&self, app: &Rc<App>, window: &AppWindow, id: &str) {
        let area = window.global::<SourcesArea>();
        area.set_preview_note("".into());
        if self.preview.source().as_deref() == Some(id) {
            return;
        }
        area.set_preview_shown(false);
        // Its shape until the first frame says.
        let aspect = items::known_size(app, id)
            .map(|(w, h)| w / h)
            .or_else(|| {
                let s = app.session.borrow();
                s.setup
                    .as_ref()
                    .map(|x| x.canvas.width as f64 / x.canvas.height.max(1) as f64)
            })
            .unwrap_or(16.0 / 9.0);
        area.set_preview_aspect(aspect as f32);
        let weak = window.as_weak();
        self.preview.start(id, move |image, aspect| {
            if let Some(window) = weak.upgrade() {
                let area = window.global::<SourcesArea>();
                area.set_preview(image);
                area.set_preview_aspect(aspect);
                area.set_preview_shown(true);
            }
        });
    }

    pub fn props_accept(&self, app: &Rc<App>) {
        let pending = self.pending.borrow().clone();
        if let Some(pending) = pending {
            let settings = settings_of(&Value::Object(pending.draft.clone()));
            if missing(&pending.kind, &settings).is_some() {
                return;
            }
            let name = pending
                .draft
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            *self.pending.borrow_mut() = None;
            self.close(app);
            add_source(
                app,
                &pending.scene,
                &pending.kind,
                settings,
                &name,
                pending.visible,
                |_, _| {},
            );
            return;
        }
        self.close(app);
    }

    pub fn props_cancel(&self, app: &Rc<App>) {
        let id = self.props_source.borrow().clone();
        let original = self.props_original.borrow().clone();
        if self.pending.borrow().is_none()
            && let (Some(original), Some(now)) = (original, source_json(app, &id))
        {
            let patch = items::object_patch(
                &Value::Object(settings_of(&now)),
                &Value::Object(settings_of(&original)),
            );
            if let Some(fields) = patch.as_object().filter(|p| !p.is_empty()) {
                let mut source = json!({ "id": id });
                if let Some(object) = source.as_object_mut() {
                    object.extend(fields.clone());
                }
                app.edit(json!({ "sources": [source] }), EditOptions::default());
            }
        }
        *self.pending.borrow_mut() = None;
        self.close(app);
    }

    /// Defaults: every setting that has one goes back to it; the ones a
    /// source can't do without (its device, its file) stay.
    pub fn props_defaults(&self, app: &Rc<App>, window: &AppWindow) {
        let Some((value, _)) = self.props_value(app) else {
            return;
        };
        let kind = value
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // Its own ISO file depth goes back to following the recording's.
        let optional: Vec<String> = form::fields("Source", &kind)
            .into_iter()
            .filter(|f| f.optional)
            .map(|f| f.name)
            .chain(["encoder".to_string()])
            .collect();
        if let Some(pending) = self.pending.borrow_mut().as_mut() {
            pending.draft.retain(|k, _| !optional.contains(k));
        } else {
            let mut changes = Map::new();
            for name in optional.iter().filter(|n| value.contains_key(n.as_str())) {
                changes.insert(name.clone(), Value::Null);
            }
            self.props_change(app, changes, None);
        }
        self.show_props(app, window);
    }

    /// Change the source's settings (or the draft's).
    fn props_change(&self, app: &Rc<App>, changes: Map<String, Value>, live: Option<&str>) {
        if changes.is_empty() {
            return;
        }
        if let Some(pending) = self.pending.borrow_mut().as_mut() {
            for (k, v) in changes {
                if v.is_null() {
                    pending.draft.remove(&k);
                } else {
                    pending.draft.insert(k, v);
                }
            }
            return;
        }
        let id = self.props_source.borrow().clone();
        let mut changes = changes;
        pair_size(app, &id, &mut changes);
        let mut source = json!({ "id": id });
        if let Some(object) = source.as_object_mut() {
            object.extend(changes);
        }
        let options = match live {
            Some(name) => EditOptions {
                key: Some(format!("src:{id}:{name}")),
                interval: Duration::from_millis(80),
                ..EditOptions::default()
            },
            None => EditOptions::default(),
        };
        app.edit(json!({ "sources": [source] }), options);
    }

    // ---------- Forms ----------

    /// A form's control changed (`form`: "props" or "filter").
    pub fn form_event(
        &self,
        app: &Rc<App>,
        window: &AppWindow,
        form_id: &str,
        key: &str,
        event: FormEvent,
    ) {
        if let FormEvent::Refresh = event {
            self.refresh_devices(app, window);
            return;
        }
        if form_id == "props" && key == ISO_DEPTH {
            if let FormEvent::Choose(chosen) = event {
                let mut changes = Map::new();
                let own = chosen.parse::<u8>().ok();
                changes.insert(
                    "encoder".into(),
                    own.map_or(Value::Null, |depth| json!({ "bit_depth": depth })),
                );
                self.props_change(app, changes, None);
                self.reshow(app, window, form_id);
            }
            return;
        }
        let (fields, value, kind, typing) = match form_id {
            "props" => {
                let Some((value, _)) = self.props_value(app) else {
                    return;
                };
                let kind = value
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                (
                    form::fields("Source", &kind),
                    value,
                    kind,
                    &self.props_typing,
                )
            }
            "filter" => {
                let Some((list, filter)) = self.selected_filter(app) else {
                    return;
                };
                let def = if list == "audio_filters" {
                    "AudioFilter"
                } else {
                    "Filter"
                };
                let kind = filter["kind"].as_str().unwrap_or_default().to_string();
                (
                    form::fields(def, &kind),
                    filter.as_object().cloned().unwrap_or_default(),
                    "filter".to_string(),
                    &self.filter_typing,
                )
            }
            _ => return,
        };
        let devices = app.session.borrow().devices.clone();
        let target = Target {
            kind: &kind,
            fields: &fields,
            value: &value,
            devices: devices.as_ref(),
            typing,
        };
        match form::change(app, &target, key, event) {
            Outcome::Set(changes, live) => match form_id {
                "props" => self.props_change(app, changes, live.as_deref()),
                _ => self.filter_change(app, changes, live.as_deref()),
            },
            Outcome::Reshow => {}
            Outcome::Nothing => return,
            Outcome::Pick => {
                // A draft isn't in the setup yet: the dialog opens when it
                // first starts.
                if self.pending.borrow().is_none() {
                    pick_screen(app, self.props_source.borrow().clone());
                }
                return;
            }
        }
        self.reshow(app, window, form_id);
    }

    fn reshow(&self, app: &Rc<App>, window: &AppWindow, form_id: &str) {
        if form_id == "props" {
            self.show_props(app, window);
        } else {
            self.show_filters(app, window);
        }
    }

    fn refresh_devices(&self, app: &Rc<App>, window: &AppWindow) {
        self.refreshing.set(true);
        self.update(app, window);
        app.call_as::<isocast_protocol::devices::Devices>(
            isocast_protocol::api::Method::Devices,
            |app, result| {
                app.sources.dialogs.refreshing.set(false);
                match result {
                    Ok(devices) => {
                        app.session.borrow_mut().devices = Some(devices);
                        app.changed(crate::session::Changed::DEVICES);
                    }
                    Err(error) => app.failed(
                        crate::engine::pick(
                            "Couldn’t list this Mac’s devices",
                            "Couldn’t list this PC’s devices",
                        ),
                        &error,
                    ),
                }
                if let Some(window) = app.window() {
                    app.sources.dialogs.update(app, &window);
                }
            },
        );
    }

    // ---------- Filters ----------

    fn selected_filter(&self, app: &Rc<App>) -> Option<(String, Value)> {
        let (list, key) = self.filter.borrow().clone()?;
        let source = source_json(app, &self.filters_source.borrow())?;
        let filter = source[list.as_str()]
            .as_array()?
            .iter()
            .find(|f| filter_key(f) == key)?
            .clone();
        Some((list, filter))
    }

    pub fn show_filters(&self, app: &Rc<App>, window: &AppWindow) {
        let area = window.global::<SourcesArea>();
        let id = self.filters_source.borrow().clone();
        let Some(source) = source_json(app, &id) else {
            self.close(app);
            return;
        };
        let kind = source["kind"].as_str().unwrap_or_default().to_string();
        let video = kind_has_video(&kind);
        let audio = kind_has_audio(&source);
        area.set_filters_title(format!("Filters for “{}”", name_or_kind(&source)).into());
        area.set_filters_name(name_or_kind(&source).into());
        area.set_filters_icon(setup_model::kind_info(&kind).icon.into());
        let format = source_format(app.session.borrow().running.get(&id));
        area.set_preview_format(format.into());
        area.set_filters_audio(audio);
        area.set_filters_video(video);
        let rows = |list: &str| -> Vec<FilterRow> {
            source[list]
                .as_array()
                .map(|l| {
                    l.iter()
                        .map(|f| FilterRow {
                            key: filter_key(f).into(),
                            name: filter_name(f).into(),
                            enabled: f["enabled"] != false,
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        area.set_audio_filters(model(rows("audio_filters")));
        area.set_video_filters(model(rows("filters")));
        let selected = self.selected_filter(app);
        let (list, _) = self.filter.borrow().clone().unwrap_or_default();
        match selected {
            Some((list, filter)) => {
                area.set_filter_list(list.as_str().into());
                area.set_filter_key(filter_key(&filter).into());
                area.set_filter_heading(filter_name(&filter).into());
                area.set_filter_off(filter["enabled"] == false);
                let def = if list == "audio_filters" {
                    "AudioFilter"
                } else {
                    "Filter"
                };
                let fields = form::fields(def, filter["kind"].as_str().unwrap_or_default());
                let s = app.session.borrow();
                let skip = filter_skips(&filter);
                let ctx = Context {
                    kind: "filter",
                    source: Some(&id),
                    setup: s.setup.as_deref(),
                    devices: s.devices.as_ref(),
                    running: None,
                    skip: &skip,
                    refreshing: false,
                    typing: self.filter_typing.get(),
                };
                let groups = form::build(&ctx, &fields, filter.as_object().unwrap_or(&Map::new()));
                drop(s);
                self.filter_form.show(groups);
                self.show_mask_handles(window, &filter, &source["filters"]);
            }
            None => {
                area.set_mask_handles(false);
                area.set_filter_list(list.into());
                area.set_filter_key(SharedString::new());
                area.set_filter_heading("".into());
                area.set_filter_off(false);
                self.filter_form.clear();
            }
        }
        area.set_filter_form(self.filter_form.model());
        if video {
            let failed = app
                .session
                .borrow()
                .running
                .get(&id)
                .is_some_and(|r| r.state == isocast_protocol::api::SourceRun::Failed);
            if failed {
                self.preview_note(window, "No picture: the source isn’t running.");
            } else {
                self.start_preview(app, window, &id);
            }
        } else {
            self.preview.stop();
        }
    }

    pub fn filter_select(&self, app: &Rc<App>, window: &AppWindow, list: &str, key: &str) {
        *self.filter.borrow_mut() = Some((list.to_string(), key.to_string()));
        self.filter_typing.set(false);
        self.filter_form.clear();
        self.show_filters(app, window);
    }

    fn filters_patch(&self, app: &Rc<App>, list: &str, items: Vec<Value>, options: EditOptions) {
        let id = self.filters_source.borrow().clone();
        app.edit(json!({ "sources": [{ "id": id, list: items }] }), options);
    }

    fn filters_of(&self, app: &Rc<App>, list: &str) -> Vec<Value> {
        source_json(app, &self.filters_source.borrow())
            .and_then(|s| s[list].as_array().cloned())
            .unwrap_or_default()
    }

    pub fn filter_toggle(&self, app: &Rc<App>, list: &str, key: &str) {
        let Some(filter) = self
            .filters_of(app, list)
            .into_iter()
            .find(|f| filter_key(f) == key)
        else {
            return;
        };
        let mut patch = filter_identity(&filter);
        patch["enabled"] = json!(filter["enabled"] == false);
        self.filters_patch(app, list, vec![patch], EditOptions::default());
    }

    /// + under a list: the menu of filter kinds.
    pub fn filter_menu(&self, list: &str) -> kit::MenuBuilder {
        let def = if list == "audio_filters" {
            "AudioFilter"
        } else {
            "Filter"
        };
        schema::kinds_of(def)
            .into_iter()
            .fold(kit::MenuBuilder::new(), |menu, kind| {
                menu.item(&format!("add:{kind}"), &humanize(&kind))
            })
    }

    pub fn filter_add(&self, app: &Rc<App>, window: &AppWindow, list: &str, kind: &str) {
        let filters = self.filters_of(app, list);
        let taken: Vec<String> = filters.iter().map(filter_key).collect();
        let mut filter = json!({ "kind": kind });
        if taken.iter().any(|t| t == kind) {
            // Ids are lowercase letters, digits and dashes: "noise-gate-2".
            let base = kind.replace('_', "-");
            let id = (2..)
                .map(|n| format!("{base}-{n}"))
                .find(|id| !taken.contains(id))
                .expect("an unused number");
            filter["id"] = json!(id);
        }
        let key = filter_key(&filter);
        self.filters_patch(app, list, vec![filter], EditOptions::default());
        self.filter_select(app, window, list, &key);
    }

    pub fn filter_remove(&self, app: &Rc<App>, window: &AppWindow, list: &str) {
        let filters = self.filters_of(app, list);
        let Some((selected_list, key)) = self.filter.borrow().clone() else {
            return;
        };
        if selected_list != list {
            return;
        }
        let Some(index) = filters.iter().position(|f| filter_key(f) == key) else {
            return;
        };
        let mut patch = filter_identity(&filters[index]);
        patch["remove"] = json!(true);
        self.filters_patch(app, list, vec![patch], EditOptions::default());
        let next = filters
            .get(index + 1)
            .or(index.checked_sub(1).and_then(|i| filters.get(i)));
        match next {
            Some(next) => self.filter_select(app, window, list, &filter_key(next)),
            None => {
                *self.filter.borrow_mut() = None;
                self.filter_form.clear();
                self.show_filters(app, window);
            }
        }
    }

    pub fn filter_move(&self, app: &Rc<App>, list: &str, by: i32) {
        let filters = self.filters_of(app, list);
        let Some((selected_list, key)) = self.filter.borrow().clone() else {
            return;
        };
        if selected_list != list {
            return;
        }
        let Some(index) = filters.iter().position(|f| filter_key(f) == key) else {
            return;
        };
        let to = index as i32 + by;
        if to < 0 || to as usize >= filters.len() {
            return;
        }
        let mut next = filters.clone();
        let f = next.remove(index);
        next.insert(to as usize, f);
        let mut patch: Vec<Value> = filters
            .iter()
            .map(|f| {
                let mut id = filter_identity(f);
                id["remove"] = json!(true);
                id
            })
            .collect();
        patch.extend(next);
        self.filters_patch(app, list, patch, EditOptions::default());
    }

    pub fn filter_defaults(&self, app: &Rc<App>) {
        let Some((list, filter)) = self.selected_filter(app) else {
            return;
        };
        let mut patch = filter_identity(&filter);
        let mut any = false;
        for k in filter
            .as_object()
            .map(|o| o.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default()
        {
            if !matches!(k.as_str(), "id" | "kind" | "enabled") {
                patch[k.as_str()] = Value::Null;
                any = true;
            }
        }
        if any {
            self.filters_patch(app, &list, vec![patch], EditOptions::default());
        }
    }

    /// A filter row's menu (right-click), as OBS has it.
    pub fn filter_item_menu(&self, list: &str) -> kit::MenuBuilder {
        let can_paste = self
            .filter_clipboard
            .borrow()
            .iter()
            .any(|(l, _)| l == list);
        kit::MenuBuilder::new()
            .item("rename", "Rename")
            .item("duplicate", "Duplicate")
            .separator()
            .item("copy", "Copy")
            .item("paste", "Paste")
            .disabled(!can_paste)
            .separator()
            .item("remove", "Remove")
            .danger()
    }

    /// A choice from a filter row's menu (`list`, the filter's `key`).
    pub fn filter_item_chosen(
        &self,
        app: &Rc<App>,
        window: &AppWindow,
        list: &str,
        key: &str,
        id: &str,
    ) {
        let filters = self.filters_of(app, list);
        let Some(index) = filters.iter().position(|f| filter_key(f) == key) else {
            return;
        };
        match id {
            "rename" => {
                self.filter_select(app, window, list, key);
                window
                    .global::<SourcesArea>()
                    .set_renaming_filter(format!("{list}:{key}").into());
            }
            "duplicate" => {
                let mut copy = filters[index].clone();
                let taken: Vec<String> = filters.iter().map(filter_key).collect();
                let new_key = unique_filter_id(&taken, &filter_key(&copy));
                copy["id"] = json!(new_key);
                let mut next = filters.clone();
                next.insert(index + 1, copy);
                self.filters_replace(app, list, &filters, next);
                self.filter_select(app, window, list, &new_key);
            }
            "copy" => {
                *self.filter_clipboard.borrow_mut() =
                    vec![(list.to_string(), filters[index].clone())];
                app.toast(
                    ToastKind::Success,
                    &format!("Copied “{}”", filter_name(&filters[index])),
                    "Paste it into another source’s filters.",
                );
            }
            "paste" => {
                let source = self.filters_source.borrow().clone();
                self.paste_filters(app, &source, Some(list));
            }
            "remove" => {
                self.filter_select(app, window, list, key);
                self.filter_remove(app, window, list);
            }
            _ => {}
        }
    }

    /// Rename a filter: its id becomes the name as an id ("Warm Glow" →
    /// `warm-glow`), kept in its place.
    pub fn filter_rename(
        &self,
        app: &Rc<App>,
        window: &AppWindow,
        list: &str,
        key: &str,
        name: &str,
    ) {
        let filters = self.filters_of(app, list);
        let Some(index) = filters.iter().position(|f| filter_key(f) == key) else {
            return;
        };
        let base = filter_id_from(name);
        if base.is_empty() || base == key || name.trim() == filter_name(&filters[index]) {
            return;
        }
        let taken: Vec<String> = filters
            .iter()
            .map(filter_key)
            .filter(|k| k != key)
            .collect();
        let new_key = if taken.contains(&base) {
            unique_filter_id(&taken, &base)
        } else {
            base
        };
        let mut next = filters.clone();
        next[index]["id"] = json!(new_key);
        self.filters_replace(app, list, &filters, next);
        self.filter_select(app, window, list, &new_key);
    }

    /// Copy every filter of `source` (a source's Copy Filters).
    pub fn copy_filters(&self, app: &Rc<App>, source: &str) {
        let Some(json) = source_json(app, source) else {
            return;
        };
        let copied: Vec<(String, Value)> = ["filters", "audio_filters"]
            .into_iter()
            .flat_map(|list| {
                json[list]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(move |f| (list.to_string(), f))
            })
            .collect();
        let count = copied.len();
        *self.filter_clipboard.borrow_mut() = copied;
        app.toast(
            ToastKind::Success,
            &format!(
                "Copied {count} filter{} of “{}”",
                if count == 1 { "" } else { "s" },
                name_or_kind(&json)
            ),
            "Paste Filters on another source adds them to it.",
        );
    }

    /// Whether there are copied filters to paste.
    pub fn has_copied_filters(&self) -> bool {
        !self.filter_clipboard.borrow().is_empty()
    }

    /// Add the copied filters to `source` (only those of `only`'s list,
    /// if given), after its own, with ids of their own: one edit.
    pub fn paste_filters(&self, app: &Rc<App>, source: &str, only: Option<&str>) {
        let Some(json) = source_json(app, source) else {
            return;
        };
        let kind = json["kind"].as_str().unwrap_or_default().to_string();
        let mut patch = Map::new();
        patch.insert("id".into(), json!(source));
        let mut added = 0;
        for list in ["filters", "audio_filters"] {
            if only.is_some_and(|only| only != list) {
                continue;
            }
            // A source without a picture (or sound) takes no such filters.
            let fits = if list == "filters" {
                kind_has_video(&kind)
            } else {
                kind_has_audio(&json)
            };
            let copied: Vec<Value> = self
                .filter_clipboard
                .borrow()
                .iter()
                .filter(|(l, _)| l == list)
                .map(|(_, f)| f.clone())
                .collect();
            if copied.is_empty() || !fits {
                continue;
            }
            let mut taken: Vec<String> = json[list]
                .as_array()
                .map(|a| a.iter().map(filter_key).collect())
                .unwrap_or_default();
            let items: Vec<Value> = copied
                .into_iter()
                .map(|mut f| {
                    let key = filter_key(&f);
                    if taken.contains(&key) {
                        let id = unique_filter_id(&taken, &key);
                        f["id"] = json!(id);
                    }
                    taken.push(filter_key(&f));
                    f
                })
                .collect();
            added += items.len();
            patch.insert(list.into(), Value::Array(items));
        }
        if added == 0 {
            app.toast(
                ToastKind::Info,
                "Nothing to paste here",
                "The copied filters are for a picture or sound this source doesn’t have.",
            );
            return;
        }
        app.edit(
            json!({ "sources": [Value::Object(patch)] }),
            EditOptions::default(),
        );
        if let Some(window) = app.window()
            && window.global::<Ui>().get_dialog() == "filters"
        {
            self.show_filters(app, &window);
        }
    }

    /// Put `next` in place of `filters` (`list` of the open source), in
    /// order, in one edit.
    fn filters_replace(&self, app: &Rc<App>, list: &str, filters: &[Value], next: Vec<Value>) {
        let mut patch: Vec<Value> = filters
            .iter()
            .map(|f| {
                let mut id = filter_identity(f);
                id["remove"] = json!(true);
                id
            })
            .collect();
        patch.extend(next);
        self.filters_patch(app, list, patch, EditOptions::default());
    }

    fn filter_change(&self, app: &Rc<App>, changes: Map<String, Value>, live: Option<&str>) {
        let Some((list, filter)) = self.selected_filter(app) else {
            return;
        };
        if changes.is_empty() {
            return;
        }
        let mut patch = filter_identity(&filter);
        if let Some(object) = patch.as_object_mut() {
            object.extend(changes);
        }
        let id = self.filters_source.borrow().clone();
        let options = match live {
            Some(name) => EditOptions {
                key: Some(format!("filter:{id}:{}:{name}", filter_key(&filter))),
                interval: Duration::from_millis(80),
                ..EditOptions::default()
            },
            None => EditOptions::default(),
        };
        self.filters_patch(app, &list, vec![patch], options);
    }

    /// Show the handles of `filter` if it is a shape mask (#115): its box
    /// and rotation, its own or its kind's defaults. Not while a filter
    /// after it in `chain` moves or resizes the picture (crop/pad,
    /// scaling, scroll): the box would be drawn in the wrong place.
    fn show_mask_handles(&self, window: &AppWindow, filter: &Value, chain: &Value) {
        let area = window.global::<SourcesArea>();
        let key = filter_key(filter);
        let reshaped = chain.as_array().is_some_and(|chain| {
            chain
                .iter()
                .skip_while(|f| filter_key(f) != key)
                .skip(1)
                .filter(|f| f["enabled"] != false)
                .any(|f| matches!(f["kind"].as_str(), Some("crop_pad" | "scaling" | "scroll")))
        });
        let shape = filter["kind"] == "shape_mask" && !reshaped;
        area.set_mask_handles(shape);
        if !shape {
            return;
        }
        let defaults = isocast_protocol::ShapeMask::default();
        let number = |key: &str, default: f32| filter[key].as_f64().map_or(default, |v| v as f32);
        area.set_mask_shape(filter["shape"].as_str().unwrap_or("rectangle").into());
        area.set_mask_x(number("x", defaults.x));
        area.set_mask_y(number("y", defaults.y));
        area.set_mask_w(number("width", defaults.width));
        area.set_mask_h(number("height", defaults.height));
        area.set_mask_rotation(number("rotation", defaults.rotation));
    }

    /// The shape mask's handles moved it to `[x, y, width, height,
    /// rotation]`: change the filter, as one gesture until `done`.
    pub fn mask_moved(&self, app: &Rc<App>, window: &AppWindow, box_: [f32; 5], done: bool) {
        let Some((_, filter)) = self.selected_filter(app) else {
            return;
        };
        if filter["kind"] != "shape_mask" {
            return;
        }
        // Tidy numbers: a ten-thousandth of the picture, a tenth of a degree.
        let fraction = |v: f32| ((f64::from(v) * 10_000.0).round() / 10_000.0).clamp(-10.0, 10.0);
        let [x, y, width, height, rotation] = box_;
        let rotation = (f64::from(rotation) * 10.0).round() / 10.0;
        // -180 to 180, as the form shows it.
        let rotation = (rotation + 180.0).rem_euclid(360.0) - 180.0;
        let mut changes = Map::new();
        changes.insert("x".into(), json!(fraction(x)));
        changes.insert("y".into(), json!(fraction(y)));
        changes.insert("width".into(), json!(fraction(width).max(0.0)));
        changes.insert("height".into(), json!(fraction(height).max(0.0)));
        changes.insert("rotation".into(), json!(rotation));
        self.filter_change(app, changes, (!done).then_some("handles"));
        // Where it was let go, until the changed setup comes back.
        let area = window.global::<SourcesArea>();
        area.set_mask_x(x);
        area.set_mask_y(y);
        area.set_mask_w(width.max(0.0));
        area.set_mask_h(height.max(0.0));
        area.set_mask_rotation(rotation as f32);
    }

    // ---------- Edit Transform ----------

    pub fn show_transform(&self, app: &Rc<App>, window: &AppWindow) {
        let area = window.global::<SourcesArea>();
        if area.get_align_options().row_count() == 0 {
            area.set_align_options(model(
                ALIGNS.iter().map(|a| kit::option(a, &humanize(a), "")),
            ));
            area.set_fit_options(model(BOUNDS.iter().map(|(v, l)| kit::option(v, l, ""))));
        }
        let found = items::selected_json(app);
        area.set_transform_item(found.is_some());
        let Some((_, chain)) = found else {
            area.set_transform_title("Edit Transform".into());
            area.set_transform_name("".into());
            return;
        };
        let item = &chain[chain.len() - 1];
        let label = app
            .session
            .borrow()
            .selected(|_, c| {
                let s = app.session.borrow();
                s.setup
                    .as_ref()
                    .map(|setup| setup_model::item_label(setup, c[c.len() - 1]))
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        area.set_transform_title(format!("Edit Transform for “{label}”").into());
        area.set_transform_name(label.as_str().into());
        area.set_transform_locked(item["locked"] == true);
        let n = |v: &Value, d: f64| v.as_f64().unwrap_or(d);
        let rect = &item["rect"];
        let (x, y, w, h) = (
            n(&rect["x"], 0.0),
            n(&rect["y"], 0.0),
            n(&rect["width"], 0.0),
            n(&rect["height"], 0.0),
        );
        let (ax, ay) = anchor(&area.get_transform_anchor());
        let t = &item["style"]["transform"];
        // Without bounds the scale sizes it: the fields are the box shown.
        let (x, y, w, h) = match shown_box(item) {
            Some(b) => (b[0], b[1], b[2], b[3]),
            None => (x, y, w, h),
        };
        area.set_transform_x((x + ax * w) as f32);
        area.set_transform_y((y + ay * h) as f32);
        area.set_transform_w(w as f32);
        area.set_transform_h(h as f32);
        area.set_transform_rotate(n(&t["rotate"], 0.0) as f32);
        area.set_transform_flip_horizontal(t["flip_horizontal"] == true);
        area.set_transform_flip_vertical(t["flip_vertical"] == true);
        area.set_transform_scale(((n(&t["scale"], 1.0) * 1000.0).round() / 10.0) as f32);
        area.set_transform_fit(item["fit"].as_str().unwrap_or("stretch").into());
        area.set_transform_align(item["align"].as_str().unwrap_or("center").into());
        let crop = &item["crop"];
        area.set_crop_left(n(&crop["left"], 0.0) as f32);
        area.set_crop_right(n(&crop["right"], 0.0) as f32);
        area.set_crop_top(n(&crop["top"], 0.0) as f32);
        area.set_crop_bottom(n(&crop["bottom"], 0.0) as f32);
        let style = &item["style"];
        area.set_transform_opacity(n(&style["opacity"], 1.0) as f32);
        let radius = match &style["radius"] {
            Value::Array(a) => a.first().and_then(Value::as_f64).unwrap_or(0.0),
            other => other.as_f64().unwrap_or(0.0),
        };
        area.set_transform_radius(radius as f32);
        let border = &style["border"];
        area.set_has_border(!border.is_null());
        area.set_border_width(n(&border["width"], 0.0) as f32);
        let color = |v: &Value| v.as_str().unwrap_or_default().to_string();
        area.set_border_color(color(&border["color"]).into());
        area.set_border_swatch(form::swatch(&color(&border["color"])));
        let shadow = &style["shadow"];
        area.set_has_shadow(!shadow.is_null());
        area.set_shadow_blur(n(&shadow["blur"], 0.0) as f32);
        area.set_shadow_color(color(&shadow["color"]).into());
        area.set_shadow_swatch(form::swatch(&color(&shadow["color"])));
        area.set_has_background(!style["background"].is_null());
        area.set_background_color(color(&style["background"]).into());
        area.set_background_swatch(form::swatch(&color(&style["background"])));
    }

    pub fn transform_number(
        &self,
        app: &Rc<App>,
        window: &AppWindow,
        field: &str,
        v: f32,
        live: bool,
    ) {
        let Some((scene, chain)) = items::selected_json(app) else {
            return;
        };
        let item = &chain[chain.len() - 1];
        if item["locked"] == true {
            return;
        }
        let v = v as f64;
        let rect = &item["rect"];
        let n = |v: &Value| v.as_f64().unwrap_or(0.0);
        let (ax, ay) = anchor(&window.global::<SourcesArea>().get_transform_anchor());
        let crop_side = |side: &str| {
            let mut crop = json!({ "left": 0, "top": 0, "right": 0, "bottom": 0 });
            if let Some(old) = item["crop"].as_object() {
                for (k, x) in old {
                    crop[k.as_str()] = x.clone();
                }
            }
            crop[side] = items::whole(v);
            json!({ "crop": crop })
        };
        if let Some([sx, sy, sw, sh]) = shown_box(item)
            && matches!(field, "x" | "y" | "w" | "h")
        {
            // An item without bounds: its size is its scale (the rect stays
            // the source's own), changed about the anchor, which stays put.
            let (rw, rh) = (n(&rect["width"]).max(1.0), n(&rect["height"]).max(1.0));
            let t = &item["style"]["transform"]["translate"];
            let (tx, ty) = (n(&t[0]), n(&t[1]));
            let scale0 = sw / rw;
            let scale = match field {
                "w" => v.max(1.0) / rw,
                "h" => v.max(1.0) / rh,
                _ => scale0,
            };
            let (nw, nh) = (rw * scale, rh * scale);
            let left = match field {
                "x" => v - ax * nw,
                _ => sx + ax * sw - ax * nw,
            };
            let top = match field {
                "y" => v - ay * nh,
                _ => sy + ay * sh - ay * nh,
            };
            let fields = json!({
                "rect": {
                    "x": items::int(left + nw / 2.0 - rw / 2.0 - tx),
                    "y": items::int(top + nh / 2.0 - rh / 2.0 - ty),
                },
                "style": { "transform": { "scale": (scale * 10_000.0).round() / 10_000.0 } },
            });
            // One live key for size and position: they change the same fields.
            let live = live.then(|| format!("transform:size:{scene}"));
            items::update_selected(app, fields, live);
            self.show_transform(app, window);
            return;
        }
        let fields = match field {
            "x" => json!({ "rect": { "x": items::int(v - ax * n(&rect["width"])) } }),
            "y" => json!({ "rect": { "y": items::int(v - ay * n(&rect["height"])) } }),
            "w" => json!({ "rect": { "width": items::whole(v.max(1.0)) } }),
            "h" => json!({ "rect": { "height": items::whole(v.max(1.0)) } }),
            "rotate" => json!({ "style": { "transform": { "rotate": v } } }),
            "scale" => json!({ "style": { "transform": { "scale": v / 100.0 } } }),
            "crop-left" => crop_side("left"),
            "crop-right" => crop_side("right"),
            "crop-top" => crop_side("top"),
            "crop-bottom" => crop_side("bottom"),
            "opacity" => json!({ "style": { "opacity": (v * 100.0).round() / 100.0 } }),
            "radius" => {
                if v == 0.0 && !live {
                    json!({ "style": { "radius": null } })
                } else {
                    json!({ "style": { "radius": v.round() } })
                }
            }
            "border-width" => json!({ "style": { "border": { "width": v.round() } } }),
            "shadow-blur" => json!({ "style": { "shadow": { "blur": v.round() } } }),
            _ => return,
        };
        let live = live.then(|| format!("transform:{field}:{scene}"));
        items::update_selected(app, fields, live);
        self.show_transform(app, window);
    }

    pub fn transform_choose(&self, app: &Rc<App>, window: &AppWindow, field: &str, value: &str) {
        match field {
            "anchor" => {
                window
                    .global::<SourcesArea>()
                    .set_transform_anchor(value.into());
            }
            "fit" => items::update_selected(app, json!({ "fit": value }), None),
            "align" => items::update_selected(
                app,
                json!({ "align": if value == "center" { Value::Null } else { json!(value) } }),
                None,
            ),
            _ => {}
        }
        self.show_transform(app, window);
    }

    pub fn transform_color(
        &self,
        app: &Rc<App>,
        window: &AppWindow,
        field: &str,
        text: &str,
        live: bool,
    ) {
        let text = text.trim();
        if form::valid_color(text) {
            let fields = match field {
                "border-color" => json!({ "style": { "border": { "color": text } } }),
                "shadow-color" => json!({ "style": { "shadow": { "color": text } } }),
                "background-color" => json!({ "style": { "background": text } }),
                _ => return,
            };
            let scene = app.session.borrow().editing_scene().unwrap_or_default();
            items::update_selected(
                app,
                fields,
                live.then(|| format!("transform:{field}:{scene}")),
            );
        }
        self.show_transform(app, window);
    }

    pub fn transform_action(&self, app: &Rc<App>, window: &AppWindow, action: &str) {
        let fields = match action {
            "reset" => {
                items::reset_transform(app);
                return;
            }
            "flip-horizontal" | "flip-vertical" => {
                items::flip(app, &action.replace('-', "_"));
                self.show_transform(app, window);
                return;
            }
            "add-border" => json!({ "style": { "border": { "width": 4, "color": "#eeeae2" } } }),
            "remove-border" => json!({ "style": { "border": null } }),
            "add-shadow" => {
                json!({ "style": { "shadow": { "x": 0, "y": 8, "blur": 24, "color": "#00000099" } } })
            }
            "remove-shadow" => json!({ "style": { "shadow": null } }),
            "add-background" => json!({ "style": { "background": "#000000" } }),
            "remove-background" => json!({ "style": { "background": null } }),
            _ => return,
        };
        items::update_selected(app, fields, None);
        self.show_transform(app, window);
    }

    // ---------- Rename ----------

    pub fn rename_accept(&self, app: &Rc<App>, window: &AppWindow) {
        let id = self.rename_source.borrow().clone();
        let name = window
            .global::<SourcesArea>()
            .get_rename_name()
            .trim()
            .to_string();
        let now = source_json(app, &id).map(|s| name_or_kind(&s));
        if !name.is_empty() && now.as_deref() != Some(name.as_str()) {
            app.edit(
                json!({ "sources": [{ "id": id, "name": name }] }),
                EditOptions::default(),
            );
        }
        self.close(app);
    }
}

fn filter_key(f: &Value) -> String {
    f["id"]
        .as_str()
        .or(f["kind"].as_str())
        .unwrap_or_default()
        .to_string()
}

fn filter_identity(f: &Value) -> Value {
    match f["id"].as_str() {
        Some(id) => json!({ "id": id }),
        None => json!({ "kind": f["kind"] }),
    }
}

/// Width and height go together (a colour, a browser, a test pattern):
/// one of them set alone takes the other from the source as it is now
/// (its running size, else the canvas's), so the change applies at once
/// instead of being refused; one cleared clears both.
fn pair_size(app: &Rc<App>, id: &str, changes: &mut Map<String, Value>) {
    let (w, h) = (
        changes.get("width").cloned(),
        changes.get("height").cloned(),
    );
    let (key, other) = match (&w, &h) {
        (Some(_), None) => ("width", "height"),
        (None, Some(_)) => ("height", "width"),
        _ => return,
    };
    let Some(source) = source_json(app, id) else {
        return;
    };
    if !source[other].is_null() {
        if changes[key].is_null() {
            changes.insert(other.into(), Value::Null);
        }
        return;
    }
    if changes[key].is_null() {
        return;
    }
    let s = app.session.borrow();
    let running = s.running.get(id).and_then(|r| {
        let size = if other == "width" { r.width } else { r.height };
        size.filter(|v| *v > 0)
    });
    let canvas = s.setup.as_ref().map(|x| {
        if other == "width" {
            x.canvas.width
        } else {
            x.canvas.height
        }
    });
    if let Some(v) = running.or(canvas) {
        changes.insert(other.into(), json!(v));
    }
}

/// The box an item without bounds (`fit: none`) shows, in its parent's
/// pixels (left, top, width, height): its rect (the source's own size)
/// scaled about its centre and moved by its translate. `None` for an item
/// whose rect is its box.
fn shown_box(item: &Value) -> Option<[f64; 4]> {
    if item["fit"] != "none" || !item["layout"].is_null() {
        return None;
    }
    let n = |v: &Value, d: f64| v.as_f64().unwrap_or(d);
    let rect = &item["rect"];
    let t = &item["style"]["transform"];
    let scale = n(&t["scale"], 1.0);
    let (w, h) = (n(&rect["width"], 0.0), n(&rect["height"], 0.0));
    let cx = n(&rect["x"], 0.0) + w / 2.0 + n(&t["translate"][0], 0.0);
    let cy = n(&rect["y"], 0.0) + h / 2.0 + n(&t["translate"][1], 0.0);
    Some([
        cx - w * scale / 2.0,
        cy - h * scale / 2.0,
        w * scale,
        h * scale,
    ])
}

/// `base`, or `base-2`, `base-3`… if taken (ids are lowercase letters,
/// digits and dashes).
fn unique_filter_id(taken: &[String], base: &str) -> String {
    let base = base.replace('_', "-");
    // A filter without an id goes by its kind (`color_correction`).
    let taken: Vec<String> = taken.iter().map(|t| t.replace('_', "-")).collect();
    if !taken.contains(&base) {
        return base;
    }
    let stem = match base.rsplit_once('-') {
        Some((stem, n)) if n.chars().all(|c| c.is_ascii_digit()) && !stem.is_empty() => stem,
        _ => base.as_str(),
    };
    (2..)
        .map(|n| format!("{stem}-{n}"))
        .find(|id| !taken.contains(id))
        .expect("an unused number")
}

/// A typed name as a filter id: "Warm Glow" → `warm-glow`.
fn filter_id_from(name: &str) -> String {
    let mut id = String::new();
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            id.push(c.to_ascii_lowercase());
        } else if !id.is_empty() && !id.ends_with('-') {
            id.push('-');
        }
    }
    id.trim_end_matches('-').to_string()
}

fn filter_name(f: &Value) -> String {
    match f["id"].as_str() {
        Some(id) => humanize(&id.replace('-', "_")),
        None => humanize(f["kind"].as_str().unwrap_or_default()),
    }
}

/// Whether `scene` shows `target` through nested scenes (to refuse loops).
fn shows_scene(setup: &isocast_protocol::Setup, scene: &str, target: &str, depth: u32) -> bool {
    if scene == target {
        return true;
    }
    if depth > 8 {
        return false;
    }
    fn walk(
        setup: &isocast_protocol::Setup,
        items: &[isocast_protocol::SceneItem],
        target: &str,
        depth: u32,
    ) -> bool {
        items.iter().any(|i| {
            i.scene
                .as_deref()
                .is_some_and(|s| shows_scene(setup, s, target, depth + 1))
                || walk(setup, &i.children, target, depth)
        })
    }
    setup_model::find_scene(setup, scene).is_some_and(|sc| walk(setup, &sc.items, target, depth))
}

const ALIGNS: [&str; 9] = [
    "top_left",
    "top",
    "top_right",
    "left",
    "center",
    "right",
    "bottom_left",
    "bottom",
    "bottom_right",
];

/// OBS's bounding box types and Isocast's fits.
const BOUNDS: [(&str, &str); 7] = [
    ("stretch", "Stretch to bounds"),
    ("contain", "Scale to inner bounds"),
    ("cover", "Scale to outer bounds"),
    ("width", "Scale to width of bounds"),
    ("height", "Scale to height of bounds"),
    ("scale_down", "Maximum size only"),
    ("none", "No scaling (original size)"),
];

/// Where an alignment sits in a box, as fractions of its width and height.
fn anchor(align: &str) -> (f64, f64) {
    match align {
        "top" => (0.5, 0.0),
        "top_right" => (1.0, 0.0),
        "left" => (0.0, 0.5),
        "center" => (0.5, 0.5),
        "right" => (1.0, 0.5),
        "bottom_left" => (0.0, 1.0),
        "bottom" => (0.5, 1.0),
        "bottom_right" => (1.0, 1.0),
        _ => (0.0, 0.0),
    }
}

use slint::Model as _;

/// The fields a filter's form leaves out: its id and switch (the list has
/// them), a mask's animations (written in the setup), and a shape mask's
/// numbers its shape doesn't use (#115).
fn filter_skips(filter: &Value) -> Vec<&'static str> {
    let mut skip = vec!["id", "enabled"];
    let kind = filter["kind"].as_str().unwrap_or_default();
    if kind.ends_with("_mask") && kind != "image_mask" {
        skip.push("animations");
    }
    if kind == "shape_mask" {
        let shape = filter["shape"].as_str().unwrap_or("rectangle");
        if shape != "polygon" {
            skip.push("sides");
        }
        if shape != "star" {
            skip.extend(["points", "inner_radius"]);
        }
        if !matches!(shape, "rectangle" | "polygon" | "star") {
            skip.push("radius");
        }
    }
    skip
}

/// Show the desktop's sharing dialog for display or window source `id`
/// (`screen_pick`). The engine keeps the choice and switches the source to
/// it. The call runs on a connection of its own: the dialog waits for the
/// person, and the window's calls mustn't wait with it.
pub fn pick_screen(app: &Rc<App>, id: String) {
    if app.mode.offscreen || id.is_empty() {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("screen pick".into())
        .spawn(move || {
            let result = crate::daemon::connect()
                .and_then(|mut client| {
                    client.call::<ScreenPicked>(Method::ScreenPick(ScreenPickParams { source: id }))
                })
                .map_err(|error| format!("{error:#}"));
            let _ = slint::invoke_from_event_loop(move || {
                crate::window::with_app(|app| match result {
                    Ok(picked) if !picked.remembered => {
                        app.toast(
                            ToastKind::Warning,
                            "The desktop won’t remember this choice",
                            "Its sharing dialog asks again each time the source starts. On \
                             Hyprland, tick “Allow a restore token” in it.",
                        );
                    }
                    Ok(_) => {}
                    Err(message) => {
                        app.toast(ToastKind::Error, "The source wasn’t changed", message);
                    }
                });
            });
        });
    if let Err(error) = spawned {
        app.toast(
            ToastKind::Error,
            "Couldn’t open the sharing dialog",
            error.to_string(),
        );
    }
}
