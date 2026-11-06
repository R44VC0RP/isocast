//! The session manifest: what a recording session wrote, on one clock.
//!
//! Every session writes `<date> <time>.session.json` next to its files. Each
//! file's `start_offset_seconds` places it on the session timeline, so an
//! editor can line the files up exactly without syncing by audio. The
//! manifest is written when the session starts, again once every file has
//! started, and when it ends, so it survives a crash.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The manifest format version.
pub const MANIFEST_VERSION: u32 = 1;

/// Video interruptions observed on the take clock. This is not a second count
/// of codec drops: a missing source layer can coexist with a healthy program.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VideoRecovery {
    /// Count of detailed incidents overlapping this take. A lower bound if history_limited.
    pub incident_count: u64,
    /// The recording exhausted its bounded incident history; absence is not proof of health.
    pub history_limited: bool,
    pub incidents: Vec<VideoIncident>,
}

impl VideoRecovery {
    /// Slots after `owner`'s file ended early (a stopped ISO capture or a
    /// failed writer), at that file's rate: they're in its `dropped_frames`
    /// tail, but no encoder dropped them.
    pub fn ended_early_slots(&self, owner: &str) -> u64 {
        self.incidents
            .iter()
            .filter(|i| i.state == VideoRecoveryState::EndedEarly && i.owner_id == owner)
            .map(|i| i.lost_slots)
            .sum()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VideoRecoveryScope {
    Source,
    Output,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VideoRecoveryState {
    Interrupted,
    Recovering,
    Resumed,
    EndedEarly,
    /// The take stopped while the owner was still interrupted or recovering.
    Stopped,
    #[serde(other)]
    Unknown,
}

/// Classify at the owner, never by searching a platform error message. The
/// legacy writer cannot distinguish a storage error from a codec/drain error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VideoFaultKind {
    CaptureStopped,
    CompositorFailed,
    WriterFailed,
    EncoderFailed,
    StorageFailed,
    DeviceLost,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VideoIncident {
    /// Stable across snapshots of this recording, including user splits.
    pub incident_id: u64,
    pub scope: VideoRecoveryScope,
    /// Source id or logical recording output id, according to scope.
    pub owner_id: String,
    pub state: VideoRecoveryState,
    pub fault_kind: VideoFaultKind,
    pub attempts_started: u8,
    pub reason: String,
    /// Slot zero is the take's epoch; user pauses consume no slots.
    pub fps: u32,
    pub first_lost_slot: u64,
    /// Exclusive, through the snapshot or Stop if still unavailable.
    pub end_slot_exclusive: u64,
    /// Slots of observed unavailability, not a count of missing native samples.
    /// For an output that ended early, these slots overlap the tail counted in
    /// that file's `dropped_frames`. Do not add the two counts.
    pub lost_slots: u64,
    /// Present only for a change of physical file within this logical output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollover: Option<VideoRollover>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RolloverState {
    /// Path and origin reserved; not evidence of a playable picture.
    Pending,
    /// The first closed IDR reached a complete fragment.
    Committed,
    /// The reservation received no media and is not a file on the take timeline.
    ReservedNoMedia,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VideoRollover {
    pub state: RolloverState,
    pub part: u32,
    pub path: String,
    pub take_offset_ns: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MoviePart {
    /// One-based physical part in this output (same `source` and take).
    pub number: u32,
    /// Picture extent; the old part may carry real audio beyond its last picture.
    pub video_duration_seconds: Option<f64>,
}

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
    /// Absent in older recordings; an observed healthy take has zero incidents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_recovery: Option<VideoRecovery>,
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
    /// Camera capture metadata by source id: every camera source running
    /// when the session started, including cameras recorded only through
    /// the program. Absent when the platform reports none.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub cameras: BTreeMap<String, CameraCaptureMetadata>,
}

/// What a camera's driver said about its frames, as observed by the
/// capture over its lifetime (from when it opened until the manifest was
/// written), not counts of the frames in the encoded files.
///
/// Each `sample_*` list holds the distinct states seen for one per-sample
/// flag, at most once each: `null` (the sample didn't carry the flag),
/// `false` and `true`. It is not a per-frame log.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CameraCaptureMetadata {
    /// The platform's identifier for the device.
    pub device_id: String,
    /// The device's name.
    pub device_name: String,
    /// The interlace mode the negotiated media type declares, as the
    /// platform's raw value (Windows: `MFVideoInterlaceMode`).
    #[serde(default)]
    pub media_type_interlace_mode: u32,
    /// How interlaced fields are handled: currently always "weave", both
    /// fields kept in one frame as delivered. Weave preserves the pixels;
    /// it does not remove temporal combing, and the codec may still be
    /// lossy.
    #[serde(default)]
    pub field_handling: String,
    /// Observed delivery paths: native GPU copy/upload or GPU conversion,
    /// with an optional Media Foundation MJPEG decoding stage.
    #[serde(default)]
    pub delivery: String,
    /// Negotiated camera subtype, e.g. NV12, YUY2, RGB24 or MJPG.
    #[serde(default)]
    pub native_format: String,
    /// Media Foundation decoder output subtype (empty without decoding).
    #[serde(default)]
    pub decoded_format: String,
    /// Exact native frame-rate ratio; the recording clock can use a nominal rate.
    #[serde(default)]
    pub native_frame_rate: String,
    /// Effective upload/conversion colour interpretation; unspecified SDR
    /// cameras default to limited BT.709 for HD and BT.601 for SD.
    #[serde(default)]
    pub color_space: String,
    /// Distinct "interlaced" sample flags seen.
    #[serde(default)]
    pub sample_interlaced: Vec<Option<bool>>,
    /// Distinct "bottom field first" sample flags seen.
    #[serde(default)]
    pub sample_bottom_field_first: Vec<Option<bool>>,
    /// Distinct "single field" sample flags seen.
    #[serde(default)]
    pub sample_single_field: Vec<Option<bool>>,
    /// Distinct "repeat first field" sample flags seen.
    #[serde(default)]
    pub sample_repeat_first_field: Vec<Option<bool>>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part: Option<MoviePart>,
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
    /// How the platform admitted this file's video encoder against a
    /// shared hardware encoder budget (Windows NVENC). Absent where the
    /// platform doesn't admit encoders that way, and in older manifests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoder: Option<ManifestEncoder>,
}

/// One file's encoder admission, as decided before any file of its take
/// opened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ManifestEncoder {
    /// `planned` (budget-selected presets), `replanned` (a planned provider
    /// failed preflight), or `unplanned` (an adapter without a budget profile).
    pub admission: String,
    /// Where the throughput figures came from: `calibrated` or `reference`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default)]
    pub calibrated: bool,
    /// The adapter/driver key the figures belong to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planned_preset: Option<u32>,
    /// The preset the native session was opened and validated with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_preset: Option<u32>,
    /// The provider that opened, like `Nvenc`.
    pub backend: String,
    /// The take's estimated share of the encoder, including sessions
    /// already open beside it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_load: Option<f64>,
    /// Why the provider or its quality changed, or why budget figures were not applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn future_recovery_values_do_not_hide_the_take_or_its_gap() {
        let manifest: SessionManifest = serde_json::from_value(serde_json::json!({
            "version": 1, "setup": "studio", "status": "failed", "started_at": "2026-11-04T12:00:00Z",
            "host_time_ns": 0, "files": [], "errors": [],
            "video_recovery": {"incident_count": 1, "history_limited": false, "incidents": [{
                "incident_id": 1, "scope": "adapter", "owner_id": "screen", "state": "sealing_prefix",
                "fault_kind": "driver_reset", "attempts_started": 0, "reason": "lost device",
                "fps": 30, "first_lost_slot": 30, "end_slot_exclusive": 60, "lost_slots": 30,
                "rollover":{"state":"future_boundary","part":2,"path":"screen.part002.mov","take_offset_ns":2000000000u64}
            }]}
        })).unwrap();
        let recovery = manifest.video_recovery.unwrap();
        let incident = &recovery.incidents[0];
        assert_eq!(incident.scope, VideoRecoveryScope::Unknown);
        assert_eq!(incident.state, VideoRecoveryState::Unknown);
        assert_eq!(incident.fault_kind, VideoFaultKind::Unknown);
        assert_eq!(
            incident.rollover.as_ref().unwrap().state,
            RolloverState::Unknown
        );
        assert_eq!(
            (incident.owner_id.as_str(), incident.lost_slots),
            ("screen", 30)
        );
    }
}
