//! The Isocast setup document.
//!
//! A setup describes everything the engine needs to run a session: the
//! canvas, the sources (screens, cameras, microphones…), the scenes that
//! arrange those sources, and how recordings are written. People write it as
//! TOML; agents and the control API exchange the same structure as JSON.

mod animation;
pub mod api;
mod audio_filter;
pub mod change;
mod content;
pub mod data;
pub mod developer;
pub mod devices;
pub mod diagnostics;
mod fields;
mod filter;
pub mod hotkeys;
pub mod init;
pub mod layout;
pub mod library;
pub mod located;
pub mod manifest;
pub mod obs;
pub mod optimize;
pub mod plugin;
pub mod project;
pub mod replay;
mod source;
pub mod stream;
pub mod timeline;
mod transition;
pub mod virtual_camera;
pub mod websocket;

pub use animation::*;
pub use audio_filter::*;
pub use content::{Content, ContentKind};
pub use fields::UnknownFields;
pub use filter::*;
pub use layout::Layout;
pub use source::*;
pub use transition::*;

use std::collections::{BTreeMap, HashSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The current setup format version.
pub const SETUP_VERSION: u32 = 1;

/// A complete session description.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Setup {
    /// Setup format version. Must be 1.
    pub version: u32,
    /// Human-readable name of this setup.
    pub name: String,
    /// The composited output canvas.
    pub canvas: Canvas,
    /// Every input the session can use.
    #[serde(default)]
    pub sources: Vec<Source>,
    /// Arrangements of sources on the canvas.
    #[serde(default)]
    pub scenes: Vec<Scene>,
    /// Keyframe sets, by id, that scene items' `animations` play (CSS's
    /// `@keyframes`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keyframes: Vec<Keyframes>,
    /// How the program changes scenes unless a switch says otherwise.
    #[serde(default, skip_serializing_if = "Transition::is_cut")]
    pub transition: Transition,
    /// How sources' balance pans them, and where monitored sources play.
    #[serde(default, skip_serializing_if = "AudioSetup::is_default")]
    pub audio: AudioSetup,
    /// How recordings are written.
    pub recording: Recording,
    /// Live streaming of the program, if set up. The stream key is never
    /// part of a setup: `isocast stream key set` keeps it in the Keychain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<stream::StreamSettings>,
    /// Profile hotkeys. Absent/empty keeps legacy profiles unchanged.
    #[serde(default, skip_serializing_if = "hotkeys::Config::is_absent")]
    pub hotkeys: hotkeys::Config,
    #[serde(
        default,
        skip_serializing_if = "virtual_camera::VirtualCameraSettings::is_default"
    )]
    pub virtual_camera: virtual_camera::VirtualCameraSettings,
    /// Developer mode: verbose logs and timing traces (the profile's).
    #[serde(
        default,
        skip_serializing_if = "developer::DeveloperSettings::is_default"
    )]
    pub developer: developer::DeveloperSettings,
}

/// Audio settings for the whole setup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AudioSetup {
    /// How a source's `balance` lowers the side it moves away from.
    #[serde(default)]
    pub pan_law: PanLaw,
    /// The output device monitored sources play through: "default" (the
    /// system output, following it when it changes), or an id, name or
    /// part of a name from `isocast devices`.
    #[serde(default = "default_monitoring_device")]
    pub monitoring_device: String,
}

fn default_monitoring_device() -> String {
    "default".into()
}

impl Default for AudioSetup {
    fn default() -> Self {
        Self {
            pan_law: PanLaw::default(),
            monitoring_device: default_monitoring_device(),
        }
    }
}

impl AudioSetup {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// How balance moves a source between left and right. Each is the classic
/// pan law of that name, taken relative to the centre and never above it:
/// at 0 both sides are as captured, moving towards one side lowers the
/// other until it is silent at the end, and the near side stays as it is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PanLaw {
    /// Sine/cosine: the far side follows √2·sin(π/4·(1 − |balance|)),
    /// −5.3 dB at a balance of ±0.5.
    #[default]
    Sine,
    /// Square root (constant power): √(1 − |balance|), −3 dB at ±0.5.
    Square,
    /// Linear: 1 − |balance|, −6 dB at ±0.5.
    Linear,
}

impl PanLaw {
    /// Left and right factors for `balance` (−1 left to 1 right).
    pub fn factors(self, balance: f64) -> [f64; 2] {
        let balance = balance.clamp(-1.0, 1.0);
        // How much of the far side is left: 1 at the centre, 0 at the end.
        let rest = 1.0 - balance.abs();
        let far = match self {
            Self::Sine => std::f64::consts::SQRT_2 * (std::f64::consts::FRAC_PI_4 * rest).sin(),
            Self::Square => rest.sqrt(),
            Self::Linear => rest,
        };
        if balance >= 0.0 {
            [far, 1.0]
        } else {
            [1.0, far]
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Canvas {
    /// An even number of pixels from 16 to 8192.
    pub width: u32,
    /// An even number of pixels from 16 to 8192.
    pub height: u32,
    /// Frames per second, from 1 to 240.
    pub fps: u32,
}

/// One input. `id` is the stable machine identifier; `name` is a label.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(remote = "Self")]
pub struct Source {
    /// Stable identifier, lowercase letters, digits and dashes.
    pub id: String,
    /// Display label. Defaults to the id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(flatten)]
    pub kind: SourceKind,
    /// For audio sources: the program mix and sync settings.
    #[serde(default, skip_serializing_if = "AudioSettings::is_default")]
    pub audio: AudioSettings,
    /// For video sources: effect filters run on the picture, in order,
    /// wherever it is shown (ISO files keep it as captured).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<Filter>,
    /// For audio sources: filters run on the sound, in order, before the
    /// program mix (ISO files keep it as captured unless
    /// `recording.iso.filtered`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audio_filters: Vec<AudioFilter>,
    /// Fields its kind doesn't have, kept for validation to report.
    #[serde(flatten)]
    #[schemars(skip)]
    pub unknown: fields::UnknownFields,
}

fields::keep_unknown_fields!(Source, kind, SourceKind);

/// The fields every source has, besides its kind's.
const SOURCE_FIELDS: &[&str] = &["id", "name", "kind", "audio", "filters", "audio_filters"];

/// How an audio source sounds in the program mix, and its sync offset.
/// ISO files keep the source's audio as captured, only shifted by the
/// offset.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AudioSettings {
    /// Downmix stereo to mono in the program and monitoring, before balance:
    /// (left + right) / 2 in both channels. Raw ISO audio stays as captured.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mono: bool,
    /// Include this source's own sound in its ISO. Absent follows the ISO
    /// audio policy; false is an explicit opt-out, true an explicit opt-in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iso: Option<bool>,
    /// Volume change in the program mix, in dB (0 keeps it as captured).
    #[serde(default)]
    pub gain_db: f64,
    /// Left out of the program mix.
    #[serde(default)]
    pub muted: bool,
    /// Shift the audio later (positive) or earlier, in milliseconds, in
    /// every file, to line it up with the video.
    #[serde(default)]
    pub offset_ms: i32,
    /// Where it sits between left (−1) and right (1) in the program mix;
    /// 0, the centre, keeps both sides as captured. `audio.pan_law` says
    /// how the far side is lowered. Mono sources start centred.
    #[serde(default)]
    pub balance: f64,
    /// Whether it is heard through the monitoring device
    /// (`audio.monitoring_device`) while recording, after gain, mute and
    /// balance.
    #[serde(default)]
    pub monitor: Monitoring,
}

/// Whether a source is heard through the monitoring device, as in OBS's
/// Advanced Audio Properties.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Monitoring {
    /// Not heard (Monitor Off).
    #[default]
    Off,
    /// Heard, but left out of the program mix; ISO files still have it
    /// (Monitor Only).
    MonitorOnly,
    /// Heard, and in the program mix (Monitor and Output).
    MonitorAndOutput,
}

impl AudioSettings {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// The gain as a factor; 0 when muted.
    pub fn factor(&self) -> f32 {
        if self.muted {
            0.0
        } else {
            10f64.powf(self.gain_db / 20.0) as f32
        }
    }

    /// Whether it goes into the program mix (it does unless monitored
    /// only).
    pub fn in_program(&self) -> bool {
        self.monitor != Monitoring::MonitorOnly
    }

    /// Left and right factors: gain, mute and balance under `law`.
    pub fn gains(&self, law: PanLaw) -> [f32; 2] {
        let factor = f64::from(self.factor());
        law.factors(self.balance).map(|side| (side * factor) as f32)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Scene {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// How its items with a `layout` are laid out: the scene is their root
    /// container, the size of the canvas (CSS block layout if absent). Only
    /// the fields for children apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<Layout>,
    /// Items drawn back to front.
    #[serde(default)]
    pub items: Vec<SceneItem>,
    /// How the program changes to this scene, in place of the setup's
    /// transition, unless a switch names its own (OBS's transition
    /// override).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transition: Option<Transition>,
}

impl Scene {
    /// The video sources its visible items show, and those of the scenes
    /// they show, each once.
    pub fn video_sources<'a>(&'a self, scenes: &'a [Scene]) -> Vec<&'a str> {
        let mut out = Vec::new();
        for scene in self.shown_scenes(scenes) {
            visit(&scene.items, false, &mut |item| {
                if let Some(source) = item.source.as_deref()
                    && !out.contains(&source)
                {
                    out.push(source);
                }
            });
        }
        out
    }

    /// This scene and the scenes its visible items show, and so on, each
    /// once, the deepest first (the order to draw them in).
    pub fn shown_scenes<'a>(&'a self, scenes: &'a [Scene]) -> Vec<&'a Scene> {
        fn add<'a>(scene: &'a Scene, scenes: &'a [Scene], out: &mut Vec<&'a Scene>, depth: u32) {
            if out.iter().any(|s| s.id == scene.id) || depth > MAX_NESTING {
                return;
            }
            visit(&scene.items, false, &mut |item| {
                if let Some(nested) = item.scene.as_deref()
                    && let Some(nested) = scenes.iter().find(|s| s.id == nested)
                {
                    add(nested, scenes, out, depth + 1);
                }
            });
            if !out.iter().any(|s| s.id == scene.id) {
                out.push(scene);
            }
        }
        let mut out = Vec::new();
        add(self, scenes, &mut out, 0);
        out
    }

    /// Every video source any of its items names, visible or not (not those
    /// of scenes it shows).
    pub fn all_sources(&self) -> Vec<&str> {
        fn walk<'a>(items: &'a [SceneItem], out: &mut Vec<&'a str>) {
            for item in items {
                if let Some(source) = item.source.as_deref()
                    && !out.contains(&source)
                {
                    out.push(source);
                }
                walk(&item.children, out);
            }
        }
        let mut out = Vec::new();
        walk(&self.items, &mut out);
        out
    }
}

/// Something drawn in a scene: a video source, a scene, content (text or an
/// image made from settings), or (with none of them) a box that is just
/// its style, and a group for the items in `children`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SceneItem {
    /// Identifies the item for patches and transitions; its source's id if
    /// absent. Unique within its scene.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The id of the video source it shows; absent for a box or group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Or the id of a scene it shows (OBS's nested scenes): the scene drawn
    /// at the canvas size, transparent where it shows nothing, and scaled
    /// to fill the rect like a source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scene: Option<String>,
    /// Or text or an image the engine makes from these settings, without
    /// a source: `{ kind = "text", text = "Hi", size = 48 }` (the text
    /// source's settings; `data = "key"` shows a data key's value) or
    /// `{ kind = "image", path = "logo.png" }`. Give it an `id` to patch it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Content>,
    /// Where it is drawn, in canvas pixels (relative to its parent's rect
    /// for children); a source is scaled to fill it. Give this or `layout`:
    /// a rect is the same as `layout = { position = "absolute", left = x,
    /// top = y, width, height }`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rect: Option<Rect>,
    /// Or where layout places it in its parent (a scene, or an item whose
    /// `layout` arranges its children), and how its own children are laid
    /// out: CSS's flexbox, grid, block and absolute positioning. Its size
    /// can come from what it shows: a source's picture size, or the canvas
    /// for a scene.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<Layout>,
    /// Pixels cut from each edge of the source before it is scaled.
    #[serde(default, skip_serializing_if = "Crop::is_none")]
    pub crop: Crop,
    /// How the picture fills the rect. New API items use `none` (native
    /// size times transform scale). Omission in old files retains legacy
    /// stretch; explicitly chosen `stretch` is always saved.
    #[serde(default, skip_serializing_if = "Fit::is_legacy")]
    pub fit: Fit,
    /// Where the picture sits in the rect when it doesn't fill it, and
    /// which part shows when it overflows (OBS's alignment in the bounding
    /// box). Centred by default.
    #[serde(default, skip_serializing_if = "Align::is_center")]
    pub align: Align,
    /// How the picture is resampled to the size it is drawn at: `point`,
    /// `bilinear` (the default), `bicubic`, `lanczos` or `area` (OBS's
    /// scale filtering).
    #[serde(default = "bilinear", skip_serializing_if = "is_bilinear")]
    pub scale_filter: ScaleFilter,
    /// How the item combines with what is drawn below it (OBS's blending
    /// modes): `normal` (the default), `additive`, `subtract`, `screen`,
    /// `multiply`, `lighten` or `darken`. Its children use their own.
    #[serde(default, skip_serializing_if = "Blend::is_normal")]
    pub blend: Blend,
    #[serde(default = "default_true")]
    pub visible: bool,
    /// How it appears when it becomes visible (OBS's show transition): a
    /// `fade`, or a `slide` in from a side; at once if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_transition: Option<ItemTransition>,
    /// How it disappears when hidden (OBS's hide transition).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hide_transition: Option<ItemTransition>,
    /// Keyframe sets it plays (CSS's `animation`), each on its clock:
    /// `[{ keyframes = "slide-in", duration_ms = 400 }]`. A later one wins
    /// over an earlier one on the same property.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub animations: Vec<Animation>,
    /// Locked in the app's canvas editor (OBS's lock): it can be selected
    /// there but not moved or resized. Patches change it as usual; the
    /// engine ignores it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub locked: bool,
    #[serde(default, skip_serializing_if = "Style::is_default")]
    pub style: Style,
    /// Items drawn on top of this one, back to front, placed relative to
    /// its rect and moved, scaled, rotated and faded with it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<SceneItem>,
}

impl SceneItem {
    /// What patches and transitions call it: its id, else its source or
    /// scene.
    pub fn key(&self) -> Option<&str> {
        self.id
            .as_deref()
            .or(self.source.as_deref())
            .or(self.scene.as_deref())
    }
}

/// How an item's picture (what is left after its crop) fills the item's
/// rect, like CSS `object-fit`; OBS calls these bounding box types. The
/// rect clips the picture, and its background shows where the picture
/// doesn't reach.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Fit {
    /// An omitted fit in a legacy setup. Kept distinct from an explicitly
    /// chosen stretch so native-sized legacy boxes can safely follow edits.
    #[default]
    #[serde(skip)]
    Legacy,
    /// Stretched to the rect, whatever its shape (OBS: stretch to bounds).
    Stretch,
    /// As large as fits inside the rect, keeping its shape (OBS: scale to
    /// inner bounds).
    Contain,
    /// Covering the rect, keeping its shape; what overflows is cut off
    /// (OBS: scale to outer bounds, cropped to the bounding box).
    Cover,
    /// As wide as the rect, keeping its shape (OBS: scale to width of
    /// bounds).
    Width,
    /// As tall as the rect, keeping its shape (OBS: scale to height of
    /// bounds).
    Height,
    /// Like `contain`, but never larger than its own size (OBS: maximum
    /// size only).
    ScaleDown,
    /// At its own size in pixels, before the item's transform (OBS: no
    /// bounds).
    None,
}

impl Fit {
    pub fn is_legacy(&self) -> bool {
        *self == Self::Legacy
    }

    pub fn is_stretch(&self) -> bool {
        matches!(self, Self::Legacy | Self::Stretch)
    }
}

fn bilinear() -> ScaleFilter {
    ScaleFilter::Bilinear
}

fn is_bilinear(filter: &ScaleFilter) -> bool {
    *filter == ScaleFilter::Bilinear
}

/// Where a picture sits in its item's rect.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Align {
    #[default]
    Center,
    Top,
    Bottom,
    Left,
    Right,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl Align {
    pub fn is_center(&self) -> bool {
        *self == Self::Center
    }

    /// How far across and down the rect the picture sits: 0 (left, top),
    /// 0.5 (centre) or 1 (right, bottom) of the room left over.
    pub fn factors(self) -> [f32; 2] {
        match self {
            Self::Center => [0.5, 0.5],
            Self::Top => [0.5, 0.0],
            Self::Bottom => [0.5, 1.0],
            Self::Left => [0.0, 0.5],
            Self::Right => [1.0, 0.5],
            Self::TopLeft => [0.0, 0.0],
            Self::TopRight => [1.0, 0.0],
            Self::BottomLeft => [0.0, 1.0],
            Self::BottomRight => [1.0, 1.0],
        }
    }
}

/// How an item's colours combine with what is already drawn below it,
/// channel by channel, in encoded (not linear) values like OBS's "sRGB off"
/// blending method. The item's opacity fades the effect.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Blend {
    /// Drawn over what is below, by its alpha.
    #[default]
    Normal,
    /// Added to what is below: lights, glows, flares on black.
    Additive,
    /// Taken away from what is below.
    Subtract,
    /// Brightens: one minus the product of the inverses, so black leaves
    /// what is below as it was.
    Screen,
    /// Darkens: the product, so white leaves what is below as it was.
    Multiply,
    /// The lighter of the two.
    Lighten,
    /// The darker of the two.
    Darken,
}

impl Blend {
    pub fn is_normal(&self) -> bool {
        *self == Self::Normal
    }
}

/// How deep scenes may show scenes that show scenes.
pub const MAX_NESTING: u32 = 8;

/// Call `f` on each visible item of `items` and their children (on every
/// item with `hidden`).
fn visit<'a>(items: &'a [SceneItem], hidden: bool, f: &mut impl FnMut(&'a SceneItem)) {
    for item in items.iter().filter(|item| hidden || item.visible) {
        f(item);
        visit(&item.children, hidden, f);
    }
}

/// How an item looks, in CSS terms.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Style {
    /// 0 (invisible) to 1 (opaque), applied to children too.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub opacity: f32,
    /// A colour behind the item, which a box is filled with: `#rgb`,
    /// `#rrggbb` or `#rrggbbaa`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<Color>,
    /// Corner radius in pixels: one for all corners, or [top left, top
    /// right, bottom right, bottom left].
    #[serde(default, skip_serializing_if = "Radius::is_zero")]
    pub radius: Radius,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub border: Option<Border>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow: Option<Shadow>,
    #[serde(default, skip_serializing_if = "Transform::is_identity")]
    pub transform: Transform,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            opacity: 1.0,
            background: None,
            radius: Radius::default(),
            border: None,
            shadow: None,
            transform: Transform::default(),
        }
    }
}

impl Style {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

fn negative_or_nan(value: f32) -> bool {
    value.is_nan() || value < 0.0
}

fn one() -> f32 {
    1.0
}

fn is_one(value: &f32) -> bool {
    *value == 1.0
}

/// A colour as `#rgb`, `#rrggbb` or `#rrggbbaa`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Color(pub String);

impl Color {
    /// Red, green, blue and alpha from 0 to 1, if it is a valid colour.
    pub fn rgba(&self) -> Option<[f32; 4]> {
        let hex = self.0.strip_prefix('#')?;
        let digits: Vec<u8> = hex
            .chars()
            .map(|c| c.to_digit(16).map(|d| d as u8))
            .collect::<Option<_>>()?;
        let channels: Vec<u8> = match digits.len() {
            3 | 4 => digits.iter().map(|d| d * 17).collect(),
            6 | 8 => digits.chunks(2).map(|p| p[0] * 16 + p[1]).collect(),
            _ => return None,
        };
        let value = |i: usize| channels.get(i).map_or(1.0, |c| f32::from(*c) / 255.0);
        Some([value(0), value(1), value(2), value(3)])
    }
}

/// Corner radii in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    untagged,
    expecting = "a radius in pixels, or four: [top left, top right, bottom right, bottom left]"
)]
pub enum Radius {
    All(f32),
    /// Top left, top right, bottom right, bottom left.
    Each([f32; 4]),
}

impl Default for Radius {
    fn default() -> Self {
        Self::All(0.0)
    }
}

impl Radius {
    pub fn corners(&self) -> [f32; 4] {
        match *self {
            Self::All(r) => [r; 4],
            Self::Each(r) => r,
        }
    }

    pub fn is_zero(&self) -> bool {
        self.corners() == [0.0; 4]
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Border {
    /// Width in pixels, drawn inside the rect.
    pub width: f32,
    pub color: Color,
}

/// A drop shadow under the item, following its corners.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Shadow {
    /// Offset in pixels.
    #[serde(default)]
    pub x: f32,
    #[serde(default)]
    pub y: f32,
    /// How far it blurs, in pixels.
    #[serde(default)]
    pub blur: f32,
    pub color: Color,
}

/// Moves, scales and rotates an item around its centre.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Transform {
    /// [x, y] in pixels.
    #[serde(default)]
    pub translate: [f32; 2],
    #[serde(default = "one")]
    pub scale: f32,
    /// Degrees, clockwise.
    #[serde(default)]
    pub rotate: f32,
}

impl Default for Transform {
    fn default() -> Self {
        Self {
            translate: [0.0; 2],
            scale: 1.0,
            rotate: 0.0,
        }
    }
}

impl Transform {
    pub fn is_identity(&self) -> bool {
        *self == Self::default()
    }
}

/// Pixels cut from the edges of a source.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Crop {
    #[serde(default)]
    pub left: u32,
    #[serde(default)]
    pub top: u32,
    #[serde(default)]
    pub right: u32,
    #[serde(default)]
    pub bottom: u32,
}

impl Crop {
    pub fn is_none(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// The source name of the program recording in manifests and reports;
/// reserved, so no source can use it.
pub const PROGRAM: &str = "program";

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Recording {
    /// Folder recordings are written to. `~` expands to the home folder.
    pub folder: String,
    /// The composited program recording.
    pub program: ProgramRecording,
    /// Separate recordings of individual sources.
    #[serde(default)]
    pub iso: IsoRecording,
    /// Free space, in GB, a recording never uses: it won't start without
    /// room for its estimated size plus this, and it stops cleanly, finishing
    /// every file, when free space falls below it (or below about 10 s of
    /// its files, at least 64 MB, if that is more).
    #[serde(default = "default_keep_free_gb")]
    pub keep_free_gb: f64,
    /// The replay buffer: how much of the program it keeps.
    #[serde(default, skip_serializing_if = "replay::ReplaySettings::is_default")]
    pub replay: replay::ReplaySettings,
}

fn default_keep_free_gb() -> f64 {
    1.0
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProgramRecording {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// File name without extension. Tokens: {date}, {time}.
    #[serde(default = "default_program_filename")]
    pub filename: String,
    pub encoder: Encoder,
    /// Audio sources mixed into the program recording.
    #[serde(default)]
    pub audio: Vec<String>,
    /// The program file's size, if not the canvas's: the encoder scales
    /// the canvas down to it (a different shape is letterboxed), like OBS's
    /// Output (Scaled) Resolution. ISO files keep their sources' sizes.
    /// Each side is an even number from 16 up to the canvas's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

impl ProgramRecording {
    /// The program file's size on `canvas`: `width`×`height` if set, else
    /// the canvas's.
    pub fn size(&self, canvas: &Canvas) -> (u32, u32) {
        match (self.width, self.height) {
            (Some(width), Some(height)) => (width, height),
            _ => (canvas.width, canvas.height),
        }
    }
}

impl ProgramRecording {
    /// A generous estimate of the program file's data rate on `canvas`, in
    /// bytes per second (`encoder_bytes_per_second` with the stereo mix,
    /// if it has one): what the free-space check plans for. 0 when the
    /// program isn't recorded.
    pub fn bytes_per_second(&self, canvas: &Canvas) -> f64 {
        if !self.enabled {
            return 0.0;
        }
        let (width, height) = self.size(canvas);
        let channels = if self.audio.is_empty() { 0 } else { 2 };
        encoder_bytes_per_second(&self.encoder, width, height, canvas.fps, channels)
    }
}

/// AAC's bit rate per channel, as the files are written.
const AUDIO_BITS_PER_CHANNEL: f64 = 128_000.0;

/// A generous estimate of a file's data rate, in bytes per second: video
/// of `width`×`height` at `fps` with `encoder`, and `audio_channels` of
/// AAC. Real files are usually far smaller (a still screen compresses to
/// almost nothing); this is for deciding whether a recording fits, and for
/// saying how fast one fills a disk.
pub fn encoder_bytes_per_second(
    encoder: &Encoder,
    width: u32,
    height: u32,
    fps: u32,
    audio_channels: u32,
) -> f64 {
    let pixels_per_second = width as f64 * height as f64 * fps as f64;
    // Apple's published ProRes rates for 1920×1080 at 29.97 fps, scaled
    // by pixel rate.
    let prores = |mbps: f64| mbps * 1e6 * pixels_per_second / (1920.0 * 1080.0 * 29.97);
    // Bits per pixel for HEVC by quality: about 0.12 at quality 70, the
    // worst case for busy camera footage.
    let q = encoder.quality.value() as f64 / 100.0;
    let hevc = pixels_per_second * (0.02 + 0.3 * q * q * q);
    let video = match encoder.codec {
        Codec::Hevc => hevc,
        Codec::H264 => hevc * 1.5,
        Codec::Prores422Proxy => prores(45.0),
        Codec::Prores422Lt => prores(102.0),
        Codec::Prores422 => prores(147.0),
        Codec::Prores422Hq => prores(220.0),
        Codec::Prores4444 => prores(330.0),
    };
    (video + audio_channels as f64 * AUDIO_BITS_PER_CHANNEL) / 8.0
}

fn default_program_filename() -> String {
    "{date} {time}".into()
}

/// ISO ("isolated") recording: each listed source is written to its own file
/// at its native resolution, on the same clock as the program recording.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IsoRecording {
    #[serde(default)]
    pub enabled: bool,
    /// Video sources to record separately.
    #[serde(default)]
    pub sources: Vec<String>,
    /// File name without extension. Tokens: {SOURCE} (upper-case id),
    /// {source}, {date}, {time}.
    #[serde(default = "default_iso_filename")]
    pub filename: String,
    #[serde(default)]
    pub encoder: Encoder,
    /// Audio sources written into every ISO file without a list of its
    /// own in `source_audio`. Omitted: each ISO uses its own source's
    /// sound. An explicit list (including []) preserves the chosen shared
    /// routing. A source's audio.iso can override its own sound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<Vec<String>>,
    /// Audio sources for one ISO file, by its video source's id, instead
    /// of `audio`: `{ camera = ["mic"], screen = ["desktop"] }`. An empty
    /// list records that file without audio. Sources not listed follow
    /// `audio`; a source's audio.iso still adds or drops its own sound.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub source_audio: BTreeMap<String, Vec<String>>,
    /// Record sources with filters as filtered (as the program shows them)
    /// rather than as captured, and audio with its audio filters (all but
    /// sidechain ducking). Keyed-out areas come out black, since the files
    /// have no transparency.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub filtered: bool,
}

impl Default for IsoRecording {
    fn default() -> Self {
        Self {
            enabled: false,
            sources: Vec::new(),
            filename: default_iso_filename(),
            encoder: Encoder::default(),
            audio: None,
            source_audio: BTreeMap::new(),
            filtered: false,
        }
    }
}

impl IsoRecording {
    /// Effective tracks for one file. `has_audio` comes from the opened
    /// source, since a media file or browser can have no audio stream.
    pub fn audio_for(&self, source: &Source, has_audio: bool) -> Vec<String> {
        let listed = self.source_audio.get(&source.id).or(self.audio.as_ref());
        let mut audio = listed.cloned().unwrap_or_default();
        let own = source.audio.iso.unwrap_or(listed.is_none());
        if !own && source.audio.iso == Some(false) {
            audio.retain(|id| id != &source.id);
        }
        if own && has_audio && !audio.contains(&source.id) {
            audio.push(source.id.clone());
        }
        audio
    }

    /// The audio sources the ISO files list (`audio`, and `source_audio`
    /// for the ISO sources), with repeats; a file's own sound isn't listed.
    pub fn listed_audio(&self) -> impl Iterator<Item = &String> {
        self.audio.iter().flatten().chain(
            self.sources
                .iter()
                .filter_map(|id| self.source_audio.get(id))
                .flatten(),
        )
    }

    /// The setup field listing audio source `id` for the ISO files, for
    /// messages: `recording.iso.audio` or `recording.iso.source_audio.<iso>`.
    pub fn audio_field(&self, id: &str) -> Option<String> {
        if self.audio.iter().flatten().any(|a| a == id) {
            return Some("recording.iso.audio".into());
        }
        self.sources.iter().find_map(|iso| {
            self.source_audio
                .get(iso)
                .is_some_and(|list| list.iter().any(|a| a == id))
                .then(|| format!("recording.iso.source_audio.{iso}"))
        })
    }
}

fn default_iso_filename() -> String {
    "{SOURCE}-{date} {time}".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(remote = "Self", deny_unknown_fields)]
pub struct Encoder {
    pub codec: Codec,
    /// A named quality preset, or the original 1–100 constant quality.
    /// Ignored by ProRes, which is constant quality.
    #[serde(default = "default_quality")]
    pub quality: EncoderQuality,
    #[serde(default)]
    pub container: Container,
    /// 8 or 10. 10-bit HEVC uses the Main 10 profile and captures displays
    /// in 10 bits, which avoids banding in gradients. ProRes is always
    /// 10-bit; H.264 can only be 8-bit. The incredible HEVC preset promotes
    /// 8 to 10, including the display capture format.
    #[serde(default = "default_bit_depth")]
    pub bit_depth: u8,
}

impl Serialize for Encoder {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        Encoder::serialize(self, serializer)
    }
}

impl<'de> Deserialize<'de> for Encoder {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut encoder = Encoder::deserialize(deserializer)?;
        // Resolve before sources open, so capture and encoding agree. Keep
        // invalid depths intact for field-path validation to report them.
        encoder.bit_depth = encoder.effective_bit_depth();
        Ok(encoder)
    }
}

impl Encoder {
    pub fn effective_bit_depth(&self) -> u8 {
        if self.codec == Codec::Hevc
            && self.quality == EncoderQuality::Preset(QualityPreset::Incredible)
            && self.bit_depth == 8
        {
            10
        } else {
            self.bit_depth
        }
    }
}

/// Numbers stay numbers and preset names stay names in setup files and API
/// replies. See docs/recording.md for the measured preset ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    untagged,
    expecting = "an integer from 1–100 or one of: low, medium, high, very_high, incredible"
)]
pub enum EncoderQuality {
    Preset(#[serde(deserialize_with = "quality_preset_name")] QualityPreset),
    Value(#[schemars(range(min = 1, max = 100))] u8),
}

fn quality_preset_name<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<QualityPreset, D::Error> {
    // Unit enums also accept {"high": null}; this field is strictly a
    // string or a number, matching its schema and the settings contract.
    let name = String::deserialize(deserializer)?;
    QualityPreset::deserialize(serde::de::value::StrDeserializer::<D::Error>::new(&name))
}

impl EncoderQuality {
    /// VideoToolbox constant quality, on the original 1–100 scale.
    pub fn value(self) -> u8 {
        match self {
            Self::Value(value) => value,
            Self::Preset(preset) => match preset {
                QualityPreset::Low => 30,
                QualityPreset::Medium => 50,
                QualityPreset::High => 70,
                QualityPreset::VeryHigh => 85,
                QualityPreset::Incredible => 100,
            },
        }
    }
}

impl From<u8> for EncoderQuality {
    fn from(value: u8) -> Self {
        Self::Value(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QualityPreset {
    Low,
    Medium,
    High,
    VeryHigh,
    Incredible,
}

impl Default for Encoder {
    fn default() -> Self {
        Self {
            codec: Codec::Hevc,
            quality: default_quality(),
            container: Container::default(),
            bit_depth: default_bit_depth(),
        }
    }
}

fn default_bit_depth() -> u8 {
    8
}

fn default_quality() -> EncoderQuality {
    EncoderQuality::Preset(QualityPreset::High)
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    H264,
    Hevc,
    Prores422Proxy,
    Prores422Lt,
    Prores422,
    Prores422Hq,
    Prores4444,
}

/// Recording encoder choices, shared by the API, schema and validation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EncoderOption {
    pub codec: Codec,
    pub label: String,
    pub containers: Vec<Container>,
}

impl Codec {
    pub const ALL: [Self; 7] = [
        Self::H264,
        Self::Hevc,
        Self::Prores422Proxy,
        Self::Prores422Lt,
        Self::Prores422,
        Self::Prores422Hq,
        Self::Prores4444,
    ];

    pub fn containers(self) -> &'static [Container] {
        match self {
            Self::H264 | Self::Hevc => &[Container::Mov, Container::Mp4],
            _ => &[Container::Mov],
        }
    }

    pub fn container_problem(self, container: Container) -> Option<String> {
        (!self.containers().contains(&container)).then(|| {
            let codec = serde_json::to_value(self).unwrap();
            let format = serde_json::to_value(container).unwrap();
            let valid = self
                .containers()
                .iter()
                .map(|c| {
                    serde_json::to_value(c)
                        .unwrap()
                        .as_str()
                        .unwrap()
                        .to_string()
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "{} cannot be recorded in {}; valid containers: {valid}; mp4 supports h264, hevc",
                codec.as_str().unwrap(),
                format.as_str().unwrap()
            )
        })
    }

    pub fn options() -> Vec<EncoderOption> {
        Self::ALL
            .into_iter()
            .map(|codec| EncoderOption {
                codec,
                label: match codec {
                    Self::H264 => "H.264",
                    Self::Hevc => "HEVC",
                    Self::Prores422Proxy => "ProRes 422 Proxy",
                    Self::Prores422Lt => "ProRes 422 LT",
                    Self::Prores422 => "ProRes 422",
                    Self::Prores422Hq => "ProRes 422 HQ",
                    Self::Prores4444 => "ProRes 4444",
                }
                .into(),
                containers: codec.containers().to_vec(),
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Container {
    #[default]
    Mov,
    Mp4,
}

/// One problem found in a setup. `path` points at the offending field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Issue {
    pub path: String,
    pub message: String,
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error(transparent)]
    Toml(#[from] toml::de::Error),
    /// `path` names where in the document, like `canvas.fsp` (empty for
    /// the document as a whole).
    #[error("{}{message}", if .path.is_empty() { String::new() } else { format!("{}: ", .path) })]
    Json { path: String, message: String },
}

impl LoadError {
    /// Where in a JSON document the problem is, like `canvas.fsp`; empty
    /// if unknown (TOML errors give a line and column instead).
    pub fn path(&self) -> &str {
        match self {
            LoadError::Json { path, .. } => path,
            LoadError::Toml(_) => "",
        }
    }

    /// The problem without its path.
    pub fn message(&self) -> String {
        match self {
            LoadError::Json { message, .. } => message.clone(),
            LoadError::Toml(error) => error.to_string(),
        }
    }

    fn json(err: serde_path_to_error::Error<serde_json::Error>) -> Self {
        located::Located::new(err.path(), &err.inner().to_string()).into()
    }
}

impl From<located::Located> for LoadError {
    fn from(located::Located { path, message }: located::Located) -> Self {
        LoadError::Json { path, message }
    }
}

impl Setup {
    /// Parse a setup from TOML.
    pub fn from_toml(text: &str) -> Result<Self, LoadError> {
        Ok(toml::from_str(text)?)
    }

    /// Parse a setup from JSON; an error names the path of what's wrong.
    pub fn from_json(text: &str) -> Result<Self, LoadError> {
        let mut deserializer = serde_json::Deserializer::from_str(text);
        let setup: serde_json::Value =
            serde_path_to_error::deserialize(&mut deserializer).map_err(LoadError::json)?;
        deserializer.end().map_err(|error| LoadError::Json {
            path: String::new(),
            message: error.to_string(),
        })?;
        Self::from_value(setup)
    }

    /// A setup from parsed JSON (as the API and MCP send it); an error
    /// names the path of what's wrong.
    pub fn from_value(value: serde_json::Value) -> Result<Self, LoadError> {
        located::from_value(&value).map_err(|mut err| {
            located::identify_source(&mut err, &value);
            err.into()
        })
    }

    /// The setup as TOML, as `isocast get` and setup files write it.
    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    /// The JSON Schema of the setup document.
    pub fn json_schema() -> schemars::Schema {
        let mut schema = schemars::schema_for!(Setup);
        schema.insert(
            "x-encoder-options".into(),
            serde_json::to_value(Codec::options()).unwrap(),
        );
        schema
    }

    pub fn source(&self, id: &str) -> Option<&Source> {
        self.sources.iter().find(|s| s.id == id)
    }

    /// Whether any source is heard through the monitoring device.
    pub fn monitors(&self) -> bool {
        self.sources
            .iter()
            .any(|s| s.audio.monitor != Monitoring::Off)
    }

    /// Make relative file paths in sources (images, slide shows, text),
    /// content items and transitions relative to `base`, the folder of the
    /// setup file they came from.
    pub fn resolve_paths(&mut self, base: &std::path::Path) {
        let resolve = |path: &mut String| {
            if !path.is_empty() && !path.starts_with('/') && !path.starts_with('~') {
                *path = base.join(&*path).to_string_lossy().into_owned();
            }
        };
        fn contents(items: &mut [SceneItem], resolve: &impl Fn(&mut String)) {
            for item in items {
                match item.content.as_mut().map(|content| &mut content.kind) {
                    Some(ContentKind::Image(image)) => resolve(&mut image.path),
                    Some(ContentKind::Text(text)) => text.file.iter_mut().for_each(resolve),
                    None => {}
                }
                contents(&mut item.children, resolve);
            }
        }
        for scene in &mut self.scenes {
            contents(&mut scene.items, &resolve);
        }
        for source in &mut self.sources {
            match &mut source.kind {
                SourceKind::Image(image) => resolve(&mut image.path),
                SourceKind::Slideshow(slideshow) => slideshow.files.iter_mut().for_each(resolve),
                SourceKind::Text(text) => text.file.iter_mut().for_each(resolve),
                SourceKind::Media(media) => media.files.iter_mut().for_each(resolve),
                SourceKind::Browser(browser) => browser.file.iter_mut().for_each(resolve),
                _ => {}
            }
            for filter in &mut source.filters {
                match &mut filter.kind {
                    FilterKind::ImageMask(mask) => resolve(&mut mask.path),
                    FilterKind::ApplyLut(lut) => resolve(&mut lut.path),
                    _ => {}
                }
            }
        }
        let scenes = self.scenes.iter_mut().filter_map(|s| s.transition.as_mut());
        for transition in std::iter::once(&mut self.transition).chain(scenes) {
            transition.files_mut().into_iter().for_each(resolve);
        }
    }

    /// A setup for recording without a setup file. Each spec becomes a
    /// source; every video source is recorded to its own file in the current
    /// folder, with every audio source as a track. See [`QUICK_SOURCES`].
    pub fn quick(specs: &[String]) -> Result<Self, String> {
        let mut sources: Vec<Source> = Vec::new();
        for spec in specs {
            let (name, arg) = match spec.split_once(':') {
                Some((name, arg)) => (name, Some(arg)),
                None => (spec.as_str(), None),
            };
            let kind = match (name, arg) {
                ("test-pattern", None) => SourceKind::TestPattern(TestPatternSource::default()),
                ("test-pattern", Some(arg)) => parse_pattern(arg).ok_or_else(|| {
                    format!("{spec:?}: expected test-pattern:WIDTHxHEIGHT or test-pattern:WIDTHxHEIGHT@FPS")
                })?,
                ("screen", None) => SourceKind::Display(DisplaySource {
                    display: "main".into(),
                    cursor: true,
                }),
                ("display", arg) => SourceKind::Display(DisplaySource {
                    display: arg.unwrap_or("main").to_string(),
                    cursor: true,
                }),
                ("window", Some(app)) => SourceKind::Window(WindowSource {
                    app: app.to_string(),
                    title: None,
                    cursor: true,
                }),
                ("app-audio", Some(app)) => SourceKind::ApplicationAudio(ApplicationAudioSource {
                    app: app.to_string(),
                }),
                ("mic", arg) => SourceKind::Microphone(MicrophoneSource {
                    device: arg.unwrap_or("default").to_string(),
                }),
                ("system-audio", None) => SourceKind::SystemAudio(SystemAudioSource::default()),
                ("system-audio", Some(arg)) => match arg.strip_prefix("except=") {
                    Some(apps) if !apps.trim().is_empty() => {
                        SourceKind::SystemAudio(SystemAudioSource {
                            exclude: apps.split(',').map(|a| a.trim().to_string()).collect(),
                        })
                    }
                    _ => {
                        return Err(format!(
                            "{spec:?}: expected system-audio or system-audio:except=APP[,APP]"
                        ));
                    }
                },
                ("camera", Some(device)) => SourceKind::Camera(CameraSource {
                    device: device.to_string(),
                    width: None,
                    height: None,
                    fps: None,
                }),
                ("tone", None) => SourceKind::Tone(ToneSource::default()),
                ("tone", Some(arg)) => SourceKind::Tone(ToneSource {
                    frequency: arg
                        .parse()
                        .map_err(|_| format!("{spec:?}: expected tone:HZ, like tone:440"))?,
                }),
                _ => {
                    return Err(format!(
                        "unknown source {spec:?}; valid: {}",
                        QUICK_SOURCES.join(", ")
                    ));
                }
            };
            let count = sources.iter().filter(|s| s.id.starts_with(name)).count();
            let id = match count {
                0 => name.to_string(),
                n => format!("{name}-{}", n + 1),
            };
            sources.push(Source {
                id,
                name: None,
                kind,
                audio: AudioSettings::default(),
                filters: Vec::new(),
                audio_filters: Vec::new(),
                unknown: UnknownFields::new(),
            });
        }
        let ids = |filter: fn(&SourceKind) -> bool| -> Vec<String> {
            sources
                .iter()
                .filter(|s| filter(&s.kind))
                .map(|s| s.id.clone())
                .collect()
        };
        let (video, audio) = (ids(SourceKind::has_video), ids(SourceKind::has_audio));
        let canvas = match sources.iter().find(|s| s.kind.has_video()).map(|s| &s.kind) {
            // A canvas size must be even and at least 16.
            Some(SourceKind::TestPattern(pattern)) => Canvas {
                width: (pattern.width & !1).max(16),
                height: (pattern.height & !1).max(16),
                fps: pattern.fps,
            },
            _ => Canvas {
                width: source::pattern_width(),
                height: source::pattern_height(),
                fps: source::pattern_fps(),
            },
        };
        Ok(Setup {
            version: SETUP_VERSION,
            name: "Quick recording".into(),
            canvas,
            sources,
            scenes: Vec::new(),
            keyframes: Vec::new(),
            transition: Transition::default(),
            audio: AudioSetup::default(),
            recording: Recording {
                folder: ".".into(),
                program: ProgramRecording {
                    enabled: false,
                    filename: default_program_filename(),
                    encoder: Encoder::default(),
                    audio: Vec::new(),
                    width: None,
                    height: None,
                },
                iso: IsoRecording {
                    enabled: true,
                    sources: video,
                    audio: Some(audio),
                    ..IsoRecording::default()
                },
                keep_free_gb: default_keep_free_gb(),
                replay: Default::default(),
            },
            stream: None,
            hotkeys: Default::default(),
            virtual_camera: Default::default(),
            developer: Default::default(),
        })
    }

    /// Check references and values the type system cannot express. An empty
    /// list means the setup is valid.
    pub fn validate(&self) -> Vec<Issue> {
        let mut issues = Issues::default();

        if self.version != SETUP_VERSION {
            issues.add(
                "version",
                format!(
                    "unsupported version {}; expected {SETUP_VERSION}",
                    self.version
                ),
            );
        }
        // The GPU makes canvases up to 8192 on a side, and 4:2:0 video
        // needs even sizes (an odd canvas would be recorded a pixel smaller).
        for (field, value) in [("width", self.canvas.width), ("height", self.canvas.height)] {
            if !(16..=8192).contains(&value) || value % 2 != 0 {
                let nearest = (value & !1).clamp(16, 8192);
                issues.add(
                    format!("canvas.{field}"),
                    format!("{value} must be an even number from 16 to 8192 (nearest: {nearest})"),
                );
            }
        }
        if !(1..=240).contains(&self.canvas.fps) {
            issues.add("canvas.fps", "fps must be between 1 and 240");
        }

        let mut source_ids = HashSet::new();
        for (i, source) in self.sources.iter().enumerate() {
            issues.check_id(format!("sources[{i}].id"), &source.id, &mut source_ids);
            let audio = &source.audio;
            if !audio.is_default() && !source.kind.has_audio() {
                issues.add(
                    format!("sources[{i}].audio"),
                    format!(
                        "only audio sources (microphone, system_audio, tone) have audio \
                         settings, and this is a {} source",
                        source.kind.label()
                    ),
                );
            }
            if !(-60.0..=24.0).contains(&audio.gain_db) {
                issues.add(
                    format!("sources[{i}].audio.gain_db"),
                    format!("{} is outside -60 to 24 dB", audio.gain_db),
                );
            }
            if !(-2000..=2000).contains(&audio.offset_ms) {
                issues.add(
                    format!("sources[{i}].audio.offset_ms"),
                    format!("{} is outside -2000 to 2000 ms", audio.offset_ms),
                );
            }
            if audio.monitor != Monitoring::Off && matches!(source.kind, SourceKind::SystemAudio(_))
            {
                issues.add(
                    format!("sources[{i}].audio.monitor"),
                    "system audio is everything this Mac plays, monitoring included, so \
                     monitoring it would feed back; use \"off\"",
                );
            }
            if !(-1.0..=1.0).contains(&audio.balance) {
                issues.add(
                    format!("sources[{i}].audio.balance"),
                    format!(
                        "{} is outside -1 (left) to 1 (right); 0 is the centre",
                        audio.balance
                    ),
                );
            }
            if source.id == PROGRAM {
                issues.add(
                    format!("sources[{i}].id"),
                    "\"program\" is reserved for the program recording; pick another id",
                );
            }
            source.kind.check(&format!("sources[{i}]"), &mut issues);
            fields::report(
                &mut issues,
                &format!("sources[{i}]"),
                &source.unknown,
                &format!("a {} source", source.kind.label()),
                source.kind.fields(),
                SOURCE_FIELDS,
            );
            if !source.filters.is_empty() && !source.kind.has_video() {
                issues.add(
                    format!("sources[{i}].filters"),
                    format!(
                        "filters change pictures, and {:?} has none; remove them",
                        source.id
                    ),
                );
            }
            let mut keys = HashSet::new();
            for (j, filter) in source.filters.iter().enumerate() {
                let path = format!("sources[{i}].filters[{j}]");
                if let Some(id) = &filter.id
                    && !is_valid_id(id)
                {
                    issues.add(
                        format!("{path}.id"),
                        format!("{id:?} must use lowercase letters, digits and dashes"),
                    );
                }
                if !keys.insert(filter.key()) {
                    issues.add(
                        format!("{path}.id"),
                        format!(
                            "another filter of this source is already called {:?}; give this \
                             one its own id",
                            filter.key()
                        ),
                    );
                }
                filter.kind.check(&path, &mut issues);
                fields::report(
                    &mut issues,
                    &path,
                    &filter.unknown,
                    &format!("a {} filter", filter.kind.label()),
                    filter.kind.fields(),
                    FILTER_FIELDS,
                );
            }
        }
        let mut scene_ids = HashSet::new();
        for (i, scene) in self.scenes.iter().enumerate() {
            issues.check_id(format!("scenes[{i}].id"), &scene.id, &mut scene_ids);
            if let Some(layout) = &scene.layout {
                layout::check(&format!("scenes[{i}].layout"), layout, true, &mut issues);
            }
        }

        let video = self.source_ids(SourceKind::has_video);
        let audio = self.source_ids(SourceKind::has_audio);
        for (i, source) in self.sources.iter().enumerate() {
            audio_filter::check_chain(&format!("sources[{i}]"), source, &audio, &mut issues);
        }

        animation::check_keyframes(&self.keyframes, &mut issues);
        let keyframes: Vec<&str> = self.keyframes.iter().map(|k| k.id.as_str()).collect();
        let scenes: Vec<&str> = self.scenes.iter().map(|s| s.id.as_str()).collect();
        for (i, scene) in self.scenes.iter().enumerate() {
            let mut keys = HashSet::new();
            let known = Known {
                video: &video,
                scenes: &scenes,
                keyframes: &keyframes,
            };
            issues.check_items(
                &format!("scenes[{i}].items"),
                &scene.items,
                &known,
                &mut keys,
            );
            if let Some(cycle) = self.scene_cycle(scene) {
                issues.add(
                    format!("scenes[{i}]"),
                    format!(
                        "scene {:?} would show itself: {}; remove one of those items",
                        scene.id,
                        cycle.join(" → ")
                    ),
                );
            } else if self.nesting_depth(scene, 0) > MAX_NESTING {
                issues.add(
                    format!("scenes[{i}]"),
                    format!("scenes nest more than {MAX_NESTING} deep"),
                );
            }
        }

        if self.audio.monitoring_device.trim().is_empty() {
            issues.add(
                "audio.monitoring_device",
                "give \"default\" (the system output) or an output device's id or name from \
                 `isocast devices`",
            );
        }
        self.transition.check("transition", &mut issues);
        if let Some(stream) = &self.stream {
            stream.check(&mut issues);
        }
        for issue in self.virtual_camera.issues(self) {
            issues.add(issue.path, issue.message);
        }
        for (i, scene) in self.scenes.iter().enumerate() {
            if let Some(transition) = &scene.transition {
                transition.check(&format!("scenes[{i}].transition"), &mut issues);
            }
        }
        let rec = &self.recording;
        if rec.program.enabled && self.scenes.is_empty() {
            issues.add(
                "recording.program.enabled",
                "the program recording shows the scene on air, but there are no scenes; add one \
                 or set enabled = false",
            );
        }
        for (path, ids) in [
            ("recording.program.audio", rec.program.audio.as_slice()),
            ("recording.iso.sources", &rec.iso.sources),
            (
                "recording.iso.audio",
                rec.iso.audio.as_deref().unwrap_or(&[]),
            ),
        ] {
            let mut seen = HashSet::new();
            for (i, id) in ids.iter().enumerate() {
                if !seen.insert(id) {
                    issues.add(format!("{path}[{i}]"), format!("{id:?} is listed twice"));
                }
            }
        }
        for (i, id) in rec.program.audio.iter().enumerate() {
            issues.check_ref(format!("recording.program.audio[{i}]"), id, &audio, "audio");
        }
        for (i, id) in rec.iso.sources.iter().enumerate() {
            issues.check_ref(format!("recording.iso.sources[{i}]"), id, &video, "video");
        }
        for (i, id) in rec.iso.audio.iter().flatten().enumerate() {
            issues.check_ref(format!("recording.iso.audio[{i}]"), id, &audio, "audio");
        }
        for (iso, ids) in &rec.iso.source_audio {
            let path = format!("recording.iso.source_audio.{iso}");
            issues.check_ref(path.clone(), iso, &video, "video");
            let mut seen = HashSet::new();
            for (i, id) in ids.iter().enumerate() {
                if !seen.insert(id) {
                    issues.add(format!("{path}[{i}]"), format!("{id:?} is listed twice"));
                }
                issues.check_ref(format!("{path}[{i}]"), id, &audio, "audio");
            }
        }
        if rec.iso.enabled
            && rec.iso.sources.len() > 1
            && !rec.iso.filename.contains("{source}")
            && !rec.iso.filename.contains("{SOURCE}")
        {
            issues.add(
                "recording.iso.filename",
                "must contain {SOURCE} or {source} when recording more than one source, \
                 or the files would get the same name",
            );
        }
        for (path, filename) in [
            ("recording.program.filename", &rec.program.filename),
            ("recording.iso.filename", &rec.iso.filename),
        ] {
            if filename.trim().is_empty() {
                issues.add(path, "must not be empty");
            } else if filename.contains(['/', '\0']) {
                issues.add(
                    path,
                    format!(
                        "{filename:?} must be a file name without \"/\"; the files go in \
                         recording.folder (or start's --folder)"
                    ),
                );
            }
        }
        match (rec.program.width, rec.program.height) {
            (None, None) => {}
            (Some(width), Some(height)) => {
                for (field, value, canvas) in [
                    ("width", width, self.canvas.width),
                    ("height", height, self.canvas.height),
                ] {
                    // Scaling up would only make the file bigger.
                    let most = canvas.max(16);
                    if !(16..=most).contains(&value) || value % 2 != 0 {
                        issues.add(
                            format!("recording.program.{field}"),
                            format!(
                                "{value} must be an even number from 16 to the canvas's {canvas}"
                            ),
                        );
                    }
                }
            }
            _ => issues.add(
                "recording.program.width",
                "give width and height together, or neither to record the canvas's size",
            ),
        }
        rec.replay.check(&mut issues);
        if !(rec.keep_free_gb >= 0.0 && rec.keep_free_gb <= 1e6) {
            issues.add(
                "recording.keep_free_gb",
                format!("{} must be a number of GB from 0 up", rec.keep_free_gb),
            );
        }
        for (path, encoder) in [
            ("recording.program.encoder", &rec.program.encoder),
            ("recording.iso.encoder", &rec.iso.encoder),
        ] {
            if let Some(problem) = encoder.codec.container_problem(encoder.container) {
                issues.add(format!("{path}.container"), problem);
            }
            if !(1..=100).contains(&encoder.quality.value()) {
                issues.add(
                    format!("{path}.quality"),
                    format!(
                        "{} is out of range; use 1–100 or low, medium, high, very_high, incredible",
                        encoder.quality.value()
                    ),
                );
            }
            match (encoder.bit_depth, encoder.codec) {
                (8, _) | (10, Codec::Hevc) => {}
                (10, Codec::H264) => issues.add(
                    format!("{path}.bit_depth"),
                    "h264 can only be 8-bit here; use hevc or a prores codec for 10-bit",
                ),
                (10, _) => {}
                (depth, _) => issues.add(
                    format!("{path}.bit_depth"),
                    format!("{depth} is not supported; use 8 or 10"),
                ),
            }
        }

        // Paths are positional while checking so every checker can share
        // the same helpers; only the public result names stable sources.
        for issue in &mut issues.0 {
            self.identify_source_issue(issue);
        }
        issues.0
    }

    /// Add a source's stable identity to a checker/plugin issue.
    pub fn identify_source_issue(&self, issue: &mut Issue) {
        let Some(rest) = issue.path.strip_prefix("sources[") else {
            return;
        };
        let Some((index, field)) = rest.split_once(']') else {
            return;
        };
        let Some(source) = index
            .parse::<usize>()
            .ok()
            .and_then(|i| self.sources.get(i))
        else {
            return;
        };
        if !is_valid_id(&source.id) {
            return;
        }
        issue.path = format!("sources.{}{field}", source.id);
        if let Some(name) = source
            .name
            .as_ref()
            .filter(|name| !name.is_empty() && **name != source.id)
        {
            issue.message = format!("{name:?} ({}): {}", source.id, issue.message);
        }
    }

    /// The chain of scenes by which `scene` shows itself, if it does.
    fn scene_cycle<'a>(&'a self, scene: &'a Scene) -> Option<Vec<&'a str>> {
        fn search<'a>(setup: &'a Setup, path: &mut Vec<&'a str>) -> bool {
            let current = setup.scenes.iter().find(|s| s.id == *path.last().unwrap());
            let mut nested = Vec::new();
            if let Some(current) = current {
                visit(&current.items, true, &mut |item| {
                    nested.extend(item.scene.as_deref())
                });
            }
            for next in nested {
                if next == path[0] {
                    path.push(next);
                    return true;
                }
                if !path.contains(&next) {
                    path.push(next);
                    if search(setup, path) {
                        return true;
                    }
                    path.pop();
                }
            }
            false
        }
        let mut path = vec![scene.id.as_str()];
        search(self, &mut path).then_some(path)
    }

    /// How deep scenes nest inside `scene` (which shows no cycle).
    fn nesting_depth(&self, scene: &Scene, depth: u32) -> u32 {
        if depth > MAX_NESTING {
            return depth;
        }
        let mut nested = Vec::new();
        visit(&scene.items, true, &mut |item| {
            nested.extend(item.scene.as_deref())
        });
        nested
            .into_iter()
            .filter_map(|id| self.scenes.iter().find(|s| s.id == id))
            .map(|s| self.nesting_depth(s, depth + 1))
            .max()
            .unwrap_or(depth)
    }

    fn source_ids(&self, filter: fn(&SourceKind) -> bool) -> Vec<&str> {
        self.sources
            .iter()
            .filter(|s| filter(&s.kind))
            .map(|s| s.id.as_str())
            .collect()
    }
}

#[derive(Default)]
pub(crate) struct Issues(Vec<Issue>);

/// The ids scene items may name: video sources, scenes and keyframe sets.
struct Known<'a> {
    video: &'a [&'a str],
    scenes: &'a [&'a str],
    keyframes: &'a [&'a str],
}

impl Issues {
    fn add(&mut self, path: impl Into<String>, message: impl Into<String>) {
        self.0.push(Issue {
            path: path.into(),
            message: message.into(),
        });
    }

    fn check_id<'a>(&mut self, path: String, id: &'a str, seen: &mut HashSet<&'a str>) {
        if !is_valid_id(id) {
            self.add(
                &path,
                format!("{id:?} must use lowercase letters, digits and dashes"),
            );
        }
        if !seen.insert(id) {
            self.add(path, format!("duplicate id {id:?}"));
        }
    }

    /// Check scene items and their children: sources, unique keys, rects,
    /// crops, styles and animations.
    fn check_items<'a>(
        &mut self,
        path: &str,
        items: &'a [SceneItem],
        known: &Known<'_>,
        keys: &mut HashSet<&'a str>,
    ) {
        let (video, scenes) = (known.video, known.scenes);
        for (j, item) in items.iter().enumerate() {
            let path = format!("{path}[{j}]");
            if let Some(id) = &item.id
                && !is_valid_id(id)
            {
                self.add(
                    format!("{path}.id"),
                    format!("{id:?} must use lowercase letters, digits and dashes"),
                );
            }
            if let Some(key) = item.key()
                && !keys.insert(key)
            {
                self.add(
                    format!("{path}.id"),
                    format!(
                        "another item in this scene is already called {key:?}; give this one \
                         its own id"
                    ),
                );
            }
            match (&item.source, &item.scene) {
                (Some(_), Some(_)) => self.add(
                    format!("{path}.scene"),
                    "an item shows a source or a scene, not both",
                ),
                (Some(source), None) => {
                    self.check_ref(format!("{path}.source"), source, video, "video")
                }
                (None, Some(scene)) => {
                    if !scenes.contains(&scene.as_str()) {
                        self.add(
                            format!("{path}.scene"),
                            format!("{scene:?} is not a scene; valid: {}", scenes.join(", ")),
                        );
                    }
                }
                (None, None) if !item.crop.is_none() && item.content.is_none() => self.add(
                    format!("{path}.crop"),
                    "only items showing a source, a scene or content can be cropped",
                ),
                (None, None) => {}
            }
            if let Some(content) = &item.content {
                if item.source.is_some() || item.scene.is_some() {
                    let other = if item.source.is_some() {
                        "a source"
                    } else {
                        "a scene"
                    };
                    self.add(
                        format!("{path}.content"),
                        format!(
                            "an item shows a source, a scene or content, and this one already \
                             shows {other}; give the content an item of its own"
                        ),
                    );
                }
                content.check(&format!("{path}.content"), self);
            }
            if item.source.is_none() && item.scene.is_none() && item.content.is_none() {
                for (field, set) in [
                    ("fit", !item.fit.is_stretch()),
                    ("align", !item.align.is_center()),
                    ("scale_filter", !is_bilinear(&item.scale_filter)),
                ] {
                    if set {
                        self.add(
                            format!("{path}.{field}"),
                            "only items showing a source, a scene or content have a picture \
                             to place; remove it",
                        );
                    }
                }
            }
            match (&item.rect, &item.layout) {
                (Some(_), Some(_)) => self.add(&path, "give rect or layout, not both"),
                (None, None) => self.add(
                    format!("{path}.rect"),
                    "an item needs a rect, or a layout to place it",
                ),
                (Some(rect), None) if rect.width == 0 || rect.height == 0 => self.add(
                    format!("{path}.rect"),
                    "width and height must be at least 1",
                ),
                (None, Some(layout)) => {
                    layout::check(&format!("{path}.layout"), layout, false, self)
                }
                (Some(_), None) => {}
            }
            for (field, transition) in [
                ("show_transition", &item.show_transition),
                ("hide_transition", &item.hide_transition),
            ] {
                if let Some(transition) = transition {
                    if !(1..=MAX_TRANSITION_MS).contains(&transition.duration_ms) {
                        self.add(
                            format!("{path}.{field}.duration_ms"),
                            "must be from 1 to 10000 (ms)",
                        );
                    }
                    transition
                        .easing
                        .check(&format!("{path}.{field}.easing"), self);
                }
            }
            self.check_style(&format!("{path}.style"), &item.style);
            animation::check_animations(
                &format!("{path}.animations"),
                &item.animations,
                known.keyframes,
                self,
            );
            self.check_items(&format!("{path}.children"), &item.children, known, keys);
        }
    }

    fn check_style(&mut self, path: &str, style: &Style) {
        if !(0.0..=1.0).contains(&style.opacity) {
            self.add(format!("{path}.opacity"), "must be between 0 and 1");
        }
        let mut colors = Vec::new();
        if let Some(color) = &style.background {
            colors.push(("background", color));
        }
        if let Some(border) = &style.border {
            colors.push(("border.color", &border.color));
            if negative_or_nan(border.width) {
                self.add(format!("{path}.border.width"), "must be 0 or more");
            }
        }
        if let Some(shadow) = &style.shadow {
            colors.push(("shadow.color", &shadow.color));
            if negative_or_nan(shadow.blur) {
                self.add(format!("{path}.shadow.blur"), "must be 0 or more");
            }
        }
        for (field, color) in colors {
            if color.rgba().is_none() {
                self.add(
                    format!("{path}.{field}"),
                    format!(
                        "{:?} is not a colour; use #rgb, #rrggbb or #rrggbbaa",
                        color.0
                    ),
                );
            }
        }
        if style.radius.corners().into_iter().any(negative_or_nan) {
            self.add(format!("{path}.radius"), "must be 0 or more");
        }
        if negative_or_nan(style.transform.scale) || style.transform.scale == 0.0 {
            self.add(format!("{path}.transform.scale"), "must be more than 0");
        }
    }

    fn check_ref(&mut self, path: String, id: &str, valid: &[&str], kind: &str) {
        if !valid.contains(&id) {
            let article = if kind.starts_with(['a', 'e', 'i', 'o', 'u']) {
                "an"
            } else {
                "a"
            };
            let message = match valid.is_empty() {
                true => format!(
                    "{id:?} is not {article} {kind} source; the setup has no {kind} sources, so \
                     add one under [[sources]] first"
                ),
                false => format!(
                    "{id:?} is not {article} {kind} source; valid: {}",
                    valid.join(", ")
                ),
            };
            self.add(path, message);
        }
    }
}

/// The source specs [`Setup::quick`] accepts.
pub const QUICK_SOURCES: &[&str] = &[
    "screen",
    "display[:NAME_OR_ID]",
    "camera:NAME_OR_ID",
    "mic[:NAME_OR_ID]",
    "system-audio[:except=APP[,APP]]",
    "window:APP",
    "app-audio:APP",
    "test-pattern[:WIDTHxHEIGHT[@FPS]]",
    "tone[:HZ]",
];

/// `WIDTHxHEIGHT` or `WIDTHxHEIGHT@FPS`.
fn parse_pattern(arg: &str) -> Option<SourceKind> {
    let (size, fps) = match arg.split_once('@') {
        Some((size, fps)) => (size, fps.parse().ok()?),
        None => (arg, source::pattern_fps()),
    };
    let (width, height) = size.split_once('x')?;
    Some(SourceKind::TestPattern(TestPatternSource {
        width: width.parse().ok()?,
        height: height.parse().ok()?,
        fps,
    }))
}

fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_errors_name_their_path() {
        let setup = serde_json::to_value(Setup::quick(&["test-pattern".into()]).unwrap()).unwrap();
        let path = |change: &dyn Fn(&mut serde_json::Value)| {
            let mut value = setup.clone();
            change(&mut value);
            let from_value = Setup::from_value(value.clone()).unwrap_err();
            let from_text = Setup::from_json(&value.to_string()).unwrap_err();
            assert_eq!(from_value.path(), from_text.path());
            assert!(from_text.to_string().starts_with(from_text.path()));
            from_value.path().to_string()
        };
        assert_eq!(path(&|v| v["canvas"]["fsp"] = 30.into()), "canvas.fsp");
        assert_eq!(
            path(&|v| v["recording"]["iso"]["sorces"] = serde_json::json!([])),
            "recording.iso.sorces"
        );
        assert_eq!(path(&|v| v["canvas"]["fps"] = "fast".into()), "canvas.fps");
        assert_eq!(
            path(&|v| v["sources"][0]["kind"] = "camra".into()),
            "sources.test-pattern.kind"
        );
        // Inside a source's kind, which serde reads through `flatten`.
        assert_eq!(
            path(&|v| v["sources"][0]["width"] = "wide".into()),
            "sources.test-pattern.width"
        );
        assert!(Setup::from_json(&format!("{setup} x")).is_err());
    }

    #[test]
    fn colours_parse_in_every_css_hex_form() {
        let rgba = |s: &str| Color(s.into()).rgba();
        assert_eq!(rgba("#fff"), Some([1.0, 1.0, 1.0, 1.0]));
        assert_eq!(rgba("#00ff0080"), Some([0.0, 1.0, 0.0, 128.0 / 255.0]));
        assert_eq!(
            rgba("#204060"),
            Some([32.0 / 255.0, 64.0 / 255.0, 96.0 / 255.0, 1.0])
        );
        assert_eq!(rgba("red"), None);
        assert_eq!(rgba("#12345"), None);
        assert_eq!(rgba("#ggg"), None);
    }

    #[test]
    fn pan_laws_keep_the_centre_and_lower_the_far_side() {
        let close =
            |a: [f64; 2], b: [f64; 2]| (a[0] - b[0]).abs() < 1e-9 && (a[1] - b[1]).abs() < 1e-9;
        for law in [PanLaw::Sine, PanLaw::Square, PanLaw::Linear] {
            assert!(close(law.factors(0.0), [1.0, 1.0]), "{law:?}");
            assert!(close(law.factors(1.0), [0.0, 1.0]), "{law:?}");
            assert!(close(law.factors(-1.0), [1.0, 0.0]), "{law:?}");
        }
        let half = |law: PanLaw| law.factors(0.5)[0];
        assert!((half(PanLaw::Linear) - 0.5).abs() < 1e-9);
        assert!((half(PanLaw::Square) - 0.5f64.sqrt()).abs() < 1e-9);
        // √2·sin(π/8): about −5.33 dB.
        assert!((half(PanLaw::Sine) - 0.541196).abs() < 1e-6);
        assert!(close(PanLaw::Sine.factors(-0.5), [1.0, 0.541196100146197]));
    }

    fn issues(scenes: &str) -> Vec<Issue> {
        let setup: Setup = toml::from_str(&format!(
            r##"
            version = 1
            name = "t"
            canvas = {{ width = 1280, height = 720, fps = 30 }}
            sources = [{{ id = "a", kind = "test_pattern" }}, {{ id = "m", kind = "tone" }}]
            {scenes}
            [recording]
            folder = "."
            program = {{ encoder = {{ codec = "hevc", quality = 70 }} }}
            iso = {{ enabled = false, encoder = {{ codec = "hevc", quality = 70 }} }}
            "##
        ))
        .unwrap();
        setup.validate()
    }

    #[test]
    fn scene_items_are_checked_through_the_tree() {
        let found = issues(
            r##"
            [[scenes]]
            id = "s"
            items = [
              { source = "a", rect = { x = 0, y = 0, width = 10, height = 10 } },
              { source = "a", rect = { x = 0, y = 0, width = 10, height = 10 } },
              { id = "box", rect = { x = 0, y = 0, width = 10, height = 10 }, blend = "multiply",
                crop = { left = 2 }, fit = "contain", scale_filter = "point",
                style = { background = "blue", opacity = 2.0 },
                children = [{ source = "m", rect = { x = 0, y = 0, width = 0, height = 5 } }] },
            ]
            "##,
        );
        let paths: Vec<&str> = found.iter().map(|i| i.path.as_str()).collect();
        assert!(paths.contains(&"scenes[0].items[1].id"), "{found:?}");
        assert!(paths.contains(&"scenes[0].items[2].crop"), "{found:?}");
        assert!(paths.contains(&"scenes[0].items[2].fit"), "{found:?}");
        assert!(
            paths.contains(&"scenes[0].items[2].scale_filter"),
            "{found:?}"
        );
        // Boxes can blend.
        assert!(!paths.contains(&"scenes[0].items[2].blend"), "{found:?}");
        assert!(
            paths.contains(&"scenes[0].items[2].style.background"),
            "{found:?}"
        );
        assert!(
            paths.contains(&"scenes[0].items[2].style.opacity"),
            "{found:?}"
        );
        assert!(
            paths.contains(&"scenes[0].items[2].children[0].source"),
            "{found:?}"
        );
        assert!(
            paths.contains(&"scenes[0].items[2].children[0].rect"),
            "{found:?}"
        );
    }

    #[test]
    fn layouts_are_checked_with_field_paths() {
        let found = issues(
            r##"
            [[scenes]]
            id = "s"
            layout = { display = "flex", gap = [12, "auto"], width = 10, grid_template_columns = "1fr 2xx" }
            items = [
              { source = "a", rect = { x = 0, y = 0, width = 10, height = 10 }, layout = { width = 1 } },
              { id = "nothing" },
              { id = "bad", layout = { width = "50 %", padding = [1, 2, -3, 4], aspect_ratio = 0, grid_row = "0 / 2", flex_grow = -1 } },
              { id = "fine", layout = { display = "grid", grid_template_columns = "repeat(3, 1fr)", width = "12px", inset = [0, "auto"], margin = -4, flex_basis = 0 }, children = [
                { id = "kid", source = "a", layout = { grid_column = "span 2", align_self = "center" } },
              ] },
            ]
            "##,
        );
        let at = |path: &str| {
            found
                .iter()
                .find(|i| i.path == path)
                .map(|i| i.message.as_str())
        };
        assert!(
            at("scenes[0].layout.width")
                .unwrap()
                .contains("only: display")
        );
        assert!(
            at("scenes[0].layout.gap[1]")
                .unwrap()
                .contains("\"auto\" doesn't apply")
        );
        assert!(
            at("scenes[0].layout.grid_template_columns")
                .unwrap()
                .contains("2xx")
        );
        assert_eq!(
            at("scenes[0].items[0]"),
            Some("give rect or layout, not both")
        );
        assert_eq!(
            at("scenes[0].items[1].rect"),
            Some("an item needs a rect, or a layout to place it")
        );
        assert_eq!(
            at("scenes[0].items[2].layout.width"),
            Some("\"50 %\" is not a length; use a number of pixels, \"50%\", or \"auto\"")
        );
        assert_eq!(
            at("scenes[0].items[2].layout.padding[2]"),
            Some("must be 0 or more")
        );
        assert!(at("scenes[0].items[2].layout.aspect_ratio").is_some());
        assert!(at("scenes[0].items[2].layout.grid_row").is_some());
        assert!(at("scenes[0].items[2].layout.flex_grow").is_some());
        assert!(
            !found
                .iter()
                .any(|i| i.path.starts_with("scenes[0].items[3]")),
            "{found:?}"
        );
    }

    #[test]
    fn a_valid_styled_tree_has_no_issues() {
        let found = issues(
            r##"
            [[scenes]]
            id = "s"
            items = [
              { id = "card", source = "a", rect = { x = 0, y = 0, width = 10, height = 10 },
                fit = "scale_down", align = "bottom_right", scale_filter = "lanczos",
                style = { radius = [1, 2, 3, 4], border = { width = 1, color = "#fff" },
                          shadow = { blur = 4, color = "#0008" }, transform = { rotate = 45 } },
                children = [{ id = "dot", rect = { x = 1, y = 1, width = 2, height = 2 },
                              style = { background = "#f00" } }] },
            ]
            "##,
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn nested_scenes_are_checked_for_references_and_cycles() {
        let found = issues(
            r##"
            [[scenes]]
            id = "a"
            items = [{ scene = "b", rect = { x = 0, y = 0, width = 10, height = 10 } }]
            [[scenes]]
            id = "b"
            items = [
              { scene = "c", rect = { x = 0, y = 0, width = 10, height = 10 }, visible = false },
              { scene = "nope", rect = { x = 0, y = 0, width = 10, height = 10 } },
              { source = "a", scene = "a", rect = { x = 0, y = 0, width = 10, height = 10 } },
            ]
            [[scenes]]
            id = "c"
            items = [{ scene = "a", rect = { x = 0, y = 0, width = 10, height = 10 } }]
            "##,
        );
        let at = |path: &str| found.iter().find(|i| i.path == path).map(|i| &i.message);
        // A hidden item still counts: showing it would loop.
        let cycle = at("scenes[0]").expect("a cycle");
        assert!(cycle.contains("a → b → c → a"), "{cycle}");
        assert!(
            at("scenes[1].items[1].scene")
                .unwrap()
                .contains("valid: a, b, c")
        );
        assert!(at("scenes[1].items[2].scene").is_some(), "{found:?}");
    }

    #[test]
    fn nested_scenes_list_their_sources_deepest_first() {
        let setup: Setup = toml::from_str(
            r##"
            version = 1
            name = "t"
            canvas = { width = 1280, height = 720, fps = 30 }
            sources = [{ id = "x", kind = "test_pattern" }, { id = "y", kind = "test_pattern" }]
            [[scenes]]
            id = "top"
            items = [
              { source = "x", rect = { x = 0, y = 0, width = 10, height = 10 } },
              { scene = "inner", rect = { x = 0, y = 0, width = 10, height = 10 } },
            ]
            [[scenes]]
            id = "inner"
            items = [{ source = "y", rect = { x = 0, y = 0, width = 10, height = 10 } }]
            [recording]
            folder = "."
            program = { encoder = { codec = "hevc", quality = 70 } }
            "##,
        )
        .unwrap();
        let top = &setup.scenes[0];
        let order: Vec<&str> = top
            .shown_scenes(&setup.scenes)
            .iter()
            .map(|s| s.id.as_str())
            .collect();
        assert_eq!(order, ["inner", "top"]);
        assert_eq!(top.video_sources(&setup.scenes), ["y", "x"]);
    }
}
