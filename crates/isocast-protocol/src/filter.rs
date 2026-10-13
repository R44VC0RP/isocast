//! Effect filters on a source's picture, as OBS has them, by `kind`. The
//! engine runs a source's enabled filters on the GPU, in order, before the
//! program or a snapshot draws it.
//!
//! To add a filter: a settings struct whose doc comments describe each
//! field (they become the JSON Schema), an `impl Check`, and a line in
//! `filter_kinds!` at the bottom; then its pass in the engine's `filter`
//! module.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Color, Issues};

/// One filter in a source's chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(remote = "Self")]
pub struct Filter {
    /// Identifies the filter for patches; its kind if absent. Unique within
    /// its source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// A filter that is off is skipped.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(flatten)]
    pub kind: FilterKind,
    /// Fields its kind doesn't have, kept for validation to report.
    #[serde(flatten)]
    #[schemars(skip)]
    pub unknown: crate::UnknownFields,
}

crate::fields::keep_unknown_fields!(Filter, kind, FilterKind);

/// The fields every filter has, besides its kind's.
pub(crate) const FILTER_FIELDS: &[&str] = &["id", "enabled", "kind"];

impl Filter {
    /// What patches call it: its id, else its kind.
    pub fn key(&self) -> &str {
        self.id.as_deref().unwrap_or(self.kind.label())
    }
}

/// What every filter kind's settings provide.
pub(crate) trait Check {
    /// Check the settings of the filter at `path` (like
    /// `sources[2].filters[0]`).
    fn check(&self, _path: &str, _issues: &mut Issues) {}
}

/// Colour, brightness and opacity changes (OBS's Color Correction).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ColorCorrection {
    /// −3 to 3: above 0 lifts the midtones, below 0 darkens them.
    #[serde(default)]
    pub gamma: f32,
    /// −4 to 4: above 0 spreads colours away from middle grey, below 0
    /// pulls them towards it.
    #[serde(default)]
    pub contrast: f32,
    /// −1 to 1, added to every colour.
    #[serde(default)]
    pub brightness: f32,
    /// −1 (grey) to 5.
    #[serde(default)]
    pub saturation: f32,
    /// −180 to 180 degrees around the colour wheel.
    #[serde(default)]
    pub hue_shift: f32,
    /// 0 (invisible) to 1.
    #[serde(default = "one")]
    pub opacity: f32,
    /// Every colour is multiplied by this, tinting the light ones; white
    /// leaves them as they are.
    #[serde(default = "white")]
    pub color_multiply: Color,
    /// Added to every colour, lifting the dark ones; black leaves them.
    #[serde(default = "black")]
    pub color_add: Color,
}

impl Default for ColorCorrection {
    fn default() -> Self {
        Self {
            gamma: 0.0,
            contrast: 0.0,
            brightness: 0.0,
            saturation: 0.0,
            hue_shift: 0.0,
            opacity: 1.0,
            color_multiply: white(),
            color_add: black(),
        }
    }
}

impl Check for ColorCorrection {
    fn check(&self, path: &str, issues: &mut Issues) {
        within(issues, path, "gamma", self.gamma, -3.0, 3.0);
        within(issues, path, "contrast", self.contrast, -4.0, 4.0);
        within(issues, path, "brightness", self.brightness, -1.0, 1.0);
        within(issues, path, "saturation", self.saturation, -1.0, 5.0);
        within(issues, path, "hue_shift", self.hue_shift, -180.0, 180.0);
        within(issues, path, "opacity", self.opacity, 0.0, 1.0);
        color(issues, path, "color_multiply", &self.color_multiply);
        color(issues, path, "color_add", &self.color_add);
    }
}

/// Which colour a key removes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum KeyColorType {
    #[default]
    Green,
    Blue,
    Magenta,
    /// The filter's `key_color`.
    Custom,
}

impl KeyColorType {
    /// The colour removed, given the filter's `custom` colour.
    pub fn rgb(self, custom: &Color) -> [f32; 3] {
        match self {
            Self::Green => [0.0, 1.0, 0.0],
            Self::Blue => [0.0, 0.0, 1.0],
            Self::Magenta => [1.0, 0.0, 1.0],
            Self::Custom => {
                let [r, g, b, _] = custom.rgba().unwrap_or([0.0, 1.0, 0.0, 1.0]);
                [r, g, b]
            }
        }
    }
}

/// Picture adjustments the key filters offer after keying.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, JsonSchema)]
pub struct KeyAdjust {
    /// −4 to 4, as in Color Correction.
    #[serde(default)]
    pub contrast: f32,
    /// −1 to 1, added to every colour.
    #[serde(default)]
    pub brightness: f32,
    /// −1 to 1: above 0 lifts the midtones.
    #[serde(default)]
    pub gamma: f32,
}

/// Flattened into the key filters, where serde can't say which of its
/// fields is wrong, so it reads them itself and names the field.
impl<'de> Deserialize<'de> for KeyAdjust {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let fields = serde_json::Map::deserialize(deserializer)?;
        let read = |name: &str| match fields.get(name) {
            None => Ok(0.0),
            Some(value) => f32::deserialize(value).map_err(|err| {
                crate::located::Located {
                    path: name.into(),
                    message: err.to_string(),
                }
                .into_error()
            }),
        };
        Ok(Self {
            contrast: read("contrast")?,
            brightness: read("brightness")?,
            gamma: read("gamma")?,
        })
    }
}

impl KeyAdjust {
    fn check(&self, path: &str, issues: &mut Issues) {
        within(issues, path, "contrast", self.contrast, -4.0, 4.0);
        within(issues, path, "brightness", self.brightness, -1.0, 1.0);
        within(issues, path, "gamma", self.gamma, -1.0, 1.0);
    }
}

/// Removes a colour by its hue and saturation, whatever its brightness,
/// for green and blue screens (OBS's Chroma Key).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ChromaKey {
    #[serde(default)]
    pub key_color_type: KeyColorType,
    /// The colour removed when `key_color_type` is `custom`.
    #[serde(default = "green")]
    pub key_color: Color,
    /// 1 to 1000: how far from the key colour a pixel may be and still be
    /// removed. Higher removes more.
    #[serde(default = "chroma_similarity")]
    pub similarity: u32,
    /// 1 to 1000: how gradually removed areas fade at their edges.
    #[serde(default = "chroma_smoothness")]
    pub smoothness: u32,
    /// 1 to 1000: how much of the key colour is taken out of what remains
    /// near it, like green fringes on hair.
    #[serde(default = "chroma_spill")]
    pub spill: u32,
    /// 0 to 1.
    #[serde(default = "one")]
    pub opacity: f32,
    #[serde(flatten)]
    pub adjust: KeyAdjust,
}

impl Check for ChromaKey {
    fn check(&self, path: &str, issues: &mut Issues) {
        color(issues, path, "key_color", &self.key_color);
        for (field, value) in [
            ("similarity", self.similarity),
            ("smoothness", self.smoothness),
            ("spill", self.spill),
        ] {
            within(issues, path, field, value as f32, 1.0, 1000.0);
        }
        within(issues, path, "opacity", self.opacity, 0.0, 1.0);
        self.adjust.check(path, issues);
    }
}

/// Removes one exact colour and those close to it, for graphics and
/// windows (OBS's Color Key).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ColorKey {
    #[serde(default)]
    pub key_color_type: KeyColorType,
    /// The colour removed when `key_color_type` is `custom`.
    #[serde(default = "green")]
    pub key_color: Color,
    /// 1 to 1000: how far from the key colour a pixel may be and still be
    /// removed.
    #[serde(default = "color_similarity")]
    pub similarity: u32,
    /// 1 to 1000: how gradually removed areas fade at their edges.
    #[serde(default = "color_smoothness")]
    pub smoothness: u32,
    /// 0 to 1.
    #[serde(default = "one")]
    pub opacity: f32,
    #[serde(flatten)]
    pub adjust: KeyAdjust,
}

impl Check for ColorKey {
    fn check(&self, path: &str, issues: &mut Issues) {
        color(issues, path, "key_color", &self.key_color);
        for (field, value) in [
            ("similarity", self.similarity),
            ("smoothness", self.smoothness),
        ] {
            within(issues, path, field, value as f32, 1.0, 1000.0);
        }
        within(issues, path, "opacity", self.opacity, 0.0, 1.0);
        self.adjust.check(path, issues);
    }
}

/// Removes the brightest and/or darkest parts (OBS's Luma Key).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LumaKey {
    /// 0 to 1: brighter than this is removed.
    #[serde(default = "one")]
    pub luma_max: f32,
    /// 0 to 1: how far below `luma_max` the removal fades in.
    #[serde(default)]
    pub luma_max_smooth: f32,
    /// 0 to 1: darker than this is removed.
    #[serde(default)]
    pub luma_min: f32,
    /// 0 to 1: how far above `luma_min` the removal fades out.
    #[serde(default)]
    pub luma_min_smooth: f32,
}

impl Check for LumaKey {
    fn check(&self, path: &str, issues: &mut Issues) {
        within(issues, path, "luma_max", self.luma_max, 0.0, 1.0);
        within(
            issues,
            path,
            "luma_max_smooth",
            self.luma_max_smooth,
            0.0,
            1.0,
        );
        within(issues, path, "luma_min", self.luma_min, 0.0, 1.0);
        within(
            issues,
            path,
            "luma_min_smooth",
            self.luma_min_smooth,
            0.0,
            1.0,
        );
        if self.luma_min > self.luma_max {
            issues.add(
                format!("{path}.luma_min"),
                "must not be above luma_max, or everything is removed",
            );
        }
    }
}

/// Cuts pixels from the edges, or adds transparent ones (OBS's Crop/Pad).
/// Unlike an item's crop, it changes the source everywhere it is shown.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CropPad {
    /// Pixels cut from the left edge; negative adds transparent ones.
    #[serde(default)]
    pub left: i32,
    #[serde(default)]
    pub top: i32,
    #[serde(default)]
    pub right: i32,
    #[serde(default)]
    pub bottom: i32,
    /// Instead of edges, the area to keep: its top-left corner and size
    /// (OBS's absolute mode). A size of 0 reaches the far edge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub area: Option<CropArea>,
}

/// An area of a picture, in its pixels.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CropArea {
    #[serde(default)]
    pub x: i32,
    #[serde(default)]
    pub y: i32,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
}

impl Check for CropPad {
    fn check(&self, path: &str, issues: &mut Issues) {
        if self.area.is_some() && (self.left, self.top, self.right, self.bottom) != (0, 0, 0, 0) {
            issues.add(
                format!("{path}.area"),
                "give edges (left, top, right, bottom) or an area, not both",
            );
        }
    }
}

/// How a picture is resampled to a new size.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScaleFilter {
    /// Nearest pixel: blocky, for pixel art.
    Point,
    Bilinear,
    /// Sharper than bilinear.
    #[default]
    Bicubic,
    /// Sharpest, with slight ringing.
    Lanczos,
    /// Averages the pixels each one covers: best for shrinking a lot.
    Area,
}

/// Resizes the picture or changes its shape (OBS's Scaling/Aspect Ratio).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Scaling {
    /// The new size in pixels (with `height`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Or a new shape, like "4:3", keeping the height.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aspect: Option<String>,
    #[serde(default)]
    pub scale_filter: ScaleFilter,
    /// When narrowing a wide picture, squeeze the sides more and the middle
    /// less, so the middle keeps its proportions.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub undistort: bool,
}

impl Scaling {
    /// The width-to-height ratio `aspect` names, if it is valid.
    pub fn ratio(&self) -> Option<f64> {
        let (w, h) = self.aspect.as_deref()?.split_once(':')?;
        let (w, h): (f64, f64) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
        (w > 0.0 && h > 0.0).then_some(w / h)
    }
}

impl Check for Scaling {
    fn check(&self, path: &str, issues: &mut Issues) {
        match (self.width, self.height, &self.aspect) {
            (Some(_), None, _) | (None, Some(_), _) => {
                issues.add(format!("{path}.height"), "give width and height together")
            }
            (Some(_), Some(_), Some(_)) => issues.add(
                format!("{path}.aspect"),
                "give a size (width and height) or an aspect, not both",
            ),
            _ => {}
        }
        for (field, value) in [("width", self.width), ("height", self.height)] {
            if let Some(value) = value
                && !(1..=16384).contains(&value)
            {
                issues.add(format!("{path}.{field}"), "must be from 1 to 16384");
            }
        }
        if self.aspect.is_some() && self.ratio().is_none() {
            issues.add(
                format!("{path}.aspect"),
                "must be two positive numbers like \"16:9\" or \"4:3\"",
            );
        }
    }
}

/// Moves the picture continuously, wrapping around (OBS's Scroll).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Scroll {
    /// Pixels per second; positive moves right.
    #[serde(default)]
    pub horizontal_speed: f32,
    /// Pixels per second; positive moves up.
    #[serde(default)]
    pub vertical_speed: f32,
    /// Show only this many pixels across (a window onto the scrolling
    /// picture); its own width if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Likewise down.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Repeat the picture as it scrolls; without it, it scrolls away once.
    #[serde(default = "yes", rename = "loop")]
    pub repeat: bool,
}

impl Check for Scroll {
    fn check(&self, path: &str, issues: &mut Issues) {
        within(
            issues,
            path,
            "horizontal_speed",
            self.horizontal_speed,
            -10_000.0,
            10_000.0,
        );
        within(
            issues,
            path,
            "vertical_speed",
            self.vertical_speed,
            -10_000.0,
            10_000.0,
        );
        for (field, value) in [("width", self.width), ("height", self.height)] {
            if let Some(value) = value
                && !(1..=16384).contains(&value)
            {
                issues.add(format!("{path}.{field}"), "must be from 1 to 16384");
            }
        }
    }
}

/// Sharpens edges (OBS's Sharpen).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Sharpen {
    /// 0 to 1.
    #[serde(default = "sharpness")]
    pub sharpness: f32,
}

impl Check for Sharpen {
    fn check(&self, path: &str, issues: &mut Issues) {
        within(issues, path, "sharpness", self.sharpness, 0.0, 1.0);
    }
}

/// How an image masks or blends with the picture.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MaskMode {
    /// The image's colour (the channels `color` names; white: all of
    /// them) sets how much of the picture shows.
    #[default]
    AlphaMaskColor,
    /// The image's transparency sets how much of the picture shows.
    AlphaMaskAlpha,
    /// The picture's colours multiplied by the image's.
    BlendMultiply,
    /// The image's colours added to the picture's.
    BlendAdd,
    /// The image's colours taken from the picture's.
    BlendSubtract,
}

/// Masks the picture with an image, or blends an image into it (OBS's
/// Image Mask/Blend).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ImageMask {
    #[serde(default)]
    pub mode: MaskMode,
    /// The image: absolute, `~/...`, or relative to the setup file.
    pub path: String,
    /// Which channels a colour mask reads, or the tint of a blend.
    #[serde(default = "white")]
    pub color: Color,
    /// 0 to 1: how strongly the image masks or blends.
    #[serde(default = "one")]
    pub opacity: f32,
    /// Stretch the image over the picture; otherwise it keeps its shape
    /// and covers the picture, centred, its overflow cut off.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stretch: bool,
}

impl Check for ImageMask {
    fn check(&self, path: &str, issues: &mut Issues) {
        if self.path.trim().is_empty() {
            issues.add(format!("{path}.path"), "name the image file");
        }
        color(issues, path, "color", &self.color);
        within(issues, path, "opacity", self.opacity, 0.0, 1.0);
    }
}

/// Colour grading with a lookup table (OBS's Apply LUT).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ApplyLut {
    /// A .cube file, or a .png LUT (512×512 tiles or a strip): absolute,
    /// `~/...`, or relative to the setup file.
    pub path: String,
    /// 0 (off) to 1 (full).
    #[serde(default = "one")]
    pub amount: f32,
}

impl Check for ApplyLut {
    fn check(&self, path: &str, issues: &mut Issues) {
        let lower = self.path.to_ascii_lowercase();
        if !lower.ends_with(".cube") && !lower.ends_with(".png") {
            issues.add(format!("{path}.path"), "must be a .cube or .png LUT");
        }
        within(issues, path, "amount", self.amount, 0.0, 1.0);
    }
}

/// Shows the picture later, to line it up with sound that arrives late
/// (OBS's Render Delay). Recordings only: a snapshot shows the moment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RenderDelay {
    /// 0 to 500 milliseconds.
    #[serde(default)]
    pub delay_ms: u32,
}

impl Check for RenderDelay {
    fn check(&self, path: &str, issues: &mut Issues) {
        within(issues, path, "delay_ms", self.delay_ms as f32, 0.0, 500.0);
    }
}

/// Report `field` of the filter at `path` unless it is from `min` to `max`.
pub(crate) fn within(issues: &mut Issues, path: &str, field: &str, value: f32, min: f32, max: f32) {
    if !(min..=max).contains(&value) {
        issues.add(
            format!("{path}.{field}"),
            format!("{value} is out of range; use {min} to {max}"),
        );
    }
}

fn color(issues: &mut Issues, path: &str, field: &str, value: &Color) {
    if value.rgba().is_none() {
        issues.add(
            format!("{path}.{field}"),
            format!(
                "{:?} is not a colour; use #rgb, #rrggbb or #rrggbbaa",
                value.0
            ),
        );
    }
}

pub(crate) fn yes() -> bool {
    true
}

pub(crate) fn is_true(value: &bool) -> bool {
    *value
}

fn one() -> f32 {
    1.0
}

fn green() -> Color {
    Color("#00ff00".into())
}

fn sharpness() -> f32 {
    0.08
}

fn chroma_similarity() -> u32 {
    400
}

fn chroma_smoothness() -> u32 {
    80
}

fn chroma_spill() -> u32 {
    100
}

fn color_similarity() -> u32 {
    80
}

fn color_smoothness() -> u32 {
    50
}

fn white() -> Color {
    Color("#ffffff".into())
}

fn black() -> Color {
    Color("#000000".into())
}

macro_rules! filter_kinds {
    ($($name:literal => $variant:ident($settings:ident),)*) => {
        /// What a filter does, by `kind`.
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
        #[serde(tag = "kind")]
        pub enum FilterKind {
            $(#[serde(rename = $name)] $variant($settings),)*
        }

        impl FilterKind {
            /// Every kind's name, as setups write it.
            pub const NAMES: &[&str] = &[$($name),*];

            /// Where `value`, an object of the kind named `name`, is wrong
            /// in that kind's fields (see [`crate::located`]).
            pub(crate) fn locate(
                name: &str,
                value: &serde_json::Value,
            ) -> Option<crate::located::Located> {
                match name {
                    $($name => crate::located::from_value::<$settings>(value).err(),)*
                    _ => None,
                }
            }

            /// The kind's name, as setups write it.
            pub fn label(&self) -> &'static str {
                match self {
                    $(Self::$variant(_) => $name,)*
                }
            }

            /// Check the kind's settings for the filter at `path`.
            pub(crate) fn check(&self, path: &str, issues: &mut Issues) {
                match self {
                    $(Self::$variant(settings) => settings.check(path, issues),)*
                }
            }

            /// The kind's own fields, as setups write them.
            pub fn fields(&self) -> &'static [String] {
                static FIELDS: std::sync::OnceLock<Vec<Vec<String>>> = std::sync::OnceLock::new();
                let table = FIELDS.get_or_init(|| vec![$(crate::fields::names::<$settings>()),*]);
                let index = Self::NAMES.iter().position(|name| *name == self.label());
                &table[index.unwrap_or_default()]
            }
        }
    };
}

filter_kinds! {
    "color_correction" => ColorCorrection(ColorCorrection),
    "chroma_key" => ChromaKey(ChromaKey),
    "color_key" => ColorKey(ColorKey),
    "luma_key" => LumaKey(LumaKey),
    "crop_pad" => CropPad(CropPad),
    "scaling" => Scaling(Scaling),
    "scroll" => Scroll(Scroll),
    "sharpen" => Sharpen(Sharpen),
    "image_mask" => ImageMask(ImageMask),
    "lut" => ApplyLut(ApplyLut),
    "render_delay" => RenderDelay(RenderDelay),
}
