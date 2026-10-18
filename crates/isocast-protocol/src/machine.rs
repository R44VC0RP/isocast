//! Local, measured hardware capabilities, independent of a recording profile.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Codec, api::Machine};

pub const PROFILE_SCHEMA: u32 = 2;
pub const MEASUREMENT_VERSION: u32 = 4;
/// Only measurements whose every session was timed at least this long may
/// back the capacity fit.
pub const FIT_MIN_SECONDS: f64 = 0.5;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MachineProfile {
    pub schema_version: u32,
    pub measurement_version: u32,
    pub app_version: String,
    /// Immutable daemon build number, so same-version updates also expire it.
    pub app_build: String,
    pub measured_at: String,
    pub machine: Machine,
    pub seconds: f64,
    pub encoders: Vec<EncoderMeasurement>,
    pub graphics: Vec<GraphicsMeasurement>,
    pub memory_copy_bytes_per_second: Option<f64>,
    /// Burst write speed of the profile's scratch volume, not necessarily
    /// the recording destination. Recording disk checks still run separately.
    pub disk_write_bytes_per_second: Option<f64>,
    /// Every planned encoder cell that has no measurement, with why: out of
    /// time, unsupported, or failed. Missing cells are never zero-cost cells.
    pub skipped: Vec<SkippedMeasurement>,
    /// Every planned encoder cell was measured (`skipped` is empty).
    pub complete: bool,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EncoderMeasurement {
    /// Which encoder path and session pool was measured.
    pub resource: EncoderResource,
    /// The native encoder backend, like `videotoolbox`.
    pub backend: String,
    /// What this measurement may be used for. Only `fit` backs capacity.
    pub evidence: Evidence,
    pub rate_control: RateControlMode,
    /// Constant-quality value (1–100) of a recording measurement.
    pub quality: Option<u8>,
    /// Target bitrate of a live measurement.
    pub bitrate_kbps: Option<u32>,
    pub codec: Codec,
    /// Requested encoder configuration, not a probe of the coded file.
    /// ProRes ignores this setting and uses its codec's native depth.
    pub bit_depth: u8,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub sessions: u32,
    /// Completed frames across all sessions, not submissions.
    pub frames: u64,
    pub seconds: f64,
    /// Aggregate completed throughput over the timed interval.
    pub frames_per_second: f64,
    /// Pictures the encoder dropped in the timed interval, where the path
    /// reports them (live); `None` when not measured.
    pub dropped: Option<u64>,
}

/// The encoder path a measurement exercised. A recording writer's speed
/// says nothing certain about the live encoder, and the reverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EncoderResource {
    /// File writer sessions, as main, vertical and ISO recordings use.
    RecordingWriter,
    /// The real-time, bitrate-limited H.264 encoder that streams use.
    LiveEncoder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    /// One of the concurrent HEVC cells the capacity fit uses, timed over a
    /// full window ([`FIT_MIN_SECONDS`]) in every session.
    Fit,
    /// An observation only; never certified capacity.
    Diagnostic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RateControlMode {
    ConstantQuality,
    ConstantBitrate,
    AverageBitrate,
}

/// A planned encoder cell without a measurement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SkippedMeasurement {
    pub label: String,
    /// Part of the required coverage (the fit cells, 4K60 HEVC 10-bit and
    /// live H.264), rather than a diagnostic cell.
    pub required: bool,
    pub reason: String,
}

impl EncoderMeasurement {
    /// Whether this may back the capacity fit: labelled fit evidence of the
    /// recording writer, timed over full windows.
    pub fn certifies_fit(&self) -> bool {
        self.evidence == Evidence::Fit
            && self.resource == EncoderResource::RecordingWriter
            && self.rate_control == RateControlMode::ConstantQuality
            && self.quality
                == Some(crate::EncoderQuality::Preset(crate::QualityPreset::High).value())
            && self.seconds >= FIT_MIN_SECONDS
            && self.frames >= 8 * u64::from(self.sessions)
    }

    fn consistent(&self) -> bool {
        let labels = !self.backend.is_empty() && self.backend.len() <= 64;
        let path = match self.resource {
            EncoderResource::RecordingWriter => {
                self.rate_control == RateControlMode::ConstantQuality
                    && self.quality.is_some_and(|q| (1..=100).contains(&q))
                    && self.bitrate_kbps.is_none()
            }
            EncoderResource::LiveEncoder => {
                self.codec == Codec::H264
                    && self.sessions == 1
                    && matches!(
                        self.rate_control,
                        RateControlMode::ConstantBitrate | RateControlMode::AverageBitrate
                    )
                    && self.quality.is_none()
                    && self
                        .bitrate_kbps
                        .is_some_and(|b| (100..=100_000).contains(&b))
                    && self.evidence == Evidence::Diagnostic
            }
        };
        let fit = self.evidence != Evidence::Fit
            || self.certifies_fit()
                && self.codec == Codec::Hevc
                && self.bit_depth == 8
                && self.sessions >= 2;
        labels && path && fit && self.dropped.is_none_or(|d| d <= 100_000)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct GraphicsMeasurement {
    pub width: u32,
    pub height: u32,
    pub frames: u32,
    pub composite_ms: f64,
    pub conversion_ms: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MachineStatus {
    pub running: bool,
    pub elapsed_seconds: f64,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<MachineProfile>,
}

impl Default for MachineStatus {
    fn default() -> Self {
        Self {
            running: false,
            elapsed_seconds: 0.0,
            summary: "Not calibrated; using the per-chip capacity table".into(),
            profile: None,
        }
    }
}

impl MachineProfile {
    /// Untrusted local files must not inject invalid costs into admission.
    pub fn valid_for(&self, machine: &Machine) -> bool {
        let positive = |n: f64| n.is_finite() && n > 0.0;
        self.schema_version == PROFILE_SCHEMA
            && self.measurement_version == MEASUREMENT_VERSION
            && self.app_version == env!("CARGO_PKG_VERSION")
            && self.machine == *machine
            && positive(self.seconds)
            && self.seconds <= 30.0
            && !self.encoders.is_empty()
            && self.encoders.len() <= 128
            && self.encoders.iter().all(|e| {
                matches!((e.width, e.height), (1920, 1080) | (3840, 2160))
                    && matches!(e.fps, 30 | 60)
                    && matches!(e.bit_depth, 8 | 10)
                    && (1..=8).contains(&e.sessions)
                    && (1..=100_000).contains(&e.frames)
                    && positive(e.seconds)
                    && e.seconds <= 30.0
                    && positive(e.frames_per_second)
                    && e.frames_per_second <= 100_000.0
                    && (e.frames as f64 / e.seconds - e.frames_per_second).abs()
                        <= e.frames_per_second * 0.01
                    && e.consistent()
            })
            && self.skipped.len() <= 128
            && self
                .skipped
                .iter()
                .all(|s| s.label.len() <= 256 && s.reason.len() <= 512)
            && self.complete == self.skipped.is_empty()
            && self.graphics.len() <= 8
            && self.graphics.iter().all(|g| {
                matches!((g.width, g.height), (1920, 1080) | (3840, 2160))
                    && (1..=10_000).contains(&g.frames)
                    && positive(g.composite_ms)
                    && positive(g.conversion_ms)
            })
            && self.memory_copy_bytes_per_second.is_none_or(positive)
            && self.disk_write_bytes_per_second.is_none_or(positive)
    }

    pub fn summary(&self) -> String {
        let fit = self.encoders.iter().filter(|e| e.certifies_fit()).count();
        let required = self.skipped.iter().filter(|s| s.required).count();
        let skipped = match (self.skipped.len(), required) {
            (0, _) => String::new(),
            (n, 0) => format!(", {n} diagnostic cells skipped"),
            (n, r) => format!(", {n} cells skipped ({r} required: partial coverage)"),
        };
        format!(
            "{}: {} encoder measurements ({fit} for the capacity fit), {} graphics sizes{skipped}; \
             calibrated {} in {:.1} s",
            self.machine.chip,
            self.encoders.len(),
            self.graphics.len(),
            self.measured_at,
            self.seconds
        )
    }
}
