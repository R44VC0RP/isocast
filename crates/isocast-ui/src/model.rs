//! Reading the setup and writing patches for it, shared by the areas
//! (the web app's lib/model.js and lib/patch.js): source kinds in OBS's
//! words, names, the items along a path, and merge patches that change
//! one item.

use isocast_protocol::{CanvasId, Rect, Scene, SceneItem, Setup, Source};
use serde_json::{Value, json};

/// Recovery differs for a contained canvas failure and poisoned scene state.
pub fn vertical_recovery(reason: &str) -> String {
    canvas_recovery(reason, "the vertical canvas")
}

/// How to bring back a canvas (`what`: "the vertical canvas", "Shorts")
/// whose program stopped for `reason`.
pub fn canvas_recovery(reason: &str, what: &str) -> String {
    if reason.contains("scene state is broken") || reason.contains("scene worker has broken state")
    {
        "Restart the engine once recording and streaming stop.".into()
    } else {
        format!(
            "After recording stops, turn {what} off and on in Settings › Video, or restart the engine."
        )
    }
}

/// A source kind as the interface shows it: OBS's name for it (what new
/// sources are called, and the Add menu's words) and its icon's name (in
/// `Icons`; Slint resolves it with `Icons.named`).
pub struct KindInfo {
    pub kind: &'static str,
    pub label: &'static str,
    pub icon: &'static str,
}

/// Every source kind, in the Add menu's order.
pub const KINDS: &[KindInfo] = &[
    kind("display", "Display Capture", "display"),
    kind("window", "Window Capture", "window"),
    kind("application", "Application Capture", "application"),
    kind("camera", "Video Capture Device", "camera"),
    kind("microphone", "Audio Input Capture", "mic"),
    kind("system_audio", "Audio Output Capture", "speaker"),
    kind(
        "application_audio",
        "Application Audio Capture",
        "app-audio",
    ),
    kind("image", "Image", "image"),
    kind("slideshow", "Image Slide Show", "slideshow"),
    kind("media", "Media Source", "media"),
    kind("text", "Text", "text"),
    kind("browser", "Browser", "browser"),
    kind("color", "Color Source", "color"),
    kind("test_pattern", "Test Pattern", "test-pattern"),
    kind("tone", "Tone", "tone"),
];

const fn kind(kind: &'static str, label: &'static str, icon: &'static str) -> KindInfo {
    KindInfo { kind, label, icon }
}

/// What the interface calls a kind (`"camera"` → Video Capture Device).
pub fn kind_info(kind: &str) -> KindInfo {
    KINDS
        .iter()
        .find(|k| k.kind == kind)
        .map(|k| KindInfo { ..*k })
        .unwrap_or(KindInfo {
            kind: "",
            label: "Source",
            icon: "box",
        })
}

pub fn source_name(source: &Source) -> &str {
    source.name.as_deref().unwrap_or(&source.id)
}

pub fn scene_name(scene: &Scene) -> &str {
    scene.name.as_deref().unwrap_or(&scene.id)
}

pub fn find_source<'a>(setup: &'a Setup, id: &str) -> Option<&'a Source> {
    setup.sources.iter().find(|s| s.id == id)
}

pub fn find_scene<'a>(setup: &'a Setup, id: &str) -> Option<&'a Scene> {
    setup.scenes.iter().find(|s| s.id == id)
}

/// What patches call an item: its id, else its source or scene.
pub fn key(item: &SceneItem) -> &str {
    item.key().unwrap_or("")
}

/// An item's rect; none (an item placed by its `layout`, which the editor
/// doesn't move yet) is an empty one at the top left, as in the web app.
pub fn rect_of(item: &SceneItem) -> Rect {
    item.rect.unwrap_or(Rect {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
    })
}

/// An item path ("group/clip") as the keys from the top level down.
pub fn path_keys(path: &str) -> Vec<&str> {
    path.split('/').filter(|k| !k.is_empty()).collect()
}

/// The items from the scene's top level down to the one at `path`.
pub fn item_chain<'a>(scene: &'a Scene, path: &str) -> Option<Vec<&'a SceneItem>> {
    let mut chain = Vec::new();
    let mut items = &scene.items;
    for k in path_keys(path) {
        let item = items.iter().find(|i| key(i) == k)?;
        chain.push(item);
        items = &item.children;
    }
    (!chain.is_empty()).then_some(chain)
}

/// How an item is labelled: its source's or scene's name, else a box or
/// group.
pub fn item_label(setup: &Setup, item: &SceneItem) -> String {
    if let Some(id) = &item.source {
        return find_source(setup, id).map_or(id.clone(), |s| source_name(s).to_string());
    }
    if let Some(id) = &item.scene {
        return find_scene(setup, id).map_or(id.clone(), |s| scene_name(s).to_string());
    }
    let fallback = if item.children.is_empty() && item.content.is_some() {
        "Box"
    } else {
        "Group"
    };
    // A group's id as a name: `group-2` → "Group 2".
    item.id
        .as_deref()
        .map(|id| {
            let words = id.replace('-', " ");
            let mut chars = words.chars();
            chars
                .next()
                .map(|c| c.to_uppercase().chain(chars).collect())
                .unwrap_or_default()
        })
        .unwrap_or_else(|| fallback.into())
}

pub fn item_icon(setup: &Setup, item: &SceneItem) -> &'static str {
    if let Some(id) = &item.source {
        return find_source(setup, id).map_or("box", |s| kind_info(s.kind.label()).icon);
    }
    if item.scene.is_some() {
        return "scene";
    }
    // Nothing of its own to show: a group (Add → Group starts empty).
    if item.children.is_empty() && item.content.is_some() {
        "box"
    } else {
        "group"
    }
}

/// The fields that identify an item in a patch.
pub fn item_identity(item: &SceneItem) -> Value {
    if let Some(id) = &item.id {
        json!({ "id": id })
    } else if let Some(source) = &item.source {
        json!({ "source": source })
    } else {
        json!({ "scene": item.scene })
    }
}

/// A patch changing `fields` (an object) of the item at the end of
/// `chain` (the items from the top level down) in scene `scene`.
pub fn item_patch(scene: &str, chain: &[&SceneItem], fields: Value) -> Value {
    let Some((last, parents)) = chain.split_last() else {
        return json!({});
    };
    let mut node = item_identity(last);
    if let (Some(node), Value::Object(fields)) = (node.as_object_mut(), fields) {
        node.extend(fields);
    }
    for parent in parents.iter().rev() {
        let mut outer = item_identity(parent);
        outer["children"] = json!([node]);
        node = outer;
    }
    json!({ "scenes": [{ "id": scene, "items": [node] }] })
}

/// Extra canvas `canvas`'s settings in setup JSON `shown`: `vertical`, or
/// its `canvases` entry (#84); null if it hasn't it (or it is main).
pub fn canvas_json(shown: &Value, canvas: CanvasId) -> &Value {
    match canvas {
        CanvasId::Main => &Value::Null,
        CanvasId::Vertical => &shown["vertical"],
        other => shown["canvases"]
            .as_array()
            .and_then(|list| list.iter().find(|c| c["id"] == other.as_str()))
            .unwrap_or(&Value::Null),
    }
}

/// Scene JSON `scene`'s own arrangement for extra canvas `canvas`
/// (`vertical`, or `canvases.<id>`); null without one.
pub fn arrangement_json(scene: &Value, canvas: CanvasId) -> &Value {
    match canvas {
        CanvasId::Main => &Value::Null,
        CanvasId::Vertical => &scene["vertical"],
        other => &scene["canvases"][other.as_str()],
    }
}

/// A merge patch making `change` to extra canvas `canvas`'s settings:
/// under `vertical`, or its `canvases` entry (the daemon merges that list
/// by id).
pub fn canvas_patch(canvas: CanvasId, change: Value) -> Value {
    if canvas.is_vertical() {
        return json!({ "vertical": change });
    }
    let mut entry = change;
    if let Some(object) = entry.as_object_mut() {
        object.insert("id".into(), canvas.as_str().into());
    }
    json!({ "canvases": [entry] })
}

/// A scene patch entry setting (or with null, removing) its arrangement
/// for extra canvas `canvas`.
fn arrangement_patch(entry: &mut Value, canvas: CanvasId, arrangement: Value) {
    if canvas.is_vertical() {
        entry["vertical"] = arrangement;
    } else {
        entry["canvases"][canvas.as_str()] = arrangement;
    }
}

/// The patch dropping scene `scene`'s own arrangement for extra canvas
/// `canvas`, so it shows the main layout there again.
pub fn drop_arrangement(scene: &str, canvas: CanvasId) -> Value {
    let mut entry = json!({ "id": scene });
    arrangement_patch(&mut entry, canvas, Value::Null);
    json!({ "scenes": [entry] })
}

/// An extra canvas's name as the interface shows it: "Vertical" for the
/// `[vertical]` canvas, else its `name` (its id without one).
pub fn canvas_label(setup: &Setup, canvas: CanvasId) -> String {
    match canvas {
        CanvasId::Main => "Main".into(),
        CanvasId::Vertical => "Vertical".into(),
        other => setup
            .extra_canvas(other)
            .and_then(|c| c.name.clone())
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| other.as_str().to_string()),
    }
}

/// The setup `shown` (JSON) as extra canvas `canvas` edits it (#86, #120):
/// each scene's own arrangement for it in place of the main items and
/// layout (a scene without one shows the main ones fitted and centred, as
/// `Setup::for_canvas` does), and the canvas's size and rate as `canvas`.
pub fn canvas_view(shown: &Value, canvas: CanvasId) -> Value {
    let mut out = shown.clone();
    let settings = canvas_json(shown, canvas);
    out["canvas"]["width"] = settings.get("width").cloned().unwrap_or(json!(1080));
    out["canvas"]["height"] = settings.get("height").cloned().unwrap_or(json!(1920));
    if let Some(fps) = settings.get("fps").filter(|f| !f.is_null()) {
        out["canvas"]["fps"] = fps.clone();
    }
    if let Some(object) = out.as_object_mut() {
        object.remove("vertical");
        object.remove("canvases");
    }
    if let Some(scenes) = out["scenes"].as_array_mut() {
        for scene in scenes {
            let own = arrangement_json(scene, canvas).clone();
            let Some(object) = scene.as_object_mut() else {
                continue;
            };
            object.remove("vertical");
            object.remove("canvases");
            if own.is_object() {
                object.insert(
                    "items".into(),
                    own.get("items").cloned().unwrap_or(json!([])),
                );
                match own.get("layout").filter(|l| !l.is_null()) {
                    Some(layout) => object.insert("layout".into(), layout.clone()),
                    None => object.remove("layout"),
                };
            } else if let Some(items) = object.get("items") {
                let fitted = fitted_items(shown, canvas, items);
                object.insert("items".into(), fitted);
            }
        }
    }
    out
}

/// The items of scene `scene` on canvas `canvas` of `shown`: the main
/// ones, or an extra canvas's own (the main ones fitted where it has none).
pub fn canvas_items(shown: &Value, scene: &str, canvas: CanvasId) -> Vec<Value> {
    let Some(scene) = shown["scenes"]
        .as_array()
        .and_then(|scenes| scenes.iter().find(|s| s["id"] == scene))
    else {
        return Vec::new();
    };
    let own = arrangement_json(scene, canvas);
    let items = if canvas.is_main() {
        scene["items"].clone()
    } else if own.is_object() {
        own["items"].clone()
    } else {
        fitted_items(shown, canvas, &scene["items"])
    };
    items.as_array().cloned().unwrap_or_default()
}

/// A scene's main `items` (JSON) as extra canvas `canvas` of `shown` shows
/// them while the scene has no arrangement of its own for it: fitted
/// inside it and centred, as `Setup::for_canvas` does (#109). Unchanged
/// if they don't parse (the daemon says why).
fn fitted_items(shown: &Value, canvas: CanvasId, items: &Value) -> Value {
    let size = |value: &Value, key: &str, default: u64| {
        value.get(key).and_then(Value::as_u64).unwrap_or(default) as u32
    };
    let settings = canvas_json(shown, canvas);
    let from = (
        size(&shown["canvas"], "width", 1920),
        size(&shown["canvas"], "height", 1080),
    );
    let to = (
        size(settings, "width", 1080),
        size(settings, "height", 1920),
    );
    let Ok(mut typed) = serde_json::from_value::<Vec<isocast_protocol::SceneItem>>(items.clone())
    else {
        return items.clone();
    };
    let letterboxed: Vec<String> = shown["scenes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|scene| serde_json::from_value::<isocast_protocol::Scene>(scene.clone()).ok())
        .filter(|scene| scene.letterboxed_on(canvas))
        .map(|scene| scene.id)
        .collect();
    isocast_protocol::fit_items(&mut typed, from, to, &|id| {
        letterboxed.iter().any(|scene| scene == id)
    });
    serde_json::to_value(typed).unwrap_or_else(|_| items.clone())
}

/// `patch` sent while editing extra canvas `canvas` (#86, #120): what it
/// changes in existing scenes' `items` goes to their arrangement for that
/// canvas instead, so no edit of its layout can change the main one. A
/// scene without an arrangement of its own yet gets one made from the
/// main items as the canvas shows them (fitted and centred, #109) with
/// the change in. Everything else (scene names, transitions, new scenes,
/// sources) is left as it is.
pub fn route_canvas(shown: &Value, canvas: CanvasId, patch: Value) -> Value {
    if canvas.is_main() {
        return patch;
    }
    let Some(entries) = patch["scenes"].as_array() else {
        return patch;
    };
    let routed: Vec<Value> = entries
        .iter()
        .map(|entry| {
            let (Some(id), Some(items)) = (entry["id"].as_str(), entry.get("items")) else {
                return entry.clone();
            };
            let existing = shown["scenes"]
                .as_array()
                .and_then(|scenes| scenes.iter().find(|s| s["id"] == id));
            let Some(scene) = existing.filter(|_| entry["remove"] != json!(true)) else {
                return entry.clone();
            };
            let mut entry = entry.clone();
            if let Some(object) = entry.as_object_mut() {
                object.remove("items");
            }
            let arrangement = if arrangement_json(scene, canvas).is_object() {
                json!({ "items": items })
            } else {
                let base = fitted_items(shown, canvas, scene.get("items").unwrap_or(&json!([])));
                let made = isocast_protocol::change::merge_patch(
                    &json!({ "items": base }),
                    &json!({ "items": items }),
                );
                let mut arrangement = json!({ "items": made["items"] });
                if !scene["layout"].is_null() {
                    arrangement["layout"] = scene["layout"].clone();
                }
                arrangement
            };
            let arrangement = match arrangement_json(&entry, canvas) {
                other if other.is_object() => {
                    isocast_protocol::change::merge_patch(other, &arrangement)
                }
                _ => arrangement,
            };
            arrangement_patch(&mut entry, canvas, arrangement);
            entry
        })
        .collect();
    let mut out = patch;
    out["scenes"] = Value::Array(routed);
    out
}

/// An id made from `name`: lowercase letters, digits and dashes, not one
/// of `taken` (or a reserved id: "program", "vertical-program", "edited",
/// "vertical").
pub fn new_id<'a>(name: &str, taken: impl IntoIterator<Item = &'a str>) -> String {
    let mut base = String::new();
    for c in name.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            base.push(c);
        } else if !base.ends_with('-') {
            base.push('-');
        }
    }
    let base: String = base.trim_matches('-').chars().take(40).collect();
    let base = if base.is_empty() {
        "source".to_string()
    } else {
        base
    };
    let used: std::collections::HashSet<&str> = taken
        .into_iter()
        .chain(isocast_protocol::RESERVED_SOURCE_IDS)
        .collect();
    if !used.contains(base.as_str()) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|id| !used.contains(id.as_str()))
        .expect("an unused number")
}

/// Every id a new source or scene could clash with.
pub fn taken_ids(setup: &Setup) -> Vec<&str> {
    setup
        .sources
        .iter()
        .map(|s| s.id.as_str())
        .chain(setup.scenes.iter().map(|s| s.id.as_str()))
        .collect()
}

/// A name like "Scene 3" that none of `names` is yet.
pub fn next_name<'a>(base: &str, names: impl IntoIterator<Item = &'a str>) -> String {
    let used: std::collections::HashSet<&str> = names.into_iter().collect();
    (1..)
        .map(|n| format!("{base} {n}"))
        .find(|name| !used.contains(name.as_str()))
        .expect("an unused number")
}

/// A level in dB as the interface writes it: a true minus sign, one
/// decimal, "−∞ dB" for silence.
pub fn db_text(db: f64) -> String {
    if db <= -100.0 {
        return "\u{2212}\u{221e} dB".into();
    }
    let text = format!("{}{db:.1} dB", if db > 0.0 { "+" } else { "" });
    text.replacen('-', "\u{2212}", 1)
}

/// What the engine says a running source is, as the control bar shows it
/// after its name: "1920×1080 · 30p · Stereo" (the parts it has).
pub fn source_format(status: &isocast_protocol::api::SourceStatus) -> String {
    let mut parts = Vec::new();
    if let (Some(w), Some(h)) = (status.width, status.height) {
        parts.push(format!("{w}×{h}"));
    }
    if let Some(fps) = status.fps.filter(|f| *f > 0) {
        parts.push(format!("{fps}p"));
    }
    match status.channels {
        Some(1) => parts.push("Mono".into()),
        Some(2) => parts.push("Stereo".into()),
        Some(n) if n > 2 => parts.push(format!("{n} channels")),
        _ => {}
    }
    parts.join(" · ")
}

/// "212 GB free", "3.2 GB free" (decimal gigabytes, as Finder counts): the
/// status bar's readout and Settings' disk badge.
pub fn gigabytes_free(bytes: u64) -> String {
    let gb = bytes as f64 / 1e9;
    if gb >= 10.0 {
        format!("{gb:.0} GB free")
    } else {
        format!("{gb:.1} GB free")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_come_from_names() {
        assert_eq!(new_id("Intro Clip", ["intro-clip"]), "intro-clip-2");
        assert_eq!(new_id("  Program ", []), "program-2");
        assert_eq!(next_name("Scene", ["Scene 1", "Scene 2"]), "Scene 3");
        assert_eq!(db_text(-12.0), "\u{2212}12.0 dB");
        assert_eq!(db_text(3.0), "+3.0 dB");
        let clip: isocast_protocol::api::SourceStatus = serde_json::from_value(json!({
            "source": "clip", "state": "running", "width": 1920, "height": 1080,
            "fps": 30, "channels": 2,
        }))
        .unwrap();
        assert_eq!(source_format(&clip), "1920×1080 · 30p · Stereo");
    }
}
