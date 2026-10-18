//! Mask filters (#115): a shape, a gradient, or another source or scene
//! decides how much of a source's picture shows. Like OBS's Advanced Masks
//! plugin, built from its public description (no plugin code). Masks stack:
//! each combines with the masks before it (`combine`), and their numbers
//! can be animated with keyframes of their own.
//!
//! Positions and sizes are fractions of the masked picture (0.5 is its
//! middle), so a mask fits a picture whatever its resolution; radii and
//! feathers are in the picture's pixels.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::animation::{is_ease_in_out, is_zero};
use crate::filter::{Check, within};
use crate::{AnimationStart, Easing, Fill, Issues, Iterations, PlayDirection, Radius};

/// How many masks deep a source's masks may reach (a mask's source
/// masked in turn, and so on); the compositor follows no further.
pub const MAX_MASK_DEPTH: u32 = 16;

/// The most stops a gradient mask has.
pub const MAX_GRADIENT_STOPS: usize = 8;

/// How a mask combines with the masks before it in the chain.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MaskCombine {
    /// Only what this mask and those before it both show stays (the first
    /// mask simply masks).
    #[default]
    Intersect,
    /// What this mask shows is added to what those before it show (of the
    /// picture before them).
    Add,
    /// What this mask shows is taken away from what those before it show.
    Subtract,
}

/// A shape mask's shape.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MaskShape {
    /// Fills its box, with each corner rounded by `radius`.
    #[default]
    Rectangle,
    /// The largest circle that fits its box.
    Circle,
    /// Fills its box.
    Ellipse,
    /// A regular polygon with `sides`, the largest that fits its box, a
    /// corner at the top; corners rounded by `radius`.
    Polygon,
    /// A star with `points`, the largest that fits its box, a point at the
    /// top; `inner_radius` sets how deep its notches go.
    Star,
    /// A heart, the largest that fits its box.
    Heart,
}

/// Shows the picture inside a shape (OBS Advanced Masks' shape masks).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ShapeMask {
    #[serde(default)]
    pub shape: MaskShape,
    /// Where the shape's centre is across the picture: 0 (left edge) to 1
    /// (right edge).
    #[serde(default = "half")]
    pub x: f32,
    /// And down: 0 (top) to 1 (bottom).
    #[serde(default = "half")]
    pub y: f32,
    /// The shape's box: its width as a fraction of the picture's.
    #[serde(default = "most")]
    pub width: f32,
    /// Its height as a fraction of the picture's.
    #[serde(default = "most")]
    pub height: f32,
    /// Turns the shape about its centre, in degrees clockwise.
    #[serde(default)]
    pub rotation: f32,
    /// Rounds corners, in pixels: each of a rectangle's (top left, top
    /// right, bottom right, bottom left), or a polygon's and star's (the
    /// first value).
    #[serde(default, skip_serializing_if = "Radius::is_zero")]
    pub radius: Radius,
    /// A polygon's sides: 3 to 32.
    #[serde(default = "six")]
    pub sides: u32,
    /// A star's points: 3 to 32.
    #[serde(default = "five")]
    pub points: u32,
    /// How far a star's notches reach out, as a fraction of its points'
    /// reach: 0.05 to 1.
    #[serde(default = "half")]
    pub inner_radius: f32,
    /// Softens the edge over this many pixels, centred on it.
    #[serde(default)]
    pub feather: f32,
    /// Show the picture outside the shape instead.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub invert: bool,
    #[serde(default)]
    pub combine: MaskCombine,
    /// Animations of its numbers (see [`MaskAnimation`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub animations: Vec<MaskAnimation>,
}

impl Default for ShapeMask {
    fn default() -> Self {
        Self {
            shape: MaskShape::Rectangle,
            x: 0.5,
            y: 0.5,
            width: 0.8,
            height: 0.8,
            rotation: 0.0,
            radius: Radius::default(),
            sides: 6,
            points: 5,
            inner_radius: 0.5,
            feather: 0.0,
            invert: false,
            combine: MaskCombine::Intersect,
            animations: Vec::new(),
        }
    }
}

impl Check for ShapeMask {
    fn check(&self, path: &str, issues: &mut Issues) {
        position(issues, path, self.x, self.y);
        within(issues, path, "width", self.width, 0.0, 10.0);
        within(issues, path, "height", self.height, 0.0, 10.0);
        within(issues, path, "rotation", self.rotation, -3600.0, 3600.0);
        for (k, corner) in self.radius.corners().into_iter().enumerate() {
            if !(0.0..=10_000.0).contains(&corner) {
                let field = match self.radius {
                    Radius::All(_) => "radius".to_string(),
                    Radius::Each(_) => format!("radius[{k}]"),
                };
                issues.add(
                    format!("{path}.{field}"),
                    format!("{corner} is out of range; use 0 to 10000 pixels"),
                );
            }
        }
        within(issues, path, "sides", self.sides as f32, 3.0, 32.0);
        within(issues, path, "points", self.points as f32, 3.0, 32.0);
        within(issues, path, "inner_radius", self.inner_radius, 0.05, 1.0);
        within(issues, path, "feather", self.feather, 0.0, 1000.0);
        check_animations(path, &self.animations, SHAPE_ANIMATES, issues);
    }
}

/// A gradient mask's shape.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GradientShape {
    /// Changes along a line through its centre, in the direction `angle`.
    #[default]
    Linear,
    /// Changes outwards from its centre.
    Radial,
}

/// One stop of a gradient mask.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GradientStop {
    /// Where along the gradient: 0 (its start) to 1 (its end).
    pub at: f32,
    /// How much of the picture shows there: 0 (none) to 1 (all).
    pub opacity: f32,
}

/// Fades the picture in or out along a gradient (OBS Advanced Masks'
/// gradient masks).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct GradientMask {
    #[serde(default)]
    pub gradient: GradientShape,
    /// The gradient's centre across the picture: 0 (left) to 1 (right).
    #[serde(default = "half")]
    pub x: f32,
    /// And down: 0 (top) to 1 (bottom).
    #[serde(default = "half")]
    pub y: f32,
    /// A linear gradient's direction, in degrees clockwise: 0 runs left to
    /// right, 90 top to bottom.
    #[serde(default)]
    pub angle: f32,
    /// How long it is: 1 spans the picture along its direction (linear),
    /// or reaches its corners from the middle (radial).
    #[serde(default = "full")]
    pub length: f32,
    /// How much shows along it, from its start to its end (2 to 8 stops,
    /// in order of `at`); between stops it changes evenly. Before the
    /// first and after the last, their opacity holds.
    #[serde(default = "fade_out")]
    pub stops: Vec<GradientStop>,
    /// Softens the gradient over this many more pixels.
    #[serde(default)]
    pub feather: f32,
    /// Show where the gradient hides, and hide where it shows.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub invert: bool,
    #[serde(default)]
    pub combine: MaskCombine,
    /// Animations of its numbers (see [`MaskAnimation`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub animations: Vec<MaskAnimation>,
}

impl Default for GradientMask {
    fn default() -> Self {
        Self {
            gradient: GradientShape::Linear,
            x: 0.5,
            y: 0.5,
            angle: 0.0,
            length: 1.0,
            stops: fade_out(),
            feather: 0.0,
            invert: false,
            combine: MaskCombine::Intersect,
            animations: Vec::new(),
        }
    }
}

impl Check for GradientMask {
    fn check(&self, path: &str, issues: &mut Issues) {
        position(issues, path, self.x, self.y);
        within(issues, path, "angle", self.angle, -3600.0, 3600.0);
        within(issues, path, "length", self.length, 0.001, 10.0);
        within(issues, path, "feather", self.feather, 0.0, 1000.0);
        if !(2..=MAX_GRADIENT_STOPS).contains(&self.stops.len()) {
            issues.add(
                format!("{path}.stops"),
                format!(
                    "give 2 to {MAX_GRADIENT_STOPS} stops, like [{{ at = 0, opacity = 1 }}, \
                     {{ at = 1, opacity = 0 }}]; it has {}",
                    self.stops.len()
                ),
            );
        }
        let mut last: Option<f32> = None;
        for (k, stop) in self.stops.iter().enumerate() {
            let stop_path = format!("{path}.stops[{k}]");
            within(issues, &stop_path, "at", stop.at, 0.0, 1.0);
            within(issues, &stop_path, "opacity", stop.opacity, 0.0, 1.0);
            if let Some(last) = last
                && stop.at < last
            {
                issues.add(
                    format!("{stop_path}.at"),
                    format!(
                        "stops go in order of `at`, and {} comes before {last}",
                        stop.at
                    ),
                );
            }
            last = Some(stop.at);
        }
        check_animations(path, &self.animations, GRADIENT_ANIMATES, issues);
    }
}

/// What a source mask reads of the mask's picture.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MaskChannel {
    /// Its brightness: white shows the picture, black (or transparent)
    /// hides it. Right for black-and-white mask videos.
    #[default]
    Luminance,
    /// Its opacity: opaque shows the picture, transparent hides it.
    Alpha,
    Red,
    Green,
    Blue,
}

/// How a source mask's picture is placed on the masked picture before
/// `scale`, `x` and `y` move it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MaskFit {
    /// Stretched over the whole picture.
    #[default]
    Stretch,
    /// As large as fits inside, keeping its shape; outside it nothing
    /// shows.
    Contain,
    /// Covers the picture, keeping its shape; its overflow is cut off.
    Cover,
    /// Pixel for pixel, at its own size.
    None,
}

/// Uses another source's or scene's live picture as the mask (OBS
/// Advanced Masks' source masks), frame by frame in step with the
/// program. Until the mask has a picture, nothing of the source shows,
/// whatever `invert`, `threshold` or `combine` say. Previews of the source
/// alone, and its ISO file, show it unmasked by other sources.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SourceMask {
    /// The id of the video source whose picture masks (give it or
    /// `mask_scene`). It runs, and is on air (media play, pages load),
    /// while a source it masks does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask_source: Option<String>,
    /// Or the id of a scene, drawn on its own canvas, that masks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask_scene: Option<String>,
    #[serde(default)]
    pub channel: MaskChannel,
    /// With `softness`, where the channel's value turns from hiding to
    /// showing the picture: 0 to 1.
    #[serde(default = "half")]
    pub threshold: f32,
    /// How gradually it turns, 0 (at once: a hard edge) to 1 (evenly from
    /// 0 to 1, which with a threshold of 0.5 uses the value as it is).
    #[serde(default = "full")]
    pub softness: f32,
    #[serde(default)]
    pub fit: MaskFit,
    /// Scales the mask's picture about its centre, after `fit`.
    #[serde(default = "full")]
    pub scale: f32,
    /// Where the mask's centre is across the masked picture: 0 (left) to
    /// 1 (right).
    #[serde(default = "half")]
    pub x: f32,
    /// And down: 0 (top) to 1 (bottom).
    #[serde(default = "half")]
    pub y: f32,
    /// Show the picture where the mask would hide it, and hide it where
    /// it would show.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub invert: bool,
    #[serde(default)]
    pub combine: MaskCombine,
    /// Animations of its numbers (see [`MaskAnimation`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub animations: Vec<MaskAnimation>,
}

impl Default for SourceMask {
    fn default() -> Self {
        Self {
            mask_source: None,
            mask_scene: None,
            channel: MaskChannel::Luminance,
            threshold: 0.5,
            softness: 1.0,
            fit: MaskFit::Stretch,
            scale: 1.0,
            x: 0.5,
            y: 0.5,
            invert: false,
            combine: MaskCombine::Intersect,
            animations: Vec::new(),
        }
    }
}

impl SourceMask {
    /// What masks: a source's id or a scene's.
    pub fn mask(&self) -> Option<MaskRef<'_>> {
        match (&self.mask_source, &self.mask_scene) {
            (Some(source), None) => Some(MaskRef::Source(source)),
            (None, Some(scene)) => Some(MaskRef::Scene(scene)),
            _ => None,
        }
    }
}

/// What a source mask's picture comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaskRef<'a> {
    Source(&'a str),
    Scene(&'a str),
}

impl Check for SourceMask {
    fn check(&self, path: &str, issues: &mut Issues) {
        match (&self.mask_source, &self.mask_scene) {
            (None, None) => issues.add(
                format!("{path}.mask_source"),
                "name the video source (mask_source) or the scene (mask_scene) whose picture \
                 masks",
            ),
            (Some(_), Some(_)) => issues.add(
                format!("{path}.mask_scene"),
                "give mask_source or mask_scene, not both",
            ),
            _ => {}
        }
        within(issues, path, "threshold", self.threshold, 0.0, 1.0);
        within(issues, path, "softness", self.softness, 0.0, 1.0);
        within(issues, path, "scale", self.scale, 0.01, 100.0);
        position(issues, path, self.x, self.y);
        check_animations(path, &self.animations, SOURCE_ANIMATES, issues);
    }
}

/// Animates a mask's numbers over time, as an item's `animations` do
/// (CSS's `animation`), with its stops written in place: each stop sets
/// some of the mask's numbers by their field names.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MaskAnimation {
    /// How long one run takes, in milliseconds.
    pub duration_ms: u32,
    /// How long after its clock starts it starts, in milliseconds; less
    /// than 0 starts it part of the way through.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub delay_ms: i32,
    /// How each stretch between stops speeds up and slows down, unless a
    /// stop says otherwise: `ease_in_out` if absent.
    #[serde(default, skip_serializing_if = "is_ease_in_out")]
    pub easing: Easing,
    /// How many runs: a number (1 if absent) or `"infinite"`.
    #[serde(default, skip_serializing_if = "Iterations::is_once")]
    pub iterations: Iterations,
    #[serde(default, skip_serializing_if = "PlayDirection::is_normal")]
    pub direction: PlayDirection,
    #[serde(default, skip_serializing_if = "Fill::is_both")]
    pub fill: Fill,
    /// The clock it runs on: `clock` (the engine's: loops stay in step,
    /// in the program and in filtered ISO files and previews alike), or
    /// `on_air` and `shown` (both: from when the masked source's picture
    /// is first drawn). Snapshots show animations at their start.
    #[serde(default, skip_serializing_if = "AnimationStart::is_on_air")]
    pub start: AnimationStart,
    /// The stops, in order of `at`. A number no stop at 0 (or 1) sets runs
    /// from (or to) the mask's own value.
    pub frames: Vec<MaskKeyframe>,
}

/// One stop of a mask animation: the mask's numbers at `at`. Each kind of
/// mask animates its own: shape masks `x`, `y`, `width`, `height`,
/// `rotation`, `radius`, `inner_radius` and `feather`; gradient masks `x`,
/// `y`, `angle`, `length` and `feather`; source masks `x`, `y`, `scale`,
/// `threshold` and `softness`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MaskKeyframe {
    /// How far through a run it is, from 0 (the start) to 1 (the end).
    pub at: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radius: Option<Radius>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inner_radius: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feather: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub angle: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub softness: Option<f32>,
    /// How the stretch from this stop to the next speeds up and slows
    /// down, in place of the animation's `easing`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub easing: Option<Easing>,
}

impl MaskKeyframe {
    /// The numbers it sets, by field name.
    pub fn values(&self) -> Vec<(&'static str, f32)> {
        let mut out = Vec::new();
        let mut put = |name, value: Option<f32>| {
            if let Some(value) = value {
                out.push((name, value));
            }
        };
        put("x", self.x);
        put("y", self.y);
        put("width", self.width);
        put("height", self.height);
        put("rotation", self.rotation);
        put("inner_radius", self.inner_radius);
        put("feather", self.feather);
        put("angle", self.angle);
        put("length", self.length);
        put("scale", self.scale);
        put("threshold", self.threshold);
        put("softness", self.softness);
        if let Some(radius) = self.radius {
            let [a, b, c, d] = radius.corners();
            out.extend([
                ("radius_top_left", a),
                ("radius_top_right", b),
                ("radius_bottom_right", c),
                ("radius_bottom_left", d),
            ]);
        }
        out
    }
}

/// The numbers each kind of mask animates, as stops write them.
pub const SHAPE_ANIMATES: &[&str] = &[
    "x",
    "y",
    "width",
    "height",
    "rotation",
    "radius",
    "inner_radius",
    "feather",
];
pub const GRADIENT_ANIMATES: &[&str] = &["x", "y", "angle", "length", "feather"];
pub const SOURCE_ANIMATES: &[&str] = &["x", "y", "scale", "threshold", "softness"];

fn check_animations(
    path: &str,
    animations: &[MaskAnimation],
    animates: &[&str],
    issues: &mut Issues,
) {
    for (k, animation) in animations.iter().enumerate() {
        let path = format!("{path}.animations[{k}]");
        if animation.duration_ms == 0 {
            issues.add(format!("{path}.duration_ms"), "must be at least 1 (ms)");
        }
        if let Iterations::Count(count) = animation.iterations
            && crate::negative_or_nan(count)
        {
            issues.add(
                format!("{path}.iterations"),
                "give 0 or more runs, or \"infinite\"",
            );
        }
        animation.easing.check(&format!("{path}.easing"), issues);
        if animation.frames.is_empty() {
            issues.add(
                format!("{path}.frames"),
                format!(
                    "give at least one stop, like {{ at = 1, {} = 0.2 }}",
                    animates[0]
                ),
            );
        }
        let mut last: Option<f32> = None;
        for (j, frame) in animation.frames.iter().enumerate() {
            let path = format!("{path}.frames[{j}]");
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
                        "stops go in order of `at`, each at its own time, and this one ({}) \
                         isn't after the one before ({last})",
                        frame.at
                    ),
                );
            }
            if frame.at.is_finite() {
                last = Some(frame.at);
            }
            if let Some(easing) = &frame.easing {
                easing.check(&format!("{path}.easing"), issues);
            }
            let mut set: Vec<&str> = frame.values().iter().map(|(name, _)| *name).collect();
            if frame.radius.is_some() {
                set.retain(|name| !name.starts_with("radius_"));
                set.push("radius");
            }
            for name in set {
                if !animates.contains(&name) {
                    issues.add(
                        format!("{path}.{name}"),
                        format!(
                            "this kind of mask doesn't animate {name}; it animates: {}",
                            animates.join(", ")
                        ),
                    );
                }
            }
            for (name, value) in frame.values() {
                if !value.is_finite() {
                    issues.add(format!("{path}.{name}"), format!("{value} is not a number"));
                }
            }
        }
    }
}

fn position(issues: &mut Issues, path: &str, x: f32, y: f32) {
    within(issues, path, "x", x, -10.0, 10.0);
    within(issues, path, "y", y, -10.0, 10.0);
}

fn half() -> f32 {
    0.5
}

fn most() -> f32 {
    0.8
}

fn full() -> f32 {
    1.0
}

fn five() -> u32 {
    5
}

fn six() -> u32 {
    6
}

fn fade_out() -> Vec<GradientStop> {
    vec![
        GradientStop {
            at: 0.0,
            opacity: 1.0,
        },
        GradientStop {
            at: 1.0,
            opacity: 0.0,
        },
    ]
}

impl crate::Setup {
    /// What the enabled source masks of source `id` read, in chain order.
    pub fn masks_of(&self, id: &str) -> Vec<MaskRef<'_>> {
        let Some(source) = self.source(id) else {
            return Vec::new();
        };
        source
            .filters
            .iter()
            .filter(|filter| filter.enabled)
            .filter_map(|filter| match &filter.kind {
                crate::FilterKind::SourceMask(mask) => mask.mask(),
                _ => None,
            })
            .collect()
    }

    /// The video sources the source masks of `ids` need running (their
    /// mask sources, the sources their mask scenes show, and theirs in
    /// turn), each once, not counting `ids` themselves.
    pub fn mask_needs<'a>(&'a self, ids: &[&'a str]) -> Vec<&'a str> {
        let mut out: Vec<&str> = Vec::new();
        let mut todo: Vec<&str> = ids.to_vec();
        while let Some(id) = todo.pop() {
            for need in self.mask_deps(id) {
                if !ids.contains(&need) && !out.contains(&need) {
                    out.push(need);
                    todo.push(need);
                }
            }
        }
        out
    }

    /// The sources source `id`'s masks read directly: mask sources, and
    /// the sources mask scenes show.
    fn mask_deps(&self, id: &str) -> Vec<&str> {
        let mut out = Vec::new();
        for mask in self.masks_of(id) {
            match mask {
                MaskRef::Source(source) => {
                    if self.source(source).is_some() && !out.contains(&source) {
                        out.push(source);
                    }
                }
                MaskRef::Scene(scene) => {
                    if let Some(scene) = self.scenes.iter().find(|s| s.id == scene) {
                        for source in scene.video_sources(&self.scenes) {
                            if !out.contains(&source) {
                                out.push(source);
                            }
                        }
                    }
                }
            }
        }
        out
    }
}

/// Check that the setup's source masks name a video source or a scene,
/// and that no mask needs, through others, the picture it masks.
pub(crate) fn check_mask_refs(setup: &crate::Setup, video: &[&str], issues: &mut Issues) {
    let scenes: Vec<&str> = setup.scenes.iter().map(|s| s.id.as_str()).collect();
    for (i, source) in setup.sources.iter().enumerate() {
        for (j, filter) in source.filters.iter().enumerate() {
            let crate::FilterKind::SourceMask(mask) = &filter.kind else {
                continue;
            };
            let path = format!("sources[{i}].filters[{j}]");
            match mask.mask() {
                Some(MaskRef::Source(id)) if !video.contains(&id) => issues.add(
                    format!("{path}.mask_source"),
                    format!(
                        "{id:?} is not a video source; valid: {}",
                        list(video.iter().copied().filter(|v| *v != source.id))
                    ),
                ),
                Some(MaskRef::Source(id)) if id == source.id => issues.add(
                    format!("{path}.mask_source"),
                    "a source can't mask itself; name another source",
                ),
                Some(MaskRef::Scene(id)) if !scenes.contains(&id) => issues.add(
                    format!("{path}.mask_scene"),
                    format!(
                        "{id:?} is not a scene; valid: {}",
                        list(scenes.iter().copied())
                    ),
                ),
                Some(_)
                    if filter.enabled && !needs_itself(setup, &source.id) && {
                        let deep = depth(setup, &source.id, &mut Default::default());
                        deep > MAX_MASK_DEPTH
                    } =>
                {
                    issues.add(
                        format!(
                            "{path}.{}",
                            if mask.mask_scene.is_some() {
                                "mask_scene"
                            } else {
                                "mask_source"
                            }
                        ),
                        format!(
                            "masks reach more than {MAX_MASK_DEPTH} deep from {:?} (a mask's \
                             source masked in turn, and so on); mask with fewer steps",
                            source.id
                        ),
                    )
                }
                Some(_) if filter.enabled && needs_itself(setup, &source.id) => issues.add(
                    format!(
                        "{path}.{}",
                        if mask.mask_scene.is_some() {
                            "mask_scene"
                        } else {
                            "mask_source"
                        }
                    ),
                    format!(
                        "the mask shows {:?} itself (through its own picture or another mask), \
                         which would need the picture before it is drawn; mask with something \
                         that doesn't show it",
                        source.id
                    ),
                ),
                _ => {}
            }
        }
    }
}

/// How many masks deep source `id`'s masks reach (0 with none). Each
/// source is worked out once; a cycle (reported apart) counts as ending.
fn depth<'a>(
    setup: &'a crate::Setup,
    id: &'a str,
    known: &mut std::collections::HashMap<&'a str, Option<u32>>,
) -> u32 {
    match known.get(id) {
        Some(Some(depth)) => return *depth,
        // On the path being worked out: a cycle.
        Some(None) => return 0,
        None => {}
    }
    known.insert(id, None);
    let deepest = setup
        .mask_deps(id)
        .into_iter()
        .map(|next| depth(setup, next, known) + 1)
        .max()
        .unwrap_or(0);
    known.insert(id, Some(deepest));
    deepest
}

/// Whether source `id`'s masks need its own picture.
fn needs_itself(setup: &crate::Setup, id: &str) -> bool {
    let mut seen: Vec<&str> = Vec::new();
    let mut todo = setup.mask_deps(id);
    while let Some(next) = todo.pop() {
        if next == id {
            return true;
        }
        if seen.contains(&next) {
            continue;
        }
        seen.push(next);
        todo.extend(setup.mask_deps(next));
    }
    false
}

fn list<'a>(ids: impl Iterator<Item = &'a str>) -> String {
    let ids: Vec<&str> = ids.collect();
    match ids.is_empty() {
        true => "none".into(),
        false => ids.join(", "),
    }
}

#[cfg(test)]
mod tests {
    use crate::Setup;

    fn problems(sources: &str, scenes: &str) -> Vec<(String, String)> {
        let text = format!(
            r#"version = 1
name = "masks"
[canvas]
width = 640
height = 360
fps = 30
{sources}
[[scenes]]
id = "main"
items = [{{ source = "cam", rect = {{ x = 0, y = 0, width = 640, height = 360 }} }}]
{scenes}
[recording]
folder = "."
program = {{ encoder = {{ codec = "hevc", quality = 70 }} }}
iso = {{ enabled = false, encoder = {{ codec = "hevc", quality = 70 }} }}
"#
        );
        let setup = Setup::from_toml(&text).expect("reads");
        setup
            .validate()
            .into_iter()
            .map(|issue| (issue.path, issue.message))
            .collect()
    }

    #[test]
    fn masks_read_their_defaults_and_check_their_ranges() {
        let found = problems(
            r#"[[sources]]
id = "cam"
kind = "test_pattern"
filters = [
  { kind = "shape_mask", shape = "star", points = 2, width = 0.5 },
  { id = "fade", kind = "gradient_mask", stops = [{ at = 0.5, opacity = 1 }, { at = 0.2, opacity = 0 }] },
  { id = "spin", kind = "shape_mask", animations = [{ duration_ms = 1000, frames = [{ at = 1, rotation = 90, scale = 2 }] }] },
]"#,
            "",
        );
        let paths: Vec<&str> = found.iter().map(|(path, _)| path.as_str()).collect();
        assert!(
            paths.contains(&"sources.cam.filters[0].points"),
            "{found:?}"
        );
        assert!(
            paths.contains(&"sources.cam.filters[1].stops[1].at"),
            "{found:?}"
        );
        let (_, message) = found
            .iter()
            .find(|(path, _)| path == "sources.cam.filters[2].animations[0].frames[0].scale")
            .expect("scale isn't a shape mask's");
        assert!(message.contains("rotation"), "{message}");
        assert_eq!(found.len(), 3, "{found:?}");
    }

    #[test]
    fn masks_reaching_too_deep_are_refused_by_field() {
        // cam is masked by m1, m1 by m2, … : 17 steps is one too many.
        let mut sources = String::from(
            "[[sources]]\nid = \"cam\"\nkind = \"test_pattern\"\nfilters = [{ kind = \"source_mask\", mask_source = \"m1\" }]\n",
        );
        for k in 1..=17 {
            let filters = match k < 17 {
                true => format!(
                    "filters = [{{ kind = \"source_mask\", mask_source = \"m{}\" }}]\n",
                    k + 1
                ),
                false => String::new(),
            };
            sources += &format!("[[sources]]\nid = \"m{k}\"\nkind = \"test_pattern\"\n{filters}");
        }
        let found = problems(&sources, "");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].0, "sources.cam.filters[0].mask_source");
        assert!(found[0].1.contains("more than 16 deep"), "{found:?}");
        // One step less is fine.
        let fewer = sources.replace(
            "filters = [{ kind = \"source_mask\", mask_source = \"m17\" }]\n",
            "",
        );
        assert!(problems(&fewer, "").is_empty());
    }

    #[test]
    fn source_masks_name_what_masks_and_never_need_themselves() {
        let sources = r#"[[sources]]
id = "cam"
kind = "test_pattern"
filters = [{ kind = "source_mask", mask_source = "wipe" }]
[[sources]]
id = "wipe"
kind = "test_pattern"
filters = [{ kind = "source_mask", mask_scene = "loop" }]
[[sources]]
id = "mic"
kind = "tone"
"#;
        let looped = r#"[[scenes]]
id = "loop"
items = [{ source = "cam", rect = { x = 0, y = 0, width = 10, height = 10 } }]"#;
        let found = problems(sources, looped);
        let paths: Vec<&str> = found.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "sources.cam.filters[0].mask_source",
                "sources.wipe.filters[0].mask_scene"
            ],
            "{found:?}"
        );
        // Masking with a source that has no picture lists those that do.
        let found = problems(
            &sources
                .replace("mask_source = \"wipe\"", "mask_source = \"mic\"")
                .replace(", mask_scene = \"loop\"", ", mask_source = \"cam\""),
            "",
        );
        assert_eq!(found[0].0, "sources.cam.filters[0].mask_source");
        assert!(found[0].1.contains("valid: wipe"), "{found:?}");
        // What masks runs while what it masks does.
        let setup = Setup::from_toml(&format!(
            "version = 1\nname = \"m\"\n[canvas]\nwidth = 8\nheight = 8\nfps = 30\n{}\n{}",
            sources.replace("mask_scene = \"loop\"", "mask_scene = \"other\""),
            "[[scenes]]\nid = \"other\"\nitems = [{ source = \"mic\", rect = { x = 0, y = 0, width = 1, height = 1 } }]\n[recording]\nfolder = \".\"\nprogram = { encoder = { codec = \"hevc\", quality = 70 } }\niso = { enabled = false, encoder = { codec = \"hevc\", quality = 70 } }"
        ))
        .unwrap();
        assert_eq!(setup.mask_needs(&["cam"]), ["wipe", "mic"]);
    }
}
