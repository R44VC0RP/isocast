//! Reading the setup and writing patches for it, shared by the areas
//! (the web app's lib/model.js and lib/patch.js): source kinds in OBS's
//! words, names, the items along a path, and merge patches that change
//! one item.

use isocast_protocol::{Rect, Scene, SceneItem, Setup, Source};
use serde_json::{Value, json};

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
    kind("color", "Colour Source", "color"),
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
    let fallback = if item.children.is_empty() {
        "Box"
    } else {
        "Group"
    };
    item.id.clone().unwrap_or_else(|| fallback.into())
}

pub fn item_icon(setup: &Setup, item: &SceneItem) -> &'static str {
    if let Some(id) = &item.source {
        return find_source(setup, id).map_or("box", |s| kind_info(s.kind.label()).icon);
    }
    if item.scene.is_some() {
        return "scene";
    }
    if item.children.is_empty() {
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

/// An id made from `name`: lowercase letters, digits and dashes, not one
/// of `taken` (or "program", which is reserved).
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
        .chain(std::iter::once("program"))
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
    }
}
