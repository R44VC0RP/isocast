//! Video and audio files decoded by the system's GStreamer: whatever
//! decoders the distro's plugins provide (VA-API hardware ones when the
//! system has them and ranks them first), converted to the engine's BGRA
//! frames and 48 kHz interleaved stereo float. Nothing is bundled; a
//! missing codec names the file's codec and the package that adds it.

use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::AppSink;
use gstreamer_video::prelude::*;
use isocast_engine::{FramePool as _, MediaInfo, MediaPart, SAMPLE_RATE};
use isocast_protocol::api::{Check, CheckStatus};

use crate::frames::{Frame, FramePool};

/// How long a file may take to show its streams (within the engine's own
/// 8 s bound on opening a source, so its reason reaches the person), and a
/// decoder to hand over its next sample, before it counts as stuck.
const PREROLL: Duration = Duration::from_secs(5);
const STALL: Duration = Duration::from_secs(10);
/// How long a file without a known length gets, playing, to report one
/// before it is refused.
const DURATION_WAIT: Duration = Duration::from_secs(1);
/// At most this many threads convert one file's pictures.
const CONVERT_THREADS: u32 = 4;
/// A frame pool exhausted for this long means frames are being kept.
const POOL_WAIT: Duration = Duration::from_secs(2);
/// Sound within this of where the last chunk ended continues it exactly:
/// it covers containers' rounding (Matroska's 1 ms). Anything further is
/// a real jump and keeps its own time, so an offset never outlives it.
const ALIGNMENT: Duration = Duration::from_millis(5);

fn init() -> Result<()> {
    static INIT: OnceLock<Result<(), String>> = OnceLock::new();
    INIT.get_or_init(|| gst::init().map_err(|err| err.to_string()))
        .clone()
        .map_err(|err| {
            anyhow!(
                "GStreamer could not start ({err}); install libgstreamer1.0-0 and \
                 gstreamer1.0-plugins-base (Debian/Ubuntu), gstreamer1 (Fedora) or gstreamer (Arch)"
            )
        })
}

/// The distro packages that add a decoder for `caps` (a caps name like
/// `video/x-h264`): Debian/Ubuntu, Fedora and Arch.
fn package_for(caps: &str) -> (&'static str, &'static str, &'static str) {
    const LIBAV: (&str, &str, &str) =
        ("gstreamer1.0-libav", "gstreamer1-plugin-libav", "gst-libav");
    const GOOD: (&str, &str, &str) = (
        "gstreamer1.0-plugins-good",
        "gstreamer1-plugins-good",
        "gst-plugins-good",
    );
    const BASE: (&str, &str, &str) = (
        "gstreamer1.0-plugins-base",
        "gstreamer1-plugins-base",
        "gst-plugins-base",
    );
    const BAD: (&str, &str, &str) = (
        "gstreamer1.0-plugins-bad",
        "gstreamer1-plugins-bad-free",
        "gst-plugins-bad",
    );
    let name = caps.split([',', ' ']).next().unwrap_or(caps);
    match name {
        "video/x-vp8" | "video/x-vp9" | "audio/x-flac" | "video/quicktime" | "video/x-matroska"
        | "video/webm" | "audio/x-wav" | "video/x-msvideo" | "application/x-id3" => GOOD,
        "audio/x-opus" | "audio/x-vorbis" | "video/x-theora" | "application/ogg"
        | "audio/x-raw" | "video/x-raw" => BASE,
        "video/x-av1" | "video/mpegts" => BAD,
        _ => LIBAV,
    }
}

/// A decoder (or demuxer) GStreamer said it lacks for a file.
struct Missing {
    description: String,
    caps: String,
}

impl Missing {
    fn from_message(message: &gst::MessageRef) -> Option<Self> {
        let missing = gstreamer_pbutils::MissingPluginMessage::parse(message).ok()?;
        // "gstreamer|1.0|app|description|decoder-video/x-h264, ..."
        let detail = missing.installer_detail().to_string();
        let what = detail.rsplit('|').next().unwrap_or_default();
        let caps = what
            .strip_prefix("decoder-")
            .or_else(|| what.strip_prefix("element-"))
            .unwrap_or(what);
        Some(Self {
            description: missing.description().to_string(),
            caps: caps.to_string(),
        })
    }

    /// For a picture or sound stream, not subtitles or data.
    fn is_media(&self) -> bool {
        self.caps.starts_with("video/") || self.caps.starts_with("audio/")
    }

    fn explain(&self, path: &str) -> String {
        let name = self.caps.split([',', ' ']).next().unwrap_or(&self.caps);
        let decodable = name
            .parse::<gst::Caps>()
            .is_ok_and(|caps| has_decoder(&caps));
        self.explain_with(path, decodable)
    }

    /// `decodable`: a decoder for the codec is installed, so what's missing
    /// is the parser that prepares the stream for it (an elementary or
    /// MPEG-TS stream); the parsers are in the "bad" plugins.
    fn explain_with(&self, path: &str, decodable: bool) -> String {
        let name = self.caps.split([',', ' ']).next().unwrap_or(&self.caps);
        if decodable {
            // The "bad" plugins hold the stream parsers (h264parse and kin).
            let (debian, fedora, arch) = package_for("video/x-av1");
            return format!(
                "{path}: GStreamer can't prepare this {name} stream for its decoder (no parser); \
                 install {debian} (Debian/Ubuntu), {fedora} (Fedora) or {arch} (Arch), or use \
                 an MP4, MOV, MKV or WebM file"
            );
        }
        let (debian, fedora, arch) = package_for(name);
        format!(
            "{path}: no GStreamer decoder for {} ({name}); install {debian} \
             (Debian/Ubuntu), {fedora} (Fedora) or {arch} (Arch), then reopen the source",
            self.description
        )
    }
}

/// Whether an autoplugged decoder accepts `caps` (any of its forms).
fn has_decoder(caps: &gst::CapsRef) -> bool {
    gst::ElementFactory::factories_with_type(gst::ElementFactoryType::DECODER, gst::Rank::MARGINAL)
        .into_iter()
        .any(|factory| factory.can_sink_any_caps(caps))
}

/// What a decoding pipeline is for: describing the file, or reading one
/// part of it.
#[derive(Clone, Copy, PartialEq)]
enum Want {
    Info,
    Part(MediaPart),
}

/// What the file showed while its pipeline prerolled.
#[derive(Default)]
struct Found {
    video: Option<(u32, u32)>,
    audio: bool,
    sink: Option<AppSink>,
}

struct Opened {
    pipeline: gst::Pipeline,
    found: Found,
    missing: Vec<Missing>,
}

impl Drop for Opened {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Whether a decoder of the part we don't want accepts `caps`: that
/// stream is then left encoded (and discarded), never decoded.
fn other_part(caps: &gst::CapsRef, want: MediaPart) -> bool {
    static DECODERS: OnceLock<(Vec<gst::ElementFactory>, Vec<gst::ElementFactory>)> =
        OnceLock::new();
    let (video, audio) = DECODERS.get_or_init(|| {
        let list = |kind| {
            gst::ElementFactory::factories_with_type(
                gst::ElementFactoryType::DECODER | kind,
                gst::Rank::MARGINAL,
            )
            .into_iter()
            .collect::<Vec<_>>()
        };
        (
            list(gst::ElementFactoryType::MEDIA_VIDEO),
            list(gst::ElementFactoryType::MEDIA_AUDIO),
        )
    });
    let others = match want {
        MediaPart::Video => audio,
        MediaPart::Audio => video,
    };
    others.iter().any(|factory| factory.can_sink_any_caps(caps))
}

fn make(name: &str) -> Result<gst::Element> {
    gst::ElementFactory::make(name).build().with_context(|| {
        let (debian, _, _) = package_for(match name {
            "videoconvert" | "audioconvert" | "audioresample" | "decodebin" | "appsink" => {
                "video/x-raw"
            }
            _ => "",
        });
        format!("GStreamer element {name} is missing; install {debian}")
    })
}

/// Link `pad` to a new chain of `names` ending in `sink`, all added to
/// `pipeline` and started.
fn link_chain(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    mut elements: Vec<gst::Element>,
) -> Result<()> {
    for element in &elements {
        pipeline.add(element)?;
    }
    gst::Element::link_many(elements.iter())?;
    let first = elements.first().context("empty chain")?;
    let sink = first.static_pad("sink").context("chain has no sink pad")?;
    pad.link(&sink).map_err(|err| anyhow!("{err:?}"))?;
    for element in elements.iter_mut().rev() {
        element.sync_state_with_parent()?;
    }
    Ok(())
}

fn discard() -> Result<gst::Element> {
    let sink = make("fakesink")?;
    sink.set_property("sync", false);
    sink.set_property("async", false);
    Ok(sink)
}

/// Open `path` with decodebin and wait until its streams are known (and,
/// for a part, its first sample is ready).
fn open(path: &Path, want: Want) -> Result<Opened> {
    init()?;
    let shown = path.display().to_string();
    if !path.exists() {
        bail!("{shown} does not exist");
    }
    let pipeline = gst::Pipeline::new();
    let source = make("filesrc")?;
    source.set_property("location", path.to_string_lossy().as_ref());
    let decode = make("decodebin")?;
    pipeline.add_many([&source, &decode])?;
    source.link(&decode)?;

    if let Want::Part(part) = want {
        // Left encoded: reading the sound never decodes the picture.
        decode.connect("autoplug-continue", false, move |values| {
            let caps = values.get(2).and_then(|v| v.get::<gst::Caps>().ok());
            let expose = caps.is_some_and(|caps| other_part(&caps, part));
            Some((!expose).to_value())
        });
    }
    let found = Arc::new(Mutex::new(Found::default()));
    let failed = Arc::new(Mutex::new(None::<String>));
    {
        let (found, failed) = (found.clone(), failed.clone());
        let pipeline = pipeline.downgrade();
        decode.connect_pad_added(move |_, pad| {
            let Some(pipeline) = pipeline.upgrade() else {
                return;
            };
            let caps = pad.current_caps().unwrap_or_else(|| pad.query_caps(None));
            let Some(structure) = caps.structure(0) else {
                return;
            };
            let name = structure.name().as_str();
            // Never panic in a GStreamer callback: unwinding across its C
            // frames aborts the engine.
            let mut found = found.lock().unwrap_or_else(|p| p.into_inner());
            let linked = (|| -> Result<()> {
                if name == "video/x-raw" && found.video.is_none() {
                    let info = gstreamer_video::VideoInfo::from_caps(&caps)?;
                    found.video = Some((info.width(), info.height()));
                    if want == Want::Part(MediaPart::Video) {
                        let convert = make("videoconvert")?;
                        // Large pictures convert on several cores, bounded.
                        let cores =
                            std::thread::available_parallelism().map_or(1, |n| n.get() as u32);
                        convert.set_property("n-threads", cores.min(CONVERT_THREADS));
                        let sink = AppSink::builder()
                            .caps(
                                &gst::Caps::builder("video/x-raw")
                                    .field("format", "BGRA")
                                    .build(),
                            )
                            .sync(false)
                            .max_buffers(2)
                            .drop(false)
                            .build();
                        link_chain(&pipeline, pad, vec![convert, sink.clone().upcast()])?;
                        found.sink = Some(sink);
                        return Ok(());
                    }
                } else if name == "audio/x-raw" && !found.audio {
                    found.audio = true;
                    if want == Want::Part(MediaPart::Audio) {
                        let sink = AppSink::builder()
                            .caps(
                                &gst::Caps::builder("audio/x-raw")
                                    .field("format", "F32LE")
                                    .field("layout", "interleaved")
                                    .field("rate", SAMPLE_RATE as i32)
                                    .field("channels", 2i32)
                                    .build(),
                            )
                            .sync(false)
                            .max_buffers(8)
                            .drop(false)
                            .build();
                        link_chain(
                            &pipeline,
                            pad,
                            vec![
                                make("audioconvert")?,
                                make("audioresample")?,
                                sink.clone().upcast(),
                            ],
                        )?;
                        found.sink = Some(sink);
                        return Ok(());
                    }
                }
                // Every other stream is consumed, so the demuxer keeps
                // going; only the first video and audio count.
                link_chain(&pipeline, pad, vec![discard()?])
            })();
            if let Err(err) = linked {
                failed
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get_or_insert(format!("{err:#}"));
            }
        });
    }

    let bus = pipeline.bus().context("GStreamer pipeline without a bus")?;
    let mut opened = Opened {
        pipeline,
        found: Found::default(),
        missing: Vec::new(),
    };
    let mut error = None;
    if opened.pipeline.set_state(gst::State::Paused).is_err() {
        error = Some("the pipeline would not start".to_string());
    }
    let deadline = Instant::now() + PREROLL;
    while error.is_none() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            error = Some(format!("no picture or sound within {PREROLL:?}"));
            break;
        }
        let Some(message) = bus.timed_pop_filtered(
            gst::ClockTime::from_nseconds(left.as_nanos() as u64),
            &[
                gst::MessageType::AsyncDone,
                gst::MessageType::Error,
                gst::MessageType::Element,
            ],
        ) else {
            continue;
        };
        match message.view() {
            gst::MessageView::AsyncDone(_) => break,
            gst::MessageView::Error(err) => error = Some(err.error().to_string()),
            _ => {
                if let Some(missing) = Missing::from_message(&message) {
                    opened.missing.push(missing);
                }
            }
        }
    }
    if let Some(err) = failed.lock().unwrap_or_else(|p| p.into_inner()).take() {
        error.get_or_insert(err);
    }
    opened.found = std::mem::take(&mut *found.lock().unwrap_or_else(|p| p.into_inner()));
    // A decoder missing for a picture or sound fails the file, even when
    // its other part plays: a silent or blank file would be a surprise.
    if let Some(missing) = opened.missing.iter().find(|m| m.is_media()) {
        bail!(missing.explain(&shown));
    }
    if let Some(err) = error {
        bail!("could not read {shown}: {err}");
    }
    if let Want::Part(part) = want {
        if opened.found.sink.is_none() {
            bail!(
                "{shown} has no {}",
                if part == MediaPart::Video {
                    "video"
                } else {
                    "audio"
                }
            );
        }
        opened
            .pipeline
            .set_state(gst::State::Playing)
            .with_context(|| format!("could not play {shown}"))?;
    } else if opened.found.video.is_none() && !opened.found.audio {
        bail!("{shown} has no video or audio GStreamer can play");
    }
    Ok(opened)
}

pub fn media_info(path: &Path) -> Result<MediaInfo> {
    let opened = open(path, Want::Info)?;
    let length = || {
        opened
            .pipeline
            .query_duration::<gst::ClockTime>()
            .filter(|d| *d > gst::ClockTime::ZERO)
            .map(|d| Duration::from_nanos(d.nseconds()))
    };
    // Parsers of streams without an index (plain MP3, ADTS AAC) estimate
    // the length from the bitrate once data flows: ask again playing.
    // Without a length the engine would start every loop at the same
    // instant and spin.
    let mut duration = length();
    if duration.is_none() && opened.pipeline.set_state(gst::State::Playing).is_ok() {
        let deadline = Instant::now() + DURATION_WAIT;
        while duration.is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            duration = length();
        }
    }
    let duration = duration.with_context(|| {
        format!(
            "{}: duration unknown; media sources need a seekable file with a known length",
            path.display()
        )
    })?;
    Ok(MediaInfo {
        video: opened.found.video,
        audio: opened.found.audio,
        duration,
    })
}

pub fn open_media(path: &Path, part: MediaPart) -> Result<Reader> {
    let opened = open(path, Want::Part(part))?;
    let sink = opened.found.sink.clone().context("opened without a sink")?;
    Ok(Reader {
        opened,
        sink,
        path: path.display().to_string(),
        pool: None,
        next: Duration::ZERO,
        started: false,
        ended: false,
    })
}

/// One part of a file being decoded, as fast as it is read.
pub struct Reader {
    opened: Opened,
    sink: AppSink,
    path: String,
    pool: Option<FramePool>,
    /// Where the next sample would start if it carried no time.
    next: Duration,
    /// Whether a sample has been read, so `next` is known.
    started: bool,
    ended: bool,
}

impl Reader {
    /// The next decoded sample and its time in the file; `None` at its end.
    fn pull(&mut self) -> Result<Option<(gst::Sample, Option<Duration>)>> {
        if self.ended {
            return Ok(None);
        }
        let started = Instant::now();
        loop {
            if let Some(sample) = self
                .sink
                .try_pull_sample(gst::ClockTime::from_mseconds(100))
            {
                let buffer = sample.buffer();
                let segment = sample
                    .segment()
                    .and_then(|s| s.downcast_ref::<gst::ClockTime>());
                let at = match (buffer.and_then(|b| b.pts()), segment) {
                    (None, _) => None,
                    (Some(pts), None) => Some(pts),
                    (Some(pts), Some(segment)) => match segment.to_stream_time(pts) {
                        Some(at) => Some(at),
                        // Before the segment starts: dropped, unless part
                        // of it plays, which then starts at the start.
                        None => {
                            let end = pts + buffer.and_then(|b| b.duration()).unwrap_or_default();
                            match segment.start() {
                                Some(start) if end > start => segment.to_stream_time(start),
                                _ => continue,
                            }
                        }
                    },
                };
                let at = at.map(|at| Duration::from_nanos(at.nseconds()));
                return Ok(Some((sample, at)));
            }
            if self.sink.is_eos() {
                self.ended = true;
                return Ok(None);
            }
            if let Some(bus) = self.opened.pipeline.bus() {
                while let Some(message) = bus.pop_filtered(&[gst::MessageType::Error]) {
                    if let gst::MessageView::Error(err) = message.view() {
                        bail!("could not read {}: {}", self.path, err.error());
                    }
                }
            }
            if started.elapsed() > STALL {
                bail!("could not read {}: the decoder stalled", self.path);
            }
        }
    }

    fn frame(&mut self, sample: &gst::Sample) -> Result<Frame> {
        let caps = sample.caps().context("decoded picture without caps")?;
        let info = gstreamer_video::VideoInfo::from_caps(caps)?;
        let buffer = sample.buffer().context("decoded picture without data")?;
        let picture = gstreamer_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)
            .map_err(|_| anyhow!("could not map a decoded picture of {}", self.path))?;
        let (width, height) = (info.width(), info.height());
        let stride = picture.plane_stride()[0] as usize;
        let data = picture.plane_data(0)?;
        if self
            .pool
            .as_ref()
            .is_none_or(|pool| pool.size() != (width, height))
        {
            self.pool = Some(FramePool::new(width, height)?);
        }
        let pool = self.pool.as_mut().expect("pool");
        let row = width as usize * 4;
        let waited = Instant::now();
        loop {
            let frame = pool.draw(&mut |pixels, pitch| {
                for (to, from) in pixels
                    .chunks_exact_mut(pitch)
                    .zip(data.chunks(stride))
                    .take(height as usize)
                {
                    to[..row].copy_from_slice(&from[..row]);
                }
            })?;
            if let Some(frame) = frame {
                return Ok(frame);
            }
            if waited.elapsed() > POOL_WAIT {
                bail!(
                    "{}: every decoded picture is still in use after {POOL_WAIT:?}",
                    self.path
                );
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

impl isocast_engine::MediaReader for Reader {
    type Frame = Frame;

    fn next_video(&mut self) -> Result<Option<(Frame, Duration)>> {
        let Some((sample, at)) = self.pull()? else {
            return Ok(None);
        };
        let at = at.unwrap_or(self.next);
        let frame = self.frame(&sample)?;
        let length = sample
            .buffer()
            .and_then(|b| b.duration())
            .map_or(Duration::ZERO, |d| Duration::from_nanos(d.nseconds()));
        self.next = at + length;
        Ok(Some((frame, at)))
    }

    fn next_audio(&mut self) -> Result<Option<(Vec<f32>, Duration)>> {
        let Some((sample, at)) = self.pull()? else {
            return Ok(None);
        };
        let buffer = sample.buffer().context("decoded audio without data")?;
        let map = buffer.map_readable()?;
        let samples: Vec<f32> = map
            .as_slice()
            .as_chunks::<4>()
            .0
            .iter()
            .map(|bytes| f32::from_le_bytes(*bytes))
            .collect();
        // Containers round times (Matroska to the millisecond): sound
        // that nearly continues the last chunk continues it exactly, as
        // GStreamer's own audio sinks align it; a real jump is kept.
        let at = match at {
            Some(at) if self.started && at.abs_diff(self.next) < ALIGNMENT => self.next,
            Some(at) => at,
            None => self.next,
        };
        self.started = true;
        self.next =
            at + Duration::from_secs_f64((samples.len() / 2) as f64 / f64::from(SAMPLE_RATE));
        Ok(Some((samples, at)))
    }
}

/// Codecs a media source commonly holds, with the caps their decoders take.
const CODECS: &[(&str, &str)] = &[
    ("H.264", "video/x-h264"),
    ("HEVC", "video/x-h265"),
    ("VP8", "video/x-vp8"),
    ("VP9", "video/x-vp9"),
    ("AV1", "video/x-av1"),
    ("ProRes", "video/x-prores"),
    ("AAC", "audio/mpeg, mpegversion=(int)4"),
    ("MP3", "audio/mpeg, mpegversion=(int)1, layer=(int)3"),
    ("Opus", "audio/x-opus"),
    ("Vorbis", "audio/x-vorbis"),
    ("FLAC", "audio/x-flac"),
];

/// `doctor`'s check of the decoders media sources can use.
pub fn decoders_check() -> Check {
    let name = "media decoders (GStreamer)".to_string();
    if let Err(err) = init() {
        return Check {
            name,
            status: CheckStatus::Warning,
            detail: format!("{err:#}; media file sources can't play"),
            fix: Some("install GStreamer and its plugins from your distribution".into()),
        };
    }
    let decoders: Vec<gst::ElementFactory> = gst::ElementFactory::factories_with_type(
        gst::ElementFactoryType::DECODER,
        gst::Rank::MARGINAL,
    )
    .into_iter()
    .collect();
    let mut found = Vec::new();
    let mut missing = Vec::new();
    for (label, caps) in CODECS {
        let Ok(caps) = caps.parse::<gst::Caps>() else {
            continue;
        };
        let names: Vec<String> = decoders
            .iter()
            .filter(|factory| factory.can_sink_any_caps(&caps))
            .map(|factory| {
                let hardware = factory.klass().contains("Hardware");
                format!(
                    "{}{}",
                    factory.name(),
                    if hardware { " (hardware)" } else { "" }
                )
            })
            .collect();
        if names.is_empty() {
            let (debian, _, _) = package_for(caps.structure(0).map_or("", |s| s.name()));
            missing.push((label.to_string(), debian));
        } else {
            found.push(format!("{label}: {}", names.join(", ")));
        }
    }
    let (major, minor, micro, _) = gst::version();
    let mut detail = format!("GStreamer {major}.{minor}.{micro}; {}", found.join("; "));
    let fix = (!missing.is_empty()).then(|| {
        let mut packages: Vec<&str> = missing.iter().map(|(_, p)| *p).collect();
        packages.sort_unstable();
        packages.dedup();
        format!("install {} for the missing decoders", packages.join(" "))
    });
    if !missing.is_empty() {
        let labels: Vec<&str> = missing.iter().map(|(l, _)| l.as_str()).collect();
        detail.push_str(&format!("; missing: {}", labels.join(", ")));
    }
    let common = ["H.264", "AAC"]
        .iter()
        .all(|codec| !missing.iter().any(|(l, _)| l == codec));
    Check {
        name,
        status: if common {
            CheckStatus::Ok
        } else {
            CheckStatus::Warning
        },
        detail,
        fix,
    }
}

#[cfg(test)]
mod tests;
