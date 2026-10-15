//! Finding OBS and optimizing a setup for recording, streaming or both
//! (OBS's Auto-Configuration Wizard): the params and results of
//! `obs_discover`, `speed_test` and `optimize`, and the rules that turn a
//! goal, an upload speed and a setup into the setup to benchmark. The
//! daemon runs the upload test and `tune` and applies the result.
//!
//! The rules (docs/notes.md, 2026-10-15, #46):
//! - Recording favours quality and ISO files: HEVC at the High preset (a
//!   better preset or ProRes is kept) for the program and every ISO file,
//!   and ISO files of every captured source (displays, windows, apps,
//!   cameras) if the setup lists none. `tune` then steps down only where
//!   this Mac can't keep up.
//! - Streaming sets the service and a bitrate from the upload speed: 75%
//!   of it, in 500 kb/s steps, at most the service's ceiling. The size
//!   sent follows the bitrate (1080p from 6 Mb/s at 60 fps or 4.5 Mb/s at
//!   30, 720p from 2.5 Mb/s, else 480p). Streaming alone turns ISO files off
//!   (the encoders serve the stream) and runs the canvas at 30 fps when
//!   the bitrate is under 4.5 Mb/s; Both keeps the recording's frame rate
//!   and ISO files.
//! - Without a measured upload speed (no stream key, or a service with no
//!   test mode) the bitrate is a safe 6 Mb/s, within the service's ceiling.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::api::{Applied, Tuning};
use crate::change::Difference;
use crate::stream::{StreamService, StreamSettings};
use crate::{Codec, EncoderQuality, QualityPreset, Setup, SourceKind};

/// The params of `obs_discover`, all optional.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ObsDiscoverParams {
    /// Look in this folder instead of OBS's (`~/Library/Application
    /// Support/obs-studio`, or the daemon's `$ISOCAST_OBS_FOLDER`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
}

/// What the setup is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Goal {
    Recording,
    Streaming,
    Both,
}

impl Goal {
    pub fn streams(self) -> bool {
        matches!(self, Self::Streaming | Self::Both)
    }

    pub fn records(self) -> bool {
        matches!(self, Self::Recording | Self::Both)
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Recording => "recording",
            Self::Streaming => "streaming",
            Self::Both => "both",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SpeedTestParams {
    /// The service to test against; the live setup's stream service if
    /// absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<StreamService>,
    /// Its ingest server (`rtmp://host/app`); the service's default, or the
    /// live setup's, if absent. Required for custom and Kick.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// How long to send for, 3 to 20 seconds; 8 if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds: Option<u32>,
}

/// What `speed_test` measured, or why it couldn't, and the stream bitrate
/// it recommends either way.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SpeedTest {
    pub service: StreamService,
    /// The server tested (never with the stream key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// Whether the upload was measured.
    pub measured: bool,
    /// The measured upload speed, kb/s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_kbps: Option<u32>,
    /// How: `acknowledged` (bytes the server said it received over time) or
    /// `sent` (bytes the connection took once its buffers were full).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// The test hit its ceiling (50 Mb/s): the upload is at least this.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub capped: bool,
    pub seconds: f64,
    pub bytes: u64,
    /// The stream bitrate for it, kb/s.
    pub recommended_kbps: u32,
    /// Why it wasn't measured, if it wasn't.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OptimizeParams {
    pub goal: Goal,
    /// Streaming: the service; the live setup's stream service if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<StreamService>,
    /// Streaming: its ingest server, if not the service's default (needed
    /// for custom and Kick).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// Streaming: the upload speed in kb/s (from `speed_test`, or known);
    /// measured with `speed_test` if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_kbps: Option<u32>,
    /// Benchmark writing to this folder instead of the setup's
    /// `recording.folder`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<PathBuf>,
    /// Make the result the live setup (refused while recording or
    /// streaming).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub apply: bool,
}

/// What `optimize` recommends, and what it applied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Optimized {
    pub goal: Goal,
    /// The result in a line, like "1080p60, HEVC High, 3 ISO files, 6 Mbps
    /// stream".
    pub summary: String,
    /// The optimized setup.
    pub setup: Setup,
    /// How it differs from the live setup.
    pub differences: Vec<Difference>,
    /// What `tune` measured on it, and the change it made (folded into
    /// `setup`).
    pub tuning: Tuning,
    /// The upload test, when streaming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<SpeedTest>,
    pub notes: Vec<String>,
    /// The apply, if asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied: Option<Applied>,
}

/// The test sends at most this, kb/s.
pub const SPEED_TEST_CAP_KBPS: u32 = 50_000;
/// The bitrate used when the upload isn't known.
pub const UNMEASURED_KBPS: u32 = 6000;

/// The highest video bitrate a service takes for a stream at `fps`, kb/s,
/// from their published guidelines (docs/streaming.md): Twitch 6000,
/// YouTube 12000 at 60 fps (9000 at 30), Facebook 9000, Kick 8000; a
/// custom server 12000.
pub fn service_ceiling(service: StreamService, fps: u32) -> u32 {
    match service {
        StreamService::Twitch => 6000,
        StreamService::Youtube if fps > 30 => 12_000,
        StreamService::Youtube => 9000,
        StreamService::Facebook => 9000,
        StreamService::Kick => 8000,
        StreamService::Custom => 12_000,
    }
}

/// The stream bitrate for an upload speed (75% of it, in 500 kb/s steps,
/// 1000 at least) within the service's ceiling; [`UNMEASURED_KBPS`] (within
/// it too) if the upload isn't known.
pub fn stream_bitrate(service: StreamService, upload_kbps: Option<u32>, fps: u32) -> u32 {
    let ceiling = service_ceiling(service, fps);
    match upload_kbps {
        Some(upload) => {
            let usable = (u64::from(upload) * 3 / 4 / 500 * 500) as u32;
            usable.clamp(1000, ceiling.max(1000))
        }
        None => UNMEASURED_KBPS.min(ceiling),
    }
}

/// The stream's height for a bitrate at `fps`: 1080, 720 or 480.
pub fn stream_height(kbps: u32, fps: u32) -> u32 {
    let full = if fps > 30 { 6000 } else { 4500 };
    if kbps >= full {
        1080
    } else if kbps >= 2500 {
        720
    } else {
        480
    }
}

/// The streaming part of an optimize: where to, and how fast the upload
/// is.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamChoice {
    pub service: StreamService,
    pub server: Option<String>,
    pub upload_kbps: Option<u32>,
}

/// `setup` changed for `goal` (see the module's rules), before `tune`
/// checks it, with notes on what changed.
pub fn prepare(setup: &Setup, goal: Goal, stream: Option<&StreamChoice>) -> (Setup, Vec<String>) {
    let mut out = setup.clone();
    let mut notes = Vec::new();
    if goal.records() {
        let program = &mut out.recording.program;
        program.enabled = true;
        quality_encoder(&mut program.encoder);
        let iso = &mut out.recording.iso;
        if iso.sources.is_empty() {
            iso.sources = setup
                .sources
                .iter()
                .filter(|s| {
                    matches!(
                        s.kind,
                        SourceKind::Display(_)
                            | SourceKind::Window(_)
                            | SourceKind::Application(_)
                            | SourceKind::Camera(_)
                    )
                })
                .map(|s| s.id.clone())
                .collect();
            if !iso.sources.is_empty() {
                notes.push(format!(
                    "records {} in ISO files of their own",
                    iso.sources.join(", ")
                ));
            }
        }
        iso.enabled = !iso.sources.is_empty();
        quality_encoder(&mut iso.encoder);
    } else if out.recording.iso.enabled {
        out.recording.iso.enabled = false;
        notes.push(
            "ISO files are off, so the encoders serve the stream (the program is still recorded)"
                .into(),
        );
    }
    if let (true, Some(choice)) = (goal.streams(), stream) {
        let mut kbps = stream_bitrate(choice.service, choice.upload_kbps, out.canvas.fps);
        if goal == Goal::Streaming && out.canvas.fps > 30 && kbps < 4500 {
            out.canvas.fps = 30;
            kbps = stream_bitrate(choice.service, choice.upload_kbps, 30);
            notes.push(
                "the canvas runs at 30 fps: the upload is too slow for 60 fps to look good".into(),
            );
        }
        let mut settings: StreamSettings = out.stream.clone().unwrap_or_else(|| {
            serde_json::from_value(serde_json::json!({ "service": choice.service.id() }))
                .expect("a service alone is a valid stream")
        });
        if settings.service != choice.service || choice.server.is_some() {
            settings.server = choice.server.clone();
        }
        settings.service = choice.service;
        settings.video_bitrate_kbps = kbps;
        let canvas = (out.canvas.width, out.canvas.height);
        let height = stream_height(kbps, out.canvas.fps);
        settings.width = None;
        settings.height = None;
        let (w, h) = settings.output_size(canvas);
        let short = w.min(h);
        if short > height {
            let scale = f64::from(height) / f64::from(short);
            let even = |n: u32| ((f64::from(n) * scale / 2.0).round() as u32 * 2).max(16);
            settings.width = Some(even(w));
            settings.height = Some(even(h));
        }
        out.stream = Some(settings);
        if choice.upload_kbps.is_none() {
            notes.push(format!(
                "the upload speed wasn't measured, so the stream uses {} (raise it in Settings › \
                 Stream if your connection allows)",
                mbps(kbps)
            ));
        }
    }
    (out, notes)
}

/// Favour quality: HEVC at the High preset, keeping ProRes or a better
/// preset.
fn quality_encoder(encoder: &mut crate::Encoder) {
    if !matches!(encoder.codec, Codec::H264 | Codec::Hevc) {
        return;
    }
    encoder.codec = Codec::Hevc;
    let better = matches!(
        encoder.quality,
        EncoderQuality::Preset(QualityPreset::VeryHigh | QualityPreset::Incredible)
    ) || matches!(encoder.quality, EncoderQuality::Value(v) if v > 60);
    if !better {
        encoder.quality = EncoderQuality::Preset(QualityPreset::High);
    }
}

/// kb/s as "6 Mbps" or "3.5 Mbps".
pub fn mbps(kbps: u32) -> String {
    let value = f64::from(kbps) / 1000.0;
    if (value - value.round()).abs() < 0.05 {
        format!("{} Mbps", value.round())
    } else {
        format!("{value:.1} Mbps")
    }
}

/// A size as "1080p" (or "1080×1920" upright).
fn size_label(width: u32, height: u32) -> String {
    if height > width {
        format!("{width}×{height}")
    } else {
        format!("{height}p")
    }
}

fn codec_label(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "H.264",
        Codec::Hevc => "HEVC",
        Codec::Prores422Proxy => "ProRes 422 Proxy",
        Codec::Prores422Lt => "ProRes 422 LT",
        Codec::Prores422 => "ProRes 422",
        Codec::Prores422Hq => "ProRes 422 HQ",
        Codec::Prores4444 => "ProRes 4444",
    }
}

fn quality_label(quality: EncoderQuality) -> String {
    match quality {
        EncoderQuality::Preset(QualityPreset::Low) => "Low".into(),
        EncoderQuality::Preset(QualityPreset::Medium) => "Medium".into(),
        EncoderQuality::Preset(QualityPreset::High) => "High".into(),
        EncoderQuality::Preset(QualityPreset::VeryHigh) => "Very High".into(),
        EncoderQuality::Preset(QualityPreset::Incredible) => "Incredible".into(),
        EncoderQuality::Value(v) => format!("quality {v}"),
    }
}

/// A setup in a line, for `goal`: "1080p60, HEVC High, 3 ISO files, 6 Mbps
/// stream" (the stream's size added when it differs from the program's).
pub fn summary(setup: &Setup, goal: Goal) -> String {
    let fps = setup.canvas.fps;
    let program = &setup.recording.program;
    let (pw, ph) = program.size(&setup.canvas);
    let mut parts = Vec::new();
    if program.enabled {
        let mut first = format!("{}{fps}", size_label(pw, ph));
        if ph > pw {
            first = format!("{} at {fps} fps", size_label(pw, ph));
        }
        parts.push(first);
        let encoder = &program.encoder;
        if matches!(encoder.codec, Codec::H264 | Codec::Hevc) {
            parts.push(format!(
                "{} {}",
                codec_label(encoder.codec),
                quality_label(encoder.quality)
            ));
        } else {
            parts.push(codec_label(encoder.codec).to_string());
        }
    } else {
        parts.push(format!("{} fps", fps));
    }
    let iso = &setup.recording.iso;
    if iso.enabled && !iso.sources.is_empty() {
        let n = iso.sources.len();
        parts.push(format!("{n} ISO file{}", if n == 1 { "" } else { "s" }));
    }
    if let (true, Some(stream)) = (goal.streams(), &setup.stream) {
        let (sw, sh) = stream.output_size((setup.canvas.width, setup.canvas.height));
        let rate = mbps(stream.video_bitrate_kbps);
        if program.enabled && (sw, sh) == (pw, ph) {
            parts.push(format!("{rate} stream"));
        } else {
            parts.push(format!("{rate} {} stream", size_label(sw, sh)));
        }
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Setup {
        let text = r#"
            version = 1
            name = "T"
            [canvas]
            width = 1920
            height = 1080
            fps = 60
            [[sources]]
            id = "screen"
            kind = "display"
            display = "main"
            [[sources]]
            id = "camera"
            kind = "camera"
            device = "Cam"
            [[scenes]]
            id = "main"
            items = [{ source = "screen", rect = { x = 0, y = 0, width = 1920, height = 1080 } }]
            [recording]
            folder = "~/Movies"
            [recording.program.encoder]
            codec = "h264"
            quality = "medium"
        "#;
        toml::from_str(text).unwrap()
    }

    #[test]
    fn bitrates_follow_the_upload_within_the_ceiling() {
        assert_eq!(
            stream_bitrate(StreamService::Twitch, Some(20_000), 60),
            6000
        );
        assert_eq!(
            stream_bitrate(StreamService::Youtube, Some(20_000), 60),
            12_000
        );
        assert_eq!(
            stream_bitrate(StreamService::Youtube, Some(5_000), 60),
            3500
        );
        assert_eq!(stream_bitrate(StreamService::Custom, Some(900), 60), 1000);
        assert_eq!(stream_bitrate(StreamService::Twitch, None, 60), 6000);
        assert_eq!(stream_height(6000, 60), 1080);
        assert_eq!(stream_height(4500, 30), 1080);
        assert_eq!(stream_height(3500, 60), 720);
        assert_eq!(stream_height(2000, 30), 480);
    }

    #[test]
    fn recording_favours_quality_and_iso_files() {
        let (out, _) = prepare(&setup(), Goal::Recording, None);
        assert_eq!(out.recording.program.encoder.codec, Codec::Hevc);
        assert_eq!(out.recording.iso.sources, ["screen", "camera"]);
        assert!(out.recording.iso.enabled);
        assert!(out.validate().is_empty(), "{:?}", out.validate());
        assert_eq!(
            summary(&out, Goal::Recording),
            "1080p60, HEVC High, 2 ISO files"
        );
    }

    #[test]
    fn streaming_sizes_the_stream_to_the_upload() {
        let choice = StreamChoice {
            service: StreamService::Twitch,
            server: None,
            upload_kbps: Some(4000),
        };
        let (out, _) = prepare(&setup(), Goal::Streaming, Some(&choice));
        assert_eq!(out.canvas.fps, 30);
        let stream = out.stream.as_ref().unwrap();
        assert_eq!(stream.video_bitrate_kbps, 3000);
        assert_eq!((stream.width, stream.height), (Some(1280), Some(720)));
        assert!(!out.recording.iso.enabled);
        assert!(out.validate().is_empty(), "{:?}", out.validate());
        assert_eq!(
            summary(&out, Goal::Streaming),
            "1080p30, H.264 Medium, 3 Mbps 720p stream"
        );
        let fast = StreamChoice {
            upload_kbps: Some(30_000),
            ..choice
        };
        let (both, _) = prepare(&setup(), Goal::Both, Some(&fast));
        assert_eq!(
            summary(&both, Goal::Both),
            "1080p60, HEVC High, 2 ISO files, 6 Mbps stream"
        );
    }
}
