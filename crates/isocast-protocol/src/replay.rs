//! The replay buffer (G.2): the last seconds of the encoded program kept in
//! memory while it runs, saved to a file on request, like OBS's. Its
//! settings are `recording.replay` in a setup (the profile's), and
//! `replay_status` reports what it holds.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::Issues;

/// The longest replay the buffer can keep, in seconds.
pub const MAX_SECONDS: u32 = 300;
/// The shortest.
pub const MIN_SECONDS: u32 = 5;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReplaySettings {
    /// How many seconds of the program a save covers, from 5 to 300 (the
    /// saved file starts at the keyframe before them, so it can be up to
    /// a keyframe interval longer).
    #[serde(default = "default_seconds")]
    pub seconds: u32,
    /// The video bitrate, in kb/s, when the buffer encodes the program
    /// itself. Without it, about 0.1 bit per pixel (12 Mb/s at 1080p60),
    /// at most 40 Mb/s. While the setup has a `[stream]`, the buffer
    /// encodes as the stream does (its size, bitrate and keyframes), so a
    /// stream and the buffer share one encoder; this is then unused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_bitrate_kbps: Option<u32>,
    /// The most memory the buffer may use, in MB, from 16 to 8192. If
    /// `seconds` of the program need more (a high bitrate), the oldest
    /// seconds go and `replay_status` says the buffer is short.
    #[serde(default = "default_max_memory_mb")]
    pub max_memory_mb: u32,
}

fn default_seconds() -> u32 {
    30
}

fn default_max_memory_mb() -> u32 {
    1024
}

impl Default for ReplaySettings {
    fn default() -> Self {
        Self {
            seconds: default_seconds(),
            video_bitrate_kbps: None,
            max_memory_mb: default_max_memory_mb(),
        }
    }
}

impl ReplaySettings {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }

    /// The video bitrate the buffer encodes at on its own, for a picture of
    /// `width`×`height` at `fps`.
    pub fn bitrate_kbps(&self, width: u32, height: u32, fps: u32) -> u32 {
        self.video_bitrate_kbps.unwrap_or_else(|| {
            let bits = f64::from(width) * f64::from(height) * f64::from(fps) * 0.1;
            ((bits / 1000.0).round() as u32).clamp(1000, 40_000)
        })
    }

    pub(crate) fn check(&self, issues: &mut Issues) {
        if !(MIN_SECONDS..=MAX_SECONDS).contains(&self.seconds) {
            issues.add(
                "recording.replay.seconds",
                format!(
                    "{} is out of range; use {MIN_SECONDS} to {MAX_SECONDS} seconds",
                    self.seconds
                ),
            );
        }
        if let Some(kbps) = self.video_bitrate_kbps
            && !(500..=100_000).contains(&kbps)
        {
            issues.add(
                "recording.replay.video_bitrate_kbps",
                format!("{kbps} is out of range; use 500 to 100000 kb/s, or omit it"),
            );
        }
        if !(16..=8192).contains(&self.max_memory_mb) {
            issues.add(
                "recording.replay.max_memory_mb",
                format!("{} is out of range; use 16 to 8192 MB", self.max_memory_mb),
            );
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReplayState {
    /// Not running (never started, stopped, or ended: see `last_error`).
    #[default]
    Off,
    /// Keeping the program.
    Running,
    /// It stopped by itself: the encoder failed or the live engine
    /// stopped. `last_error` says why.
    Failed,
}

/// What `replay_status` (and the `replay` event) reports.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ReplayStatus {
    pub state: ReplayState,
    /// The seconds a save covers, as configured.
    pub seconds: u32,
    /// The seconds of program in memory now, from its oldest keyframe to
    /// the newest frame.
    #[serde(default)]
    pub buffered_seconds: f64,
    /// Memory the buffer's media uses now, and the most it may use.
    #[serde(default)]
    pub memory_bytes: u64,
    #[serde(default)]
    pub max_memory_bytes: u64,
    /// The memory limit has cut the buffer below `seconds` since it
    /// started: a save will be shorter. Lower the bitrate or raise
    /// `recording.replay.max_memory_mb`.
    #[serde(default)]
    pub memory_limited: bool,
    /// The picture kept: H.264 at this size and rate, with AAC stereo.
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default)]
    pub fps: u32,
    #[serde(default)]
    pub video_kbps: u32,
    #[serde(default)]
    pub keyframe_interval_seconds: u32,
    /// The encoder is shared with a running stream (no extra hardware
    /// encoder session for the buffer).
    #[serde(default)]
    pub shared_encoder: bool,
    /// Program frames the buffer never got (it or its encoder fell
    /// behind), since it started. The program, recordings and the stream
    /// don't lose them.
    #[serde(default)]
    pub dropped_frames: u64,
    /// Program mix chunks lost before encoding, since it started.
    #[serde(default)]
    pub audio_dropouts: u64,
    /// Files saved since it started.
    #[serde(default)]
    pub saves: u32,
    /// The newest file saved, while this daemon runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_saved: Option<ReplaySaved>,
    /// When the state last changed (local time, RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// A saved replay (`replay_save`'s result, and the `replay_saved` event).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ReplaySaved {
    /// The file: a MOV or MP4 (as `recording.program.encoder.container`),
    /// H.264 and AAC, starting on a keyframe.
    pub path: String,
    /// How long it plays.
    pub seconds: f64,
    /// When the save was asked for (local time, RFC 3339): the file ends
    /// at that moment of the program.
    pub requested_at: String,
    pub frames: u64,
    /// Frame slots in it with no picture of their own (the buffer missed
    /// them; the previous picture shows longer).
    pub dropped_frames: u64,
    pub bytes: u64,
    /// The file is shorter than `recording.replay.seconds`: the buffer
    /// hadn't run that long yet, or the memory limit cut it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub short: bool,
}
