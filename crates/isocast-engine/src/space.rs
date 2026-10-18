//! How much disk a recording needs.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::time::Instant;

use crate::WriterSpec;

/// How long an open-ended recording must have room for before it starts.
pub const MIN_OPEN_ENDED_SECONDS: f64 = 10.0 * 60.0;

/// Device of an existing path or its closest existing parent. Only a local
/// calibration on this same filesystem may provide a disk-speed observation.
pub fn device(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        path.ancestors()
            .find_map(|p| fs::metadata(p).ok().map(|m| m.dev()))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// A generous estimate of a file's data rate, in bytes per second (the
/// protocol's `encoder_bytes_per_second`, which clients share). Real files
/// are usually far smaller (a still screen compresses to almost nothing);
/// this is for deciding whether a recording fits.
pub fn bytes_per_second(spec: &WriterSpec) -> f64 {
    let channels: u32 = spec.audio.iter().map(|track| track.channels).sum();
    let video = &spec.video;
    isocast_protocol::encoder_bytes_per_second(
        &video.encoder,
        video.width,
        video.height,
        video.fps,
        channels,
    )
}

/// Bytes as a short human figure, like "3.2 GB".
pub fn human(bytes: f64) -> String {
    if bytes >= 1e9 {
        format!("{:.1} GB", bytes / 1e9)
    } else {
        format!("{:.0} MB", bytes / 1e6)
    }
}

/// Size of the file written to measure a disk's speed.
const SPEED_TEST_BYTES: usize = 64 << 20;
const SPEED_TEST_CHUNK: usize = 8 << 20;

/// Measure how fast `folder` takes data, in bytes per second, by writing a
/// temporary file and waiting until the disk has it. Catches slow and
/// network volumes; it measures a short burst, so sustained speed on some
/// drives is lower.
pub fn write_speed(folder: &Path) -> std::io::Result<f64> {
    let path = folder.join(".isocast-speed-test");
    // Bytes that don't compress, in case the file system compresses.
    let mut chunk = vec![0u8; SPEED_TEST_CHUNK];
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for byte in chunk.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *byte = x as u8;
    }
    let started = Instant::now();
    let result = (|| {
        let mut file = File::create(&path)?;
        for _ in 0..SPEED_TEST_BYTES / SPEED_TEST_CHUNK {
            file.write_all(&chunk)?;
        }
        file.sync_all()
    })();
    let elapsed = started.elapsed().as_secs_f64();
    let _ = fs::remove_file(&path);
    result?;
    Ok(SPEED_TEST_BYTES as f64 / elapsed.max(1e-6))
}
