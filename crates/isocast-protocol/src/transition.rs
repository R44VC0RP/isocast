//! Scene transitions, as OBS has them, by `kind`: how the program changes
//! from the scene on air to the next. The setup has a default
//! (`transition`), a scene can override it for switches to that scene
//! (`scenes[].transition`), and a switch can name its own.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Color, Issues};

/// How the program changes from one scene to the next.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(remote = "Self")]
pub struct Transition {
    #[serde(flatten)]
    pub kind: TransitionKind,
    /// How long it takes, in milliseconds (300 if absent, at most 10 s). A
    /// cut ignores it, and a stinger lasts as long as its video.
    #[serde(default = "default_duration")]
    pub duration_ms: u32,
    /// Fields its kind doesn't have, kept for validation to report.
    #[serde(flatten)]
    #[schemars(skip)]
    pub unknown: crate::UnknownFields,
}

crate::fields::keep_unknown_fields!(Transition, kind, TransitionKind);

/// The longest a transition may take.
pub const MAX_TRANSITION_MS: u32 = 10_000;

fn default_duration() -> u32 {
    300
}

impl Transition {
    /// Straight to the next scene: a cut, or anything but a stinger that
    /// takes no time.
    pub fn is_cut(&self) -> bool {
        match self.kind {
            TransitionKind::Cut => true,
            TransitionKind::Stinger(_) => false,
            _ => self.duration_ms == 0,
        }
    }

    /// A crossfade lasting `ms` milliseconds.
    pub fn fade(ms: u32) -> Self {
        Self {
            kind: TransitionKind::Fade,
            duration_ms: ms,
            unknown: crate::UnknownFields::new(),
        }
    }

    /// Report fields its kind doesn't have, as at `path`.
    pub(crate) fn check_unknown(&self, path: &str, issues: &mut Issues) {
        crate::fields::report(
            issues,
            path,
            &self.unknown,
            &format!("a {} transition", self.kind.label()),
            self.kind.fields(),
            &["kind", "duration_ms"],
        );
    }

    /// The problems with this transition, as at `path` (like
    /// `scenes[1].transition`).
    pub fn issues(&self, path: &str) -> Vec<crate::Issue> {
        let mut issues = Issues::default();
        self.check(path, &mut issues);
        issues.0
    }

    pub(crate) fn check(&self, path: &str, issues: &mut Issues) {
        self.check_unknown(path, issues);
        let stinger = matches!(self.kind, TransitionKind::Stinger(_));
        if !stinger && self.duration_ms > MAX_TRANSITION_MS {
            issues.add(
                format!("{path}.duration_ms"),
                format!("{} ms is longer than 10 s", self.duration_ms),
            );
        }
        match &self.kind {
            TransitionKind::Cut | TransitionKind::Fade => {}
            TransitionKind::Swipe(Swipe { easing, .. })
            | TransitionKind::Slide(Slide { easing, .. })
            | TransitionKind::Move(Move { easing }) => {
                easing.check(&format!("{path}.easing"), issues);
            }
            TransitionKind::FadeToColor(fade) => {
                if fade.color.rgba().is_none() {
                    issues.add(
                        format!("{path}.color"),
                        format!(
                            "{:?} is not a colour; use #rgb, #rrggbb or #rrggbbaa",
                            fade.color.0
                        ),
                    );
                }
                if !(0.0..=100.0).contains(&fade.switch_point) {
                    issues.add(
                        format!("{path}.switch_point"),
                        format!("{} is out of range; use 0 to 100 (%)", fade.switch_point),
                    );
                }
            }
            TransitionKind::LumaWipe(wipe) => {
                if !(0.0..=1.0).contains(&wipe.softness) {
                    issues.add(
                        format!("{path}.softness"),
                        format!("{} is out of range; use 0 to 1", wipe.softness),
                    );
                }
                if wipe.image.as_deref() == Some("") {
                    issues.add(
                        format!("{path}.image"),
                        "give a picture file, or leave image out to use the pattern",
                    );
                }
            }
            TransitionKind::Stinger(stinger) => {
                if stinger.file.is_empty() {
                    issues.add(format!("{path}.file"), "give the stinger's video file");
                }
                if stinger.matte.is_some() && stinger.cut_point_ms.is_some() {
                    issues.add(
                        format!("{path}.cut_point_ms"),
                        "with a matte the matte switches the scenes, so there is no cut \
                         point; remove one of them",
                    );
                }
            }
        }
    }

    /// The files it reads (a luma wipe's picture, a stinger's video), to
    /// make paths relative to a setup file absolute.
    pub(crate) fn files_mut(&mut self) -> Option<&mut String> {
        match &mut self.kind {
            TransitionKind::LumaWipe(wipe) => wipe.image.as_mut(),
            TransitionKind::Stinger(stinger) => Some(&mut stinger.file),
            _ => None,
        }
    }
}

impl crate::Setup {
    /// Every transition the setup names, with its field path: the setup's,
    /// then each scene's.
    pub fn transitions(&self) -> Vec<(String, &Transition)> {
        let scenes = self.scenes.iter().enumerate().filter_map(|(i, scene)| {
            let transition = scene.transition.as_ref()?;
            Some((format!("scenes[{i}].transition"), transition))
        });
        std::iter::once(("transition".to_string(), &self.transition))
            .chain(scenes)
            .collect()
    }
}

/// What a transition does, by `kind`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind")]
pub enum TransitionKind {
    /// Straight to the next scene.
    #[default]
    #[serde(rename = "cut")]
    Cut,
    /// The next scene fades in over the current one.
    #[serde(rename = "fade")]
    Fade,
    /// The next scene slides in over the current one, or the current one
    /// slides away to uncover it.
    #[serde(rename = "swipe")]
    Swipe(Swipe),
    /// The next scene pushes the current one off the canvas.
    #[serde(rename = "slide")]
    Slide(Slide),
    /// The current scene fades to a colour, then the colour fades to the
    /// next scene.
    #[serde(rename = "fade_to_color")]
    FadeToColor(FadeToColor),
    /// The next scene appears in the order of a greyscale pattern's
    /// brightness, dark first.
    #[serde(rename = "luma_wipe")]
    LumaWipe(LumaWipe),
    /// A video plays over the switch, hiding it (OBS's Stinger).
    #[serde(rename = "stinger")]
    Stinger(Stinger),
    /// Items in both scenes (the same id, showing the same thing) move,
    /// resize, turn and fade to their new place; the others fade out or in.
    #[serde(rename = "move")]
    Move(Move),
}

impl TransitionKind {
    /// Every kind's name, as setups write it.
    pub const NAMES: &[&str] = &[
        "cut",
        "fade",
        "swipe",
        "slide",
        "fade_to_color",
        "luma_wipe",
        "stinger",
        "move",
    ];

    /// Where `value`, a transition of the kind named `name`, is wrong in
    /// that kind's fields (see [`crate::located`]).
    pub(crate) fn locate(name: &str, value: &serde_json::Value) -> Option<crate::located::Located> {
        use crate::located::from_value;
        match name {
            "swipe" => from_value::<Swipe>(value).err(),
            "slide" => from_value::<Slide>(value).err(),
            "fade_to_color" => from_value::<FadeToColor>(value).err(),
            "luma_wipe" => from_value::<LumaWipe>(value).err(),
            "stinger" => from_value::<Stinger>(value).err(),
            "move" => from_value::<Move>(value).err(),
            _ => None,
        }
    }

    /// The kind's own fields, as setups write them.
    pub fn fields(&self) -> &'static [String] {
        static FIELDS: std::sync::OnceLock<Vec<Vec<String>>> = std::sync::OnceLock::new();
        let table = FIELDS.get_or_init(|| {
            use crate::fields::names;
            vec![
                Vec::new(),
                Vec::new(),
                names::<Swipe>(),
                names::<Slide>(),
                names::<FadeToColor>(),
                names::<LumaWipe>(),
                names::<Stinger>(),
                names::<Move>(),
            ]
        });
        let index = Self::NAMES.iter().position(|name| *name == self.label());
        &table[index.unwrap_or_default()]
    }

    /// The kind's name, as setups write it.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Cut => "cut",
            Self::Fade => "fade",
            Self::Swipe(_) => "swipe",
            Self::Slide(_) => "slide",
            Self::FadeToColor(_) => "fade_to_color",
            Self::LumaWipe(_) => "luma_wipe",
            Self::Stinger(_) => "stinger",
            Self::Move(_) => "move",
        }
    }
}

/// How a scene item appears when it becomes visible, or disappears when
/// hidden (OBS's show and hide transitions).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ItemTransition {
    pub kind: ItemTransitionKind,
    /// How long it takes, in milliseconds (300 if absent, at most 10 s).
    #[serde(default = "item_transition_ms")]
    pub duration_ms: u32,
    /// Where a slide comes in from (or goes out to): `left` (the default),
    /// `right`, `up` or `down`.
    #[serde(default, skip_serializing_if = "is_left")]
    pub direction: Direction,
    #[serde(default, skip_serializing_if = "is_ease_in_out")]
    pub easing: Easing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ItemTransitionKind {
    /// Fades in or out.
    Fade,
    /// The picture slides in from (or out to) a side of the item, within
    /// the item's rect.
    Slide,
}

fn item_transition_ms() -> u32 {
    300
}

fn is_left(direction: &Direction) -> bool {
    *direction == Direction::Left
}

fn is_ease_in_out(easing: &Easing) -> bool {
    *easing == Easing::EaseInOut
}

impl ItemTransition {
    /// How the item looks `progress` (0 to 1) of the way through showing
    /// (`showing`) or hiding: its opacity, and how far its picture is moved
    /// out of its rect, as fractions of its width and height.
    pub fn look(&self, showing: bool, progress: f32) -> (f32, [f32; 2]) {
        let shown = self.easing.apply(match showing {
            true => progress,
            false => 1.0 - progress,
        });
        match self.kind {
            ItemTransitionKind::Fade => (shown, [0.0; 2]),
            ItemTransitionKind::Slide => {
                let out = 1.0 - shown;
                let shift = match self.direction {
                    Direction::Left => [-out, 0.0],
                    Direction::Right => [out, 0.0],
                    Direction::Up => [0.0, -out],
                    Direction::Down => [0.0, out],
                };
                (1.0, shift)
            }
        }
    }
}

/// Which way something moves across the canvas.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    #[default]
    Left,
    Right,
    Up,
    Down,
}

/// How motion speeds up and slows down: one of four names, or CSS's
/// `cubic-bezier()` and `steps()`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(from = "EasingForm", into = "EasingForm")]
pub enum Easing {
    /// At an even speed.
    Linear,
    /// Starting slowly.
    EaseIn,
    /// Ending slowly.
    EaseOut,
    /// Starting and ending slowly (cubic).
    #[default]
    EaseInOut,
    /// CSS's `cubic-bezier(x1, y1, x2, y2)`: a curve from (0, 0) to (1, 1)
    /// through those two control points, time across and progress up.
    CubicBezier([f32; 4]),
    /// CSS's `steps(n, position)`: `n` equal steps, jumping where
    /// `position` says.
    Steps(u32, StepPosition),
}

/// Where `steps` jumps (CSS's `jump-start`, `jump-end`, `jump-both` and
/// `jump-none`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StepPosition {
    /// At the start of each step: the first jump is at once.
    Start,
    /// At the end of each step: the last jump is at the end.
    #[default]
    End,
    /// At both ends: n + 1 levels, from 0 to 1, each held as long.
    Both,
    /// At neither: n levels from 0 to 1, the first and last held as long as
    /// the others.
    None,
}

impl StepPosition {
    fn is_end(&self) -> bool {
        *self == Self::End
    }
}

/// How an easing is written in a setup.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(
    untagged,
    expecting = "an easing: \"linear\", \"ease_in\", \"ease_out\", \"ease_in_out\", \
                 { cubic_bezier = [x1, y1, x2, y2] } or { steps = n, position = \"end\" }"
)]
enum EasingForm {
    Named(EasingName),
    CubicBezier(CubicBezierForm),
    Steps(StepsForm),
}

/// How motion speeds up and slows down over a transition.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum EasingName {
    /// At an even speed.
    Linear,
    /// Starting slowly.
    EaseIn,
    /// Ending slowly.
    EaseOut,
    /// Starting and ending slowly (cubic).
    EaseInOut,
}

/// CSS's `cubic-bezier()`: `{ cubic_bezier = [x1, y1, x2, y2] }`, the two
/// control points of a curve from (0, 0) to (1, 1), time across (x1 and
/// x2 from 0 to 1) and progress up (y1 and y2 may overshoot).
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CubicBezierForm {
    cubic_bezier: [f32; 4],
}

/// CSS's `steps()`: `{ steps = 4 }` or `{ steps = 4, position = "start" }`.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StepsForm {
    /// How many steps (at least 1; 2 with `position = "none"`).
    steps: u32,
    /// Where each step jumps: `start`, `end` (the default), `both` or
    /// `none`.
    #[serde(default, skip_serializing_if = "StepPosition::is_end")]
    position: StepPosition,
}

impl From<EasingForm> for Easing {
    fn from(form: EasingForm) -> Self {
        match form {
            EasingForm::Named(EasingName::Linear) => Self::Linear,
            EasingForm::Named(EasingName::EaseIn) => Self::EaseIn,
            EasingForm::Named(EasingName::EaseOut) => Self::EaseOut,
            EasingForm::Named(EasingName::EaseInOut) => Self::EaseInOut,
            EasingForm::CubicBezier(form) => Self::CubicBezier(form.cubic_bezier),
            EasingForm::Steps(form) => Self::Steps(form.steps, form.position),
        }
    }
}

impl From<Easing> for EasingForm {
    fn from(easing: Easing) -> Self {
        match easing {
            Easing::Linear => Self::Named(EasingName::Linear),
            Easing::EaseIn => Self::Named(EasingName::EaseIn),
            Easing::EaseOut => Self::Named(EasingName::EaseOut),
            Easing::EaseInOut => Self::Named(EasingName::EaseInOut),
            Easing::CubicBezier(cubic_bezier) => {
                Self::CubicBezier(CubicBezierForm { cubic_bezier })
            }
            Easing::Steps(steps, position) => Self::Steps(StepsForm { steps, position }),
        }
    }
}

impl Easing {
    /// How far motion has got when `t` (0 to 1) of the time has passed. The
    /// four names keep their cubic formulas; a cubic Bézier may overshoot
    /// 0 and 1.
    pub fn apply(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Self::Linear => t,
            Self::EaseIn => t * t * t,
            Self::EaseOut => 1.0 - (1.0 - t).powi(3),
            Self::EaseInOut if t < 0.5 => 4.0 * t * t * t,
            Self::EaseInOut => 1.0 - (-2.0 * t + 2.0).powi(3) / 2.0,
            Self::CubicBezier([x1, y1, x2, y2]) => cubic_bezier(x1, y1, x2, y2, t),
            Self::Steps(steps, position) => stepped(steps, position, t),
        }
    }

    /// Report what is wrong with it at `path`: control points' times
    /// outside 0 to 1, too few steps.
    pub(crate) fn check(&self, path: &str, issues: &mut Issues) {
        match *self {
            Self::CubicBezier(points) => {
                if points.iter().any(|value| !value.is_finite()) {
                    issues.add(
                        format!("{path}.cubic_bezier"),
                        "give four numbers: [x1, y1, x2, y2]",
                    );
                } else if !(0.0..=1.0).contains(&points[0]) || !(0.0..=1.0).contains(&points[2]) {
                    issues.add(
                        format!("{path}.cubic_bezier"),
                        format!(
                            "x1 and x2 (the first and third numbers) are times, from 0 to 1, \
                             and these are {} and {}",
                            points[0], points[2]
                        ),
                    );
                }
            }
            Self::Steps(steps, position) => {
                let least = if position == StepPosition::None { 2 } else { 1 };
                if steps < least {
                    issues.add(
                        format!("{path}.steps"),
                        match position {
                            StepPosition::None => "give at least 2 steps with position \"none\"",
                            _ => "give at least 1 step",
                        },
                    );
                }
            }
            _ => {}
        }
    }
}

/// CSS's `cubic-bezier(x1, y1, x2, y2)` at time `t` (0 to 1): the curve's
/// x solved for `t` (Newton's method, then halving), then its y.
fn cubic_bezier(x1: f32, y1: f32, x2: f32, y2: f32, t: f32) -> f32 {
    let [x1, y1, x2, y2, x] = [x1, y1, x2, y2, t].map(f64::from);
    // B(s) = 3(1 - s)²s·p1 + 3(1 - s)s²·p2 + s³, as a polynomial in s.
    let coefficients = |p1: f64, p2: f64| {
        let c = 3.0 * p1;
        let b = 3.0 * (p2 - p1) - c;
        (1.0 - c - b, b, c)
    };
    let (ax, bx, cx) = coefficients(x1, x2);
    let (ay, by, cy) = coefficients(y1, y2);
    let curve_x = |s: f64| ((ax * s + bx) * s + cx) * s;
    let slope_x = |s: f64| (3.0 * ax * s + 2.0 * bx) * s + cx;
    let curve_y = |s: f64| ((ay * s + by) * s + cy) * s;
    const CLOSE: f64 = 1e-9;
    let mut s = x;
    for _ in 0..8 {
        let error = curve_x(s) - x;
        if error.abs() < CLOSE {
            return curve_y(s) as f32;
        }
        let slope = slope_x(s);
        if slope.abs() < 1e-9 {
            break;
        }
        s -= error / slope;
    }
    // x rises from 0 to 1 (its control points' times are within 0 to 1),
    // so halving always finds it.
    let (mut low, mut high) = (0.0, 1.0);
    s = x;
    for _ in 0..64 {
        let at = curve_x(s);
        if (at - x).abs() < CLOSE {
            break;
        }
        if at < x {
            low = s;
        } else {
            high = s;
        }
        s = (low + high) / 2.0;
    }
    curve_y(s) as f32
}

/// CSS's `steps(steps, position)` at time `t` (0 to 1).
fn stepped(steps: u32, position: StepPosition, t: f32) -> f32 {
    let steps = steps.max(1) as f32;
    let mut step = (t * steps).floor();
    if matches!(position, StepPosition::Start | StepPosition::Both) {
        step += 1.0;
    }
    let jumps = match position {
        StepPosition::Start | StepPosition::End => steps,
        StepPosition::Both => steps + 1.0,
        StepPosition::None => (steps - 1.0).max(1.0),
    };
    step.min(jumps) / jumps
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Swipe {
    /// Which way the moving scene goes: left (it comes in from the right
    /// edge, or leaves by the left), right, up or down.
    #[serde(default)]
    pub direction: Direction,
    /// The next scene slides in over the current one (true, the default),
    /// or the current one slides away to uncover the next (false).
    #[serde(default = "yes")]
    pub swipe_in: bool,
    #[serde(default)]
    pub easing: Easing,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Slide {
    /// Which way both scenes move: left (the next comes in from the right
    /// edge), right, up or down.
    #[serde(default)]
    pub direction: Direction,
    #[serde(default)]
    pub easing: Easing,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FadeToColor {
    /// The colour passed through (`#rgb` or `#rrggbb`; black by default).
    #[serde(default = "black")]
    pub color: Color,
    /// How far through the transition, 0 to 100 (%), the colour is at its
    /// fullest and the scenes switch behind it.
    #[serde(default = "half")]
    pub switch_point: f32,
}

impl Default for FadeToColor {
    fn default() -> Self {
        Self {
            color: black(),
            switch_point: half(),
        }
    }
}

/// A built-in greyscale pattern for a luma wipe; the next scene appears
/// where it is darkest first.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LumaPattern {
    /// From the left edge to the right.
    #[default]
    Horizontal,
    /// From the top edge to the bottom.
    Vertical,
    /// From the top left corner to the bottom right.
    Diagonal,
    /// From a vertical line down the middle out to the left and right.
    BarnDoorHorizontal,
    /// From a horizontal line across the middle out to the top and bottom.
    BarnDoorVertical,
    /// A rectangle growing from the centre.
    Box,
    /// A circle growing from the centre.
    Circle,
    /// A diamond growing from the centre.
    Diamond,
    /// A hand sweeping clockwise from twelve o'clock.
    Clock,
    /// Eight horizontal bands, each wiping down.
    BlindsHorizontal,
    /// Eight vertical bands, each wiping right.
    BlindsVertical,
    /// A checkerboard of squares wiping right, the white squares after the
    /// black.
    Checkerboard,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LumaWipe {
    #[serde(default)]
    pub pattern: LumaPattern,
    /// Or a picture whose brightness gives the order (dark first),
    /// stretched over the canvas: absolute, `~/...`, or relative to the
    /// setup file. If it can't be read, the pattern is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// How soft the edge is, 0 (sharp) to 1.
    #[serde(default = "softness")]
    pub softness: f32,
    /// Reverse the order: bright first.
    #[serde(default)]
    pub invert: bool,
}

impl Default for LumaWipe {
    fn default() -> Self {
        Self {
            pattern: LumaPattern::default(),
            image: None,
            softness: softness(),
            invert: false,
        }
    }
}

/// A video played over the switch (OBS's Stinger). It lasts as long as the
/// video. Transparent parts need a video with alpha (ProRes 4444 or HEVC
/// with alpha in a .mov, or VP9 or VP8 with alpha in a .webm, as OBS uses),
/// or a matte.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Stinger {
    /// The video: absolute, `~/...`, or relative to the setup file. It
    /// must be in the setup's transitions when a recording starts, which
    /// loads it.
    pub file: String,
    /// When the scenes switch behind the video, in milliseconds from its
    /// start; half way through if absent. Not used with a matte.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cut_point_ms: Option<u32>,
    /// A track matte in the same video (OBS's track matte): where it is
    /// white the next scene shows, where black the current one, and grey
    /// blends them; the video's other half plays on top.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matte: Option<Matte>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Matte {
    #[serde(default)]
    pub layout: MatteLayout,
    /// Swap black and white.
    #[serde(default)]
    pub invert: bool,
}

/// Where the matte is in the stinger's video.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MatteLayout {
    /// The picture on the left half, the matte on the right.
    #[default]
    SideBySide,
    /// The picture on the top half, the matte below.
    Stacked,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Move {
    #[serde(default)]
    pub easing: Easing,
}

fn yes() -> bool {
    true
}

fn black() -> Color {
    Color("#000000".into())
}

fn half() -> f32 {
    50.0
}

fn softness() -> f32 {
    0.03
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_fade_and_slide_in_and_out() {
        let fade: ItemTransition = toml::from_str("kind = \"fade\"\neasing = \"linear\"").unwrap();
        assert_eq!(fade.duration_ms, 300);
        assert_eq!(fade.look(true, 0.25), (0.25, [0.0, 0.0]));
        assert_eq!(fade.look(false, 0.25), (0.75, [0.0, 0.0]));
        let slide = ItemTransition {
            kind: ItemTransitionKind::Slide,
            duration_ms: 500,
            direction: Direction::Right,
            easing: Easing::Linear,
        };
        // Coming in from the right: three quarters out at the start.
        assert_eq!(slide.look(true, 0.25), (1.0, [0.75, 0.0]));
        // Going out to the right.
        assert_eq!(slide.look(false, 0.25), (1.0, [0.25, 0.0]));
        let up = ItemTransition {
            direction: Direction::Up,
            ..slide
        };
        assert_eq!(up.look(false, 1.0), (1.0, [0.0, -1.0]));
    }

    fn parse(toml: &str) -> Result<Transition, String> {
        toml::from_str(toml).map_err(|err| err.to_string())
    }

    #[test]
    fn transitions_read_their_kind_and_settings() {
        let fade = parse("kind = \"fade\"\nduration_ms = 500").unwrap();
        assert_eq!(fade, Transition::fade(500));
        let swipe = parse("kind = \"swipe\"\ndirection = \"up\"\nswipe_in = false").unwrap();
        assert_eq!(swipe.duration_ms, 300);
        assert_eq!(
            swipe.kind,
            TransitionKind::Swipe(Swipe {
                direction: Direction::Up,
                swipe_in: false,
                easing: Easing::EaseInOut,
            })
        );
        let cut = parse("kind = \"cut\"").unwrap();
        assert!(cut.is_cut());
        // A stinger lasts as long as its video, whatever its duration.
        let stinger = parse("kind = \"stinger\"\nfile = \"a.mov\"\nduration_ms = 0").unwrap();
        assert!(!stinger.is_cut());
    }

    #[test]
    fn unknown_kinds_and_settings_are_refused_with_the_options() {
        let err = parse("kind = \"wipe\"").unwrap_err();
        assert!(
            err.contains("luma_wipe") && err.contains("fade_to_color"),
            "{err}"
        );
        // A field the kind lacks is a problem at its path, listing the
        // kind's fields.
        let slide = parse("kind = \"slide\"\nswipe_in = true").unwrap();
        let issues = slide.issues("t");
        assert_eq!(issues[0].path, "t.swipe_in", "{issues:?}");
        assert!(
            issues[0].message.contains("direction, easing"),
            "{issues:?}"
        );
        let err = parse("kind = \"slide\"\ndirection = \"sideways\"").unwrap_err();
        assert!(err.contains("left"), "{err}");
    }

    #[test]
    fn settings_out_of_range_name_their_field() {
        let wipe = parse("kind = \"luma_wipe\"\nsoftness = 2.0\nduration_ms = 20000").unwrap();
        let paths: Vec<String> = wipe.issues("t").into_iter().map(|i| i.path).collect();
        assert_eq!(paths, ["t.duration_ms", "t.softness"]);
        let fade = parse("kind = \"fade_to_color\"\ncolor = \"red\"\nswitch_point = 120").unwrap();
        let paths: Vec<String> = fade.issues("t").into_iter().map(|i| i.path).collect();
        assert_eq!(paths, ["t.color", "t.switch_point"]);
    }

    #[test]
    fn easing_runs_from_zero_to_one() {
        for easing in [
            Easing::Linear,
            Easing::EaseIn,
            Easing::EaseOut,
            Easing::EaseInOut,
        ] {
            assert_eq!(easing.apply(0.0), 0.0);
            assert!((easing.apply(1.0) - 1.0).abs() < 1e-6);
        }
        assert!((Easing::EaseInOut.apply(0.5) - 0.5).abs() < 1e-6);
        assert!(Easing::EaseIn.apply(0.25) < 0.25 && Easing::EaseOut.apply(0.25) > 0.25);
    }

    #[test]
    fn the_four_names_keep_their_cubic_formulas() {
        // Bit for bit, so existing transitions record the same frames.
        for i in 0..=1000 {
            let t = i as f32 / 1000.0;
            assert_eq!(Easing::Linear.apply(t), t);
            assert_eq!(Easing::EaseIn.apply(t), t * t * t);
            assert_eq!(Easing::EaseOut.apply(t), 1.0 - (1.0 - t).powi(3));
            let in_out = match t < 0.5 {
                true => 4.0 * t * t * t,
                false => 1.0 - (-2.0 * t + 2.0).powi(3) / 2.0,
            };
            assert_eq!(Easing::EaseInOut.apply(t), in_out);
        }
    }

    #[test]
    fn cubic_beziers_and_steps_are_csss() {
        // CSS's `ease` half way: 0.8024 (browsers agree to 4 places).
        let ease = Easing::CubicBezier([0.25, 0.1, 0.25, 1.0]);
        assert!(
            (ease.apply(0.5) - 0.802_403_4).abs() < 1e-5,
            "{}",
            ease.apply(0.5)
        );
        // A Bézier along the diagonal is linear.
        let straight = Easing::CubicBezier([0.25, 0.25, 0.75, 0.75]);
        for i in 0..=20 {
            let t = i as f32 / 20.0;
            assert!((straight.apply(t) - t).abs() < 1e-6);
        }
        // Control points beyond 0 and 1 overshoot.
        let back = Easing::CubicBezier([0.68, -0.55, 0.27, 1.55]);
        assert!(back.apply(0.1) < 0.0 && back.apply(0.9) > 1.0);
        let steps = |n, position, t| Easing::Steps(n, position).apply(t);
        assert_eq!(steps(4, StepPosition::End, 0.0), 0.0);
        assert_eq!(steps(4, StepPosition::End, 0.24), 0.0);
        assert_eq!(steps(4, StepPosition::End, 0.25), 0.25);
        assert_eq!(steps(4, StepPosition::End, 1.0), 1.0);
        assert_eq!(steps(4, StepPosition::Start, 0.0), 0.25);
        assert_eq!(steps(4, StepPosition::Start, 0.8), 1.0);
        assert_eq!(steps(4, StepPosition::Both, 0.0), 0.2);
        assert_eq!(steps(4, StepPosition::Both, 1.0), 1.0);
        assert_eq!(steps(4, StepPosition::None, 0.0), 0.0);
        assert_eq!(steps(4, StepPosition::None, 0.5), 2.0 / 3.0);
        assert_eq!(steps(4, StepPosition::None, 1.0), 1.0);
        // Checked in setups.
        let mut issues = Issues::default();
        Easing::CubicBezier([1.5, 0.0, 0.5, 1.0]).check("e", &mut issues);
        Easing::Steps(1, StepPosition::None).check("f", &mut issues);
        let paths: Vec<&str> = issues.0.iter().map(|issue| issue.path.as_str()).collect();
        assert_eq!(paths, ["e.cubic_bezier", "f.steps"]);
    }
}
