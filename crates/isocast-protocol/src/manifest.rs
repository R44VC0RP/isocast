//! The session manifest: what a recording session wrote, on one clock.
//!
//! Every session writes `<date> <time>.session.json` next to its files. Each
//! file's `start_offset_seconds` places it on the session timeline, so an
//! editor can line the files up exactly without syncing by audio. The
//! manifest is written when the session starts, again once every file has
//! started, and when it ends, so it survives a crash.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The manifest format version.
pub const MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionManifest {
    /// Manifest format version. Currently 1.
    pub version: u32,
    /// The setup's name.
    pub setup: String,
    pub status: SessionStatus,
    /// Local wall-clock time of the session timeline's zero, RFC 3339.
    pub started_at: String,
    /// Host clock (`mach_absolute_time`, in nanoseconds) at the timeline's
    /// zero. Comparable between sessions recorded in the same boot.
    pub host_time_ns: i64,
    /// Length of the session timeline; absent while recording.
    pub duration_seconds: Option<f64>,
    pub files: Vec<ManifestFile>,
    /// Markers added while recording, in order.
    #[serde(default)]
    pub markers: Vec<Marker>,
    /// The scene on air from each time on, starting with the scene at the
    /// start of the session.
    #[serde(default)]
    pub scenes: Vec<SceneChange>,
    /// Scene switches on the second canvas, on the same recording clock.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vertical_scenes: Vec<SceneChange>,
    /// Scene switches on each other recorded canvas (`[[canvases]]`, #84),
    /// by canvas id, on the same recording clock.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub canvas_scenes: std::collections::BTreeMap<String, Vec<SceneChange>>,
    /// Spans cut out of this take. Old manifests have no pauses.
    #[serde(default)]
    pub pauses: Vec<Pause>,
    /// Problems during recording.
    pub errors: Vec<String>,
}

/// One pause: start/end on the uncut take clock, and the resulting join on
/// the saved media timeline. An open pause has no end until resume or stop.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Pause {
    pub start_seconds: f64,
    pub end_seconds: Option<f64>,
    pub time_seconds: f64,
    /// Duration confirmed recoverable across every output and audio/video
    /// track at this pause. Null means unknown, NOT zero. The current file
    /// writer has no fragment-on-disk acknowledgement, so this stays null,
    /// including after resume. The cut time and accepted sample counts are
    /// not evidence of committed media. Legacy `crash_safe_seconds` values
    /// were inferred from cut time and are intentionally not read or emitted.
    #[serde(default)]
    pub recoverable_seconds_at_pause: Option<f64>,
}

/// A labelled moment on the session timeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Marker {
    pub time_seconds: f64,
    pub label: String,
}

/// A switch to `scene` (an id) at a moment on the session timeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SceneChange {
    pub time_seconds: f64,
    pub scene: String,
    /// How it came on air, unless it was a cut (it is fully on air at
    /// `time_seconds` plus the duration).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transition: Option<crate::Transition>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// Still recording, or the process ended without finishing (a crash).
    Recording,
    Complete,
    /// Finished, but with errors; see `errors`.
    Failed,
    /// The process or a critical worker stopped unexpectedly. Durable media
    /// was finalized where possible; null counts were not guessed.
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct InterruptedRecording {
    #[serde(default)]
    pub newly_detected: bool,
    pub manifest: std::path::PathBuf,
    pub recording: SessionManifest,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ManifestFile {
    /// File name, in the manifest's folder.
    pub path: String,
    /// The id of the video source recorded in the file.
    pub source: String,
    /// The source's kind, like `camera` or `display`.
    pub kind: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Audio tracks in order, after the video track.
    pub audio: Vec<ManifestAudio>,
    /// Where the file's first frame sits on the session timeline; absent
    /// until the file has started.
    pub start_offset_seconds: Option<f64>,
    /// Absent until the file is finished.
    pub duration_seconds: Option<f64>,
    pub frames: Option<u64>,
    /// Explicit tail repeats included in `frames`, without hiding drops.
    pub repeated_frames: Option<u64>,
    pub dropped_frames: Option<u64>,
    pub audio_dropouts: Option<u64>,
    /// Audio sources that stopped while this file recorded (a microphone
    /// unplugged, a picked input gone, an app's audio tap ended): the
    /// track that carries each is missing its audio from then on. Absent
    /// when none did, and in manifests written before it existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audio_stops: Vec<AudioStop>,
}

/// An audio source that stopped while recording, in one file's track.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AudioStop {
    /// The id of the audio source that stopped.
    pub source: String,
    /// The audio track (from 0, in `audio` order) missing its audio: the
    /// source's own track, or the program mix it was mixed into.
    pub track: u32,
    /// When its audio stopped, on the take's timeline (like markers); 0 if
    /// it had already stopped when this take began.
    pub time_seconds: f64,
    /// When it came back in this track, if the engine opened it again (only
    /// the program mix picks a reopened source up; an ISO track stays
    /// silent to the end of the recording).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_seconds: Option<f64>,
    /// Why it stopped.
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ManifestAudio {
    /// The id of the audio source.
    pub source: String,
    pub channels: u32,
}
