//! A per-adapter encoder preset throughput calibration, as written by a
//! calibration run. This only reads and type-checks the file; the platform
//! that owns the encoder enforces that its policy is the one it runs (the
//! Windows backend has no JSON dependency of its own).

use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Bounded: a cache is tens of kilobytes including its raw evidence.
const MAX_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Deserialize)]
pub struct PresetCalibration {
    /// Adapter PCI ids, subsystem, revision and driver version.
    pub key: String,
    /// Completed 1080p-equivalent frames/s, in `preset_order`.
    pub h264: [f64; 3],
    pub hevc: [f64; 3],
    pub preset_order: [u32; 3],
    pub units: String,
    pub qualification: String,
    pub policy: CalibrationPolicy,
    pub binary_sha256: String,
    pub harness_sha256: String,
}

/// The encoding and admission policy the figures were measured with.
#[derive(Debug, Clone, Deserialize)]
pub struct CalibrationPolicy {
    pub tuning: String,
    pub rate_control: String,
    pub qp: u32,
    pub spatial_aq: u32,
    pub temporal_aq: bool,
    pub b_frames: u32,
    pub lookahead: bool,
    pub multipass: bool,
    pub input: String,
    pub output: String,
    pub gop_frames: u32,
    pub nominal_fps: u32,
    /// Pictures the encoder admits at once, and its submitted-plus-ready
    /// limit, during the measurement.
    pub native_in_flight: u32,
    pub pending_ready_limit: u32,
}

/// `Ok(None)` when there is no cache. Any other problem (unreadable,
/// oversized, malformed, missing fields, non-finite or non-positive
/// rates, missing provenance) is an error: never a calibration.
pub fn load(path: &Path) -> Result<Option<PresetCalibration>> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("calibration cache: cannot open"),
    };
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("calibration cache: cannot read")?;
    if bytes.len() as u64 > MAX_BYTES {
        bail!("calibration cache: larger than {MAX_BYTES} bytes");
    }
    let cache: PresetCalibration =
        serde_json::from_slice(&bytes).context("calibration cache: malformed")?;
    if cache.key.is_empty() {
        bail!("calibration cache.key: empty");
    }
    for rates in [cache.h264, cache.hevc] {
        if rates.iter().any(|rate| !rate.is_finite() || *rate <= 0.0) {
            bail!("calibration cache.h264/hevc: rates must be finite and positive");
        }
    }
    for (field, hash) in [
        ("binary_sha256", &cache.binary_sha256),
        ("harness_sha256", &cache.harness_sha256),
    ] {
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("calibration cache.{field}: provenance must be a SHA-256 hex digest");
        }
    }
    Ok(Some(cache))
}
