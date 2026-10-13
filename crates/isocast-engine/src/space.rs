//! How much disk a recording needs.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::time::Instant;

use isocast_protocol::Codec;

use crate::{VideoTrackSpec, WriterSpec};

/// How long an open-ended recording must have room for before it starts.
pub const MIN_OPEN_ENDED_SECONDS: f64 = 10.0 * 60.0;

/// AAC bit rate per channel.
const AUDIO_BITS_PER_CHANNEL: f64 = 128_000.0;

/// A generous estimate of a file's data rate, in bytes per second. Real
/// files are usually far smaller (a still screen compresses to almost
/// nothing); this is for deciding whether a recording fits.
pub fn bytes_per_second(spec: &WriterSpec) -> f64 {
    let audio: f64 = spec
        .audio
        .iter()
        .map(|track| track.channels as f64 * AUDIO_BITS_PER_CHANNEL)
        .sum();
    (video_bits_per_second(&spec.video) + audio) / 8.0
}

fn video_bits_per_second(video: &VideoTrackSpec) -> f64 {
    let pixels_per_second = video.width as f64 * video.height as f64 * video.fps as f64;
    // Apple's published ProRes rates for 1920×1080 at 29.97 fps, scaled
    // by pixel rate.
    let prores = |mbps: f64| mbps * 1e6 * pixels_per_second / (1920.0 * 1080.0 * 29.97);
    // Bits per pixel for HEVC by quality: about 0.12 at quality 70, the
    // worst case for busy camera footage.
    let q = video.encoder.quality.value() as f64 / 100.0;
    let hevc = pixels_per_second * (0.02 + 0.3 * q * q * q);
    match video.encoder.codec {
        Codec::Hevc => hevc,
        Codec::H264 => hevc * 1.5,
        Codec::Prores422Proxy => prores(45.0),
        Codec::Prores422Lt => prores(102.0),
        Codec::Prores422 => prores(147.0),
        Codec::Prores422Hq => prores(220.0),
        Codec::Prores4444 => prores(330.0),
    }
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
