//! What a machine can capture, and play monitored audio on, as reported by
//! `isocast devices`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Everything Isocast can capture on this machine.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Devices {
    pub displays: Vec<Display>,
    /// Windows that can be captured. Empty without screen recording
    /// permission.
    pub windows: Vec<Window>,
    pub cameras: Vec<Camera>,
    pub microphones: Vec<Microphone>,
    /// Output devices monitored sources can play through
    /// (`audio.monitoring_device`).
    #[serde(default)]
    pub audio_outputs: Vec<AudioOutput>,
    /// Apps that have opened audio, which `application_audio` sources
    /// capture and `system_audio` sources can leave out (`exclude`). Those
    /// playing now come first. Empty before macOS 14.2.
    #[serde(default)]
    pub audio_apps: Vec<AudioApp>,
    pub permissions: Permissions,
    /// Why parts of the list may be missing, and how to fix it.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Display {
    /// Stable across reboots and reconnects.
    pub id: String,
    pub name: String,
    pub main: bool,
    pub built_in: bool,
    /// Current mode, in pixels.
    pub width: u32,
    pub height: u32,
    /// Hz; 0 if the display doesn't report one.
    pub refresh_rate: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Window {
    /// Valid only while the window exists. Setups match windows by app
    /// and title instead.
    pub id: u32,
    pub app: String,
    pub bundle_id: String,
    pub title: Option<String>,
    /// Size in points.
    pub width: u32,
    pub height: u32,
}

/// An app (or a command-line tool) that has opened audio.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AudioApp {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
    /// Valid only while the app runs. Setups match apps by name or bundle
    /// id instead.
    pub pid: u32,
    /// It is playing sound now.
    pub playing: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Camera {
    /// Stable for the same device; a setup's `device` may use it or the name.
    pub id: String,
    pub name: String,
    pub model: String,
    pub manufacturer: String,
    pub kind: CameraKind,
    /// Distinct capture formats, largest first.
    pub formats: Vec<VideoFormat>,
    /// It can't deliver a picture right now, like a laptop's built-in
    /// camera with the lid closed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub suspended: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CameraKind {
    BuiltIn,
    /// USB and Thunderbolt cameras, including capture cards.
    External,
    /// An iPhone used as a camera.
    Continuity,
    /// An iPhone or iPad screen over USB.
    DeviceScreen,
    DeskView,
    /// A camera made by an app, like OBS Virtual Camera or NDI's.
    Virtual,
    Other,
}

impl CameraKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::BuiltIn => "built_in",
            Self::External => "external",
            Self::Continuity => "continuity",
            Self::DeviceScreen => "device_screen",
            Self::DeskView => "desk_view",
            Self::Virtual => "virtual",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct VideoFormat {
    pub width: u32,
    pub height: u32,
    /// Four-character pixel format code, like `420v` or `2vuy`.
    pub pixel_format: String,
    /// The highest frame rates the device offers at this size, ascending.
    pub frame_rates: Vec<f64>,
    /// The ranges of frame rates it runs at in this format, by highest
    /// first; a source's `fps` may be any rate in one.
    #[serde(default)]
    pub frame_rate_ranges: Vec<FrameRateRange>,
}

/// Frame rates from `min` to `max`, inclusive.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FrameRateRange {
    pub min: f64,
    pub max: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Microphone {
    /// Stable for the same device; a setup's `device` may use it or the name.
    pub id: String,
    pub name: String,
    pub model: String,
    pub manufacturer: String,
    /// The system's default input.
    pub default: bool,
    pub sample_rates: Vec<f64>,
    pub channels: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AudioOutput {
    /// Stable for the same device; `audio.monitoring_device` may use it or
    /// the name.
    pub id: String,
    pub name: String,
    /// The system's default output, which "default" plays through.
    pub default: bool,
    /// The rate the device runs at now, in Hz.
    pub sample_rate: f64,
    pub channels: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Permissions {
    /// Needed for displays and windows, and for system and app audio
    /// before macOS 14.2 (or while `system_audio` isn't granted).
    pub screen_recording: Permission,
    pub camera: Permission,
    pub microphone: Permission,
    /// System Audio Recording (macOS 14.2+): system and app audio through
    /// Core Audio process taps, without screen recording. Reported as
    /// granted where macOS has no such permission.
    #[serde(default)]
    pub system_audio: Permission,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    Granted,
    Denied,
    /// Never asked; recording will trigger the system prompt.
    #[default]
    NotDetermined,
    /// Blocked by a policy such as parental controls or MDM.
    Restricted,
    /// Not granted; the system doesn't say whether it was ever asked.
    NotGranted,
}

impl Permission {
    pub fn label(self) -> &'static str {
        match self {
            Self::Granted => "granted",
            Self::Denied => "denied",
            Self::NotDetermined => "not asked yet",
            Self::Restricted => "restricted",
            Self::NotGranted => "not granted",
        }
    }
}

/// One of the privacy permissions recording needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermissionKind {
    ScreenRecording,
    Camera,
    Microphone,
    /// System Audio Recording (macOS 14.2+).
    SystemAudio,
}

impl PermissionKind {
    pub const ALL: [PermissionKind; 4] = [
        Self::ScreenRecording,
        Self::Camera,
        Self::Microphone,
        Self::SystemAudio,
    ];
    pub const NAMES: &[&str] = &["screen_recording", "camera", "microphone", "system_audio"];

    pub fn label(self) -> &'static str {
        match self {
            Self::ScreenRecording => "screen recording",
            Self::Camera => "camera",
            Self::Microphone => "microphone",
            Self::SystemAudio => "system audio recording",
        }
    }
}

impl Permissions {
    pub fn get(&self, kind: PermissionKind) -> Permission {
        match kind {
            PermissionKind::ScreenRecording => self.screen_recording,
            PermissionKind::Camera => self.camera,
            PermissionKind::Microphone => self.microphone,
            PermissionKind::SystemAudio => self.system_audio,
        }
    }
}

/// The privacy permissions and what to do about each one that isn't
/// granted. Its `screen_recording`, `camera` and `microphone` are as in
/// [`Permissions`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PermissionsReport {
    #[serde(flatten)]
    pub permissions: Permissions,
    /// The app to turn on in System Settings, like "Isocast Dev".
    pub app: String,
    /// Each permission in turn: screen recording, camera, microphone,
    /// system audio recording.
    pub details: Vec<PermissionDetail>,
}

/// One permission: its state and the next step if it isn't granted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PermissionDetail {
    pub permission: PermissionKind,
    pub state: Permission,
    /// What needs it.
    pub needed_for: String,
    /// What to do next.
    pub action: PermissionAction,
    /// Opens the System Settings pane where it is granted by hand
    /// (`open_permission_settings` opens it).
    pub settings_url: String,
    /// The next step in words, if it isn't granted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    /// Granting it takes effect once isocastd restarts (`isocast restart`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub restart_after_granting: bool,
    /// The app macOS lists this permission under in System Settings (the
    /// one to turn on), like "Isocast"; it can differ by permission.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub app: String,
    /// That app's bundle, to drag into System Settings' list when it isn't
    /// listed there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_path: Option<String>,
    /// macOS refused it without showing its prompt (the app isn't allowed
    /// to ask, as Isocast 0.0.2 wasn't for the camera): asking again won't
    /// help; `next_step` says what will.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub denied_without_prompt: bool,
}

/// What to do about a permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermissionAction {
    /// Nothing: it is granted.
    None,
    /// Ask (`request_permissions`): macOS shows its prompt.
    Request,
    /// Turn it on in System Settings (`open_permission_settings`).
    OpenSettings,
    /// A policy blocks it; only the Mac's administrator can change that.
    AskAdministrator,
    /// It is granted in System Settings but takes effect only once
    /// isocastd restarts (`isocast restart`, `shutdown_if_idle`).
    Restart,
}
