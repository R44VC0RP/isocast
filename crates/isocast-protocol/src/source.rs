//! Source kinds: each kind's settings and checks, registered in the
//! `source_kinds!` list at the bottom.
//!
//! To add a kind: write its settings struct here (doc comments become the
//! JSON Schema), implement [`Kind`] for it, and add it to the list. Then
//! give the engine a way to open it (`isocast_engine::source`).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Color, Issues};

/// What every source kind's settings provide.
pub(crate) trait Kind {
    /// It makes pictures.
    const VIDEO: bool = false;
    /// It makes sound.
    const AUDIO: bool = false;

    /// It makes sound with these settings.
    fn audio(&self) -> bool {
        Self::AUDIO
    }

    /// Check the settings of the source at `path` (like `sources[2]`).
    fn check(&self, _path: &str, _issues: &mut Issues) {}
}

/// A whole display (OBS's macOS Screen Capture, display mode).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DisplaySource {
    /// "main", or a display's name or id from `isocast devices`.
    #[serde(default = "main")]
    pub display: String,
    /// Show the mouse pointer.
    #[serde(default = "yes")]
    pub cursor: bool,
}

impl Kind for DisplaySource {
    const VIDEO: bool = true;
}

/// One window (OBS's macOS Screen Capture, window mode), matched by its
/// app and optionally its title; the largest match wins. It keeps its size
/// if the window is resized, scaling the window to fit. On Linux the first
/// start asks for the window in the desktop's sharing dialog instead, and
/// the source reopens that window by its id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WindowSource {
    /// The app's name or bundle id, or part of the name.
    pub app: String,
    /// Part of the window's title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Show the mouse pointer.
    #[serde(default = "yes")]
    pub cursor: bool,
}

impl Kind for WindowSource {
    const VIDEO: bool = true;
}

/// Every window of an app on a display (OBS's macOS Screen Capture,
/// application mode), at the display's size.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ApplicationSource {
    /// The app's name or bundle id, or part of the name.
    pub app: String,
    /// "main", or a display's name or id.
    #[serde(default = "main")]
    pub display: String,
    /// Show the mouse pointer.
    #[serde(default = "yes")]
    pub cursor: bool,
}

impl Kind for ApplicationSource {
    const VIDEO: bool = true;
}

/// The sound of one app (OBS's Application Audio Capture).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ApplicationAudioSource {
    /// A stable app id from `devices`, or the app's name or bundle id, or part of
    /// the name on macOS. macOS matches substrings; Linux names and executable
    /// aliases match exactly, case-insensitively (stable ids are case-sensitive).
    pub app: String,
}

impl Kind for ApplicationAudioSource {
    const AUDIO: bool = true;
}

/// A camera, capture card, Continuity Camera, or an iPhone or iPad screen
/// over USB. Without `width`/`height`/`fps`, the camera's best format is
/// used: the highest frame rate up to the canvas fps, then the largest size
/// that fits on the canvas either way round (else the smallest larger one).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CameraSource {
    /// An id from `isocast devices`, a name, or part of a name (the
    /// shortest name containing it wins).
    pub device: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
}

impl Kind for CameraSource {
    const VIDEO: bool = true;

    fn check(&self, path: &str, issues: &mut Issues) {
        if self.width.is_some() != self.height.is_some() {
            issues.add(
                format!(
                    "{path}.{}",
                    if self.width.is_none() {
                        "width"
                    } else {
                        "height"
                    }
                ),
                "give both width and height, or neither for the camera's best size",
            );
        }
    }
}

/// A microphone or audio interface input.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MicrophoneSource {
    /// "default", or an id, name or part of a name from `isocast devices`.
    #[serde(default = "default")]
    pub device: String,
    /// Which of the device's input channels to record, numbered from 1 as
    /// `isocast devices` lists them: one (`[1]`, a mic on input 1) is
    /// recorded as centred mono at full level; two (`[1, 2]`, `[3, 4]`) as
    /// left and right. Absent records inputs 1 and 2 as left and right (a
    /// 1-input device's input as one channel), on larger interfaces too;
    /// pick other inputs here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<Vec<u32>>,
}

impl MicrophoneSource {
    /// A source for `device` with its default channels.
    pub fn new(device: impl Into<String>) -> Self {
        Self {
            device: device.into(),
            channels: None,
        }
    }
}

impl Kind for MicrophoneSource {
    const AUDIO: bool = true;

    fn check(&self, path: &str, issues: &mut Issues) {
        let Some(channels) = &self.channels else {
            return;
        };
        let path = format!("{path}.channels");
        if !(1..=2).contains(&channels.len()) {
            issues.add(
                path,
                format!(
                    "give one channel (like [1], recorded as mono) or two (like [1, 2], \
                     recorded as stereo), not {}; or leave it out for the device's first two",
                    channels.len()
                ),
            );
            return;
        }
        for (i, channel) in channels.iter().enumerate() {
            if *channel == 0 {
                issues.add(
                    format!("{path}[{i}]"),
                    "channels are numbered from 1, as `isocast devices` lists them",
                );
            }
        }
        if channels.len() == 2 && channels[0] == channels[1] {
            issues.add(
                format!("{path}[1]"),
                format!(
                    "channel {} is given twice; give it once ([{}]) for mono at full level",
                    channels[0], channels[0]
                ),
            );
        }
    }
}

/// Audio the computer is playing, optionally leaving some apps out.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SystemAudioSource {
    /// Apps whose sound is left out: names or bundle ids, or parts of
    /// names, as for `application_audio`'s `app` (`isocast devices` lists
    /// the apps that have opened audio).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
}

impl Kind for SystemAudioSource {
    const AUDIO: bool = true;

    fn check(&self, path: &str, issues: &mut Issues) {
        for (i, app) in self.exclude.iter().enumerate() {
            if app.trim().is_empty() {
                issues.add(
                    format!("{path}.exclude[{i}]"),
                    "name an app (a name, bundle id or part of a name), or remove the entry",
                );
            }
        }
    }
}

/// A solid colour (OBS's Color Source).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ColorSource {
    /// `#rgb`, `#rrggbb` or `#rrggbbaa`.
    pub color: Color,
    /// Size in pixels; the canvas size if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

impl Kind for ColorSource {
    const VIDEO: bool = true;

    fn check(&self, path: &str, issues: &mut Issues) {
        if self.color.rgba().is_none() {
            issues.add(
                format!("{path}.color"),
                format!(
                    "{:?} is not a colour; use #rgb, #rrggbb or #rrggbbaa",
                    self.color.0
                ),
            );
        }
        check_size(path, self.width, self.height, issues);
    }
}

/// An image file: PNG, JPEG, GIF (animated ones play), WebP, BMP, TGA or
/// SVG.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ImageSource {
    /// The file: absolute, `~/...`, or relative to the setup file.
    pub path: String,
    /// For SVG, the size to draw it at; its own size if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

impl Kind for ImageSource {
    const VIDEO: bool = true;

    fn check(&self, path: &str, issues: &mut Issues) {
        if self.path.is_empty() {
            issues.add(format!("{path}.path"), "give the image file");
        }
        check_size(path, self.width, self.height, issues);
    }
}

/// Images shown one after another (OBS's Image Slide Show), each fitted
/// inside the slide show's size and centred.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SlideshowSource {
    /// Image files, in order: absolute, `~/...`, or relative to the setup
    /// file.
    pub files: Vec<String>,
    /// How long each image shows, in milliseconds.
    #[serde(default = "slide_ms")]
    pub slide_ms: u32,
    /// How one image changes to the next (a fade by default).
    #[serde(default = "slide_transition")]
    pub transition: crate::Transition,
    /// Start again after the last image (else stay on it).
    #[serde(default = "yes", rename = "loop")]
    pub repeat: bool,
    /// Show the images in a random order.
    #[serde(default)]
    pub shuffle: bool,
    /// Size in pixels; the first image's size if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

impl Kind for SlideshowSource {
    const VIDEO: bool = true;

    fn check(&self, path: &str, issues: &mut Issues) {
        if self.files.is_empty() {
            issues.add(format!("{path}.files"), "give at least one image file");
        }
        if self.slide_ms < 100 {
            issues.add(format!("{path}.slide_ms"), "must be at least 100");
        }
        if !matches!(
            self.transition.kind,
            crate::TransitionKind::Cut | crate::TransitionKind::Fade
        ) {
            issues.add(
                format!("{path}.transition.kind"),
                format!(
                    "slide shows change images with a cut or a fade, not a {}",
                    self.transition.kind.label()
                ),
            );
        }
        self.transition
            .check_unknown(&format!("{path}.transition"), issues);
        if !self.transition.is_cut() && self.transition.duration_ms >= self.slide_ms {
            issues.add(
                format!("{path}.transition.duration_ms"),
                "must be shorter than slide_ms",
            );
        }
        check_size(path, self.width, self.height, issues);
    }
}

/// Text (OBS's Text source), from `text`, a file or a data key, in a font,
/// with an optional outline, shadow and background.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TextSource {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Or a file to read the text from, re-read when it changes: absolute,
    /// `~/...`, or relative to the setup file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Or a data key whose value it shows, drawn again when the value
    /// changes (`isocast data set KEY VALUE`, MCP set_data): strings as
    /// they are, numbers formatted, anything else as JSON; nothing while
    /// the key isn't set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    /// A font family installed on this Mac, like "Helvetica Neue" or
    /// "Menlo".
    #[cfg_attr(
        windows,
        schemars(description = "A font family installed on this PC, such as Segoe UI or Arial.")
    )]
    #[serde(default = "font")]
    pub font: String,
    /// Font size in pixels.
    #[serde(default = "text_size")]
    pub size: f32,
    /// 100 (thin) to 900 (black); 400 is regular, 700 bold.
    #[serde(default = "regular")]
    pub weight: u16,
    #[serde(default)]
    pub italic: bool,
    #[serde(default = "white")]
    pub color: Color,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outline: Option<TextOutline>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow: Option<TextShadow>,
    /// A colour behind the text box.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<Color>,
    #[serde(default)]
    pub align: TextAlign,
    /// Wrap lines at this width in pixels; otherwise each line of the text
    /// is one line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wrap_width: Option<u32>,
    /// Space around the text, in pixels.
    #[serde(default)]
    pub padding: u32,
    /// Line spacing, as a multiple of the font size.
    #[serde(default = "line_height")]
    pub line_height: f32,
    /// A fixed size in pixels; the text's own size if absent. Set it for
    /// text that changes, so the source keeps one size. `align` places
    /// the lines across this width, inside the padding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TextOutline {
    /// Width in pixels.
    pub width: f32,
    pub color: Color,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TextShadow {
    /// Offset in pixels.
    #[serde(default = "two")]
    pub x: f32,
    #[serde(default = "two")]
    pub y: f32,
    pub color: Color,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TextAlign {
    #[default]
    Left,
    Center,
    Right,
}

impl Kind for TextSource {
    const VIDEO: bool = true;

    fn check(&self, path: &str, issues: &mut Issues) {
        let given = [
            self.text.is_some(),
            self.file.is_some(),
            self.data.is_some(),
        ];
        if given.into_iter().filter(|given| *given).count() != 1 {
            issues.add(path, "give one of text, file or data");
        }
        if let Some(problem) = self.data.as_deref().and_then(crate::data::key_problem) {
            issues.add(format!("{path}.data"), problem);
        }
        if !(1.0..=1000.0).contains(&self.size) {
            issues.add(format!("{path}.size"), "must be from 1 to 1000 pixels");
        }
        if !(100..=900).contains(&self.weight) {
            issues.add(format!("{path}.weight"), "must be from 100 to 900");
        }
        if !(0.5..=3.0).contains(&self.line_height) {
            issues.add(format!("{path}.line_height"), "must be from 0.5 to 3");
        }
        let mut colors = vec![("color", &self.color)];
        colors.extend(self.outline.as_ref().map(|o| ("outline.color", &o.color)));
        colors.extend(self.shadow.as_ref().map(|s| ("shadow.color", &s.color)));
        colors.extend(self.background.as_ref().map(|c| ("background", c)));
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
        if self
            .outline
            .as_ref()
            .is_some_and(|o| !(0.0..=100.0).contains(&o.width))
        {
            issues.add(
                format!("{path}.outline.width"),
                "must be from 0 to 100 pixels",
            );
        }
        if self.wrap_width == Some(0) {
            issues.add(format!("{path}.wrap_width"), "must be at least 1");
        }
        check_size(path, self.width, self.height, issues);
    }
}

fn font() -> String {
    "Helvetica Neue".into()
}

fn text_size() -> f32 {
    72.0
}

fn regular() -> u16 {
    400
}

fn white() -> Color {
    Color("#ffffff".into())
}

fn line_height() -> f32 {
    1.2
}

fn two() -> f32 {
    2.0
}

/// Video and audio files (OBS's Media Source): one file, or a playlist
/// played in order. Its sound goes into the mixer like any audio source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MediaSource {
    /// Files to play, in order: absolute, `~/...`, or relative to the setup
    /// file. MOV, MP4, M4A, MP3, WAV and anything else macOS can decode.
    pub files: Vec<String>,
    /// Start again after the last file (else hold its last frame).
    #[serde(default, rename = "loop")]
    pub repeat: bool,
    /// Playback speed, 0.25 to 4; sound changes pitch with it.
    #[serde(default = "one_speed")]
    pub speed: f32,
    /// Play from the start each time the program shows it, and not while
    /// it is off air (else it plays all along). Media without video is on
    /// air while a scene on air has an item for it; with no item in any
    /// scene it always plays.
    #[serde(default = "yes")]
    pub restart_on_activate: bool,
}

impl Kind for MediaSource {
    const VIDEO: bool = true;
    const AUDIO: bool = true;

    fn check(&self, path: &str, issues: &mut Issues) {
        if self.files.is_empty() {
            issues.add(format!("{path}.files"), "give at least one media file");
        }
        if !(0.25..=4.0).contains(&self.speed) {
            issues.add(format!("{path}.speed"), "must be from 0.25 to 4");
        }
    }
}

fn one_speed() -> f32 {
    1.0
}

/// A web page (OBS's Browser Source), rendered by WebKit at a set size,
/// transparent where the page is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BrowserSource {
    /// An http(s) or file URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Or a local HTML file: absolute, `~/...`, or relative to the setup
    /// file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// The page's size in CSS pixels.
    #[serde(default = "browser_width")]
    pub width: u32,
    #[serde(default = "browser_height")]
    pub height: u32,
    /// Frames drawn per second.
    #[serde(default = "browser_fps")]
    pub fps: u32,
    /// CSS added to the page (OBS's default makes the page transparent).
    #[serde(default = "browser_css")]
    pub css: String,
    /// Send the page's sound to the mixer as this source's audio, instead
    /// of playing it aloud (OBS's "Control audio via OBS").
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub control_audio: bool,
    /// Unload the page while the program doesn't show it, and load it
    /// afresh when it does (OBS's "Shutdown source when not visible").
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shutdown_when_hidden: bool,
    /// Reload the page each time the program shows it again (OBS's
    /// "Refresh browser when scene becomes active").
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub refresh_when_shown: bool,
}

impl Kind for BrowserSource {
    const VIDEO: bool = true;

    fn audio(&self) -> bool {
        self.control_audio
    }

    fn check(&self, path: &str, issues: &mut Issues) {
        match (&self.url, &self.file) {
            (Some(url), None) => {
                if !["http://", "https://", "file://"]
                    .iter()
                    .any(|p| url.starts_with(p))
                {
                    issues.add(
                        format!("{path}.url"),
                        "must start with http://, https:// or file://",
                    );
                }
            }
            (None, Some(_)) => {}
            _ => issues.add(path, "give either url or file"),
        }
        check_size(path, Some(self.width), Some(self.height), issues);
        if !(1..=60).contains(&self.fps) {
            issues.add(format!("{path}.fps"), "must be from 1 to 60");
        }
    }
}

fn browser_width() -> u32 {
    800
}

fn browser_height() -> u32 {
    600
}

fn browser_fps() -> u32 {
    30
}

pub(crate) fn browser_css() -> String {
    "body { background-color: rgba(0, 0, 0, 0); margin: 0px auto; overflow: hidden; }".into()
}

/// Width and height come together, from 1 to 8192.
fn check_size(path: &str, width: Option<u32>, height: Option<u32>, issues: &mut Issues) {
    match (width, height) {
        (Some(w), Some(h)) if (1..=8192).contains(&w) && (1..=8192).contains(&h) => {}
        (None, None) => {}
        (Some(w), Some(_)) => issues.add(
            format!(
                "{path}.{}",
                if !(1..=8192).contains(&w) {
                    "width"
                } else {
                    "height"
                }
            ),
            "width and height must be from 1 to 8192",
        ),
        _ => issues.add(
            format!(
                "{path}.{}",
                if width.is_none() { "width" } else { "height" }
            ),
            "give both width and height in one source patch, or neither",
        ),
    }
}

fn slide_ms() -> u32 {
    8000
}

fn slide_transition() -> crate::Transition {
    crate::Transition::fade(700)
}

fn yes() -> bool {
    true
}

/// Synthetic video that needs no permissions: moving colour bars, a frame
/// counter and a burnt-in clock. It flashes for 100 ms at every whole
/// second of the session clock, in step with `tone` beeps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TestPatternSource {
    #[serde(default = "pattern_width")]
    pub width: u32,
    #[serde(default = "pattern_height")]
    pub height: u32,
    #[serde(default = "pattern_fps")]
    pub fps: u32,
}

impl Default for TestPatternSource {
    fn default() -> Self {
        Self {
            width: pattern_width(),
            height: pattern_height(),
            fps: pattern_fps(),
        }
    }
}

impl Kind for TestPatternSource {
    const VIDEO: bool = true;

    fn check(&self, path: &str, issues: &mut Issues) {
        for (field, value) in [("width", self.width), ("height", self.height)] {
            if value == 0 || value % 2 != 0 || value > 8192 {
                issues.add(
                    format!("{path}.{field}"),
                    format!("{value} must be an even number from 2 to 8192"),
                );
            }
        }
        if !(1..=240).contains(&self.fps) {
            issues.add(format!("{path}.fps"), "fps must be between 1 and 240");
        }
    }
}

/// Synthetic stereo audio that needs no permissions: a 100 ms beep at
/// every whole second of the session clock.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToneSource {
    /// Beep pitch in Hz.
    #[serde(default = "tone_frequency")]
    pub frequency: u32,
}

impl Default for ToneSource {
    fn default() -> Self {
        Self {
            frequency: tone_frequency(),
        }
    }
}

impl Kind for ToneSource {
    const AUDIO: bool = true;

    fn check(&self, path: &str, issues: &mut Issues) {
        if !(20..=20_000).contains(&self.frequency) {
            issues.add(
                format!("{path}.frequency"),
                format!("{} Hz is out of range; use 20–20000", self.frequency),
            );
        }
    }
}

fn main() -> String {
    "main".into()
}

fn default() -> String {
    "default".into()
}

pub(crate) fn pattern_width() -> u32 {
    1920
}

pub(crate) fn pattern_height() -> u32 {
    1080
}

pub(crate) fn pattern_fps() -> u32 {
    60
}

fn tone_frequency() -> u32 {
    1000
}

macro_rules! source_kinds {
    ($($name:literal => $variant:ident($settings:ident),)*) => {
        /// What a source records, by `kind`.
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
        #[serde(tag = "kind")]
        pub enum SourceKind {
            $(#[serde(rename = $name)] $variant($settings),)*
        }

        impl SourceKind {
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

            pub fn has_video(&self) -> bool {
                match self {
                    $(Self::$variant(_) => <$settings as Kind>::VIDEO,)*
                }
            }

            pub fn has_audio(&self) -> bool {
                match self {
                    $(Self::$variant(settings) => settings.audio(),)*
                }
            }

            /// Check the kind's settings for the source at `path`.
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

source_kinds! {
    "display" => Display(DisplaySource),
    "window" => Window(WindowSource),
    "application" => Application(ApplicationSource),
    "application_audio" => ApplicationAudio(ApplicationAudioSource),
    "camera" => Camera(CameraSource),
    "microphone" => Microphone(MicrophoneSource),
    "system_audio" => SystemAudio(SystemAudioSource),
    "color" => Color(ColorSource),
    "image" => Image(ImageSource),
    "slideshow" => Slideshow(SlideshowSource),
    "text" => Text(TextSource),
    "media" => Media(MediaSource),
    "browser" => Browser(BrowserSource),
    "test_pattern" => TestPattern(TestPatternSource),
    "tone" => Tone(ToneSource),
}
