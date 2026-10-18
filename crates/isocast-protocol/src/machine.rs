//! Local, measured hardware capabilities, independent of a recording profile.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Codec, api::Machine};

pub const PROFILE_SCHEMA: u32 = 1;
pub const MEASUREMENT_VERSION: u32 = 3;

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
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EncoderMeasurement {
    pub codec: Codec,
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
            })
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
        format!(
            "{}: {} encoder measurements, {} graphics sizes; calibrated {} in {:.1} s",
            self.machine.chip,
            self.encoders.len(),
            self.graphics.len(),
            self.measured_at,
            self.seconds
        )
    }
}
