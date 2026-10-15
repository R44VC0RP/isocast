//! Profile hotkeys. Stable action ids, strict argument schemas and portable
//! key names shared by the daemon, CLI and settings clients.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::timeline::{RetakeParams, TimelineNewParams, TimelineOpenParams};
use crate::{Issue, Transition};

pub type Bindings = BTreeMap<String, Binding>;

/// Lossless, non-critical configuration on a setup/profile. Unknown or invalid
/// entries survive interchange but never execute. Only hotkeys_set uses the
/// strict Binding type directly. None distinguishes omission from explicit {}.
#[derive(Debug, Clone, Default, PartialEq, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct Config {
    pub value: Option<Value>,
    #[serde(skip)]
    pub load_warning: Option<String>,
}

impl<'de> Deserialize<'de> for Config {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self {
            value: Some(Value::deserialize(deserializer)?),
            load_warning: None,
        })
    }
}

impl From<Bindings> for Config {
    fn from(bindings: Bindings) -> Self {
        Self {
            value: Some(serde_json::to_value(bindings).unwrap()),
            load_warning: None,
        }
    }
}

impl Config {
    pub fn is_absent(&self) -> bool {
        self.value.is_none()
    }

    pub fn usable(&self) -> (Bindings, Vec<Issue>) {
        if let Some(message) = &self.load_warning {
            return (
                Bindings::new(),
                vec![Issue {
                    path: "hotkeys".into(),
                    message: bounded(message, 1024),
                }],
            );
        }
        let Some(value) = &self.value else {
            return Default::default();
        };
        let Some(object) = value.as_object() else {
            return (
                Bindings::new(),
                vec![Issue {
                    path: "hotkeys".into(),
                    message: "expected an object keyed by binding id; ignored".into(),
                }],
            );
        };
        if object.len() > 256 {
            return (Bindings::new(), limit_issue());
        }
        let mut parsed = Bindings::new();
        let mut problems = Vec::new();
        for (id, value) in object {
            match serde_path_to_error::deserialize::<_, Binding>(value) {
                Ok(binding) if value.is_object() => {
                    let single = [(id.clone(), binding.clone())].into();
                    let found = issues(&single);
                    if found.is_empty() {
                        parsed.insert(id.clone(), binding);
                    } else {
                        problems.extend(found);
                    }
                }
                result => {
                    let message = result
                        .err()
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "expected a binding object".into());
                    problems.push(Issue {
                        path: format!("hotkeys.{}", bounded(id, 64)),
                        message: bounded(&message, 1024),
                    });
                }
            }
        }
        // Disable every member of a conflict, never pick an arbitrary winner.
        let mut counts = BTreeMap::new();
        for binding in parsed.values() {
            *counts
                .entry(Chord::parse(&binding.keys).unwrap().canonical())
                .or_insert(0) += 1;
        }
        problems.extend(issues(&parsed));
        parsed.retain(|_, binding| counts[&Chord::parse(&binding.keys).unwrap().canonical()] == 1);
        (parsed, problems)
    }
}

/// Bound untrusted diagnostic values at a UTF-8 boundary.
pub fn bounded(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.into();
    }
    let mut end = max.saturating_sub(3);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

pub fn describe(issues: &[Issue]) -> String {
    let mut text = issues
        .iter()
        .take(16)
        .map(|i| format!("{}: {}", bounded(&i.path, 160), bounded(&i.message, 1024)))
        .collect::<Vec<_>>()
        .join("\n");
    if issues.len() > 16 {
        text.push_str(&format!(
            "\n{} more issue(s); fix these and retry",
            issues.len() - 16
        ));
    }
    bounded(&text, 8192)
}

fn limit_issue() -> Vec<Issue> {
    vec![Issue {
        path: "hotkeys".into(),
        message: "at most 256 bindings per profile; no bindings were accepted".into(),
    }]
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    /// Chord such as cmd+shift+r, f13 or ctrl+numpad_0 (docs/hotkeys.md).
    pub keys: String,
    /// Stable id from hotkeys_actions.
    pub action: String,
    #[serde(default = "empty_args")]
    pub args: Value,
}

fn empty_args() -> Value {
    json!({})
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetParams {
    /// Replace all bindings, including an empty map to disable them all.
    pub bindings: Option<Bindings>,
    /// Or replace bindings by id; null removes one. Exactly one of bindings/patch.
    pub patch: Option<BTreeMap<String, Option<Binding>>>,
    /// Optional optimistic concurrency check, from hotkeys_get.
    pub revision: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Press,
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InputSource {
    Global,
    Focused,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InputParams {
    pub keys: String,
    pub phase: Phase,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Registration {
    pub binding: String,
    pub global: bool,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Hotkeys {
    pub profile: Option<String>,
    pub revision: u64,
    pub bindings: Bindings,
    /// Lossless stored configuration, including disabled/unknown entries.
    #[serde(default)]
    pub configured: Config,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub pending: bool,
    pub registrations: Vec<Registration>,
    /// Temporary mute overrides while a push key is held; never saved.
    pub held_sources: BTreeMap<String, bool>,
    pub studio_mode: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Action {
    pub id: String,
    pub label: String,
    pub group: String,
    pub arguments: Value,
    pub hold: bool,
    /// Reserved actions are accepted in profiles but cannot execute yet.
    pub available: bool,
    pub unavailable_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Catalogue {
    pub actions: Vec<Action>,
    /// Suggested, opt-in defaults: never silently added to an old profile.
    pub defaults: Bindings,
    pub keys: Vec<String>,
    pub modifiers: Vec<String>,
}

macro_rules! arguments {
    ($name:ident { $($field:ident: $ty:ty),* $(,)? }) => {
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub struct $name { $(pub $field: $ty),* }
    };
}
arguments!(Empty {});
arguments!(SceneArgs { scene: String });
arguments!(IndexArgs { index: u32 });
arguments!(SourceArgs { source: String });
arguments!(ItemArgs {
    scene: String,
    item: String
});
arguments!(MarkerArgs { label: String });
arguments!(TransitionArgs {
    transition: Transition
});
arguments!(IdArgs { id: String });
/// The old reserved actions used `id`; retain that spelling as an alias while
/// exposing the real timeline method's `clip`/`timeline` fields in the schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineRemoveArgs {
    #[serde(alias = "id")]
    pub clip: String,
    pub timeline: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineMoveArgs {
    #[serde(alias = "id")]
    pub clip: String,
    /// Zero-based position, as on timeline_move (the CLI's positions are one-based).
    pub index: usize,
    pub timeline: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineExportArgs {
    #[serde(alias = "id")]
    pub timeline: Option<String>,
    pub folder: Option<std::path::PathBuf>,
}
arguments!(ScreenshotArgs {
    path: std::path::PathBuf
});

/// Decoded once at validation/firing. The action id selects the argument type.
#[derive(Debug)]
pub enum Arguments {
    Empty,
    Scene(SceneArgs),
    Index(IndexArgs),
    Source(SourceArgs),
    Item(ItemArgs),
    Marker(MarkerArgs),
    Transition(TransitionArgs),
    Id(IdArgs),
    Retake(RetakeParams),
    TimelineNew(TimelineNewParams),
    TimelineOpen(TimelineOpenParams),
    TimelineRemove(TimelineRemoveArgs),
    TimelineMove(TimelineMoveArgs),
    TimelineExport(TimelineExportArgs),
    Screenshot(ScreenshotArgs),
}

fn parse<T: DeserializeOwned>(value: &Value) -> Result<T, String> {
    if !value.is_object() {
        return Err("args: must be an object".into());
    }
    crate::located::from_value(value).map_err(|e| bounded(&e.within("args").to_string(), 2048))
}

impl Binding {
    pub fn arguments(&self) -> Result<Arguments, String> {
        match self.action.as_str() {
            "recording.start"
            | "recording.stop"
            | "recording.toggle"
            | "recording.pause"
            | "recording.resume"
            | "recording.pause_toggle"
            | "recording.split"
            | "streaming.start"
            | "streaming.stop"
            | "streaming.toggle"
            | "replay.start"
            | "replay.stop"
            | "replay.toggle"
            | "replay.save"
            | "studio.toggle"
            | "transition.take"
            | "timeline.close"
            | "timeline.clear" => parse::<Empty>(&self.args).map(|_| Arguments::Empty),
            "recording.marker" | "timeline.marker" => parse(&self.args).map(Arguments::Marker),
            "timeline.retake" => parse(&self.args).map(Arguments::Retake),
            "timeline.new" => parse(&self.args).map(Arguments::TimelineNew),
            "timeline.open" => parse(&self.args).map(Arguments::TimelineOpen),
            "timeline.remove" => parse(&self.args).map(Arguments::TimelineRemove),
            "timeline.move" => parse(&self.args).map(Arguments::TimelineMove),
            "timeline.export" => parse(&self.args).map(Arguments::TimelineExport),
            "scene.switch" | "scene.preview" => parse(&self.args).map(Arguments::Scene),
            "scene.switch_index" | "scene.preview_index" => {
                let args: IndexArgs = parse(&self.args)?;
                if args.index == 0 {
                    return Err("args.index: use a one-based scene index (1 or greater)".into());
                }
                Ok(Arguments::Index(args))
            }
            "item.show" | "item.hide" | "item.toggle" => parse(&self.args).map(Arguments::Item),
            "audio.mute" | "audio.unmute" | "audio.toggle" | "audio.push_to_talk"
            | "audio.push_to_mute" => parse(&self.args).map(Arguments::Source),
            "transition.set" | "transition.take_with" => {
                let args: TransitionArgs = parse(&self.args)?;
                let issues = args.transition.issues("args.transition");
                if !issues.is_empty() {
                    return Err(issues
                        .iter()
                        .map(|i| format!("{}: {}", i.path, i.message))
                        .collect::<Vec<_>>()
                        .join("; "));
                }
                Ok(Arguments::Transition(args))
            }
            "program.screenshot" => {
                let args: ScreenshotArgs = parse(&self.args)?;
                if !args.path.is_absolute() {
                    return Err("args.path: give an absolute PNG file path".into());
                }
                Ok(Arguments::Screenshot(args))
            }
            "profile.switch" | "collection.switch" => parse(&self.args).map(Arguments::Id),
            _ => Err(format!(
                "action: unknown {:?}; valid actions: {}",
                self.action,
                catalogue()
                    .actions
                    .iter()
                    .map(|a| a.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    pub fn hold(&self) -> bool {
        matches!(
            self.action.as_str(),
            "audio.push_to_talk" | "audio.push_to_mute"
        )
    }
}

pub fn catalogue() -> Catalogue {
    fn action<T: JsonSchema>(id: &str, label: &str, group: &str) -> Action {
        let unavailable = if id == "timeline.clear" {
            Some("no timeline_clear method yet; remove individual clips or create a new timeline")
        } else {
            None
        };
        Action {
            id: id.into(),
            label: label.into(),
            group: group.into(),
            arguments: serde_json::to_value(schemars::schema_for!(T)).unwrap(),
            hold: matches!(id, "audio.push_to_talk" | "audio.push_to_mute"),
            available: unavailable.is_none(),
            unavailable_reason: unavailable.map(str::to_string),
        }
    }
    let mut actions = Vec::new();
    for (prefix, group) in [("recording", "Recording"), ("streaming", "Streaming")] {
        for (suffix, label) in [("start", "Start"), ("stop", "Stop"), ("toggle", "Toggle")] {
            actions.push(action::<Empty>(
                &format!("{prefix}.{suffix}"),
                &format!("{label} {prefix}"),
                group,
            ));
        }
    }
    for (id, label) in [
        ("start", "Start replay buffer"),
        ("stop", "Stop replay buffer"),
        ("toggle", "Toggle replay buffer"),
        ("save", "Save replay"),
    ] {
        actions.push(action::<Empty>(
            &format!("replay.{id}"),
            label,
            "Replay Buffer",
        ));
    }
    for (id, label) in [
        ("pause", "Pause recording"),
        ("resume", "Resume recording"),
        ("pause_toggle", "Toggle pause"),
        ("split", "Split take"),
    ] {
        actions.push(action::<Empty>(
            &format!("recording.{id}"),
            label,
            "Recording",
        ));
    }
    actions.extend([
        action::<MarkerArgs>("recording.marker", "Add marker", "Recording"),
        action::<Empty>("studio.toggle", "Toggle Studio Mode", "Scenes"),
        action::<Empty>("transition.take", "Take preview to program", "Transitions"),
        action::<SceneArgs>(
            "scene.switch",
            "Select scene (preview in Studio Mode)",
            "Scenes",
        ),
        action::<SceneArgs>("scene.preview", "Stage scene in preview", "Scenes"),
        action::<IndexArgs>("scene.switch_index", "Select scene by position", "Scenes"),
        action::<IndexArgs>("scene.preview_index", "Preview scene by position", "Scenes"),
        action::<TransitionArgs>("transition.set", "Set default transition", "Transitions"),
        action::<TransitionArgs>(
            "transition.take_with",
            "Take with transition",
            "Transitions",
        ),
        action::<ScreenshotArgs>("program.screenshot", "Save program screenshot", "Program"),
        action::<IdArgs>("profile.switch", "Switch profile", "Library"),
        action::<IdArgs>("collection.switch", "Switch scene collection", "Library"),
        action::<RetakeParams>("timeline.retake", "Retake", "Timeline"),
        action::<TimelineRemoveArgs>("timeline.remove", "Remove clip (keep files)", "Timeline"),
        action::<TimelineMoveArgs>("timeline.move", "Reorder clip", "Timeline"),
        action::<TimelineExportArgs>("timeline.export", "Export timeline", "Timeline"),
        action::<TimelineNewParams>("timeline.new", "Create and select timeline", "Timeline"),
        action::<TimelineOpenParams>("timeline.open", "Select timeline", "Timeline"),
        action::<Empty>("timeline.close", "Close current timeline", "Timeline"),
        action::<MarkerArgs>("timeline.marker", "Mark current recording take", "Timeline"),
        action::<Empty>("timeline.clear", "Clear timeline", "Timeline"),
    ]);
    for (id, label) in [
        ("show", "Show item"),
        ("hide", "Hide item"),
        ("toggle", "Toggle item visibility"),
    ] {
        actions.push(action::<ItemArgs>(&format!("item.{id}"), label, "Sources"));
    }
    for (id, label) in [
        ("mute", "Mute source"),
        ("unmute", "Unmute source"),
        ("toggle", "Toggle mute"),
        ("push_to_talk", "Push to talk"),
        ("push_to_mute", "Push to mute"),
    ] {
        actions.push(action::<SourceArgs>(&format!("audio.{id}"), label, "Audio"));
    }
    let defaults = [
        ("record", "ctrl+alt+cmd+r", "recording.toggle", json!({})),
        ("split", "ctrl+alt+cmd+s", "recording.split", json!({})),
        (
            "marker",
            "ctrl+alt+cmd+m",
            "recording.marker",
            json!({"label":"Marker"}),
        ),
        ("studio", "ctrl+alt+cmd+d", "studio.toggle", json!({})),
        ("take", "ctrl+alt+cmd+return", "transition.take", json!({})),
        ("replay", "ctrl+alt+cmd+y", "replay.save", json!({})),
    ]
    .into_iter()
    .map(|(id, keys, action, args)| {
        (
            id.into(),
            Binding {
                keys: keys.into(),
                action: action.into(),
                args,
            },
        )
    })
    .collect();
    Catalogue {
        actions,
        defaults,
        keys: key_names(),
        modifiers: ["ctrl", "alt", "shift", "cmd"].map(str::to_string).into(),
    }
}

/// Physical US key positions (layout independent); numpad keys are distinct.
pub fn key_names() -> Vec<String> {
    let mut keys: Vec<String> = ('a'..='z')
        .chain('0'..='9')
        .map(|c| c.to_string())
        .collect();
    keys.extend((1..=20).map(|n| format!("f{n}")));
    keys.extend((0..=9).map(|n| format!("numpad_{n}")));
    keys.extend(
        [
            "space",
            "return",
            "tab",
            "escape",
            "backspace",
            "delete",
            "left",
            "right",
            "up",
            "down",
            "home",
            "end",
            "pageup",
            "pagedown",
            "minus",
            "equal",
            "left_bracket",
            "right_bracket",
            "backslash",
            "semicolon",
            "quote",
            "comma",
            "period",
            "slash",
            "grave",
            "numpad_decimal",
            "numpad_multiply",
            "numpad_plus",
            "numpad_clear",
            "numpad_divide",
            "numpad_enter",
            "numpad_minus",
            "numpad_equal",
            "media_play_pause",
            "media_next",
            "media_previous",
            "media_volume_up",
            "media_volume_down",
            "media_mute",
        ]
        .map(str::to_string),
    );
    keys
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chord {
    pub key: String,
    /// ctrl=1, alt=2, shift=4, cmd=8. Not native modifier flags.
    pub modifiers: u32,
}

impl Chord {
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.len() > 128 {
            return Err("chord must be at most 128 bytes; use modifiers plus one named key".into());
        }
        let lower = text.to_ascii_lowercase();
        let parts: Vec<&str> = lower.split('+').collect();
        let key = *parts.last().unwrap();
        if !key_names().iter().any(|k| k == key) {
            return Err(format!(
                "unknown key {key:?}; valid keys: {}",
                key_names().join(", ")
            ));
        }
        let mut modifiers = 0;
        for part in &parts[..parts.len() - 1] {
            let bit = match *part {
                "ctrl" => 1,
                "alt" => 2,
                "shift" => 4,
                "cmd" => 8,
                _ => {
                    return Err(format!(
                        "unknown modifier {part:?}; valid modifiers: ctrl, alt, shift, cmd"
                    ));
                }
            };
            if modifiers & bit != 0 {
                return Err(format!(
                    "duplicate modifier {part:?}; use each of ctrl, alt, shift, cmd once"
                ));
            }
            modifiers |= bit;
        }
        let bare_allowed = key.starts_with("numpad_")
            || key.starts_with("media_")
            || key
                .strip_prefix('f')
                .and_then(|n| n.parse::<u32>().ok())
                .is_some_and(|n| (13..=20).contains(&n));
        if modifiers & (1 | 8) == 0 && !bare_allowed {
            return Err("this key needs ctrl or cmd; shift/alt alone or together are reserved for typing and navigation. Bare f13–f20, numpad and media keys are allowed".into());
        }
        let letter = key.len() == 1 && key.as_bytes()[0].is_ascii_alphabetic();
        let digit = key.len() == 1 && key.as_bytes()[0].is_ascii_digit();
        let navigation = matches!(
            key,
            "space"
                | "tab"
                | "return"
                | "backspace"
                | "delete"
                | "left"
                | "right"
                | "up"
                | "down"
                | "home"
                | "end"
                | "pageup"
                | "pagedown"
        );
        // Command letters/digits cover editing, Find, tabs, reload, windows
        // and screenshots. Control letters are terminal/editor control keys.
        // Qualifying a chord is still allowed unless that exact combination
        // is a common macOS command (full screen, lock, force quit, etc.).
        let command_shortcut = matches!(modifiers, 8 | 12)
            && (letter
                || digit
                || navigation
                || matches!(
                    key,
                    "comma"
                        | "grave"
                        | "period"
                        | "minus"
                        | "equal"
                        | "left_bracket"
                        | "right_bracket"
                        | "semicolon"
                        | "slash"
                        | "backslash"
                ));
        let control_shortcut = matches!(modifiers, 1 | 5) && (letter || navigation);
        let system_shortcut = (matches!(modifiers, 9 | 13) && matches!(key, "f" | "q" | "space"))
            || (modifiers == 13 && matches!(key, "3" | "4" | "5"))
            || (matches!(modifiers, 10 | 14)
                && matches!(
                    key,
                    "escape"
                        | "q"
                        | "h"
                        | "m"
                        | "w"
                        | "d"
                        | "space"
                        | "c"
                        | "v"
                        | "f"
                        | "r"
                        | "t"
                        | "i"
                        | "j"
                        | "l"
                        | "n"
                        | "p"
                        | "s"
                        | "u"
                ));
        if command_shortcut || control_shortcut || system_shortcut {
            return Err(
                "reserved system/app shortcut; choose an unused ctrl+alt or ctrl+cmd chord, or f13–f20/a keypad/media key".into(),
            );
        }
        Ok(Self {
            key: key.into(),
            modifiers,
        })
    }

    pub fn canonical(&self) -> String {
        let mut parts: Vec<&str> = [(1, "ctrl"), (2, "alt"), (4, "shift"), (8, "cmd")]
            .into_iter()
            .filter_map(|(bit, name)| (self.modifiers & bit != 0).then_some(name))
            .collect();
        parts.push(&self.key);
        parts.join("+")
    }
}

pub fn issues(bindings: &Bindings) -> Vec<Issue> {
    let mut issues = Vec::new();
    let mut chords: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if bindings.len() > 256 {
        return limit_issue();
    }
    for (id, binding) in bindings {
        let path = format!("hotkeys.{}", bounded(id, 64));
        if !crate::library::valid_id(id) {
            issues.push(Issue {
                path: path.clone(),
                message: "binding id: use lowercase letters, digits and dashes, at most 64".into(),
            });
        }
        match Chord::parse(&binding.keys) {
            Ok(chord) => chords
                .entry(chord.canonical())
                .or_default()
                .push(id.clone()),
            Err(message) => issues.push(Issue {
                path: format!("{path}.keys"),
                message,
            }),
        }
        if let Err(message) = binding.arguments() {
            let (field, detail) = message.split_once(": ").unwrap_or(("args", &message));
            issues.push(Issue {
                path: format!("{path}.{field}"),
                message: detail.to_string(),
            });
        }
    }
    for (keys, ids) in chords {
        if ids.len() > 1 {
            issues.push(Issue {
                path: format!("hotkeys.{}.keys", bounded(&ids[0], 64)),
                message: format!(
                    "{keys}: conflicts among {} bindings: {}{}",
                    ids.len(),
                    ids.iter()
                        .take(8)
                        .map(|id| bounded(id, 64))
                        .collect::<Vec<_>>()
                        .join(", "),
                    if ids.len() > 8 { ", ..." } else { "" }
                ),
            });
        }
    }
    issues
}
