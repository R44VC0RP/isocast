//! Keyframe animations: keyframe sets by id (`[[keyframes]]`, CSS's
//! `@keyframes`) and the animations scene items play with them (CSS's
//! `animation`), each on one of three clocks. Only what the compositor can
//! change without laying out or drawing text again animates: opacity,
//! translate, scale, rotate, background, border, radius, shadow and crop.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::layout::{Length, LengthValue};
use crate::{Color, Crop, Easing, Issues, Radius, Shadow, negative_or_nan};

/// Ids kept for the built-in show and hide keyframe sets (5.2b): setups
/// can't use them for their own.
pub const BUILT_IN_KEYFRAMES: &[&str] = &[
    "fade-in",
    "fade-out",
    "slide-in-left",
    "slide-in-right",
    "slide-in-up",
    "slide-in-down",
    "slide-out-left",
    "slide-out-right",
    "slide-out-up",
    "slide-out-down",
];

/// A keyframe set: how an item's properties change over one run of an
/// animation, as stops along it (CSS's `@keyframes`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Keyframes {
    /// What animations call it.
    pub id: String,
    /// The stops, in order of `at`. A property no stop at 0 (or 1) sets
    /// runs from (or to) the item's own value, as in CSS.
    pub frames: Vec<Keyframe>,
}

/// One stop of a keyframe set: the values it gives at `at`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Keyframe {
    /// How far through a run it is, from 0 (the start) to 1 (the end).
    pub at: f32,
    /// Multiplies the item's opacity (and its children's): 0 to 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f32>,
    /// Moves the item [x, y]: pixels, or `"50%"` of its own width and
    /// height. Applied around its own `style.transform`, like CSS's
    /// `translate` property.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub translate: Option<[Length; 2]>,
    /// Scales the item about its centre (1 is its size; 0 or more).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<f32>,
    /// Turns the item about its centre, in degrees clockwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotate: Option<f32>,
    /// In place of the item's background colour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<Color>,
    /// In place of its border's colour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub border_color: Option<Color>,
    /// In place of its border's width, in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub border_width: Option<f32>,
    /// In place of its corner radius.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radius: Option<Radius>,
    /// In place of its shadow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow: Option<Shadow>,
    /// In place of its crop, in pixels of its picture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crop: Option<Crop>,
    /// How the stretch from this stop to the next speeds up and slows
    /// down, in place of the animation's `easing`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub easing: Option<Easing>,
}

/// A keyframe set played on an item (CSS's `animation`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Animation {
    /// The id of the keyframe set it plays.
    pub keyframes: String,
    /// How long one run takes, in milliseconds.
    pub duration_ms: u32,
    /// How long after its clock starts it starts, in milliseconds; less
    /// than 0 starts it part of the way through.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub delay_ms: i32,
    /// How each stretch between stops speeds up and slows down, unless a
    /// stop says otherwise (as CSS's `animation-timing-function`):
    /// `ease_in_out` if absent.
    #[serde(default, skip_serializing_if = "is_ease_in_out")]
    pub easing: Easing,
    /// How many runs: a number (1 if absent; 2.5 ends half way through the
    /// third) or `"infinite"`.
    #[serde(default, skip_serializing_if = "Iterations::is_once")]
    pub iterations: Iterations,
    /// Which way runs go: `normal` (the default), `reverse`, `alternate`
    /// (forwards, then backwards) or `alternate_reverse`.
    #[serde(default, skip_serializing_if = "PlayDirection::is_normal")]
    pub direction: PlayDirection,
    /// Whether its first stop holds before it starts and its last after it
    /// ends: `both` (the default), `forwards`, `backwards` or `none`.
    #[serde(default, skip_serializing_if = "Fill::is_both")]
    pub fill: Fill,
    /// The clock it runs on: `on_air` (the default; from when the program,
    /// or studio mode's preview, starts showing the item's scene, again
    /// each time), `shown` (from when the item appears or is made visible)
    /// or `clock` (the engine's clock: loops stay in step across scene
    /// switches and recordings).
    #[serde(default, skip_serializing_if = "AnimationStart::is_on_air")]
    pub start: AnimationStart,
}

pub(crate) fn is_zero(value: &i32) -> bool {
    *value == 0
}

pub(crate) fn is_ease_in_out(easing: &Easing) -> bool {
    *easing == Easing::EaseInOut
}

/// How many times an animation runs.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(from = "IterationsForm", into = "IterationsForm")]
pub enum Iterations {
    /// This many runs (0 or more, and may end part of the way through).
    Count(f32),
    /// Runs for as long as its clock does.
    Infinite,
}

impl Default for Iterations {
    fn default() -> Self {
        Self::Count(1.0)
    }
}

impl Iterations {
    pub(crate) fn is_once(&self) -> bool {
        *self == Self::Count(1.0)
    }
}

/// How iterations are written: a number or `"infinite"`.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(untagged, expecting = "a number of runs (1 if absent) or \"infinite\"")]
enum IterationsForm {
    Count(#[serde(serialize_with = "crate::layout::whole")] f32),
    Infinite(Infinite),
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Infinite {
    /// For as long as its clock runs.
    Infinite,
}

impl From<IterationsForm> for Iterations {
    fn from(form: IterationsForm) -> Self {
        match form {
            IterationsForm::Count(count) => Self::Count(count),
            IterationsForm::Infinite(_) => Self::Infinite,
        }
    }
}

impl From<Iterations> for IterationsForm {
    fn from(iterations: Iterations) -> Self {
        match iterations {
            Iterations::Count(count) => Self::Count(count),
            Iterations::Infinite => Self::Infinite(Infinite::Infinite),
        }
    }
}

/// Which way an animation's runs go (CSS's `animation-direction`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlayDirection {
    /// From the first stop to the last, every run.
    #[default]
    Normal,
    /// From the last stop to the first, every run (easings run backwards
    /// too).
    Reverse,
    /// Forwards, then backwards, and so on.
    Alternate,
    /// Backwards, then forwards, and so on.
    AlternateReverse,
}

impl PlayDirection {
    pub(crate) fn is_normal(&self) -> bool {
        *self == Self::Normal
    }
}

/// Whether an animation's values hold outside its runs (CSS's
/// `animation-fill-mode`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Fill {
    /// Its start holds before it starts, and its end after it ends.
    #[default]
    Both,
    /// Its end holds after it ends; before it starts the item looks as
    /// its own values say.
    Forwards,
    /// Its start holds before it starts; after it ends the item looks as
    /// its own values say.
    Backwards,
    /// Neither: outside its runs the item looks as its own values say.
    None,
}

impl Fill {
    pub(crate) fn is_both(&self) -> bool {
        *self == Self::Both
    }
}

/// The clock an animation runs on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AnimationStart {
    /// From when the program (or studio mode's preview, which keeps its
    /// own) starts showing the item's scene, again each time it does; a
    /// scene shown inside another starts with that one.
    #[default]
    OnAir,
    /// From when the item appears (with the engine, or added by a change)
    /// or is made visible.
    Shown,
    /// The engine's clock, from when it started: never restarts, so loops
    /// stay in step across scene switches and recordings.
    Clock,
}

impl AnimationStart {
    pub(crate) fn is_on_air(&self) -> bool {
        *self == Self::OnAir
    }
}

/// A translate length's value for an item `size` pixels long: pixels, or
/// a percentage of `size`. `None` for `auto`, which can't move anything.
pub fn translate_px(length: &Length, size: f32) -> Option<f32> {
    match length.value().ok()? {
        LengthValue::Px(px) => Some(px),
        LengthValue::Percent(percent) => Some(percent / 100.0 * size),
        LengthValue::Auto => None,
    }
}

impl crate::Setup {
    /// The keyframe set with `id`.
    pub fn keyframe_set(&self, id: &str) -> Option<&Keyframes> {
        self.keyframes.iter().find(|set| set.id == id)
    }
}

/// Check the setup's keyframe sets.
pub(crate) fn check_keyframes(keyframes: &[Keyframes], issues: &mut Issues) {
    let mut ids = std::collections::HashSet::new();
    for (i, set) in keyframes.iter().enumerate() {
        let path = format!("keyframes[{i}]");
        issues.check_id(format!("{path}.id"), &set.id, &mut ids);
        if BUILT_IN_KEYFRAMES.contains(&set.id.as_str()) {
            issues.add(
                format!("{path}.id"),
                format!(
                    "{:?} is kept for the built-in show and hide animations; pick another id",
                    set.id
                ),
            );
        }
        if set.frames.is_empty() {
            issues.add(
                format!("{path}.frames"),
                "give at least one stop, like { at = 0.0, opacity = 0.0 }",
            );
        }
        let mut last: Option<f32> = None;
        for (j, frame) in set.frames.iter().enumerate() {
            check_frame(&format!("{path}.frames[{j}]"), frame, last, issues);
            if frame.at.is_finite() {
                last = Some(frame.at);
            }
        }
    }
}

/// Check one stop at `path`, coming after a stop at `last`.
fn check_frame(path: &str, frame: &Keyframe, last: Option<f32>, issues: &mut Issues) {
    if !(0.0..=1.0).contains(&frame.at) {
        issues.add(
            format!("{path}.at"),
            format!("{} is outside 0 (the start) to 1 (the end)", frame.at),
        );
    } else if let Some(last) = last
        && frame.at <= last
    {
        issues.add(
            format!("{path}.at"),
            format!(
                "stops go in order of `at`, each at its own time, and this one ({}) isn't \
                 after the one before ({last})",
                frame.at
            ),
        );
    }
    if let Some(opacity) = frame.opacity
        && !(0.0..=1.0).contains(&opacity)
    {
        issues.add(format!("{path}.opacity"), "must be between 0 and 1");
    }
    if let Some(translate) = &frame.translate {
        for (k, length) in translate.iter().enumerate() {
            match length.value() {
                Ok(LengthValue::Auto) => issues.add(
                    format!("{path}.translate[{k}]"),
                    "\"auto\" can't move an item; use a number of pixels, or \"50%\" of its \
                     own size",
                ),
                Ok(_) => {}
                Err(message) => issues.add(format!("{path}.translate[{k}]"), message),
            }
        }
    }
    if let Some(scale) = frame.scale
        && negative_or_nan(scale)
    {
        issues.add(format!("{path}.scale"), "must be 0 or more");
    }
    if let Some(rotate) = frame.rotate
        && !rotate.is_finite()
    {
        issues.add(format!("{path}.rotate"), "give a number of degrees");
    }
    let mut colors = Vec::new();
    if let Some(color) = &frame.background {
        colors.push(("background", color));
    }
    if let Some(color) = &frame.border_color {
        colors.push(("border_color", color));
    }
    if let Some(shadow) = &frame.shadow {
        colors.push(("shadow.color", &shadow.color));
        if negative_or_nan(shadow.blur) {
            issues.add(format!("{path}.shadow.blur"), "must be 0 or more");
        }
        if !shadow.x.is_finite() || !shadow.y.is_finite() {
            issues.add(
                format!("{path}.shadow"),
                "give x and y as numbers of pixels",
            );
        }
    }
    for (field, color) in colors {
        if color.rgba().is_none() {
            issues.add(
                format!("{path}.{field}"),
                format!(
                    "{:?} is not a colour; use #rgb, #rrggbb or #rrggbbaa",
                    color.0
                ),
            );
        }
    }
    if let Some(width) = frame.border_width
        && negative_or_nan(width)
    {
        issues.add(format!("{path}.border_width"), "must be 0 or more");
    }
    if let Some(radius) = frame.radius
        && radius.corners().into_iter().any(negative_or_nan)
    {
        issues.add(format!("{path}.radius"), "must be 0 or more");
    }
    if let Some(easing) = &frame.easing {
        easing.check(&format!("{path}.easing"), issues);
    }
}

/// Check an item's animations at `path` (`….animations`), against the
/// setup's keyframe set ids.
pub(crate) fn check_animations(
    path: &str,
    animations: &[Animation],
    keyframes: &[&str],
    issues: &mut Issues,
) {
    for (k, animation) in animations.iter().enumerate() {
        let path = format!("{path}[{k}]");
        if !keyframes.contains(&animation.keyframes.as_str()) {
            let message = match keyframes.is_empty() {
                true => format!(
                    "{:?} is not a keyframe set; the setup has none, so add one under \
                     [[keyframes]] first",
                    animation.keyframes
                ),
                false => format!(
                    "{:?} is not a keyframe set; valid: {}",
                    animation.keyframes,
                    keyframes.join(", ")
                ),
            };
            issues.add(format!("{path}.keyframes"), message);
        }
        if let Iterations::Count(count) = animation.iterations
            && negative_or_nan(count)
        {
            issues.add(
                format!("{path}.iterations"),
                "give 0 or more runs, or \"infinite\"",
            );
        }
        animation.easing.check(&format!("{path}.easing"), issues);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn animations_read_and_save_as_written() {
        let animation: Animation = toml::from_str(
            r#"keyframes = "slide-in"
               duration_ms = 400
               easing = { cubic_bezier = [0.0, 0.0, 0.58, 1.0] }
               iterations = "infinite"
               direction = "alternate""#,
        )
        .unwrap();
        assert_eq!(animation.iterations, Iterations::Infinite);
        assert_eq!(animation.easing, Easing::CubicBezier([0.0, 0.0, 0.58, 1.0]));
        assert_eq!(animation.fill, Fill::Both);
        assert_eq!(animation.start, AnimationStart::OnAir);
        let text = toml::to_string(&animation).unwrap();
        assert!(text.contains("iterations = \"infinite\""), "{text}");
        assert!(!text.contains("fill"), "{text}");
        let twice: Animation =
            toml::from_str("keyframes = \"a\"\nduration_ms = 1\niterations = 2").unwrap();
        assert_eq!(twice.iterations, Iterations::Count(2.0));
        let text = toml::to_string(&twice).unwrap();
        assert!(text.contains("iterations = 2\n"), "{text}");
        let steps: Easing = serde_json::from_str(r#"{"steps": 4}"#).unwrap();
        assert_eq!(steps, Easing::Steps(4, crate::StepPosition::End));
        assert_eq!(serde_json::to_string(&steps).unwrap(), r#"{"steps":4}"#);
        assert_eq!(
            serde_json::to_string(&Easing::EaseIn).unwrap(),
            r#""ease_in""#
        );
    }

    #[test]
    fn bad_easings_and_iterations_say_what_is_valid() {
        let err = serde_json::from_str::<Easing>(r#""ease""#).unwrap_err();
        assert!(err.to_string().contains("cubic_bezier"), "{err}");
        let err = serde_json::from_str::<Easing>(r#"{"steps": 2, "jump": "start"}"#).unwrap_err();
        assert!(err.to_string().contains("steps = n"), "{err}");
        let err = serde_json::from_str::<Iterations>(r#""forever""#).unwrap_err();
        assert!(err.to_string().contains("infinite"), "{err}");
    }

    #[test]
    fn keyframes_are_checked_with_their_paths() {
        let sets: Vec<Keyframes> = serde_json::from_value(serde_json::json!([
            {"id": "fade-in", "frames": [{"at": 0.0, "opacity": 0.0}]},
            {"id": "pop", "frames": [
                {"at": 0.5, "scale": -1.0},
                {"at": 0.25, "translate": ["auto", "10%"], "background": "red"},
                {"at": 2.0, "easing": {"cubic_bezier": [1.5, 0.0, 0.5, 1.0]}},
            ]},
            {"id": "empty", "frames": []},
        ]))
        .unwrap();
        let mut issues = Issues::default();
        check_keyframes(&sets, &mut issues);
        let paths: Vec<&str> = issues.0.iter().map(|issue| issue.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "keyframes[0].id",
                "keyframes[1].frames[0].scale",
                "keyframes[1].frames[1].at",
                "keyframes[1].frames[1].translate[0]",
                "keyframes[1].frames[1].background",
                "keyframes[1].frames[2].at",
                "keyframes[1].frames[2].easing.cubic_bezier",
                "keyframes[2].frames",
            ],
            "{:?}",
            issues.0
        );
        let mut issues = Issues::default();
        let animations: Vec<Animation> = serde_json::from_value(serde_json::json!([
            {"keyframes": "slid-in", "duration_ms": 400, "iterations": -1},
        ]))
        .unwrap();
        check_animations("a", &animations, &["slide-in", "chat-pop"], &mut issues);
        assert_eq!(issues.0[0].path, "a[0].keyframes");
        assert!(
            issues.0[0].message.contains("valid: slide-in, chat-pop"),
            "{:?}",
            issues.0
        );
        assert_eq!(issues.0[1].path, "a[0].iterations");
    }
}
