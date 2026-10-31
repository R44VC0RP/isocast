//! Forms from the setup schema (the web app's fields.js and
//! device-fields.js), shared by Properties, Filters and Transition
//! Properties: the rows a source's, filter's or transition's settings
//! get, with dropdowns where the choices are known (devices, apps,
//! windows, a camera's formats, a compressor's sidechain, a scaling
//! filter's shape), and what each of their controls changes
//! ([`change`]). ui/widgets/settings-form.slint draws the rows; its
//! `SettingsForms` callbacks come to [`connect`], which hands each to the
//! view that shows the form, by the form's id.
//!
//! The rows go to Slint in models that are changed in place, row by row,
//! so a field being typed in or scrubbed isn't rebuilt under the pointer.

pub mod devices;
pub mod files;
pub mod schema;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use isocast_protocol::api::{SourceRun, SourceStatus};
use isocast_protocol::devices::Devices;
use serde_json::{Map, Value, json};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::app::App;
use crate::{AppWindow, DropdownOption, FormField, FormGroup, SettingsForms};
use devices::{self as dev, Choice};
use schema::{Field, FieldType, humanize};

/// One row as Rust builds it (compared with the last one shown, so only
/// changed rows reach Slint).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Row {
    pub key: String,
    pub label: String,
    pub kind: &'static str,
    pub text: String,
    pub number: f64,
    pub empty: bool,
    pub on: bool,
    pub unit: String,
    /// Right after the number ("×"), which then shows all its decimals.
    pub suffix: String,
    pub step: f64,
    pub min: f64,
    pub max: f64,
    pub placeholder: String,
    pub options: Vec<Choice>,
    pub value: String,
    pub mono: bool,
    pub files: Vec<String>,
    /// The folder each of `files` is in, the home folder as `~`.
    pub dirs: Vec<String>,
    pub removable: bool,
    pub refresh: bool,
    pub refreshing: bool,
    pub lane: bool,
    pub hint: String,
    pub hint_warn: bool,
    pub extra: bool,
    pub extra_text: String,
    pub extra_placeholder: String,
    pub tooltip: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Group {
    pub row: Row,
    pub children: Vec<Row>,
}

/// What a form needs besides its fields and values.
pub struct Context<'a> {
    /// The source kind, or "filter" for a filter's settings.
    pub kind: &'a str,
    /// The source the settings are (or whose filter they are), if it
    /// exists already.
    pub source: Option<&'a str>,
    pub setup: Option<&'a isocast_protocol::Setup>,
    pub devices: Option<&'a Devices>,
    pub running: Option<&'a SourceStatus>,
    /// Fields not shown ("id", "enabled" for filters).
    pub skip: &'a [&'a str],
    pub refreshing: bool,
    /// "Part of a title…" or "Custom…" chosen: the typed value shows.
    pub typing: bool,
}

// A camera's width, height and fps are one Resolution and one Frame rate
// dropdown, drawn after its device.
const CAMERA_FORMAT: [&str; 3] = ["width", "height", "fps"];
const PATH_FIELDS: [&str; 4] = ["path", "file", "image", "matte"];

/// Pseudo-keys of the rows that aren't one field.
pub const RESOLUTION: &str = "@resolution";
pub const FRAME_RATE: &str = "@fps";

fn may_trail(f: &Field) -> bool {
    (f.ty == FieldType::String && PATH_FIELDS.contains(&f.name.as_str()))
        || (f.optional && matches!(f.ty, FieldType::Color | FieldType::Enum(_)))
}

fn trailing(f: &Field, current: &Value) -> bool {
    (f.ty == FieldType::String && PATH_FIELDS.contains(&f.name.as_str()))
        || (f.optional
            && !current.is_null()
            && matches!(f.ty, FieldType::Color | FieldType::Enum(_)))
}

/// A JSON value as the interface writes it in a field.
pub fn text_of(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

fn file_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// The folder a file is in, as the Files list shows it ("~/Movies").
fn folder_of(path: &str) -> String {
    match path.rsplit_once('/') {
        Some(("", _)) => "/".into(),
        Some((folder, _)) => crate::platform::abbreviate_home(folder),
        None => String::new(),
    }
}

/// Every row of `fields` for settings `value` (keys prefixed with
/// `prefix.` inside an object).
pub fn build(ctx: &Context, fields: &[Field], value: &Map<String, Value>) -> Vec<Group> {
    let camera = ctx.kind == "camera";
    let shown: Vec<&Field> = fields
        .iter()
        .filter(|f| !ctx.skip.contains(&f.name.as_str()))
        .filter(|f| !(camera && CAMERA_FORMAT.contains(&f.name.as_str())))
        // The desktop's dialog chooses the window itself, not by its title.
        .filter(|f| !(dev::PICKED_IN_DIALOG && ctx.kind == "window" && f.name == "title"))
        .collect();
    // When any row ends in a button, every row keeps that lane, so the
    // fields share one right edge.
    let lane = shown.iter().any(|f| may_trail(f));
    let mut groups = Vec::new();
    for f in shown {
        let current = value.get(&f.name).cloned().unwrap_or(Value::Null);
        if camera && f.name == "device" {
            let mut row = control(ctx, f, &current, value, "");
            row.lane = lane;
            groups.push(Group {
                row,
                children: Vec::new(),
            });
            let (resolution, rate) = camera_format(ctx, value);
            groups.push(Group {
                row: resolution,
                children: Vec::new(),
            });
            groups.push(Group {
                row: rate,
                children: Vec::new(),
            });
            continue;
        }
        if let FieldType::Object(inner) = &f.ty {
            let on = !current.is_null();
            let row = Row {
                key: f.name.clone(),
                label: f.label.clone(),
                kind: "object",
                on,
                lane,
                tooltip: f.description.clone(),
                ..Row::default()
            };
            let children = if on {
                let object = current.as_object().cloned().unwrap_or_default();
                let inner_lane = inner.iter().any(may_trail);
                inner
                    .iter()
                    .map(|c| {
                        let now = object.get(&c.name).cloned().unwrap_or(Value::Null);
                        let mut row = control(ctx, c, &now, &object, &f.name);
                        row.lane = inner_lane && !trailing(c, &now);
                        row
                    })
                    .collect()
            } else {
                Vec::new()
            };
            groups.push(Group { row, children });
            continue;
        }
        let mut row = control(ctx, f, &current, value, "");
        // A microphone's channels end where its device's dropdown does,
        // before the refresh button.
        row.lane =
            (lane && !trailing(f, &current)) || (ctx.kind == "microphone" && f.name == "channels");
        groups.push(Group {
            row,
            children: Vec::new(),
        });
    }
    groups
}

fn key_of(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}.{name}")
    }
}

/// The row for one field.
fn control(
    ctx: &Context,
    f: &Field,
    current: &Value,
    all: &Map<String, Value>,
    prefix: &str,
) -> Row {
    let mut row = Row {
        key: key_of(prefix, &f.name),
        label: f.label.clone(),
        tooltip: f.description.clone(),
        ..Row::default()
    };
    let shown = if current.is_null() {
        f.default.clone().unwrap_or(Value::Null)
    } else {
        current.clone()
    };
    // An optional colour or choice that is off: "+ Add".
    if f.optional && current.is_null() && matches!(f.ty, FieldType::Color | FieldType::Enum(_)) {
        row.kind = "add";
        return row;
    }
    if prefix.is_empty() && choice_control(ctx, f, &shown, all, &mut row) {
        return row;
    }
    row.removable =
        f.optional && !current.is_null() && matches!(f.ty, FieldType::Color | FieldType::Enum(_));
    match &f.ty {
        FieldType::Boolean => {
            row.kind = "switch";
            row.on = shown.as_bool().unwrap_or(false);
        }
        FieldType::Color => {
            row.kind = "color";
            row.text = text_of(current);
            row.mono = true;
        }
        FieldType::Enum(options) => {
            row.kind = "choice";
            row.value = text_of(&shown);
            if row.value.is_empty() {
                row.value = options.first().map(text_of).unwrap_or_default();
            }
            row.options = options
                .iter()
                .map(|o| (text_of(o), humanize(&text_of(o)), String::new()))
                .collect();
        }
        FieldType::Number { integer, min, max } => {
            row.kind = "number";
            row.unit = f.unit.to_string();
            // A rate of play is a multiple: "1.00×" (Paper's Speed).
            if f.name == "speed" {
                row.suffix = "×".into();
            }
            row.number = shown.as_f64().unwrap_or(0.0);
            row.empty = shown.is_null();
            row.min = if min.is_finite() { *min } else { -1e9 };
            row.max = if max.is_finite() { *max } else { 1e9 };
            row.step = if *integer {
                1.0
            } else if (max - min).abs() <= 10.0 {
                0.01
            } else {
                0.1
            };
            if f.optional && current.is_null() {
                row.placeholder = "Auto".into();
            }
        }
        FieldType::Strings => {
            row.kind = "files";
            let paths: Vec<&str> = current
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            row.files = paths.iter().map(|p| file_name(p)).collect();
            row.dirs = paths.iter().map(|p| folder_of(p)).collect();
        }
        FieldType::String => {
            row.text = text_of(current);
            if PATH_FIELDS.contains(&f.name.as_str()) {
                row.kind = "path";
                row.placeholder = "A file path".into();
            } else {
                row.kind = if f.name == "text" || f.name == "css" {
                    "textarea"
                } else {
                    "text"
                };
                row.mono = f.name == "css";
                if f.optional {
                    row.placeholder = "None".into();
                }
            }
        }
        FieldType::Object(_) | FieldType::Json => {
            row.kind = "json";
            row.text = current.to_string();
        }
    }
    row
}

/// Why a list of choices is empty, if the engine said.
fn empty_note(devices: Option<&Devices>, what: &str) -> Option<&'static str> {
    use isocast_protocol::devices::Permission;
    let devices = devices?;
    // Unknown isn't a refusal, so it gets no note.
    let refused = |p: Permission| !matches!(p, Permission::Granted | Permission::Unknown);
    if what == "windows" && refused(devices.permissions.screen_recording) {
        return Some("Screen recording isn’t allowed, so no windows are listed.");
    }
    if what == "cameras" && refused(devices.permissions.camera) {
        return Some("Camera access isn’t allowed, so no cameras are listed.");
    }
    None
}

/// The control for a setting with known choices, if `f` of this kind has
/// them (filled into `row`).
fn choice_control(
    ctx: &Context,
    f: &Field,
    value: &Value,
    all: &Map<String, Value>,
    row: &mut Row,
) -> bool {
    let kind = ctx.kind;
    let name = f.name.as_str();
    let v = text_of(value);
    let devices = ctx.devices;
    row.kind = "choice";
    match (kind, name) {
        ("camera", "device") => {
            let none = devices.is_none_or(|d| d.cameras.is_empty());
            row.options = if none && v.is_empty() {
                let note = if devices.is_none() {
                    "Looking…"
                } else {
                    empty_note(devices, "cameras").unwrap_or("No cameras found.")
                };
                vec![(String::new(), note.into(), String::new())]
            } else {
                dev::camera_options(devices, &v)
            };
            row.value = v;
            row.refresh = true;
        }
        ("microphone", "device") => {
            row.options = dev::microphone_options(devices, &v);
            row.value = v;
            row.refresh = true;
        }
        ("microphone", "channels") => {
            let device = all.get("device").map(text_of);
            let device = device.as_deref().unwrap_or("default");
            let v = dev::channels_value(value);
            row.options = dev::channel_options(devices, device, &v);
            row.value = v;
        }
        ("display" | "application", "display") => {
            row.options = dev::display_options(devices, &v);
            row.value = v;
            row.refresh = true;
        }
        ("application_audio", "app") => {
            let none = devices.is_none_or(|d| d.windows.is_empty() && d.audio_apps.is_empty());
            row.options = if none && v.is_empty() {
                let note = if devices.is_none() {
                    "Looking…"
                } else {
                    "No apps with sound found."
                };
                vec![(String::new(), note.into(), String::new())]
            } else {
                dev::audio_app_options(devices, &v)
            };
            row.value = v;
            row.refresh = true;
        }
        ("system_audio", "exclude") => {
            // The apps left out, each removable, and a menu to add one.
            let excluded: Vec<String> = value
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default();
            row.kind = "files";
            row.files = excluded.iter().map(|e| dev::app_name(devices, e)).collect();
            row.dirs = excluded
                .iter()
                .map(|e| {
                    let name = dev::app_name(devices, e);
                    if name == *e { String::new() } else { e.clone() }
                })
                .collect();
            row.options = dev::exclude_options(devices, &excluded);
            row.value = String::new();
            row.refresh = true;
        }
        ("window", "app") if dev::PICKED_IN_DIALOG => {
            // The dialog chooses the window; `app` only names it.
            row.options = dev::picked_window_options(&v);
            row.value = v.clone();
            row.extra = true;
            row.extra_text = v;
            row.extra_placeholder = "A name for this window, like its app".into();
            row.lane = true;
        }
        ("window" | "application", "app") => {
            let none = devices.is_none_or(|d| d.windows.is_empty());
            row.options = if none && v.is_empty() {
                let note = if devices.is_none() {
                    "Looking…"
                } else {
                    empty_note(devices, "windows").unwrap_or("No apps with windows found.")
                };
                vec![(String::new(), note.into(), String::new())]
            } else {
                dev::app_options(devices, &v)
            };
            row.value = v;
            row.refresh = true;
        }
        ("window", "title") => {
            let app = all.get("app").map(text_of).unwrap_or_default();
            row.options = dev::title_options(devices, &app, &v);
            let whole = !v.is_empty() && row.options.iter().any(|o| o.0 == v);
            let part = ctx.typing || (!v.is_empty() && !whole);
            row.value = if part {
                dev::TITLE_PART.into()
            } else {
                v.clone()
            };
            row.extra = part;
            row.extra_text = v;
            row.extra_placeholder = "Part of the window’s title".into();
            row.lane = true;
        }
        ("filter", "sidechain_source") => {
            let sources = ctx.setup.map(|s| s.sources.as_slice()).unwrap_or_default();
            let mut options = vec![(
                String::new(),
                "None".to_string(),
                "its own sound".to_string(),
            )];
            let audio: Vec<_> = sources
                .iter()
                .filter(|s| Some(s.id.as_str()) != ctx.source && s.kind.has_audio())
                .collect();
            options.extend(audio.iter().map(|s| {
                (
                    s.id.clone(),
                    crate::model::source_name(s).to_string(),
                    crate::model::kind_info(s.kind.label()).label.to_string(),
                )
            }));
            if !v.is_empty() && !audio.iter().any(|s| s.id == v) {
                options.push((v.clone(), v.clone(), "not in this setup".into()));
            }
            row.options = options;
            row.value = v;
        }
        ("filter", "aspect") => {
            let common = ["16:9", "4:3", "3:2", "1:1", "9:16", "21:9"];
            let custom = ctx.typing || (!v.is_empty() && !common.contains(&v.as_str()));
            let mut options = vec![(String::new(), "Unchanged".to_string(), String::new())];
            options.extend(
                common
                    .iter()
                    .map(|a| (a.to_string(), a.to_string(), String::new())),
            );
            options.push((
                "custom".into(),
                if custom { "Custom" } else { "Custom…" }.into(),
                String::new(),
            ));
            row.options = options;
            row.value = if custom { "custom".into() } else { v.clone() };
            row.extra = custom;
            row.extra_text = v;
            row.extra_placeholder = "w:h, like 5:4".into();
        }
        _ => {
            row.kind = "";
            return false;
        }
    }
    row.refreshing = ctx.refreshing && row.refresh;
    true
}

/// A camera's Resolution and Frame rate rows, from the formats the chosen
/// camera offers, and what the engine receives now.
fn camera_format(ctx: &Context, all: &Map<String, Value>) -> (Row, Row) {
    let device = all.get("device").map(text_of).unwrap_or_default();
    let cameras = ctx
        .devices
        .map(|d| d.cameras.as_slice())
        .unwrap_or_default();
    let camera = dev::match_device(cameras, &device);
    let number = |name: &str| all.get(name).and_then(Value::as_f64);
    let size = match (number("width"), number("height")) {
        (Some(w), Some(h)) if w > 0.0 && h > 0.0 => Some((w as u32, h as u32)),
        _ => None,
    };
    let fps = number("fps").map(|f| f.round() as i64);
    let canvas = ctx
        .setup
        .map(|s| (s.canvas.width, s.canvas.height, s.canvas.fps))
        .unwrap_or((1920, 1080, 30));
    let sizes = dev::camera_sizes(camera);
    let auto = camera.and_then(|c| dev::auto_format(c, canvas, None, fps));
    let mut size_options = vec![(
        "auto".to_string(),
        "Auto".to_string(),
        auto.map(|(w, h, _)| format!("{w}×{h} on this canvas"))
            .unwrap_or_default(),
    )];
    size_options.extend(sizes.iter().map(|(w, h)| {
        (
            format!("{w}x{h}"),
            format!("{w}×{h}"),
            dev::aspect_name(*w, *h).to_string(),
        )
    }));
    if let Some((w, h)) = size
        && !sizes.contains(&(w, h))
    {
        size_options.push((
            format!("{w}x{h}"),
            format!("{w}×{h}"),
            if camera.is_some() {
                "not offered"
            } else {
                "not connected"
            }
            .into(),
        ));
    }
    let resolution = Row {
        key: RESOLUTION.into(),
        label: "Resolution".into(),
        kind: "choice",
        options: size_options,
        value: size.map(|(w, h)| format!("{w}x{h}")).unwrap_or("auto".into()),
        lane: true,
        tooltip: "The size the camera sends. Auto: the largest that fits on the canvas at the best frame rate.".into(),
        ..Row::default()
    };

    let rates = dev::camera_rates(camera, size);
    let auto_rate = camera.and_then(|c| dev::auto_format(c, canvas, size, None));
    let mut rate_options = vec![(
        "auto".to_string(),
        "Auto".to_string(),
        auto_rate
            .map(|(_, _, f)| {
                format!(
                    "{f} fps{}",
                    if size.is_some() {
                        ""
                    } else {
                        " on this canvas"
                    }
                )
            })
            .unwrap_or_default(),
    )];
    rate_options.extend(
        rates
            .iter()
            .map(|(v, label)| (v.to_string(), label.clone(), String::new())),
    );
    if let Some(f) = fps
        && !rates.iter().any(|(r, _)| *r == f)
    {
        rate_options.push((
            f.to_string(),
            format!("{f} fps"),
            if camera.is_some() {
                "not offered"
            } else {
                "not connected"
            }
            .into(),
        ));
    }
    let (hint, warn) = match ctx.source {
        Some(_) => receiving(ctx.running),
        None => (String::new(), false),
    };
    let rate = Row {
        key: FRAME_RATE.into(),
        label: "Frame rate".into(),
        kind: "choice",
        options: rate_options,
        value: fps.map(|f| f.to_string()).unwrap_or("auto".into()),
        lane: true,
        hint,
        hint_warn: warn,
        tooltip: "Frames per second the camera sends. Auto: the canvas’s rate, or the nearest the camera offers.".into(),
        ..Row::default()
    };
    (resolution, rate)
}

/// What the engine gets from a running camera.
fn receiving(status: Option<&SourceStatus>) -> (String, bool) {
    let Some(status) = status else {
        return ("Not running: no scene or output uses it now.".into(), false);
    };
    match status.state {
        SourceRun::Opening => return ("Opening…".into(), false),
        SourceRun::Failed => return ("Not running.".into(), false),
        _ => {}
    }
    let (Some(w), Some(h)) = (status.width, status.height) else {
        return ("Running.".into(), false);
    };
    let size = format!("{w}×{h}");
    let opened = status.fps.unwrap_or(0);
    match status.received_fps {
        None => (format!("Opened at {size}, {opened} fps."), false),
        Some(0.0) => (
            format!("Opened at {size}, {opened} fps, but no frames are arriving."),
            true,
        ),
        Some(got) => {
            let mut text = format!("Receiving {size} at {} fps", fps_text(got));
            let mut warn = false;
            if opened > 0 && got < opened as f64 - 1.5 {
                text += &format!(" (set to {opened}; the camera sends fewer)");
                warn = true;
            }
            (text, warn)
        }
    }
}

fn fps_text(n: f64) -> String {
    if (n - n.round()).abs() < 0.05 {
        format!("{}", n.round() as i64)
    } else {
        format!("{n:.1}")
    }
}

// ---------- Changes ----------

/// The field a row's key names, and the object field it is in, if any.
pub fn find<'a>(fields: &'a [Field], key: &str) -> Option<(Option<&'a Field>, &'a Field)> {
    match key.split_once('.') {
        Some((outer, inner)) => {
            let parent = fields.iter().find(|f| f.name == outer)?;
            let FieldType::Object(children) = &parent.ty else {
                return None;
            };
            Some((Some(parent), children.iter().find(|f| f.name == inner)?))
        }
        None => Some((None, fields.iter().find(|f| f.name == key)?)),
    }
}

/// The top-level settings `key` set to `value` change: for a field inside
/// an object, the whole object with it changed.
pub fn set(
    fields: &[Field],
    all: &Map<String, Value>,
    key: &str,
    value: Value,
) -> Map<String, Value> {
    let mut out = Map::new();
    match find(fields, key) {
        Some((Some(parent), f)) => {
            let mut object = all
                .get(&parent.name)
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            if value.is_null() {
                object.remove(&f.name);
            } else {
                object.insert(f.name.clone(), value);
            }
            out.insert(parent.name.clone(), Value::Object(object));
        }
        Some((None, f)) => {
            out.insert(f.name.clone(), value);
        }
        None => {
            out.insert(key.to_string(), value);
        }
    }
    out
}

/// A colour as the setup takes it (#rgb, #rgba, #rrggbb, #rrggbbaa).
pub fn valid_color(text: &str) -> bool {
    let Some(hex) = text.strip_prefix('#') else {
        return false;
    };
    matches!(hex.len(), 3 | 4 | 6 | 8) && hex.chars().all(|c| c.is_ascii_hexdigit())
}

/// A colour field's swatch.
pub fn swatch(text: &str) -> slint::Color {
    let hex = text.trim_start_matches('#');
    let digits: String = match hex.len() {
        3 | 4 => hex.chars().flat_map(|c| [c, c]).collect(),
        _ => hex.to_string(),
    };
    let byte = |i: usize| u8::from_str_radix(digits.get(i..i + 2).unwrap_or("00"), 16).unwrap_or(0);
    if !valid_color(text) {
        return slint::Color::from_argb_u8(0, 0, 0, 0);
    }
    let alpha = if digits.len() == 8 { byte(6) } else { 255 };
    slint::Color::from_argb_u8(alpha, byte(0), byte(2), byte(4))
}

/// What typing `text` into a text, path or colour field sets (None: not
/// a value it takes).
pub fn text_value(f: &Field, text: &str) -> Option<Value> {
    match &f.ty {
        FieldType::Color => {
            let text = text.trim();
            if text.is_empty() && f.optional {
                Some(Value::Null)
            } else if valid_color(text) {
                Some(json!(text))
            } else {
                None
            }
        }
        FieldType::String if PATH_FIELDS.contains(&f.name.as_str()) => Some(if text.is_empty() {
            Value::Null
        } else {
            json!(text)
        }),
        _ => Some(if text.is_empty() && f.optional {
            Value::Null
        } else {
            json!(text)
        }),
    }
}

/// A number as the field holds it.
pub fn number_value(f: &Field, v: f64) -> Value {
    match f.ty {
        FieldType::Number { integer: true, .. } => json!(v.round() as i64),
        _ => json!((v * 10000.0).round() / 10000.0),
    }
}

/// An enum's value chosen in its dropdown (its own JSON type).
pub fn enum_value(f: &Field, chosen: &str) -> Value {
    if let FieldType::Enum(options) = &f.ty
        && let Some(o) = options.iter().find(|o| text_of(o) == chosen)
    {
        return o.clone();
    }
    json!(chosen)
}

/// The change choosing camera `device` makes: its resolution and frame
/// rate stay only if the new camera offers them, else back to Auto.
/// A microphone's `channels` dropdown value ("", "1", "3,4") as the
/// setting.
fn channels_json(chosen: &str) -> Value {
    if chosen.is_empty() {
        return Value::Null;
    }
    let channels: Vec<u32> = chosen
        .split(',')
        .filter_map(|c| c.trim().parse().ok())
        .collect();
    json!(channels)
}

/// Choosing a microphone: its channels go back to the default when the
/// new device doesn't have them.
fn microphone_change(
    devices: Option<&Devices>,
    all: &Map<String, Value>,
    device: &str,
) -> Map<String, Value> {
    let mut out = one("device", json!(device));
    let highest = all
        .get("channels")
        .and_then(Value::as_array)
        .and_then(|list| list.iter().filter_map(Value::as_u64).max());
    if let (Some(highest), Some(mic)) = (highest, dev::find_microphone(devices, device))
        && highest > u64::from(mic.channels)
    {
        out.insert("channels".into(), Value::Null);
    }
    out
}

pub fn camera_change(
    devices: Option<&Devices>,
    all: &Map<String, Value>,
    device: &str,
) -> Map<String, Value> {
    let cameras = devices.map(|d| d.cameras.as_slice()).unwrap_or_default();
    let camera = dev::match_device(cameras, device);
    let number = |name: &str| all.get(name).and_then(Value::as_f64);
    let size = match (number("width"), number("height")) {
        (Some(w), Some(h)) => Some((w as u32, h as u32)),
        _ => None,
    };
    let fps = number("fps").map(|f| f.round() as i64);
    let keep_size = size.is_none() || dev::camera_supports(camera, size, None);
    let keep_fps =
        fps.is_none() || dev::camera_supports(camera, if keep_size { size } else { None }, fps);
    let mut out = Map::new();
    out.insert("device".into(), json!(device));
    if !keep_size {
        out.insert("width".into(), Value::Null);
        out.insert("height".into(), Value::Null);
    }
    if !keep_fps {
        out.insert("fps".into(), Value::Null);
    }
    out
}

/// The camera format rows' changes.
pub fn camera_format_change(
    devices: Option<&Devices>,
    all: &Map<String, Value>,
    key: &str,
    chosen: &str,
) -> Map<String, Value> {
    let mut out = Map::new();
    if key == FRAME_RATE {
        out.insert(
            "fps".into(),
            chosen
                .parse::<i64>()
                .map(|f| json!(f))
                .unwrap_or(Value::Null),
        );
        return out;
    }
    let next = chosen
        .split_once('x')
        .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)));
    let device = all.get("device").map(text_of).unwrap_or_default();
    let cameras = devices.map(|d| d.cameras.as_slice()).unwrap_or_default();
    let camera = dev::match_device(cameras, &device);
    let fps = all
        .get("fps")
        .and_then(Value::as_f64)
        .map(|f| f.round() as i64);
    let keep_fps = fps.is_none() || dev::camera_supports(camera, next, fps);
    out.insert(
        "width".into(),
        next.map(|n| json!(n.0)).unwrap_or(Value::Null),
    );
    out.insert(
        "height".into(),
        next.map(|n| json!(n.1)).unwrap_or(Value::Null),
    );
    if !keep_fps {
        out.insert("fps".into(), Value::Null);
    }
    out
}

// ---------- To Slint ----------

fn options_model(options: &[Choice]) -> ModelRc<DropdownOption> {
    let mut list: Vec<DropdownOption> = options
        .iter()
        .map(|(v, l, d)| crate::kit::option(v, l, d))
        .collect();
    crate::kit::disambiguate(&mut list);
    ModelRc::new(VecModel::from(list))
}

fn field(row: &Row) -> FormField {
    FormField {
        key: row.key.as_str().into(),
        label: row.label.as_str().into(),
        kind: row.kind.into(),
        text: row.text.as_str().into(),
        rest_text: if row.kind == "path" {
            crate::areas::settings::middle_short(&row.text).into()
        } else {
            SharedString::new()
        },
        number: row.number as f32,
        empty: row.empty,
        on: row.on,
        unit: row.unit.as_str().into(),
        suffix: row.suffix.as_str().into(),
        step: row.step as f32,
        minimum: row.min as f32,
        maximum: row.max as f32,
        placeholder: row.placeholder.as_str().into(),
        options: options_model(&row.options),
        value: row.value.as_str().into(),
        mono: row.mono,
        swatch: swatch(&row.text),
        files: ModelRc::new(VecModel::from(
            row.files.iter().map(SharedString::from).collect::<Vec<_>>(),
        )),
        file_dirs: ModelRc::new(VecModel::from(
            row.dirs.iter().map(SharedString::from).collect::<Vec<_>>(),
        )),
        removable: row.removable,
        refresh: row.refresh,
        refreshing: row.refreshing,
        lane: row.lane,
        hint: row.hint.as_str().into(),
        hint_warn: row.hint_warn,
        extra: row.extra,
        extra_text: row.extra_text.as_str().into(),
        extra_placeholder: row.extra_placeholder.as_str().into(),
        tooltip: row.tooltip.as_str().into(),
    }
}

/// A form's rows as Slint shows them, changed in place.
pub struct FormModel {
    groups: Rc<VecModel<FormGroup>>,
    children: RefCell<Vec<Rc<VecModel<FormField>>>>,
    last: RefCell<Vec<Group>>,
}

impl Default for FormModel {
    fn default() -> Self {
        Self {
            groups: Rc::new(VecModel::default()),
            children: RefCell::default(),
            last: RefCell::default(),
        }
    }
}

impl FormModel {
    pub fn model(&self) -> ModelRc<FormGroup> {
        ModelRc::from(self.groups.clone())
    }

    /// Show `groups`, changing only the rows that differ.
    pub fn show(&self, groups: Vec<Group>) {
        let mut last = self.last.borrow_mut();
        let mut children = self.children.borrow_mut();
        for (i, group) in groups.iter().enumerate() {
            if i >= children.len() {
                children.push(Rc::new(VecModel::default()));
            }
            let kids = children[i].clone();
            let old = last.get(i);
            let old_children: &[Row] = old.map(|g| g.children.as_slice()).unwrap_or_default();
            for (j, child) in group.children.iter().enumerate() {
                if old_children.get(j) == Some(child) && j < kids.row_count() {
                    continue;
                }
                if j < kids.row_count() {
                    kids.set_row_data(j, field(child));
                } else {
                    kids.push(field(child));
                }
            }
            while kids.row_count() > group.children.len() {
                kids.remove(kids.row_count() - 1);
            }
            let same = old.is_some_and(|g| g.row == group.row) && i < self.groups.row_count();
            if !same {
                let data = FormGroup {
                    field: field(&group.row),
                    children: ModelRc::from(kids.clone()),
                };
                if i < self.groups.row_count() {
                    self.groups.set_row_data(i, data);
                } else {
                    self.groups.push(data);
                }
            }
        }
        while self.groups.row_count() > groups.len() {
            self.groups.remove(self.groups.row_count() - 1);
        }
        children.truncate(groups.len());
        *last = groups;
    }

    /// Forget the rows (a form for something else is next).
    pub fn clear(&self) {
        self.groups.clear();
        self.children.borrow_mut().clear();
        self.last.borrow_mut().clear();
    }
}

/// The settings fields of a source kind or filter kind ("Filter",
/// "AudioFilter").
pub fn fields(def: &str, kind: &str) -> Vec<Field> {
    schema::kind_fields(def, kind)
}

// ---------- Edits ----------

/// What a form's control did (`SettingsForms`' callbacks).
pub enum FormEvent {
    Text(String),
    /// A colour from the colour panel (a live gesture, like scrubbing).
    Color(String),
    Number(f64, bool),
    Switch(bool),
    Choose(String),
    Extra(String),
    Remove,
    Add,
    Browse,
    /// What a Browse chose on another computer's engine (area J's engine
    /// file browser answers later).
    Chosen(Vec<String>),
    RemoveFile(usize),
    Refresh,
}

/// The form an event is for: its fields and current settings, the kind
/// they belong to ("filter" for a filter's, else the source or transition
/// kind: it picks which files the chooser offers), and whether a choice
/// that is typed instead ("Part of a title…", "Custom…") is being typed.
pub struct Target<'a> {
    pub kind: &'a str,
    pub fields: &'a [Field],
    pub value: &'a Map<String, Value>,
    pub devices: Option<&'a Devices>,
    pub typing: &'a Cell<bool>,
}

/// What an event comes to.
pub enum Outcome {
    /// Top-level settings to merge (`null` removes one), and the setting
    /// being scrubbed or picked live, if the edit is part of a gesture.
    Set(Map<String, Value>, Option<String>),
    /// Nothing to send, but the form changes (a choice now typed).
    Reshow,
    /// Nothing at all (cancelled, not a value it takes, devices asked for).
    Nothing,
    /// Choose the source's screen or window in the desktop's sharing
    /// dialog (`screen_pick`).
    Pick,
}

/// The setting `key` changes when its control does `event`.
pub fn change(app: &Rc<App>, t: &Target, key: &str, event: FormEvent) -> Outcome {
    let (fields, value, kind, devices) = (t.fields, t.value, t.kind, t.devices);
    let mut live = None;
    let colour_panel = matches!(event, FormEvent::Color(_));
    let changes: Map<String, Value> = match event {
        FormEvent::Choose(chosen) => {
            if key == RESOLUTION || key == FRAME_RATE {
                camera_format_change(devices, value, key, &chosen)
            } else {
                let Some((_, f)) = find(fields, key) else {
                    return Outcome::Nothing;
                };
                if chosen == dev::PICK {
                    return Outcome::Pick;
                }
                match (kind, f.name.as_str()) {
                    ("window", "app") if dev::PICKED_IN_DIALOG => return Outcome::Nothing,
                    ("camera", "device") => camera_change(devices, value, &chosen),
                    ("microphone", "device") => microphone_change(devices, value, &chosen),
                    ("microphone", "channels") => one("channels", channels_json(&chosen)),
                    ("window", "title") => {
                        if chosen == dev::TITLE_PART {
                            t.typing.set(true);
                            return Outcome::Reshow;
                        }
                        t.typing.set(false);
                        one(
                            "title",
                            if chosen.is_empty() {
                                Value::Null
                            } else {
                                json!(chosen)
                            },
                        )
                    }
                    ("filter", "aspect") => {
                        if chosen == "custom" {
                            t.typing.set(true);
                            return Outcome::Reshow;
                        }
                        t.typing.set(false);
                        aspect((!chosen.is_empty()).then_some(chosen))
                    }
                    ("window", "app") => {
                        let mut out = one("app", json!(chosen));
                        let title = value.get("title").map(text_of).unwrap_or_default();
                        if !title.is_empty()
                            && dev::match_window(devices, &chosen, &title).is_none()
                        {
                            out.insert("title".into(), Value::Null);
                        }
                        out
                    }
                    ("system_audio", "exclude") => {
                        let mut list = value
                            .get(key)
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default();
                        if chosen.is_empty() || list.iter().any(|v| v == &json!(chosen)) {
                            return Outcome::Nothing;
                        }
                        list.push(json!(chosen));
                        one(key, Value::Array(list))
                    }
                    ("filter", "sidechain_source") => one(
                        "sidechain_source",
                        if chosen.is_empty() {
                            Value::Null
                        } else {
                            json!(chosen)
                        },
                    ),
                    _ if matches!(f.ty, FieldType::Enum(_)) => {
                        set(fields, value, key, enum_value(f, &chosen))
                    }
                    _ => set(fields, value, key, json!(chosen)),
                }
            }
        }
        FormEvent::Extra(text) => {
            let text = text.trim().to_string();
            let text = (!text.is_empty()).then_some(text);
            if dev::PICKED_IN_DIALOG && kind == "window" && key == "app" {
                // The window's name: required, so an empty one is kept.
                match text {
                    Some(text) => one("app", json!(text)),
                    None => return Outcome::Reshow,
                }
            } else if kind == "filter" {
                aspect(text)
            } else {
                one("title", text.map_or(Value::Null, Value::String))
            }
        }
        FormEvent::Text(text) | FormEvent::Color(text) => {
            let Some((_, f)) = find(fields, key) else {
                return Outcome::Nothing;
            };
            if colour_panel {
                live = Some(key.split('.').next().unwrap_or(key).to_string());
            }
            match text_value(f, &text) {
                Some(v) => set(fields, value, key, v),
                // Not a colour: show the one it has again.
                None => return Outcome::Reshow,
            }
        }
        FormEvent::Number(v, is_live) => {
            let Some((_, f)) = find(fields, key) else {
                return Outcome::Nothing;
            };
            if is_live {
                live = Some(key.split('.').next().unwrap_or(key).to_string());
            }
            set(fields, value, key, number_value(f, v))
        }
        FormEvent::Switch(on) => {
            let Some((_, f)) = find(fields, key) else {
                return Outcome::Nothing;
            };
            let v = match f.ty {
                FieldType::Object(_) if on => schema::start_value(f),
                FieldType::Object(_) => Value::Null,
                _ => json!(on),
            };
            set(fields, value, key, v)
        }
        FormEvent::Remove => set(fields, value, key, Value::Null),
        FormEvent::Add => {
            let Some((_, f)) = find(fields, key) else {
                return Outcome::Nothing;
            };
            set(fields, value, key, schema::start_value(f))
        }
        FormEvent::RemoveFile(index) => {
            let mut list = value
                .get(key)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if index < list.len() {
                list.remove(index);
            }
            one(key, Value::Array(list))
        }
        FormEvent::Browse | FormEvent::Chosen(_) => {
            if app.mode.offscreen {
                return Outcome::Nothing;
            }
            let Some((_, f)) = find(fields, key) else {
                return Outcome::Nothing;
            };
            let types = files::file_types(kind, &f.name);
            let multiple = f.ty == FieldType::Strings;
            let prompt = if multiple { "Add" } else { "Choose" };
            let paths = match event {
                FormEvent::Chosen(paths) => paths,
                // Another computer's engine: its disk, through the engine
                // file browser (area J), then this again with the paths.
                // Linux too: the desktop portal's open panel, answered
                // later (or that browser without a portal).
                _ if crate::areas::remote::is_remote(app) || cfg!(target_os = "linux") => {
                    let (form, key) = (FORM.with(|f| f.borrow().clone()), key.to_string());
                    let options = crate::platform::Choose {
                        folders: false,
                        multiple,
                        prompt,
                        types,
                    };
                    crate::areas::remote::choose(app, options, move |app, chosen| {
                        let paths = chosen.unwrap_or_default();
                        if let (false, Some(window)) = (paths.is_empty(), app.window()) {
                            dispatch(app, &window, &form, &key, FormEvent::Chosen(paths));
                        }
                    });
                    return Outcome::Nothing;
                }
                _ => files::choose(types, multiple, prompt),
            };
            if multiple {
                if paths.is_empty() {
                    return Outcome::Nothing;
                }
                let mut list = value
                    .get(key)
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                list.extend(paths.into_iter().map(Value::String));
                one(key, Value::Array(list))
            } else {
                let Some(path) = paths.into_iter().next() else {
                    return Outcome::Nothing;
                };
                set(fields, value, key, json!(path))
            }
        }
        FormEvent::Refresh => return Outcome::Nothing,
    };
    Outcome::Set(changes, live)
}

/// `{key: value}`.
pub fn one(key: &str, value: Value) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert(key.to_string(), value);
    out
}

/// A scaling filter's shape: a named aspect (its own width and height go)
/// or none.
fn aspect(value: Option<String>) -> Map<String, Value> {
    let mut out = Map::new();
    match value {
        Some(v) => {
            out.insert("aspect".into(), json!(v));
            out.insert("width".into(), Value::Null);
            out.insert("height".into(), Value::Null);
        }
        None => {
            out.insert("aspect".into(), Value::Null);
        }
    }
    out
}

/// The merge patch that turns `from` into `to` (the web app's
/// `objectPatch`): settings `to` lacks become `null`, objects patch
/// inside. Cancel sends `object_patch(now, original)`.
pub fn object_patch(from: &Value, to: &Value) -> Map<String, Value> {
    let empty = Map::new();
    let from = from.as_object().unwrap_or(&empty);
    let to = to.as_object().unwrap_or(&empty);
    let mut out = Map::new();
    for key in from.keys() {
        if !to.contains_key(key) {
            out.insert(key.clone(), Value::Null);
        }
    }
    for (key, value) in to {
        match (from.get(key), value) {
            (Some(old @ Value::Object(_)), Value::Object(_)) => {
                let inner = object_patch(old, value);
                if !inner.is_empty() {
                    out.insert(key.clone(), Value::Object(inner));
                }
            }
            (old, _) if old != Some(value) => {
                out.insert(key.clone(), value.clone());
            }
            _ => {}
        }
    }
    out
}

// ---------- Routing ----------

/// Hand an event to the view that shows form `form`: Transition
/// Properties ("transition"), else Properties ("props") or Filters
/// ("filter").
pub fn dispatch(app: &Rc<App>, window: &AppWindow, form: &str, key: &str, event: FormEvent) {
    FORM.with(|f| *f.borrow_mut() = form.to_string());
    match form {
        "transition" => app.controls.form_event(app, key, event),
        _ => app
            .sources
            .dialogs
            .form_event(app, window, form, key, event),
    }
}

thread_local! {
    /// The form the event being dispatched is for (a Browse on another
    /// computer's engine answers later, to the same form).
    static FORM: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// Send every form control's report to [`dispatch`].
pub fn connect(app: &Rc<App>, window: &AppWindow) {
    let forms = window.global::<SettingsForms>();
    macro_rules! route {
        ($on:ident, |$($arg:ident),*| ($form:expr, $key:expr, $event:expr)) => {{
            let weak = Rc::downgrade(app);
            forms.$on(move |$($arg),*| {
                let Some(app) = weak.upgrade() else { return };
                let Some(window) = app.window() else { return };
                dispatch(&app, &window, &$form, &$key, $event);
            });
        }};
    }
    route!(on_text, |form, key, text| (
        form,
        key,
        FormEvent::Text(text.into())
    ));
    route!(on_number, |form, key, v, live| (
        form,
        key,
        FormEvent::Number(v as f64, live)
    ));
    route!(on_switch, |form, key, on| (
        form,
        key,
        FormEvent::Switch(on)
    ));
    route!(on_choose, |form, key, v| (
        form,
        key,
        FormEvent::Choose(v.into())
    ));
    route!(on_extra, |form, key, v| (
        form,
        key,
        FormEvent::Extra(v.into())
    ));
    route!(on_remove, |form, key| (form, key, FormEvent::Remove));
    route!(on_add, |form, key| (form, key, FormEvent::Add));
    route!(on_browse, |form, key| (form, key, FormEvent::Browse));
    route!(on_remove_file, |form, key, index| (
        form,
        key,
        FormEvent::RemoveFile(index.max(0) as usize)
    ));
    route!(on_refresh, |form| (form, "", FormEvent::Refresh));
    let weak = Rc::downgrade(app);
    forms.on_pick_color(move |form, key, now| {
        if let Some(app) = weak.upgrade() {
            app.sources.pick_form_color(&app, &form, &key, &now);
        }
    });
}
