//! The live setup as OBS sees it: scenes, inputs (sources) and scene items
//! with OBS names, uuids and ids derived from Isocast's ids.
//!
//! - A scene's or input's OBS name is its Isocast name, or its id if it has
//!   none; a name two scenes (or two sources) share gets ` [id]` appended
//!   for every one but the first, so names stay unique. Lookups take the
//!   OBS name, the Isocast id or the uuid.
//! - A uuid is the SHA-256 of `isocast:<scene|input>:<id>` laid out as a
//!   version-8 UUID: the same id always gives the same uuid.
//! - A scene item's `sceneItemId` is a positive 31-bit number from the
//!   SHA-256 of its scene's id and its key (its id, else its source's or
//!   nested scene's id, else its position), so it survives reordering.
//! - OBS lists scenes bottom-up (`sceneIndex` 0 is the last in the UI);
//!   scene items are back to front in both.

use isocast_protocol::api::{Live, Status};
use isocast_protocol::{Scene, SceneItem, Setup, Source};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// What a request reads: the live setup and the engine's status.
pub struct World {
    pub revision: u64,
    pub setup: Option<Setup>,
    /// The scene on air.
    pub program: Option<String>,
    /// The scene staged in studio mode (studio mode is on while one is).
    pub preview: Option<String>,
}

impl World {
    pub fn new(live: Live, status: Option<&Status>) -> Self {
        Self {
            revision: live.revision,
            setup: live.setup,
            program: status.and_then(|s| s.scene.clone()),
            preview: status.and_then(|s| s.preview.clone()),
        }
    }

    pub fn scenes(&self) -> &[Scene] {
        self.setup.as_ref().map_or(&[], |s| &s.scenes)
    }

    pub fn sources(&self) -> &[Source] {
        self.setup.as_ref().map_or(&[], |s| &s.sources)
    }

    pub fn scene(&self, id: &str) -> Option<&Scene> {
        self.scenes().iter().find(|s| s.id == id)
    }

    pub fn source(&self, id: &str) -> Option<&Source> {
        self.sources().iter().find(|s| s.id == id)
    }

    pub fn scene_name(&self, id: &str) -> String {
        unique_name(
            self.scenes()
                .iter()
                .map(|s| (s.id.as_str(), s.name.as_deref())),
            id,
        )
    }

    pub fn input_name(&self, id: &str) -> String {
        unique_name(
            self.sources()
                .iter()
                .map(|s| (s.id.as_str(), s.name.as_deref())),
            id,
        )
    }

    /// `{sceneName, sceneUuid}` of a scene, or nulls.
    pub fn scene_ref(&self, id: Option<&str>) -> (Value, Value) {
        match id {
            Some(id) => (json!(self.scene_name(id)), json!(uuid("scene", id))),
            None => (Value::Null, Value::Null),
        }
    }

    /// The scene a name, id or uuid means.
    pub fn find_scene(&self, name: Option<&str>, uuid_: Option<&str>) -> Option<&Scene> {
        let scenes = self.scenes();
        if let Some(wanted) = uuid_ {
            return scenes.iter().find(|s| uuid("scene", &s.id) == wanted);
        }
        let name = name?;
        scenes
            .iter()
            .find(|s| self.scene_name(&s.id) == name)
            .or_else(|| scenes.iter().find(|s| s.id == name))
            .or_else(|| scenes.iter().find(|s| s.name.as_deref() == Some(name)))
    }

    /// The source a name, id or uuid means.
    pub fn find_input(&self, name: Option<&str>, uuid_: Option<&str>) -> Option<&Source> {
        let sources = self.sources();
        if let Some(wanted) = uuid_ {
            return sources.iter().find(|s| uuid("input", &s.id) == wanted);
        }
        let name = name?;
        sources
            .iter()
            .find(|s| self.input_name(&s.id) == name)
            .or_else(|| sources.iter().find(|s| s.id == name))
            .or_else(|| sources.iter().find(|s| s.name.as_deref() == Some(name)))
    }

    /// The OBS source name, uuid, source type and input kind an item
    /// shows.
    pub fn item_source(&self, item: &SceneItem) -> (String, String, &'static str, Value) {
        if let Some(source) = item.source.as_deref() {
            let kind = self.source(source).map(input_kind);
            return (
                self.input_name(source),
                uuid("input", source),
                "OBS_SOURCE_TYPE_INPUT",
                kind.map_or(Value::Null, |k| json!(k)),
            );
        }
        if let Some(scene) = item.scene.as_deref() {
            return (
                self.scene_name(scene),
                uuid("scene", scene),
                "OBS_SOURCE_TYPE_SCENE",
                Value::Null,
            );
        }
        let name = item.id.clone().unwrap_or_else(|| {
            if item.children.is_empty() {
                "content".into()
            } else {
                "group".into()
            }
        });
        let uuid_ = uuid("item", &name);
        if item.children.is_empty() {
            (
                name,
                uuid_,
                "OBS_SOURCE_TYPE_INPUT",
                json!("isocast_content"),
            )
        } else {
            (name, uuid_, "OBS_SOURCE_TYPE_SCENE", Value::Null)
        }
    }

    /// The scene item a client means by `sceneItemId`, with its index.
    pub fn find_item<'a>(&self, scene: &'a Scene, id: i64) -> Option<(usize, &'a SceneItem)> {
        scene
            .items
            .iter()
            .enumerate()
            .find(|(i, item)| item_id(&scene.id, item, *i) == id)
    }

    /// Whether a video source is drawn in the program, and in the program
    /// or studio mode's preview.
    pub fn source_active(&self, source: &str) -> (bool, bool) {
        let shows = |scene: Option<&String>| {
            scene
                .and_then(|id| self.scene(id))
                .is_some_and(|scene| scene.video_sources(self.scenes()).contains(&source))
        };
        let program = shows(self.program.as_ref());
        (program, program || shows(self.preview.as_ref()))
    }

    /// `GetSceneList`'s `scenes`: bottom-up, `sceneIndex` 0 first.
    pub fn scene_list(&self) -> Vec<Value> {
        let count = self.scenes().len();
        self.scenes()
            .iter()
            .enumerate()
            .rev()
            .map(|(i, scene)| {
                json!({
                    "sceneIndex": count - 1 - i,
                    "sceneName": self.scene_name(&scene.id),
                    "sceneUuid": uuid("scene", &scene.id),
                })
            })
            .collect()
    }

    /// One `GetInputList` entry.
    pub fn input_entry(&self, source: &Source) -> Value {
        let kind = input_kind(source);
        json!({
            "inputName": self.input_name(&source.id),
            "inputUuid": uuid("input", &source.id),
            "inputKind": kind,
            "unversionedInputKind": unversioned(kind),
            "inputKindCaps": input_caps(source),
        })
    }

    /// `GetSceneItemList`'s `sceneItems` for `scene`.
    pub fn item_list(&self, scene: &Scene) -> Vec<Value> {
        scene
            .items
            .iter()
            .enumerate()
            .map(|(i, item)| self.item_entry(scene, i, item))
            .collect()
    }

    pub fn item_entry(&self, scene: &Scene, index: usize, item: &SceneItem) -> Value {
        let (name, uuid_, source_type, kind) = self.item_source(item);
        json!({
            "sceneItemId": item_id(&scene.id, item, index),
            "sceneItemIndex": index,
            "sceneItemEnabled": item.visible,
            "sceneItemLocked": item.locked,
            "sceneItemTransform": transform(item),
            "sceneItemBlendMode": "OBS_BLEND_NORMAL",
            "sourceName": name,
            "sourceUuid": uuid_,
            "sourceType": source_type,
            "inputKind": kind,
            "isGroup": if source_type == "OBS_SOURCE_TYPE_SCENE" {
                json!(!item.children.is_empty())
            } else {
                Value::Null
            },
        })
    }
}

/// The OBS name for the item `id` among `(id, name)` pairs.
fn unique_name<'a>(
    all: impl Iterator<Item = (&'a str, Option<&'a str>)> + Clone,
    id: &str,
) -> String {
    let label = |(id, name): (&'a str, Option<&'a str>)| name.unwrap_or(id).to_string();
    let Some(mine) = all.clone().find(|(i, _)| *i == id) else {
        return id.to_string();
    };
    let base = label(mine);
    // The first holder of a name keeps it.
    let first = all
        .clone()
        .find(|pair| label(*pair) == base)
        .map(|(i, _)| i);
    if first == Some(id) {
        base
    } else {
        format!("{base} [{id}]")
    }
}

/// A uuid for `id` of `kind` (`scene`, `input`, `item`).
pub fn uuid(kind: &str, id: &str) -> String {
    let digest = Sha256::digest(format!("isocast:{kind}:{id}"));
    let mut b = [0u8; 16];
    b.copy_from_slice(&digest[..16]);
    // Version 8 (custom), RFC 9562 variant.
    b[6] = (b[6] & 0x0f) | 0x80;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// The `sceneItemId` of the item at `index` of the scene `scene`.
pub fn item_id(scene: &str, item: &SceneItem, index: usize) -> i64 {
    let key = item
        .key()
        .map(str::to_string)
        .unwrap_or_else(|| format!("#{index}"));
    let digest = Sha256::digest(format!("isocast:item:{scene}:{key}"));
    let n = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]) & 0x7fff_ffff;
    i64::from(n.max(1))
}

/// The OBS input kind closest to an Isocast source's kind.
pub fn input_kind(source: &Source) -> &'static str {
    kind_for(source_kind_name(source))
}

pub fn source_kind_name(source: &Source) -> &'static str {
    source.kind.label()
}

/// OBS's kind for an Isocast kind name; `isocast_<kind>` where OBS has
/// none.
pub fn kind_for(kind: &str) -> &'static str {
    match kind {
        "display" | "window" | "application" => "screen_capture",
        "application_audio" | "system_audio" => "sck_audio_capture",
        "camera" => "macos-avcapture",
        "microphone" => "coreaudio_input_capture",
        "color" => "color_source_v3",
        "image" => "image_source",
        "slideshow" => "slideshow_v2",
        "text" => "text_ft2_source_v2",
        "media" => "ffmpeg_source",
        "browser" => "browser_source",
        "test_pattern" => "isocast_test_pattern",
        "tone" => "isocast_tone",
        _ => "isocast_plugin",
    }
}

/// Every input kind [`kind_for`] gives, for `GetInputKindList`.
pub const INPUT_KINDS: &[&str] = &[
    "screen_capture",
    "sck_audio_capture",
    "macos-avcapture",
    "coreaudio_input_capture",
    "color_source_v3",
    "image_source",
    "slideshow_v2",
    "text_ft2_source_v2",
    "ffmpeg_source",
    "browser_source",
    "isocast_test_pattern",
    "isocast_tone",
    "isocast_plugin",
];

pub fn unversioned(kind: &str) -> &str {
    kind.trim_end_matches(char::is_numeric)
        .strip_suffix("_v")
        .unwrap_or(kind)
}

/// Whether a source has sound (it shows in OBS's mixer).
pub fn has_audio(source: &Source) -> bool {
    source.kind.has_audio()
}

/// OBS's `inputKindCaps` (output flags): video, audio.
fn input_caps(source: &Source) -> u32 {
    const VIDEO: u32 = 1;
    const AUDIO: u32 = 1 << 1;
    (if source.kind.has_video() { VIDEO } else { 0 })
        | if source.kind.has_audio() { AUDIO } else { 0 }
}

/// A `sceneItemTransform` from an item's rect (zeros where it has none).
pub fn transform(item: &SceneItem) -> Value {
    let (x, y, w, h) = item.rect.as_ref().map_or((0.0, 0.0, 0.0, 0.0), |r| {
        (
            f64::from(r.x),
            f64::from(r.y),
            f64::from(r.width),
            f64::from(r.height),
        )
    });
    json!({
        "positionX": x, "positionY": y, "rotation": 0.0,
        "scaleX": 1.0, "scaleY": 1.0,
        "width": w, "height": h, "sourceWidth": w, "sourceHeight": h,
        "alignment": 5, "boundsType": "OBS_BOUNDS_STRETCH", "boundsAlignment": 0,
        "boundsWidth": w, "boundsHeight": h,
        "cropLeft": 0, "cropRight": 0, "cropTop": 0, "cropBottom": 0, "cropToBounds": false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_stay_unique_and_uuids_stable() {
        let all = [("a", Some("Cam")), ("b", Some("Cam")), ("c", None)];
        assert_eq!(unique_name(all.iter().copied(), "a"), "Cam");
        assert_eq!(unique_name(all.iter().copied(), "b"), "Cam [b]");
        assert_eq!(unique_name(all.iter().copied(), "c"), "c");
        let u = uuid("scene", "intro");
        assert_eq!(u, uuid("scene", "intro"));
        assert_ne!(u, uuid("input", "intro"));
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "8");
        assert_eq!(unversioned("color_source_v3"), "color_source");
        assert_eq!(unversioned("image_source"), "image_source");
    }
}
