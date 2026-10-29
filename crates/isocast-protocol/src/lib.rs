//! The Isocast setup document.
//!
//! A setup describes everything the engine needs to run a session: the
//! canvas, the sources (screens, cameras, microphones…), the scenes that
//! arrange those sources, and how recordings are written. People write it as
//! TOML; agents and the control API exchange the same structure as JSON.

mod animation;
pub mod api;
mod audio_filter;
pub mod caller;
pub mod change;
mod content;
pub mod data;
pub mod developer;
pub mod devices;
pub mod diagnostics;
mod fields;
mod filter;
pub mod governor;
pub mod h264;
pub mod hello;
pub mod hotkeys;
pub mod init;
pub mod layout;
pub mod library;
pub mod located;
pub mod machine;
pub mod manifest;
mod mask;
pub mod mp4_edits;
pub mod nle;
pub mod obs;
pub mod openrpc;
pub mod optimize;
pub mod platform_text;
pub mod plugin;
pub mod project;
pub mod projector;
pub mod remote;
pub mod replay;
pub mod roots;
mod source;
pub mod stream;
mod transition;
pub mod virtual_camera;
pub mod websocket;

pub use animation::*;
pub use audio_filter::*;
pub use content::{Content, ContentKind};
pub use fields::UnknownFields;
pub use filter::*;
pub use layout::Layout;
pub use manifest::CameraCaptureMetadata;
pub use mask::*;
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
    /// A second, vertical canvas (for Shorts, Reels, TikTok) showing the
    /// same scenes, with its own recording and stream. Absent: none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vertical: Option<VerticalCanvas>,
    /// More canvases, each with its own id, size, frame rate, scene (or
    /// the main one's, laid out its own way), recording and streams, for
    /// streaming one show to several platforms in their own shapes (#84):
    /// `[[canvases]]` with an `id`, otherwise as `[vertical]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub canvases: Vec<VerticalCanvas>,
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
    #[serde(
        default,
        skip_serializing_if = "governor::GovernorSettings::is_default"
    )]
    pub governor: governor::GovernorSettings,
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

/// Which canvas, by id: `main` (the setup's `canvas`), `vertical` (its
/// `[vertical]` canvas) or the id of one of its `[[canvases]]` (#84).
///
/// The id is stored inline (at most [`MAX_CANVAS_ID_LEN`] bytes), so a
/// `CanvasId` is `Copy`, compares by value and needs no interning: an id
/// a request names costs nothing until it is used, and one that names no
/// canvas of the live setup is refused there, with the setup's canvases
/// listed. `CanvasId::Main` and `CanvasId::Vertical` are constants, usable
/// in patterns.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CanvasId {
    len: u8,
    bytes: [u8; MAX_CANVAS_ID_LEN],
}

/// A canvas id from text already known to be valid.
const fn fixed_id(text: &str) -> CanvasId {
    let from = text.as_bytes();
    let mut bytes = [0u8; MAX_CANVAS_ID_LEN];
    let mut i = 0;
    while i < from.len() {
        bytes[i] = from[i];
        i += 1;
    }
    CanvasId {
        len: from.len() as u8,
        bytes,
    }
}

#[allow(non_upper_case_globals)]
impl CanvasId {
    /// The setup's `canvas`.
    pub const Main: CanvasId = fixed_id("main");
    /// The setup's `[vertical]` canvas.
    pub const Vertical: CanvasId = fixed_id("vertical");
}

impl Default for CanvasId {
    fn default() -> Self {
        Self::Main
    }
}

impl std::fmt::Debug for CanvasId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::fmt::Display for CanvasId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The longest canvas id.
pub const MAX_CANVAS_ID_LEN: usize = 32;

/// The most canvases a setup may have besides the main one (the vertical
/// one included).
pub const MAX_EXTRA_CANVASES: usize = 8;

impl CanvasId {
    pub fn is_main(&self) -> bool {
        *self == Self::Main
    }

    /// Whether this is the `[vertical]` canvas.
    pub fn is_vertical(&self) -> bool {
        *self == Self::Vertical
    }

    /// The id as written in setups and requests.
    pub fn as_str(&self) -> &str {
        // Only `parse` and `fixed_id` make one, from ASCII.
        std::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or_default()
    }

    /// The canvas `id`, or why it can't be one: lowercase letters, digits
    /// and dashes, at most [`MAX_CANVAS_ID_LEN`].
    pub fn parse(id: &str) -> Result<Self, String> {
        let id = id.trim();
        if !is_valid_id(id) || id.len() > MAX_CANVAS_ID_LEN {
            return Err(format!(
                "canvas {id:?}: a canvas id uses lowercase letters, digits and dashes, at \
                 most {MAX_CANVAS_ID_LEN}"
            ));
        }
        Ok(fixed_id(id))
    }

    /// The reserved source name of this canvas's program recording in
    /// manifests and reports: [`PROGRAM`], [`VERTICAL_PROGRAM`] or
    /// `<id>-program`. Never another canvas's.
    pub fn program(self) -> String {
        match self {
            Self::Main => PROGRAM.into(),
            Self::Vertical => VERTICAL_PROGRAM.into(),
            other => format!("{other}-program"),
        }
    }

    /// The monitor target of this canvas's program (`program`,
    /// `vertical-program`, `<id>-program`): the same as [`program`](Self::program).
    pub fn program_target(self) -> String {
        self.program()
    }

    /// The monitor target of this canvas's Studio Mode preview
    /// (`preview`, `vertical-preview`, `<id>-preview`).
    pub fn preview_target(self) -> String {
        match self {
            Self::Main => "preview".into(),
            other => format!("{other}-preview"),
        }
    }

    /// The canvas a program or preview monitor target (as
    /// [`program_target`](Self::program_target) and
    /// [`preview_target`](Self::preview_target) make) shows, and whether it
    /// is the preview.
    pub fn from_target(target: &str) -> Option<(CanvasId, bool)> {
        match target {
            PROGRAM => return Some((Self::Main, false)),
            "preview" => return Some((Self::Main, true)),
            _ => {}
        }
        let (id, preview) = if let Some(id) = target.strip_suffix("-program") {
            (id, false)
        } else {
            (target.strip_suffix("-preview")?, true)
        };
        if id == "main" {
            return None;
        }
        Some((Self::parse(id).ok()?, preview))
    }

    /// The canvas and scene id of a scene monitor target (`scene:ID`,
    /// `vertical-scene:ID`, `<canvas>-scene:ID`).
    pub fn from_scene_target(target: &str) -> Option<(CanvasId, &str)> {
        if let Some(id) = target.strip_prefix("scene:") {
            return Some((Self::Main, id));
        }
        let (canvas, id) = target.split_once("-scene:")?;
        if canvas == "main" {
            return None;
        }
        Some((Self::parse(canvas).ok()?, id))
    }

    /// The prefix of this canvas's scene monitor targets (`scene:`,
    /// `vertical-scene:`, `<id>-scene:`).
    pub fn scene_prefix(self) -> String {
        match self {
            Self::Main => "scene:".into(),
            other => format!("{other}-scene:"),
        }
    }
}

impl Serialize for CanvasId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for CanvasId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let id = std::borrow::Cow::<'de, str>::deserialize(deserializer)?;
        CanvasId::parse(&id).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for CanvasId {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "CanvasId".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "A canvas id: `main`, `vertical` or one of the setup's `[[canvases]]`.",
            "pattern": "^[a-z0-9-]{1,32}$",
        })
    }
}

/// Another canvas, upright by default, that shows the setup's scenes: the
/// `[vertical]` canvas or one of `[[canvases]]`. A scene is laid out on it
/// by its arrangement for it (`vertical`, or `canvases.<id>`) if it has
/// one, else it shows the main picture fitted inside this canvas and
/// centred (letterboxed; items placed by `layout` are laid out again on
/// this canvas's size). Its program has the same audio mix as the main
/// one (`recording.program.audio`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VerticalCanvas {
    /// Its id in `[[canvases]]` (lowercase letters, digits and dashes, at
    /// most 32; not `main` or `vertical`), as requests' `canvas` names it.
    /// Absent on `[vertical]`, whose id is `vertical`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Display label, like "YouTube Shorts". Defaults to the id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// An even number of pixels from 16 to 8192.
    #[serde(default = "default_vertical_width")]
    pub width: u32,
    /// An even number of pixels from 16 to 8192.
    #[serde(default = "default_vertical_height")]
    pub height: u32,
    /// Frames per second, from 1 to 240; the main canvas's if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
    /// Show the scene the main canvas has on air, switching with it. False
    /// lets the vertical canvas put its own scene on air.
    #[serde(default = "default_true", skip_serializing_if = "filter::is_true")]
    pub follow_main: bool,
    /// The vertical program recording. Absent: not recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<VerticalRecording>,
    /// Live streaming of the vertical program, if set up (its own
    /// service; the key stays in the Keychain).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<stream::StreamSettings>,
}

fn default_vertical_width() -> u32 {
    1080
}

fn default_vertical_height() -> u32 {
    1920
}

impl Default for VerticalCanvas {
    fn default() -> Self {
        Self {
            id: None,
            name: None,
            width: default_vertical_width(),
            height: default_vertical_height(),
            fps: None,
            follow_main: true,
            recording: None,
            stream: None,
        }
    }
}

/// A scene's main items as the vertical canvas shows them when the scene
/// has no vertical arrangement (#109): the main picture fitted inside the
/// `to`-sized canvas and centred (letterboxed, like OBS's Fit to Screen).
/// Items placed by `rect` (in `from`-sized main canvas pixels) are scaled
/// by the fitting factor about the canvas's top-left and moved by the
/// centring offset, with their children, translations, radii, borders and
/// shadows; items shown at their own size (`fit = "none"`) keep it and get
/// the factor in their transform's scale instead. Items placed by `layout`
/// are laid out again on the vertical canvas, as before. Nothing changes
/// when both canvases have the same size.
///
/// Items showing a scene that `letterboxed` says is itself drawn
/// letterboxed (see [`Scene::letterboxed`]) are cropped to the main picture
/// inside it (#112): a nested scene is drawn at the vertical canvas's size,
/// so without the crop its letterboxed picture would be fitted twice.
pub fn fit_items(
    items: &mut [SceneItem],
    from: (u32, u32),
    to: (u32, u32),
    letterboxed: &dyn Fn(&str) -> bool,
) {
    let Some(canvas) = CanvasFit::new(from, to) else {
        return;
    };
    let mut fitting = Fitting {
        canvas,
        letterboxed,
        on_animation: &mut |_, _| {},
    };
    fit_level(items, canvas.offset, &mut fitting);
}

/// How the main canvas fits inside the vertical one.
#[derive(Clone, Copy)]
struct CanvasFit {
    factor: f32,
    offset: [f32; 2],
    from: (u32, u32),
    to: (u32, u32),
}

impl CanvasFit {
    fn new(from: (u32, u32), to: (u32, u32)) -> Option<Self> {
        if from == to || from.0 == 0 || from.1 == 0 {
            return None;
        }
        let (fw, fh) = (from.0 as f32, from.1 as f32);
        let (tw, th) = (to.0 as f32, to.1 as f32);
        let factor = (tw / fw).min(th / fh);
        let offset = [(tw - fw * factor) / 2.0, (th - fh * factor) / 2.0];
        Some(Self {
            factor,
            offset,
            from,
            to,
        })
    }

    /// `crop` of a main-canvas-sized picture of a letterboxed scene, as a
    /// crop of its vertical-canvas-sized picture: the same part of the
    /// main picture, which sits letterboxed in it. Its edges round as
    /// [`fit_level`] rounds rect corners, so the cropped picture is what
    /// the nested scene's full-canvas items cover.
    fn crop(&self, crop: Crop) -> Crop {
        let place = |offset: f32, at: u32| (offset + self.factor * at as f32).round().max(0.0);
        let left = place(self.offset[0], crop.left);
        let top = place(self.offset[1], crop.top);
        let right = place(self.offset[0], self.from.0.saturating_sub(crop.right));
        let bottom = place(self.offset[1], self.from.1.saturating_sub(crop.bottom));
        Crop {
            left: left as u32,
            top: top as u32,
            right: (self.to.0 as f32 - right).max(0.0) as u32,
            bottom: (self.to.1 as f32 - bottom).max(0.0) as u32,
        }
    }
}

/// How a fitted item's keyframe values change with the fit (#112), so its
/// animations look as on the main canvas, only smaller. Keyframe `scale`
/// multiplies the item's own (fitted) transform, so it never changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KeyframeFit {
    /// Translations in pixels, in its parent's (fitted) pixels.
    translate_px: bool,
    /// Translations in percent of its rect, which kept its main size.
    translate_percent: bool,
    /// Border width, radius and shadow: its own pixels were scaled.
    style: bool,
    /// Crop: it shows a letterboxed scene ([`CanvasFit::crop`]).
    crop: bool,
}

impl KeyframeFit {
    /// A suffix naming it in a keyframe set's id.
    fn suffix(self) -> String {
        let mut out = String::from("vertical-fit");
        for (on, name) in [
            (self.translate_px, "-px"),
            (self.translate_percent, "-percent"),
            (self.style, "-style"),
            (self.crop, "-crop"),
        ] {
            if on {
                out.push_str(name);
            }
        }
        out
    }

    /// `set` with its values fitted.
    fn apply(self, set: &Keyframes, canvas: &CanvasFit) -> Keyframes {
        let factor = canvas.factor;
        let mut set = set.clone();
        for frame in &mut set.frames {
            if let Some(translate) = frame.translate.as_mut() {
                for length in translate {
                    *length = match length.value() {
                        Ok(layout::LengthValue::Px(px)) if self.translate_px => {
                            layout::Length::Px(px * factor)
                        }
                        Ok(layout::LengthValue::Percent(percent)) if self.translate_percent => {
                            layout::Length::Text(format!("{}%", percent * factor))
                        }
                        _ => length.clone(),
                    };
                }
            }
            if self.style {
                frame.border_width = frame.border_width.map(|w| w * factor);
                frame.radius = frame.radius.map(|radius| match radius {
                    Radius::All(r) => Radius::All(r * factor),
                    Radius::Each(r) => Radius::Each(r.map(|r| r * factor)),
                });
                if let Some(shadow) = frame.shadow.as_mut() {
                    shadow.x *= factor;
                    shadow.y *= factor;
                    shadow.blur *= factor;
                }
            }
            if self.crop {
                frame.crop = frame.crop.map(|crop| canvas.crop(crop));
            }
        }
        set
    }
}

/// What fitting a scene's items needs as it walks them.
struct Fitting<'a> {
    canvas: CanvasFit,
    letterboxed: &'a dyn Fn(&str) -> bool,
    /// Called with each animation of a fitted item and how its keyframe
    /// values fit.
    on_animation: &'a mut dyn FnMut(&mut Animation, KeyframeFit),
}

impl Fitting<'_> {
    fn animations(&mut self, item: &mut SceneItem, fit: KeyframeFit) {
        for animation in &mut item.animations {
            (self.on_animation)(animation, fit);
        }
    }

    fn shows_letterboxed(&self, item: &SceneItem) -> bool {
        item.scene
            .as_deref()
            .is_some_and(|id| (self.letterboxed)(id))
    }
}

fn fit_level(items: &mut [SceneItem], offset: [f32; 2], fitting: &mut Fitting<'_>) {
    let canvas = fitting.canvas;
    let factor = canvas.factor;
    for item in items {
        if item.layout.is_some() {
            // Laid out on the canvas it is drawn on; its children are
            // relative to wherever layout puts it.
            continue;
        }
        let nested = fitting.shows_letterboxed(item);
        let Some(rect) = item.rect.as_mut() else {
            continue;
        };
        let style = &mut item.style;
        style.transform.translate = style.transform.translate.map(|t| t * factor);
        if nested {
            // A scene drawn at the vertical canvas's size with the main
            // picture letterboxed in it: crop to that picture, which is the
            // main one scaled by the factor, and place the item like a rect
            // item. At its own size (`fit = "none"`) it is the main canvas
            // less its crop there; the engine sizes it from the new crop.
            if item.fit == Fit::None {
                let crop = item.crop;
                rect.width = canvas.from.0.saturating_sub(crop.left + crop.right).max(1);
                rect.height = canvas.from.1.saturating_sub(crop.top + crop.bottom).max(1);
            }
            item.crop = canvas.crop(item.crop);
        } else if item.fit == Fit::None {
            // Drawn at its own size (the rect follows the picture), scaled
            // by its transform about its centre: move the centre, scale the
            // transform. Its children are in its own pixels, which the
            // transform scales.
            let (w, h) = (rect.width as f32, rect.height as f32);
            let cx = offset[0] + factor * (rect.x as f32 + w / 2.0);
            let cy = offset[1] + factor * (rect.y as f32 + h / 2.0);
            rect.x = (cx - w / 2.0).round() as i32;
            rect.y = (cy - h / 2.0).round() as i32;
            style.transform.scale *= factor;
            fitting.animations(
                item,
                KeyframeFit {
                    translate_px: true,
                    translate_percent: true,
                    style: false,
                    crop: false,
                },
            );
            fit_scaled(&mut item.children, fitting);
            continue;
        }
        let x0 = offset[0] + factor * rect.x as f32;
        let y0 = offset[1] + factor * rect.y as f32;
        let x1 = offset[0] + factor * (rect.x as f32 + rect.width as f32);
        let y1 = offset[1] + factor * (rect.y as f32 + rect.height as f32);
        rect.x = x0.round() as i32;
        rect.y = y0.round() as i32;
        rect.width = ((x1.round() - x0.round()) as u32).max(1);
        rect.height = ((y1.round() - y0.round()) as u32).max(1);
        style.radius = match style.radius {
            Radius::All(r) => Radius::All(r * factor),
            Radius::Each(r) => Radius::Each(r.map(|r| r * factor)),
        };
        if let Some(border) = style.border.as_mut() {
            border.width *= factor;
        }
        if let Some(shadow) = style.shadow.as_mut() {
            shadow.x *= factor;
            shadow.y *= factor;
            shadow.blur *= factor;
        }
        fitting.animations(
            item,
            KeyframeFit {
                translate_px: true,
                translate_percent: false,
                style: true,
                crop: nested,
            },
        );
        fit_level(&mut item.children, [0.0, 0.0], fitting);
    }
}

/// Items inside a fitted `fit = "none"` item, whose transform scales them:
/// they keep their main values, except that a letterboxed scene's picture
/// is cropped to the main picture in it and, at its own size, stretched
/// to its main size, which its parent's transform scales.
fn fit_scaled(items: &mut [SceneItem], fitting: &mut Fitting<'_>) {
    let canvas = fitting.canvas;
    for item in items {
        if item.layout.is_some() {
            continue;
        }
        if fitting.shows_letterboxed(item)
            && let Some(rect) = item.rect.as_mut()
        {
            if item.fit == Fit::None {
                let crop = item.crop;
                rect.width = canvas.from.0.saturating_sub(crop.left + crop.right).max(1);
                rect.height = canvas.from.1.saturating_sub(crop.top + crop.bottom).max(1);
                item.fit = Fit::Stretch;
            }
            item.crop = canvas.crop(item.crop);
            fitting.animations(
                item,
                KeyframeFit {
                    translate_px: false,
                    translate_percent: false,
                    style: false,
                    crop: true,
                },
            );
        }
        fit_scaled(&mut item.children, fitting);
    }
}

impl VerticalCanvas {
    /// The vertical canvas, with `main`'s frame rate unless it has its own.
    pub fn canvas(&self, main: &Canvas) -> Canvas {
        Canvas {
            width: self.width,
            height: self.height,
            fps: self.fps.unwrap_or(main.fps),
        }
    }

    /// A `[[canvases]]` entry `id` with default settings.
    pub fn with_id(id: &str) -> Self {
        Self {
            id: Some(id.to_string()),
            ..Self::default()
        }
    }

    /// Its display label: `name`, else its id (`vertical` without one).
    pub fn label(&self) -> &str {
        self.name
            .as_deref()
            .or(self.id.as_deref())
            .unwrap_or("vertical")
    }
}

/// The default program file name of the extra canvas `canvas`.
pub fn canvas_filename(canvas: CanvasId) -> String {
    format!("{{date}} {{time}} {canvas}")
}

/// How the vertical program is recorded, in `recording.folder` alongside
/// the main program. Its sound is the main program's mix
/// (`recording.program.audio`): both canvases share one audio mix.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VerticalRecording {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// File name without extension. Tokens: {date}, {time}. Must differ
    /// from `recording.program.filename`.
    #[serde(default = "default_vertical_filename")]
    pub filename: String,
    #[serde(default)]
    pub encoder: Encoder,
    /// Must be empty: the vertical program has the main program's audio
    /// mix (`recording.program.audio`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audio: Vec<String>,
    /// The file's size, if not the vertical canvas's (each side even, from
    /// 16 up to the vertical canvas's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

impl VerticalRecording {
    /// The file name canvas `canvas` records with: `filename`, except that
    /// a `[[canvases]]` entry left at the default (`recording = {}` in
    /// TOML gives "{date} {time} vertical") uses its own id, so two
    /// canvases never share a name (#84).
    pub fn filename_for(&self, canvas: CanvasId) -> String {
        if !canvas.is_vertical()
            && !canvas.is_main()
            && self.filename == default_vertical_filename()
        {
            canvas_filename(canvas)
        } else {
            self.filename.clone()
        }
    }
}

fn default_vertical_filename() -> String {
    "{date} {time} vertical".into()
}

impl Default for VerticalRecording {
    fn default() -> Self {
        Self {
            enabled: true,
            filename: default_vertical_filename(),
            encoder: Encoder::default(),
            audio: Vec::new(),
            width: None,
            height: None,
        }
    }
}

/// How a scene is arranged on the vertical canvas, in place of its own
/// `layout` and `items`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SceneLayout {
    /// How its items with a `layout` are laid out, as the scene's `layout`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<Layout>,
    /// Items drawn back to front, in vertical canvas pixels.
    #[serde(default)]
    pub items: Vec<SceneItem>,
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
    /// For video sources: how its own ISO file is encoded, over
    /// `recording.iso.encoder`. Left out, it follows the recording
    /// settings.
    #[serde(default, skip_serializing_if = "SourceEncoder::is_default")]
    pub encoder: SourceEncoder,
    /// Fields its kind doesn't have, kept for validation to report.
    #[serde(flatten)]
    #[schemars(skip)]
    pub unknown: fields::UnknownFields,
}

fields::keep_unknown_fields!(Source, kind, SourceKind);

/// The fields every source has, besides its kind's.
const SOURCE_FIELDS: &[&str] = &[
    "id",
    "name",
    "kind",
    "audio",
    "filters",
    "audio_filters",
    "encoder",
];

/// A video source's own ISO file encoding, over `recording.iso.encoder`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceEncoder {
    /// 8 or 10: this source's ISO file depth, over
    /// `recording.iso.encoder.bit_depth` (an 8 here stays 8 even at the
    /// incredible preset). 10-bit HEVC uses the Main 10 profile, which
    /// avoids banding in gradients, at more encoder work and bigger files;
    /// a display, window or application is then also captured in 10 bits.
    /// Needs a 10-bit ISO codec (HEVC; ProRes is always 10-bit). Left out,
    /// the file follows the recording settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_depth: Option<u8>,
}

impl SourceEncoder {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

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
    /// Its arrangement on the setup's vertical canvas, in place of `layout`
    /// and `items`. Absent: the main picture fitted inside the vertical
    /// canvas and centred (letterboxed): items placed by `rect` are scaled
    /// and centred with it, items placed by `layout` are laid out again on
    /// the vertical canvas's size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vertical: Option<SceneLayout>,
    /// Its arrangements on the setup's `[[canvases]]`, by canvas id, as
    /// `vertical` is on the vertical canvas.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub canvases: BTreeMap<String, SceneLayout>,
}

impl Scene {
    /// Its arrangement for the extra canvas `canvas`, if it has one
    /// (`vertical`, or `canvases.<id>`). None for the main canvas.
    pub fn arrangement(&self, canvas: CanvasId) -> Option<&SceneLayout> {
        match canvas {
            CanvasId::Main => None,
            CanvasId::Vertical => self.vertical.as_ref(),
            other => self.canvases.get(other.as_str()),
        }
    }

    /// Set (or with None, remove) its arrangement for the extra canvas
    /// `canvas`. Does nothing for the main canvas.
    pub fn set_arrangement(&mut self, canvas: CanvasId, layout: Option<SceneLayout>) {
        match (canvas, layout) {
            (CanvasId::Main, _) => {}
            (CanvasId::Vertical, layout) => self.vertical = layout,
            (other, Some(layout)) => {
                self.canvases.insert(other.as_str().to_string(), layout);
            }
            (other, None) => {
                self.canvases.remove(other.as_str());
            }
        }
    }

    /// Whether it has an arrangement for any extra canvas.
    pub fn has_arrangements(&self) -> bool {
        self.vertical.is_some() || !self.canvases.is_empty()
    }

    /// Whether the vertical canvas draws it as the main picture fitted and
    /// centred (letterboxed, #109): it has no vertical arrangement and
    /// nothing in it is placed by `layout`, which lays it out again on the
    /// vertical canvas instead.
    pub fn letterboxed(&self) -> bool {
        self.letterboxed_on(CanvasId::Vertical)
    }

    /// Whether the extra canvas `canvas` draws it as the main picture
    /// fitted and centred, as [`letterboxed`](Self::letterboxed) says for
    /// the vertical one.
    pub fn letterboxed_on(&self, canvas: CanvasId) -> bool {
        fn any_layout(items: &[SceneItem]) -> bool {
            items
                .iter()
                .any(|item| item.layout.is_some() || any_layout(&item.children))
        }
        self.arrangement(canvas).is_none() && self.layout.is_none() && !any_layout(&self.items)
    }

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

/// Moves, scales, rotates and flips an item around its centre.
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
    /// Mirrored left to right, before it is rotated: its picture,
    /// children, border and shadow.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub flip_horizontal: bool,
    /// Mirrored top to bottom, before it is rotated.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub flip_vertical: bool,
}

impl Default for Transform {
    fn default() -> Self {
        Self {
            translate: [0.0; 2],
            scale: 1.0,
            rotate: 0.0,
            flip_horizontal: false,
            flip_vertical: false,
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

/// The source name of the vertical canvas's program recording in manifests
/// and reports; reserved, so no source can use it.
pub const VERTICAL_PROGRAM: &str = "vertical-program";

/// Source ids no source can have: the program recordings' names.
pub const RESERVED_SOURCE_IDS: [&str; 2] = [PROGRAM, VERTICAL_PROGRAM];

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
    /// Retired with the editor (2026-10-22): read and ignored, so setups
    /// that set it still load; never written.
    #[serde(default, skip_serializing)]
    #[schemars(schema_with = "crate::retired_open_editor")]
    pub open_editor: serde::de::IgnoredAny,
}

/// `recording.open_editor`'s schema: still allowed (old setups set it),
/// deprecated, and ignored.
fn retired_open_editor(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "boolean",
        "deprecated": true,
        "description": "Retired with the editor (2026-10-22): accepted and ignored.",
    })
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
    pub encoder: IsoEncoder,
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
            encoder: IsoEncoder::default(),
            audio: None,
            source_audio: BTreeMap::new(),
            filtered: false,
        }
    }
}

impl IsoRecording {
    /// The depth of `source`'s ISO file: the source's own
    /// `encoder.bit_depth` if set, else the ISO encoder's
    /// ([`IsoEncoder::depth`]).
    pub fn bit_depth_for(&self, source: &Source) -> u8 {
        source.encoder.bit_depth.unwrap_or(self.encoder.depth())
    }

    /// The encoder `source`'s ISO file is written with: `encoder`, at the
    /// depth [`IsoRecording::bit_depth_for`] gives it.
    pub fn encoder_for(&self, source: &Source) -> Encoder {
        self.encoder.at_depth(self.bit_depth_for(source))
    }

    /// Whether screen `source` opens its capture in 10 bits. With a depth
    /// of its own, only while it records a 10-bit ISO file; without one,
    /// while ISO recording is enabled and its encoder's depth is 10
    /// (every screen).
    pub fn captures_ten_bit(&self, source: &Source) -> bool {
        match source.encoder.bit_depth {
            Some(_) => {
                self.enabled
                    && self.sources.contains(&source.id)
                    && self.encoder_for(source).bit_depth == 10
            }
            None => self.enabled && self.encoder.depth() == 10,
        }
    }

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
    /// 8 or 10. 10-bit HEVC uses the Main 10 profile, which avoids banding
    /// in gradients. ProRes is always 10-bit; H.264 can only be 8-bit. The
    /// incredible HEVC preset promotes 8 to 10.
    #[serde(default = "default_bit_depth")]
    pub bit_depth: u8,
}

/// The ISO files' encoder: [`Encoder`]'s fields, but `bit_depth` can be
/// left out (it then stays out when saved).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IsoEncoder {
    pub codec: Codec,
    /// A named quality preset, or the original 1–100 constant quality.
    /// Ignored by ProRes, which is constant quality.
    #[serde(default = "default_quality")]
    pub quality: EncoderQuality,
    #[serde(default)]
    pub container: Container,
    /// 8 or 10, for every ISO file without a depth of its own (a source's
    /// `encoder.bit_depth`). 10-bit HEVC uses the Main 10 profile, which
    /// avoids banding in gradients, and captures displays, windows and
    /// applications in 10 bits, at more encoder work and bigger files.
    /// ProRes is always 10-bit; H.264 can only be 8-bit. A set 8 stays 8
    /// even at the incredible preset. Left out: 8, but 10 at incredible
    /// HEVC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bit_depth: Option<u8>,
}

impl Default for IsoEncoder {
    fn default() -> Self {
        Self {
            codec: Codec::platform_default(),
            quality: default_quality(),
            container: Container::default(),
            bit_depth: None,
        }
    }
}

impl IsoEncoder {
    /// The depth of an ISO file without one of its own: `bit_depth` if
    /// set (8 stays 8 at the incredible preset); left out, 8, but 10 for
    /// HEVC at the incredible preset.
    pub fn depth(&self) -> u8 {
        match self.bit_depth {
            Some(depth) => depth,
            None if self.codec == Codec::Hevc
                && self.quality == EncoderQuality::Preset(QualityPreset::Incredible) =>
            {
                10
            }
            None => default_bit_depth(),
        }
    }

    /// This encoder as a writer takes it, at `bit_depth`. An 8-bit HEVC
    /// file at the incredible preset gets its number (100), which the
    /// writer doesn't promote to 10 bits.
    pub fn at_depth(&self, bit_depth: u8) -> Encoder {
        let quality = match self.quality {
            EncoderQuality::Preset(QualityPreset::Incredible)
                if self.codec == Codec::Hevc && bit_depth == 8 =>
            {
                EncoderQuality::Value(self.quality.value())
            }
            quality => quality,
        };
        Encoder {
            codec: self.codec,
            quality,
            container: self.container,
            bit_depth,
        }
    }
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
            codec: Codec::platform_default(),
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
    /// The codec a new recording uses: HEVC where the OS has a hardware
    /// encoder for it (macOS, Windows), H.264 on Linux, whose software
    /// recording (OpenH264) has only that.
    pub fn platform_default() -> Self {
        if cfg!(target_os = "linux") {
            Self::H264
        } else {
            Self::Hevc
        }
    }

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
        self.for_each_file_mut(&mut |_, path| {
            if !path.is_empty() && !path.starts_with('/') && !path.starts_with('~') {
                *path = base.join(&*path).to_string_lossy().into_owned();
            }
        });
    }

    /// Every file the setup reads (images, slide shows, media, text and
    /// browser files, mask and LUT images, transition files), with its
    /// field path, as written. Browser URLs and the recording folder
    /// aren't files it reads; see [`Setup::browser_urls`].
    pub fn file_paths(&self) -> Vec<(String, String)> {
        let mut copy = self.clone();
        let mut out = Vec::new();
        copy.for_each_file_mut(&mut |field, path| {
            if !path.is_empty() {
                out.push((field, path.clone()));
            }
        });
        out
    }

    /// Every browser source's URL, with its field path.
    pub fn browser_urls(&self) -> Vec<(String, String)> {
        self.sources
            .iter()
            .filter_map(|source| match &source.kind {
                SourceKind::Browser(browser) => browser
                    .url
                    .as_ref()
                    .map(|url| (format!("sources[{}].url", source.id), url.clone())),
                _ => None,
            })
            .collect()
    }

    /// Call `f` with each file path the setup reads and its field path.
    fn for_each_file_mut(&mut self, f: &mut dyn FnMut(String, &mut String)) {
        fn contents(items: &mut [SceneItem], at: &str, f: &mut dyn FnMut(String, &mut String)) {
            for (i, item) in items.iter_mut().enumerate() {
                let at = format!("{at}[{i}]");
                match item.content.as_mut().map(|content| &mut content.kind) {
                    Some(ContentKind::Image(image)) => {
                        f(format!("{at}.content.path"), &mut image.path)
                    }
                    Some(ContentKind::Text(text)) => {
                        if let Some(file) = &mut text.file {
                            f(format!("{at}.content.file"), file);
                        }
                    }
                    None => {}
                }
                contents(&mut item.children, &format!("{at}.children"), f);
            }
        }
        for (i, scene) in self.scenes.iter_mut().enumerate() {
            contents(&mut scene.items, &format!("scenes[{i}].items"), f);
        }
        for source in &mut self.sources {
            let at = format!("sources[{}]", source.id);
            match &mut source.kind {
                SourceKind::Image(image) => f(format!("{at}.path"), &mut image.path),
                SourceKind::Slideshow(slideshow) => {
                    for (i, file) in slideshow.files.iter_mut().enumerate() {
                        f(format!("{at}.files[{i}]"), file);
                    }
                }
                SourceKind::Text(text) => {
                    if let Some(file) = &mut text.file {
                        f(format!("{at}.file"), file);
                    }
                }
                SourceKind::Media(media) => {
                    for (i, file) in media.files.iter_mut().enumerate() {
                        f(format!("{at}.files[{i}]"), file);
                    }
                }
                SourceKind::Browser(browser) => {
                    if let Some(file) = &mut browser.file {
                        f(format!("{at}.file"), file);
                    }
                }
                _ => {}
            }
            for (i, filter) in source.filters.iter_mut().enumerate() {
                match &mut filter.kind {
                    FilterKind::ImageMask(mask) => {
                        f(format!("{at}.filters[{i}].path"), &mut mask.path)
                    }
                    FilterKind::ApplyLut(lut) => {
                        f(format!("{at}.filters[{i}].path"), &mut lut.path)
                    }
                    _ => {}
                }
            }
        }
        if let Some(file) = self.transition.files_mut() {
            f("transition".into(), file);
        }
        for (i, scene) in self.scenes.iter_mut().enumerate() {
            if let Some(file) = scene.transition.as_mut().and_then(|t| t.files_mut()) {
                f(format!("scenes[{i}].transition"), file);
            }
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
                ("mic", arg) => SourceKind::Microphone(MicrophoneSource::new(arg.unwrap_or("default"))),
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
                encoder: SourceEncoder::default(),
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
                open_editor: serde::de::IgnoredAny,
            },
            stream: None,
            vertical: None,
            canvases: Default::default(),
            hotkeys: Default::default(),
            virtual_camera: Default::default(),
            developer: Default::default(),
            governor: Default::default(),
        })
    }

    /// This setup as one canvas sees it, as a setup of its own: for `Main`
    /// a copy; for `Vertical` (None without a vertical canvas) the vertical
    /// canvas as `canvas`, each scene's vertical arrangement (if it has
    /// one) as its `layout` and `items`, the vertical recording as
    /// `recording.program` (with the main program's audio mix; disabled if
    /// there is none) and the vertical stream as `stream`. ISO files, the
    /// replay buffer and the virtual camera are the main canvas's, so ISO
    /// recording is off in it and the other two have their defaults, and it
    /// has no `vertical` of its own. Clones the
    /// whole setup: call it off the frame thread.
    pub fn for_canvas(&self, canvas: CanvasId) -> Option<Setup> {
        if canvas.is_main() {
            return Some(self.clone());
        }
        let vertical = self.extra_canvas(canvas)?;
        let mut setup = self.clone();
        setup.vertical = None;
        setup.canvases.clear();
        setup.canvas = vertical.canvas(&self.canvas);
        let from = (self.canvas.width, self.canvas.height);
        let to = (setup.canvas.width, setup.canvas.height);
        let letterboxed: HashSet<&str> = self
            .scenes
            .iter()
            .filter(|scene| scene.letterboxed_on(canvas))
            .map(|scene| scene.id.as_str())
            .collect();
        let letterboxed = |id: &str| letterboxed.contains(id);
        // Keyframe sets fitted items play, fitted with them (#112): each
        // (set, how it fits) once, as a set of its own appended to the
        // setup's, under an id no set has.
        let mut fitted: Vec<(String, KeyframeFit, String)> = Vec::new();
        let mut taken: HashSet<String> = self.keyframes.iter().map(|k| k.id.clone()).collect();
        let fit = CanvasFit::new(from, to);
        let mut on_animation = |animation: &mut Animation, how: KeyframeFit| {
            let Some(canvas) = fit else {
                return;
            };
            let Some(set) = self.keyframe_set(&animation.keyframes) else {
                return;
            };
            if let Some((_, _, id)) = fitted
                .iter()
                .find(|(from, fit, _)| *from == set.id && *fit == how)
            {
                animation.keyframes = id.clone();
                return;
            }
            if how.apply(set, &canvas).frames == set.frames {
                return;
            }
            let base = format!("{}-{}", set.id, how.suffix());
            let mut id = base.clone();
            let mut n = 2;
            while taken.contains(&id) {
                id = format!("{base}-{n}");
                n += 1;
            }
            taken.insert(id.clone());
            fitted.push((set.id.clone(), how, id.clone()));
            animation.keyframes = id;
        };
        for scene in &mut setup.scenes {
            let arrangement = scene.arrangement(canvas).cloned();
            scene.vertical = None;
            scene.canvases.clear();
            if let Some(SceneLayout { layout, items }) = arrangement {
                scene.layout = layout;
                scene.items = items;
            } else if let Some(canvas) = fit {
                let mut fitting = Fitting {
                    canvas,
                    letterboxed: &letterboxed,
                    on_animation: &mut on_animation,
                };
                fit_level(&mut scene.items, canvas.offset, &mut fitting);
            }
        }
        if let Some(canvas) = fit {
            for (from, how, id) in fitted {
                if let Some(set) = self.keyframe_set(&from) {
                    let mut set = how.apply(set, &canvas);
                    set.id = id;
                    setup.keyframes.push(set);
                }
            }
        }
        let main = &self.recording.program;
        setup.recording.program = match &vertical.recording {
            Some(recording) => ProgramRecording {
                enabled: recording.enabled,
                filename: recording.filename_for(canvas),
                encoder: recording.encoder.clone(),
                audio: main.audio.clone(),
                width: recording.width,
                height: recording.height,
            },
            None => ProgramRecording {
                enabled: false,
                filename: canvas_filename(canvas),
                encoder: main.encoder.clone(),
                audio: main.audio.clone(),
                width: None,
                height: None,
            },
        };
        setup.recording.iso.enabled = false;
        setup.recording.replay = Default::default();
        setup.stream = vertical.stream.clone();
        setup.virtual_camera = Default::default();
        Some(setup)
    }

    /// The extra canvas `canvas` (`vertical`, or one of `canvases`), if
    /// the setup has it. None for the main canvas.
    pub fn extra_canvas(&self, canvas: CanvasId) -> Option<&VerticalCanvas> {
        match canvas {
            CanvasId::Main => None,
            CanvasId::Vertical => self.vertical.as_ref(),
            other => self
                .canvases
                .iter()
                .find(|c| c.id.as_deref() == Some(other.as_str())),
        }
    }

    /// As [`extra_canvas`](Self::extra_canvas), to change it.
    pub fn extra_canvas_mut(&mut self, canvas: CanvasId) -> Option<&mut VerticalCanvas> {
        match canvas {
            CanvasId::Main => None,
            CanvasId::Vertical => self.vertical.as_mut(),
            other => self
                .canvases
                .iter_mut()
                .find(|c| c.id.as_deref() == Some(other.as_str())),
        }
    }

    /// Every extra canvas's id, the vertical one first, then `canvases` in
    /// order (those with a valid id).
    pub fn extra_canvas_ids(&self) -> Vec<CanvasId> {
        self.vertical
            .iter()
            .map(|_| CanvasId::Vertical)
            .chain(
                self.canvases
                    .iter()
                    .filter_map(|c| CanvasId::parse(c.id.as_deref()?).ok()),
            )
            .collect()
    }

    /// Every canvas's id, `main` first.
    pub fn canvas_ids(&self) -> Vec<CanvasId> {
        std::iter::once(CanvasId::Main)
            .chain(self.extra_canvas_ids())
            .collect()
    }

    /// Whether a recording writes any canvas's program file.
    pub fn records_a_program(&self) -> bool {
        self.recording.program.enabled
            || self.extra_canvas_ids().into_iter().any(|canvas| {
                self.extra_canvas(canvas)
                    .and_then(|c| c.recording.as_ref())
                    .is_some_and(|r| r.enabled)
            })
    }

    /// Whether the setup has the canvas `canvas`.
    pub fn has_canvas(&self, canvas: CanvasId) -> bool {
        canvas.is_main() || self.extra_canvas(canvas).is_some()
    }

    /// The size and frame rate of canvas `canvas`, if the setup has it.
    pub fn canvas_of(&self, canvas: CanvasId) -> Option<Canvas> {
        match canvas {
            CanvasId::Main => Some(self.canvas.clone()),
            other => Some(self.extra_canvas(other)?.canvas(&self.canvas)),
        }
    }

    /// The stream section of canvas `canvas` (`[stream]`, `[vertical.stream]`
    /// or a `[[canvases]]` entry's `stream`), if it has one.
    pub fn stream_of(&self, canvas: CanvasId) -> Option<&stream::StreamSettings> {
        match canvas {
            CanvasId::Main => self.stream.as_ref(),
            other => self.extra_canvas(other)?.stream.as_ref(),
        }
    }

    /// Why `canvas` can't be used here: the setup has no such canvas;
    /// lists those it has. `field` is the request's field (`canvas`).
    pub fn unknown_canvas(&self, field: &str, canvas: CanvasId) -> String {
        let ids: Vec<String> = self.canvas_ids().iter().map(|c| c.to_string()).collect();
        if canvas.is_vertical() {
            return format!(
                "{field}: the vertical canvas is not enabled; add [vertical] (canvases: {})",
                ids.join(", ")
            );
        }
        format!(
            "{field}: the setup has no canvas {canvas:?}; canvases: {}",
            ids.join(", ")
        )
    }

    /// The setup field an extra canvas's settings are under: `vertical`, or
    /// `canvases[i]`.
    pub fn canvas_field(&self, canvas: CanvasId) -> String {
        match canvas {
            CanvasId::Main => "canvas".into(),
            CanvasId::Vertical => "vertical".into(),
            other => match self
                .canvases
                .iter()
                .position(|c| c.id.as_deref() == Some(other.as_str()))
            {
                Some(i) => format!("canvases[{i}]"),
                None => format!("canvases[id={other}]"),
            },
        }
    }

    /// Check references and values the type system cannot express. An empty
    /// list means the setup is valid.
    pub fn validate(&self) -> Vec<Issue> {
        let mut issues = Issues::default();
        if self.governor.pins.len() > 128 {
            issues.add("governor.pins", "at most 128 pins");
        }
        for (i, pin) in self.governor.pins.iter().enumerate() {
            if !governor::GovernorSettings::valid_pin(pin) {
                issues.add(format!("governor.pins[{i}]"), "valid pins: preview.fps, recording.iso.fps, recording.iso.sources.<source-id>.fps, vertical.fps, canvases.<canvas-id>.fps (secondary canvases only), stream.video_bitrate_kbps");
            }
        }

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
                    format!(
                        "system audio is everything this {} plays, monitoring included, so \
                     monitoring it would feed back; use \"off\"",
                        platform_text::MACHINE
                    ),
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
            if source.id == VERTICAL_PROGRAM {
                issues.add(
                    format!("sources[{i}].id"),
                    "\"vertical-program\" is reserved for the vertical program recording; pick \
                     another id",
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
            if let Some(depth) = source.encoder.bit_depth {
                let path = format!("sources[{i}].encoder.bit_depth");
                let codec = self.recording.iso.encoder.codec;
                if !source.kind.has_video() {
                    issues.add(
                        format!("sources[{i}].encoder"),
                        format!(
                            "only video sources have an ISO file to encode, and this is a {} \
                             source; remove encoder",
                            source.kind.label()
                        ),
                    );
                } else if !matches!(depth, 8 | 10) {
                    issues.add(
                        path,
                        format!(
                            "{depth} is not supported; use 8 or 10, or leave it out to follow \
                             recording.iso.encoder.bit_depth"
                        ),
                    );
                } else if depth == 10 && codec == Codec::H264 {
                    issues.add(
                        path,
                        "h264 can only be 8-bit here; set recording.iso.encoder.codec to hevc \
                         (or a prores codec) for a 10-bit ISO file, or use 8",
                    );
                }
            }
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
            if scene.vertical.is_some() && self.vertical.is_none() {
                issues.add(
                    format!("scenes[{i}].vertical"),
                    "a vertical arrangement needs the setup's vertical canvas; add [vertical] \
                     or remove this",
                );
            }
            for id in scene.canvases.keys() {
                let known = self.canvases.iter().any(|c| c.id.as_deref() == Some(id));
                if !known {
                    let ids: Vec<&str> = self
                        .canvases
                        .iter()
                        .filter_map(|c| c.id.as_deref())
                        .collect();
                    issues.add(
                        format!("scenes[{i}].canvases.{id}"),
                        format!(
                            "an arrangement for canvas {id:?}, which [[canvases]] doesn't have; \
                             canvases: {}",
                            if ids.is_empty() {
                                "none".to_string()
                            } else {
                                ids.join(", ")
                            }
                        ),
                    );
                }
            }
        }
        self.check_canvases(&mut issues);

        let video = self.source_ids(SourceKind::has_video);
        let audio = self.source_ids(SourceKind::has_audio);
        for (i, source) in self.sources.iter().enumerate() {
            audio_filter::check_chain(&format!("sources[{i}]"), source, &audio, &mut issues);
        }
        mask::check_mask_refs(self, &video, &mut issues);

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
        let iso = &rec.iso.encoder;
        for (path, codec, container, quality, bit_depth) in [
            (
                "recording.program.encoder",
                rec.program.encoder.codec,
                rec.program.encoder.container,
                rec.program.encoder.quality,
                Some(rec.program.encoder.bit_depth),
            ),
            (
                "recording.iso.encoder",
                iso.codec,
                iso.container,
                iso.quality,
                iso.bit_depth,
            ),
        ] {
            if let Some(problem) = codec.container_problem(container) {
                issues.add(format!("{path}.container"), problem);
            }
            if !(1..=100).contains(&quality.value()) {
                issues.add(
                    format!("{path}.quality"),
                    format!(
                        "{} is out of range; use 1–100 or low, medium, high, very_high, incredible",
                        quality.value()
                    ),
                );
            }
            // Left out (ISO files only), the engine picks a valid depth.
            let Some(bit_depth) = bit_depth else {
                continue;
            };
            match (bit_depth, codec) {
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
        if let Some(vertical) = &self.vertical {
            self.validate_vertical(CanvasId::Vertical, vertical, &mut issues);
        }
        let mut seen = HashSet::new();
        for canvas in &self.canvases {
            let Some(id) = canvas.id.as_deref().and_then(|id| CanvasId::parse(id).ok()) else {
                continue;
            };
            if id.is_main() || id.is_vertical() || !seen.insert(id) {
                continue;
            }
            self.validate_vertical(id, canvas, &mut issues);
        }
        issues.0
    }

    /// `[[canvases]]`'s own problems: ids, how many, and names a program
    /// file or source would share.
    fn check_canvases(&self, issues: &mut Issues) {
        if self.canvases.len() + usize::from(self.vertical.is_some()) > MAX_EXTRA_CANVASES {
            issues.add(
                "canvases",
                format!(
                    "{} canvases besides the main one; at most {MAX_EXTRA_CANVASES} (the vertical \
                     one included)",
                    self.canvases.len() + usize::from(self.vertical.is_some())
                ),
            );
        }
        let mut seen: HashSet<&str> = HashSet::new();
        for (i, canvas) in self.canvases.iter().enumerate() {
            let path = format!("canvases[{i}].id");
            let Some(id) = canvas.id.as_deref() else {
                issues.add(path, "each [[canvases]] entry needs an id, like \"shorts\"");
                continue;
            };
            if let Err(err) = CanvasId::parse(id) {
                issues.add(path, err);
                continue;
            }
            if id == "main" || id == "vertical" {
                issues.add(
                    path,
                    format!(
                        "{id:?} is reserved: `main` is the setup's canvas and `vertical` its \
                         [vertical] one; pick another id"
                    ),
                );
                continue;
            }
            if !seen.insert(id) {
                issues.add(path, format!("{id:?} is used by another canvas"));
                continue;
            }
            // Its program recording's name, and its monitor targets.
            for reserved in [format!("{id}-program"), format!("{id}-preview")] {
                if let Some(j) = self.sources.iter().position(|s| s.id == reserved) {
                    issues.add(
                        format!("sources[{j}].id"),
                        format!(
                            "{reserved:?} is reserved for canvas {id:?}'s program and preview; \
                             pick another id"
                        ),
                    );
                }
            }
        }
        if self
            .vertical
            .as_ref()
            .is_some_and(|v| v.id.as_deref().is_some_and(|id| id != "vertical"))
        {
            issues.add(
                "vertical.id",
                "[vertical]'s id is always `vertical`; leave it out, or move this canvas to \
                 [[canvases]]",
            );
        }
        // Program files are all written to recording.folder.
        let mut files: Vec<(String, String)> = Vec::new();
        if self.recording.program.enabled {
            files.push((
                "recording.program.filename".into(),
                self.recording.program.filename.clone(),
            ));
        }
        for canvas in self.extra_canvas_ids() {
            let field = self.canvas_field(canvas);
            if let Some(recording) = self.extra_canvas(canvas).and_then(|c| c.recording.as_ref())
                && recording.enabled
            {
                let path = format!("{field}.recording.filename");
                let filename = recording.filename_for(canvas);
                if let Some((other, _)) = files.iter().find(|(_, f)| *f == filename)
                    && !(canvas.is_vertical() && other == "recording.program.filename")
                {
                    {
                        issues.add(
                            path.clone(),
                            format!(
                                "{:?} is {other} too, and every program is written to \
                                 recording.folder; give this canvas's file its own name, like \
                                 \"{}\"",
                                filename,
                                canvas_filename(canvas)
                            ),
                        );
                    }
                }
                files.push((path, filename));
            }
        }
    }

    /// The vertical canvas's problems: its own fields, then everything the
    /// setup it sees ([`Setup::for_canvas`]) adds to the main canvas's
    /// problems, under the vertical fields they come from (`vertical.width`,
    /// `vertical.recording.encoder`, `scenes[2].vertical.items[0].rect`…).
    fn validate_vertical(&self, canvas: CanvasId, vertical: &VerticalCanvas, issues: &mut Issues) {
        let field = self.canvas_field(canvas);
        if let Some(recording) = &vertical.recording {
            if !recording.audio.is_empty() {
                issues.add(
                    format!("{field}.recording.audio"),
                    "this canvas's program has the main program's audio mix: both canvases \
                     share one mix, so leave this empty and list the audio sources in \
                     recording.program.audio",
                );
            }
            if canvas.is_vertical()
                && recording.enabled
                && self.recording.program.enabled
                && recording.filename == self.recording.program.filename
            {
                issues.add(
                    "vertical.recording.filename",
                    format!(
                        "{:?} is recording.program.filename too, and both programs are written \
                         to recording.folder; give the vertical file its own name, like \
                         \"{{date}} {{time}} vertical\"",
                        recording.filename
                    ),
                );
            }
        }
        let Some(setup) = self.for_canvas(canvas) else {
            return;
        };
        let main: HashSet<(String, String)> = issues
            .0
            .iter()
            .map(|issue| (issue.path.clone(), issue.message.clone()))
            .collect();
        for mut issue in setup.validate() {
            match self.vertical_path(canvas, vertical, &issue.path) {
                VerticalPath::Own(path) => issue.path = path,
                VerticalPath::Main => continue,
                VerticalPath::Shared => {
                    if main.contains(&(issue.path.clone(), issue.message.clone())) {
                        continue;
                    }
                    issue.message = match canvas {
                        CanvasId::Vertical => format!("on the vertical canvas: {}", issue.message),
                        other => format!("on canvas {other:?}: {}", issue.message),
                    };
                }
            }
            issues.0.push(issue);
        }
    }

    /// Where a problem of the vertical canvas's setup (`path` in it) is in
    /// this setup.
    fn vertical_path(
        &self,
        canvas: CanvasId,
        vertical: &VerticalCanvas,
        path: &str,
    ) -> VerticalPath {
        let field = self.canvas_field(canvas);
        let under = |field: &str| {
            path.strip_prefix(field)
                .filter(|rest| rest.is_empty() || rest.starts_with(['.', '[']))
        };
        if let Some(rest) = under("canvas.width").or(under("canvas.height")) {
            let field = &path["canvas.".len()..path.len() - rest.len()];
            return VerticalPath::Own(format!("{}.{field}{rest}", self.canvas_field(canvas)));
        }
        if let Some(rest) = under("canvas.fps") {
            return match vertical.fps {
                Some(_) => VerticalPath::Own(format!("{field}.fps{rest}")),
                // Inherited: the main canvas reports it.
                None => VerticalPath::Main,
            };
        }
        if under("recording.program.audio").is_some() {
            // The main program's mix, checked there.
            return VerticalPath::Main;
        }
        if let Some(rest) = under("recording.program") {
            return match vertical.recording {
                Some(_) => VerticalPath::Own(format!("{field}.recording{rest}")),
                // Disabled stand-in with the main encoder: nothing of its own.
                None => VerticalPath::Main,
            };
        }
        if let Some(rest) = under("stream") {
            return VerticalPath::Own(format!("{field}.stream{rest}"));
        }
        if let Some(rest) = path.strip_prefix("keyframes[")
            && let Some((index, _)) = rest.split_once(']')
            && index
                .parse::<usize>()
                .is_ok_and(|i| i >= self.keyframes.len())
        {
            // A fitted copy of one of the setup's sets (#112), whose own
            // problems the main canvas reports.
            return VerticalPath::Main;
        }
        if let Some(rest) = path.strip_prefix("scenes[")
            && let Some((index, rest)) = rest.split_once(']')
            && let Some(scene) = index.parse::<usize>().ok().and_then(|i| self.scenes.get(i))
            && scene.arrangement(canvas).is_some()
            && (rest.is_empty() || rest.starts_with(".layout") || rest.starts_with(".items"))
        {
            let arrangement = match canvas {
                CanvasId::Vertical => "vertical".to_string(),
                other => format!("canvases.{other}"),
            };
            return VerticalPath::Own(format!("scenes[{index}].{arrangement}{rest}"));
        }
        VerticalPath::Shared
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

/// Where a problem the vertical canvas's setup has belongs.
enum VerticalPath {
    /// At this vertical field.
    Own(String),
    /// The main canvas's, which reports it itself.
    Main,
    /// Shared with the main canvas: reported unless the main canvas has
    /// the same problem.
    Shared,
}

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
    use serde_json::json;

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
    fn a_scene_without_a_vertical_arrangement_is_letterboxed() {
        // #109: the main 16:9 picture sits in the middle of the 9:16
        // canvas, scaled to its width, not at the top at main pixels.
        let mut setup = Setup::from_toml(
            r##"
version = 1
name = "t"
canvas = { width = 1920, height = 1080, fps = 30 }

[[sources]]
id = "cam"
kind = "test_pattern"

[[scenes]]
id = "main"
items = [
  { source = "cam", rect = { x = 0, y = 0, width = 1920, height = 1080 }, children = [
    { id = "badge", style = { background = "#fff" }, rect = { x = 100, y = 200, width = 400, height = 80 } },
  ] },
  { id = "logo", source = "cam", fit = "none", rect = { x = 1600, y = 40, width = 200, height = 100 }, animations = [{ keyframes = "pop", duration_ms = 400 }] },
  { id = "flow", source = "cam", layout = { width = 320, height = 180 } },
  { id = "inner", scene = "card", rect = { x = 0, y = 0, width = 1920, height = 1080 }, animations = [{ keyframes = "pop", duration_ms = 400 }] },
  { id = "inner-own", scene = "card", fit = "none", crop = { left = 960 }, rect = { x = 0, y = 0, width = 1, height = 1 } },
  { id = "vertical-own", scene = "own", rect = { x = 0, y = 0, width = 960, height = 540 } },
]

[[scenes]]
id = "card"
items = [{ source = "cam", rect = { x = 0, y = 0, width = 1920, height = 1080 } }]

[[keyframes]]
id = "pop"
frames = [{ at = 0.0, translate = [100, "50%"], border_width = 8, scale = 0.5, crop = { top = 108 } }, { at = 1.0, opacity = 1.0 }]

[[scenes]]
id = "own"
items = [{ source = "cam", rect = { x = 0, y = 0, width = 1920, height = 1080 } }]
vertical = { items = [{ source = "cam", rect = { x = 0, y = 0, width = 1080, height = 1920 } }] }

[recording]
folder = "."
program = { encoder = { codec = "hevc", quality = 70 } }
iso = { enabled = false, encoder = { codec = "hevc", quality = 70 } }

[vertical]
"##,
        )
        .unwrap();
        let view = setup.for_canvas(CanvasId::Vertical).unwrap();
        let main = &view.scenes[0].items;
        let rect = |x, y, width, height| {
            Some(Rect {
                x,
                y,
                width,
                height,
            })
        };
        assert_eq!(main[0].rect, rect(0, 656, 1080, 608));
        assert_eq!(main[0].children[0].rect, rect(56, 113, 225, 45));
        // At its own size, scaled about its centre by its transform.
        assert_eq!(main[1].rect, rect(856, 657, 200, 100));
        assert_eq!(main[1].style.transform.scale, 0.5625);
        assert_eq!(main[2].rect, None);
        assert_eq!(view.scenes[2].items[0].rect, rect(0, 0, 1080, 1920));
        // #112: a letterboxed nested scene is cropped to the main picture
        // in it (drawn at the vertical canvas's size), not fitted twice.
        let band = Crop {
            left: 0,
            top: 656,
            right: 0,
            bottom: 656,
        };
        assert_eq!(main[3].rect, rect(0, 656, 1080, 608));
        assert_eq!(main[3].crop, band);
        // At its own size: its main size (the canvas less its crop), fitted.
        assert_eq!(main[4].rect, rect(0, 656, 540, 608));
        assert_eq!(main[4].style.transform.scale, 1.0);
        assert_eq!(main[4].crop.left, 540);
        // A scene with its own vertical arrangement isn't letterboxed.
        assert_eq!(main[5].crop, Crop::default());
        // Keyframes play fitted copies: a rect item's pixels and style
        // scale (and a nested scene's crop maps), a fit = "none" item's
        // translations; scale multiplies the fitted transform, unchanged.
        let set = |id: &str| view.keyframe_set(id).unwrap().frames[0].clone();
        let rect_set = set(&main[3].animations[0].keyframes);
        assert_eq!(
            rect_set.translate.as_ref().unwrap()[0],
            layout::Length::Px(56.25)
        );
        assert_eq!(
            rect_set.translate.as_ref().unwrap()[1],
            layout::Length::Text("50%".into())
        );
        assert_eq!(rect_set.border_width, Some(4.5));
        assert_eq!(rect_set.scale, Some(0.5));
        assert_eq!(rect_set.crop.unwrap().top, 717);
        let none_set = set(&main[1].animations[0].keyframes);
        assert_eq!(
            none_set.translate.as_ref().unwrap()[1],
            layout::Length::Text("28.125%".into())
        );
        assert_eq!(none_set.border_width, Some(8.0));
        assert_ne!(main[1].animations[0].keyframes, "pop");
        assert!(setup.validate().is_empty(), "{:?}", setup.validate());
        // A same-sized canvas changes nothing.
        setup.vertical.as_mut().unwrap().width = 1920;
        setup.vertical.as_mut().unwrap().height = 1080;
        let same = setup.for_canvas(CanvasId::Vertical).unwrap();
        assert_eq!(same.scenes[0].items, setup.scenes[0].items);
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

    /// A display, a window, a camera and a keyed display, all ISO
    /// sources, and a tone, with `encoder` as the ISO encoder and
    /// `screen` as the display's own `encoder` (if any).
    fn iso_setup(encoder: &str, screen: &str) -> Setup {
        Setup::from_toml(&format!(
            r##"
version = 1
name = "t"
canvas = {{ width = 1920, height = 1080, fps = 30 }}

[[sources]]
id = "screen"
kind = "display"
display = "main"
{screen}

[[sources]]
id = "window"
kind = "window"
app = "Safari"

[[sources]]
id = "camera"
kind = "camera"
device = "Cam"

[[sources]]
id = "keyed"
kind = "display"
display = "main"
filters = [{{ kind = "chroma_key" }}]

[[sources]]
id = "beep"
kind = "tone"

[[scenes]]
id = "main"
items = [{{ source = "screen", rect = {{ x = 0, y = 0, width = 1920, height = 1080 }} }}]

[recording]
folder = "."
program = {{ encoder = {{ codec = "hevc", quality = 70 }} }}
iso = {{ enabled = true, sources = ["screen", "window", "camera", "keyed"], encoder = {encoder} }}
"##
        ))
        .unwrap()
    }

    /// Each ISO source's file depth.
    fn iso_depths(setup: &Setup) -> Vec<(&str, u8)> {
        let iso = &setup.recording.iso;
        iso.sources
            .iter()
            .map(|id| {
                let source = setup.source(id).unwrap();
                (id.as_str(), iso.encoder_for(source).bit_depth)
            })
            .collect()
    }

    /// Which screens capture in 10 bits.
    fn ten_bit_captures(setup: &Setup) -> Vec<&str> {
        ["screen", "window", "keyed"]
            .into_iter()
            .filter(|id| {
                let source = setup.source(id).unwrap();
                setup.recording.iso.captures_ten_bit(source)
            })
            .collect()
    }

    /// Saved and read back through JSON (the API) and TOML.
    fn round_trips(setup: &Setup) -> serde_json::Value {
        let json = serde_json::to_value(setup).unwrap();
        assert_eq!(&Setup::from_value(json.clone()).unwrap(), setup);
        assert_eq!(&Setup::from_toml(&setup.to_toml().unwrap()).unwrap(), setup);
        json
    }

    #[test]
    fn iso_files_are_eight_bit_unless_set() {
        let setup = iso_setup(r#"{ codec = "hevc" }"#, "");
        assert_eq!(setup.validate(), Vec::new());
        // Left out, it stays out, and so does every source's own.
        let json = round_trips(&setup);
        assert_eq!(json["recording"]["iso"]["encoder"].get("bit_depth"), None);
        assert_eq!(json["recording"]["program"]["encoder"]["bit_depth"], 8);
        assert!(
            json["sources"]
                .as_array()
                .unwrap()
                .iter()
                .all(|s| s.get("encoder").is_none())
        );
        // Every file is 8-bit, on every platform (nothing asks one), and
        // no screen captures 10 bits.
        assert_eq!(
            iso_depths(&setup),
            [("screen", 8), ("window", 8), ("camera", 8), ("keyed", 8)]
        );
        assert!(ten_bit_captures(&setup).is_empty());
        for codec in ["h264", "prores422"] {
            let other = iso_setup(&format!(r#"{{ codec = "{codec}" }}"#), "");
            assert_eq!(other.validate(), Vec::new(), "{codec}");
            assert!(iso_depths(&other).iter().all(|(_, d)| *d == 8), "{codec}");
        }
        // Left out at the incredible HEVC preset: 10 bits, as before.
        let incredible = iso_setup(r#"{ codec = "hevc", quality = "incredible" }"#, "");
        assert!(iso_depths(&incredible).iter().all(|(_, d)| *d == 10));
        assert_eq!(ten_bit_captures(&incredible), ["screen", "window", "keyed"]);
        assert_eq!(
            round_trips(&incredible)["recording"]["iso"]["encoder"].get("bit_depth"),
            None
        );
    }

    #[test]
    fn saved_iso_depths_keep_their_meaning() {
        for depth in [8u8, 10] {
            let setup = iso_setup(&format!(r#"{{ codec = "hevc", bit_depth = {depth} }}"#), "");
            assert_eq!(setup.validate(), Vec::new());
            let json = round_trips(&setup);
            assert_eq!(json["recording"]["iso"]["encoder"]["bit_depth"], depth);
            // Every ISO file, cameras too, keeps the saved depth. Screen
            // capture uses it only while ISO recording is enabled.
            assert!(iso_depths(&setup).iter().all(|(_, d)| *d == depth));
            let mut program_only = setup.clone();
            program_only.recording.iso.enabled = false;
            assert!(ten_bit_captures(&program_only).is_empty());
            assert_eq!(
                round_trips(&program_only)["recording"]["iso"]["encoder"]["bit_depth"],
                depth
            );
            let expected: &[&str] = if depth == 10 {
                &["screen", "window", "keyed"]
            } else {
                &[]
            };
            program_only.recording.iso.enabled = true;
            assert_eq!(ten_bit_captures(&program_only), expected);
            // The program's encoder is its own.
            assert_eq!(setup.recording.program.encoder.bit_depth, 8);
        }
        // A set 8 stays 8 at the incredible preset: its files are written
        // at quality 100, which the writer doesn't promote, no screen
        // captures 10 bits, and the setup keeps the preset's name and 8.
        let incredible = iso_setup(
            r#"{ codec = "hevc", quality = "incredible", bit_depth = 8 }"#,
            "",
        );
        assert_eq!(incredible.validate(), Vec::new());
        assert_eq!(incredible.recording.iso.encoder.bit_depth, Some(8));
        assert!(iso_depths(&incredible).iter().all(|(_, d)| *d == 8));
        let camera = incredible
            .recording
            .iso
            .encoder_for(incredible.source("camera").unwrap());
        assert_eq!(camera.quality, EncoderQuality::Value(100));
        assert_eq!(camera.effective_bit_depth(), 8);
        assert!(ten_bit_captures(&incredible).is_empty());
        let json = round_trips(&incredible);
        assert_eq!(json["recording"]["iso"]["encoder"]["quality"], "incredible");
        assert_eq!(json["recording"]["iso"]["encoder"]["bit_depth"], 8);
        // A set 10 there is 10.
        let ten = iso_setup(
            r#"{ codec = "hevc", quality = "incredible", bit_depth = 10 }"#,
            "",
        );
        assert!(iso_depths(&ten).iter().all(|(_, d)| *d == 10));
    }

    #[test]
    fn a_source_depth_beats_the_iso_encoders() {
        // Its own 8 under a set 10: only that file and capture are 8.
        let setup = iso_setup(
            r#"{ codec = "hevc", bit_depth = 10 }"#,
            "encoder = { bit_depth = 8 }",
        );
        assert_eq!(setup.validate(), Vec::new());
        let json = round_trips(&setup);
        assert_eq!(json["sources"][0]["encoder"], json!({ "bit_depth": 8 }));
        assert_eq!(json["sources"][1].get("encoder"), None);
        assert_eq!(
            iso_depths(&setup),
            [("screen", 8), ("window", 10), ("camera", 10), ("keyed", 10)]
        );
        assert_eq!(ten_bit_captures(&setup), ["window", "keyed"]);
        // Its own 10 over the 8 left out: only that file and capture.
        let setup = iso_setup(r#"{ codec = "hevc" }"#, "encoder = { bit_depth = 10 }");
        assert_eq!(setup.validate(), Vec::new());
        assert_eq!(
            round_trips(&setup)["recording"]["iso"]["encoder"].get("bit_depth"),
            None
        );
        assert_eq!(
            iso_depths(&setup),
            [("screen", 10), ("window", 8), ("camera", 8), ("keyed", 8)]
        );
        assert_eq!(ten_bit_captures(&setup), ["screen"]);
        // Without its ISO file, it captures for the program in 8 bits.
        let mut program_only = setup.clone();
        program_only.recording.iso.enabled = false;
        assert!(ten_bit_captures(&program_only).is_empty());
        let mut others = setup.clone();
        others.recording.iso.sources.retain(|id| id != "screen");
        assert!(ten_bit_captures(&others).is_empty());
        // Its own 8 holds at the incredible preset: quality 100, which the
        // writer doesn't promote; the others stay 10.
        let setup = iso_setup(
            r#"{ codec = "hevc", quality = "incredible" }"#,
            "encoder = { bit_depth = 8 }",
        );
        let screen = setup
            .recording
            .iso
            .encoder_for(setup.source("screen").unwrap());
        assert_eq!(screen.bit_depth, 8);
        assert_eq!(screen.quality, EncoderQuality::Value(100));
        assert_eq!(screen.effective_bit_depth(), 8);
        let camera = setup
            .recording
            .iso
            .encoder_for(setup.source("camera").unwrap());
        assert_eq!(camera.effective_bit_depth(), 10);
        assert_eq!(
            camera.quality,
            EncoderQuality::Preset(QualityPreset::Incredible)
        );
    }

    #[test]
    fn source_depths_are_checked() {
        let paths = |setup: &Setup| -> Vec<String> {
            setup.validate().into_iter().map(|i| i.path).collect()
        };
        let odd = iso_setup(r#"{ codec = "hevc" }"#, "encoder = { bit_depth = 12 }");
        assert_eq!(paths(&odd), ["sources.screen.encoder.bit_depth"]);
        assert!(odd.validate()[0].message.contains("use 8 or 10"));
        let h264 = iso_setup(r#"{ codec = "h264" }"#, "encoder = { bit_depth = 10 }");
        assert_eq!(paths(&h264), ["sources.screen.encoder.bit_depth"]);
        let message = &h264.validate()[0].message;
        assert!(message.contains("can only be 8-bit"));
        assert!(message.contains("hevc") && message.contains("prores"));
        let h264 = iso_setup(r#"{ codec = "h264" }"#, "encoder = { bit_depth = 8 }");
        assert_eq!(paths(&h264), Vec::<String>::new());
        let prores = iso_setup(r#"{ codec = "prores422" }"#, "encoder = { bit_depth = 10 }");
        assert_eq!(paths(&prores), Vec::<String>::new());
        let global = iso_setup(r#"{ codec = "hevc", bit_depth = 12 }"#, "");
        assert_eq!(paths(&global), ["recording.iso.encoder.bit_depth"]);
        assert!(global.validate()[0].message.contains("use 8 or 10"));
        let global = iso_setup(r#"{ codec = "h264", bit_depth = 10 }"#, "");
        assert_eq!(paths(&global), ["recording.iso.encoder.bit_depth"]);
        assert!(global.validate()[0].message.contains("can only be 8-bit"));
        // A sound has no ISO picture to encode.
        let mut value = serde_json::to_value(iso_setup(r#"{ codec = "hevc" }"#, "")).unwrap();
        value["sources"][4]["encoder"] = json!({ "bit_depth": 8 });
        assert_eq!(
            paths(&Setup::from_value(value.clone()).unwrap()),
            ["sources.beep.encoder"]
        );
        // Misspelt, it doesn't load.
        value["sources"][4]
            .as_object_mut()
            .unwrap()
            .remove("encoder");
        value["sources"][0]["encoder"] = json!({ "depth": 10 });
        assert!(Setup::from_value(value).is_err());
    }
}
