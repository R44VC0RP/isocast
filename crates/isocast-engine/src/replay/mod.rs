//! The replay buffer (G.2): the last seconds of the encoded program, kept
//! in memory as compressed samples and saved to a file on request, like
//! OBS's.
//!
//! The buffer is an output of the program's shared encoder (see
//! [`crate::stream::encoder`]): while the setup has a `[stream]`, it asks
//! for the stream's format, so a stream and the buffer share one hardware
//! session; otherwise it encodes on its own (H.264 at the program file's
//! size, 1 s keyframes). Recordings encode inside their file writers and
//! can't share yet.
//!
//! It keeps whole groups of pictures: from the newest keyframe at least
//! `seconds` (plus a little slack for a save to wait) before the newest
//! frame, and the AAC frames alongside. A save waits for the moment it was
//! asked for to come out of the encoder, then writes from the keyframe
//! before `seconds` ago up to that moment, without re-encoding (see
//! [`mp4`]), under a temporary name, renamed when complete.

mod mp4;

use parking_lot::{Condvar, Mutex};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{Local, SecondsFormat};
use isocast_protocol::Container;
use isocast_protocol::replay::{ReplaySaved, ReplaySettings, ReplayState, ReplayStatus};
use isocast_protocol::stream::StreamSettings;

use crate::record::Engine;
use crate::stream::encoder::{EncoderSpec, Sink, Subscription};
use crate::stream::queue::{Kind, Media};
use crate::stream::{Ended, StreamSpec, flv};
use crate::{LiveAudioSpec, LiveVideoSpec, Platform, SAMPLE_RATE, Time};

/// Kept beyond `seconds`: a save waits for its moment to be encoded, and
/// still needs the keyframe before it.
const SLACK: Duration = Duration::from_secs(3);
/// The longest a save waits for its moment to come out of the encoder
/// (the interleaver holds pictures up to a second for their audio).
const SAVE_WAIT: Duration = Duration::from_secs(3);
/// Keyframes when the buffer encodes on its own: a save then starts at
/// most this long before `seconds` ago.
const KEYFRAME_SECONDS: u32 = 1;
/// AAC bitrate when the buffer encodes on its own.
const AUDIO_KBPS: u32 = 160;
const CHANNELS: u32 = 2;
/// Memory each sample costs beyond its data (bookkeeping).
const SAMPLE_OVERHEAD: u64 = 64;

/// Called whenever the buffer's state changes or a save completes.
pub type OnChange = Arc<dyn Fn(&ReplayStatus) + Send + Sync>;

/// The encoder the buffer asks for: the stream's when the setup has a
/// stream (so they share it), else its own.
pub fn encoder_spec<P: Platform>(
    engine: &Engine<P>,
    settings: &ReplaySettings,
    stream: Option<&StreamSettings>,
) -> EncoderSpec {
    if let Some(stream) = stream {
        return StreamSpec::of(engine, stream.clone()).encoder();
    }
    let setup = engine.setup();
    let (width, height) = setup.recording.program.size(&setup.canvas);
    let fps = setup.canvas.fps;
    EncoderSpec {
        video: LiveVideoSpec {
            width,
            height,
            fps,
            bitrate_kbps: settings.bitrate_kbps(width, height, fps),
            constant_bitrate: false,
            keyframe_interval: 2 * fps * KEYFRAME_SECONDS,
            low_latency: false,
        },
        audio: LiveAudioSpec {
            channels: CHANNELS,
            bitrate_kbps: AUDIO_KBPS,
        },
        keyframe_seconds: KEYFRAME_SECONDS,
    }
}

/// A running replay buffer. Dropping it stops it.
pub struct ReplayBuffer {
    shared: Arc<Shared>,
    now: Arc<dyn Fn() -> Time + Send + Sync>,
    /// One save at a time.
    saving: Mutex<()>,
}

struct Shared {
    spec: EncoderSpec,
    keep: Duration,
    max_bytes: u64,
    ring: Mutex<Ring>,
    arrived: Condvar,
    subscription: Mutex<Option<Subscription>>,
    missed: AtomicU64,
    encoder_dropped: AtomicU64,
    status: Mutex<ReplayStatus>,
    on_change: OnChange,
}

#[derive(Clone)]
struct Sample {
    ts: Time,
    keyframe: bool,
    data: Arc<[u8]>,
    config: Option<Arc<Vec<u8>>>,
}

impl Sample {
    fn cost(&self) -> u64 {
        self.data.len() as u64 + SAMPLE_OVERHEAD
    }
}

#[derive(Default)]
struct Ring {
    /// From a keyframe on.
    video: VecDeque<Sample>,
    audio: VecDeque<Sample>,
    bytes: u64,
    /// The newest video frame's time, and the time the newest audio frame
    /// ends.
    newest_video: Option<Time>,
    audio_end: Option<Time>,
    /// The memory limit cut the buffer below what was asked.
    memory_limited: bool,
    /// The encoder ended; nothing more comes.
    ended: bool,
}

impl Ring {
    fn push(&mut self, media: &Media, keep: Duration, max_bytes: u64) {
        let sample = Sample {
            ts: media.ts,
            keyframe: media.kind == Kind::Video { keyframe: true },
            data: Arc::from(media.data.as_slice()),
            config: media.config.clone(),
        };
        match media.kind {
            Kind::Video { keyframe } => {
                // The buffer starts at a keyframe with its configuration.
                if self.video.is_empty() && !(keyframe && sample.config.is_some()) {
                    return;
                }
                self.newest_video = Some(media.ts);
                self.bytes += sample.cost();
                self.video.push_back(sample);
            }
            Kind::Audio => {
                let frame = Duration::from_nanos(
                    1_000_000_000 * u64::from(AAC_FRAME) / u64::from(SAMPLE_RATE),
                );
                self.audio_end = Some(media.ts + frame);
                self.bytes += sample.cost();
                self.audio.push_back(sample);
            }
        }
        self.trim(keep, max_bytes);
    }

    /// The index of the second keyframe, if there is one: everything
    /// before it can go as a whole.
    fn second_keyframe(&self) -> Option<usize> {
        self.video
            .iter()
            .skip(1)
            .position(|s| s.keyframe)
            .map(|i| i + 1)
    }

    fn trim(&mut self, keep: Duration, max_bytes: u64) {
        if let Some(newest) = self.newest_video {
            while let Some(next) = self.second_keyframe() {
                let old = self.video[next].ts + keep <= newest;
                let over = self.bytes > max_bytes;
                if !old && !over {
                    break;
                }
                if over && !old {
                    self.memory_limited = true;
                }
                for sample in self.video.drain(..next) {
                    self.bytes -= sample.cost();
                }
            }
        }
        // Audio from just before the oldest picture (a frame needs the one
        // before it); without pictures yet, the last `keep`.
        let from = match (self.video.front(), self.audio_end) {
            (Some(first), _) => Some(first.ts - Duration::from_millis(100)),
            (None, Some(end)) => Some(end - keep),
            (None, None) => None,
        };
        if let Some(from) = from {
            while self.audio.front().is_some_and(|a| a.ts < from) {
                let sample = self.audio.pop_front().expect("checked");
                self.bytes -= sample.cost();
            }
        }
    }

    fn buffered(&self) -> Duration {
        match (self.video.front(), self.newest_video) {
            (Some(first), Some(newest)) => newest.saturating_since(first.ts),
            _ => Duration::ZERO,
        }
    }
}

const AAC_FRAME: u32 = 1024;

impl Sink for Shared {
    fn missed(&self, frames: u64) {
        self.missed.fetch_add(frames, Ordering::Relaxed);
    }

    fn encoder_dropped(&self, frames: u64) {
        self.encoder_dropped.fetch_add(frames, Ordering::Relaxed);
    }

    fn push(&self, ready: &[Media]) {
        let mut ring = self.ring.lock();
        for media in ready {
            ring.push(media, self.keep, self.max_bytes);
        }
        drop(ring);
        self.arrived.notify_all();
    }

    fn ended(&self, ended: &Ended) {
        // Nothing can be saved once it fails: let go of the memory now.
        *self.ring.lock() = Ring {
            ended: true,
            ..Ring::default()
        };
        self.arrived.notify_all();
        let message = match ended {
            Ended::Failed(message) => message.clone(),
            _ => "the live engine stopped, and the replay buffer with it".into(),
        };
        let status = {
            let mut status = self.status.lock();
            status.state = ReplayState::Failed;
            status.last_error = Some(message);
            status.since = Some(now());
            status.clone()
        };
        (self.on_change)(&status);
    }
}

fn now() -> String {
    Local::now().to_rfc3339_opts(SecondsFormat::Millis, false)
}

/// Start keeping `engine`'s program with `settings`. With `stream` (the
/// setup's stream settings) it encodes as the stream does, sharing a
/// running stream's encoder.
pub fn start<P: Platform>(
    engine: &Engine<P>,
    settings: &ReplaySettings,
    stream: Option<&StreamSettings>,
    on_change: OnChange,
) -> Result<ReplayBuffer> {
    let spec = encoder_spec(engine, settings, stream);
    let status = ReplayStatus {
        state: ReplayState::Running,
        seconds: settings.seconds,
        max_memory_bytes: u64::from(settings.max_memory_mb) << 20,
        width: spec.video.width,
        height: spec.video.height,
        fps: spec.video.fps,
        video_kbps: spec.video.bitrate_kbps,
        keyframe_interval_seconds: spec.keyframe_seconds,
        since: Some(now()),
        ..Default::default()
    };
    let shared = Arc::new(Shared {
        spec,
        keep: Duration::from_secs(settings.seconds.into()) + SLACK,
        max_bytes: u64::from(settings.max_memory_mb) << 20,
        ring: Mutex::default(),
        arrived: Condvar::new(),
        subscription: Mutex::new(None),
        missed: AtomicU64::new(0),
        encoder_dropped: AtomicU64::new(0),
        status: Mutex::new(status),
        on_change,
    });
    let subscription =
        engine
            .encoders()
            .subscribe(engine, &spec, "the replay buffer", shared.clone())?;
    *shared.subscription.lock() = Some(subscription);
    let platform = engine.platform().clone();
    let replay = ReplayBuffer {
        shared,
        now: Arc::new(move || platform.now()),
        saving: Mutex::new(()),
    };
    (replay.shared.on_change)(&replay.status());
    Ok(replay)
}

impl ReplayBuffer {
    /// Its state, memory and counters now.
    pub fn status(&self) -> ReplayStatus {
        let shared = &self.shared;
        let mut status = shared.status.lock().clone();
        {
            let ring = shared.ring.lock();
            status.buffered_seconds = ring.buffered().as_secs_f64();
            status.memory_bytes = ring.bytes;
            status.memory_limited = ring.memory_limited;
        }
        status.dropped_frames =
            shared.missed.load(Ordering::Relaxed) + shared.encoder_dropped.load(Ordering::Relaxed);
        if let Some(subscription) = shared.subscription.lock().as_ref() {
            status.audio_dropouts = subscription.tap_audio_dropped();
            status.shared_encoder = subscription.shared();
        }
        status
    }

    /// Whether it still keeps the program (it hasn't failed or stopped).
    pub fn is_running(&self) -> bool {
        self.shared.status.lock().state == ReplayState::Running
    }

    /// Stop keeping the program and let go of what it holds. Returns the
    /// final status.
    pub fn stop(&self) -> ReplayStatus {
        let mut status = self.status();
        // Waits for the encoder's thread when nothing else uses it.
        let subscription = self.shared.subscription.lock().take();
        drop(subscription);
        *self.shared.ring.lock() = Ring::default();
        self.shared.arrived.notify_all();
        {
            let mut current = self.shared.status.lock();
            if current.state == ReplayState::Running {
                current.state = ReplayState::Off;
                current.since = Some(now());
            }
            status.state = current.state;
            status.since = current.since.clone();
            status.last_error = current.last_error.clone();
        }
        status.buffered_seconds = 0.0;
        status.memory_bytes = 0;
        status.shared_encoder = false;
        // The final counters stay in the status kept for it.
        *self.shared.status.lock() = status.clone();
        status
    }

    /// Save what the buffer holds up to now into `folder`, as `name`
    /// (without extension) in `container`, numbered if taken. `room`
    /// checks the disk has room for the given number of bytes.
    pub fn save(
        &self,
        folder: &Path,
        name: &str,
        container: Container,
        room: &dyn Fn(u64) -> Result<()>,
    ) -> Result<ReplaySaved> {
        let _one = self.saving.lock();
        let at = (self.now)();
        let requested_at = now();
        let clip = self.clip(at)?;
        let short = clip.short;
        let clip = clip.clip;
        let size = mp4::size(&clip);
        std::fs::create_dir_all(folder)
            .with_context(|| format!("could not make the folder {}", folder.display()))?;
        room(size)?;
        let extension = match container {
            Container::Mov => "mov",
            Container::Mp4 => "mp4",
        };
        let partial = folder.join(format!(".{name}.{}.partial", std::process::id()));
        let written = mp4::write(&partial, &clip, container);
        let bytes = match written {
            Ok(bytes) => bytes,
            Err(err) => {
                let _ = std::fs::remove_file(&partial);
                return Err(err).with_context(|| {
                    format!("could not write the replay in {}", folder.display())
                });
            }
        };
        let path = match finish(&partial, folder, name, extension) {
            Ok(path) => path,
            Err(err) => {
                let _ = std::fs::remove_file(&partial);
                return Err(err);
            }
        };
        let frame_ns = 1_000_000_000 / i64::from(clip.fps.max(1));
        let dropped = clip
            .video
            .windows(2)
            .map(|w| {
                let gap = w[1].0.as_nanos() - w[0].0.as_nanos();
                ((gap + frame_ns / 2) / frame_ns - 1).max(0) as u64
            })
            .sum();
        let start = clip.video[0].0;
        let saved = ReplaySaved {
            path: path.display().to_string(),
            seconds: clip.end.saturating_since(start).as_secs_f64(),
            requested_at,
            frames: clip.video.len() as u64,
            dropped_frames: dropped,
            bytes,
            short,
        };
        {
            let mut status = self.shared.status.lock();
            status.saves += 1;
            status.last_saved = Some(saved.clone());
        }
        (self.shared.on_change)(&self.status());
        Ok(saved)
    }

    /// What a save at `at` writes: waits (up to [`SAVE_WAIT`]) for the
    /// picture showing at `at` and the audio up to its end.
    fn clip(&self, at: Time) -> Result<Picked> {
        let shared = &self.shared;
        let spec = &shared.spec;
        let frame_ns = 1_000_000_000 / i64::from(spec.video.fps.max(1));
        let want = Duration::from_secs(shared.status.lock().seconds.into());
        let deadline = Instant::now() + SAVE_WAIT;
        let mut ring = shared.ring.lock();
        // The picture showing at `at` (the newest frame at or before it)
        // is known once a later one, or one within a frame, has come.
        loop {
            if shared.subscription.lock().is_none() {
                bail!("the replay buffer is stopped; start it with `isocast replay start`");
            }
            let done = ring
                .newest_video
                .is_some_and(|t| t.as_nanos() + frame_ns > at.as_nanos());
            if done || ring.ended {
                break;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            shared.arrived.wait_for(&mut ring, left);
        }
        let Some(last) = ring.video.iter().rposition(|s| s.ts <= at) else {
            bail!(
                "the replay buffer has nothing to save yet; it starts keeping the program \
                 within a second of starting"
            );
        };
        let end = ring.video[last].ts + Duration::from_nanos(frame_ns as u64);
        // And the audio to the end of that picture.
        loop {
            let done = ring.audio_end.is_some_and(|t| t >= end);
            let left = deadline.saturating_duration_since(Instant::now());
            if done || ring.ended || left.is_zero() {
                break;
            }
            shared.arrived.wait_for(&mut ring, left);
        }
        let target = end - want;
        // The newest keyframe at or before `seconds` ago, else the oldest.
        let keyframes = || (0..=last).filter(|&i| ring.video[i].keyframe);
        let mut first = keyframes()
            .rev()
            .find(|&i| ring.video[i].ts <= target)
            .unwrap_or(0);
        let mut short = ring.video[first].ts > target;
        // One configuration per file: start after any change.
        let config = ring
            .video
            .range(..=last)
            .rev()
            .find_map(|s| s.config.clone())
            .context("the replay buffer has no decoder configuration")?;
        if let Some(changed) = keyframes()
            .rev()
            .find(|&i| ring.video[i].config.as_ref() != Some(&config))
            && changed >= first
        {
            first = keyframes()
                .find(|&i| i > changed)
                .context("the replay buffer has no keyframe to start at")?;
            short = true;
        }
        let start = ring.video[first].ts;
        let aac =
            Duration::from_nanos(1_000_000_000 * u64::from(AAC_FRAME) / u64::from(SAMPLE_RATE));
        let video: Vec<_> = ring
            .video
            .range(first..=last)
            .map(|s| (s.ts, s.keyframe, s.data.clone()))
            .collect();
        // Whole AAC frames from the one before the start (its priming) to
        // the end.
        let audio: Vec<_> = ring
            .audio
            .iter()
            .filter(|a| a.ts + aac + aac > start && a.ts < end)
            .map(|a| (a.ts, a.data.clone()))
            .collect();
        Ok(Picked {
            short,
            clip: mp4::Clip {
                width: spec.video.width,
                height: spec.video.height,
                fps: spec.video.fps,
                avc_config: config,
                video,
                end,
                audio,
                audio_config: flv::audio_specific_config(SAMPLE_RATE, CHANNELS),
                channels: CHANNELS,
                audio_kbps: spec.audio.bitrate_kbps,
            },
        })
    }
}

impl Drop for ReplayBuffer {
    fn drop(&mut self) {
        let subscription = self.shared.subscription.lock().take();
        drop(subscription);
    }
}

struct Picked {
    clip: mp4::Clip,
    short: bool,
}

/// Give the finished `partial` its name in `folder`: `name.extension`, or
/// `name 2.extension` and so on if taken. Never replaces a file.
fn finish(partial: &Path, folder: &Path, name: &str, extension: &str) -> Result<PathBuf> {
    let mut last_error = None;
    for n in 1..1000 {
        let path = match n {
            1 => folder.join(format!("{name}.{extension}")),
            n => folder.join(format!("{name} {n}.{extension}")),
        };
        // A hard link fails if the name is taken, so two saves (or
        // anything else) can't take the same name; a disk without hard
        // links (exFAT) gets a check, then a rename.
        match std::fs::hard_link(partial, &path) {
            Ok(()) => {
                std::fs::remove_file(partial).ok();
                return Ok(path);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) if !path.exists() => {
                std::fs::rename(partial, &path)
                    .with_context(|| format!("could not name the replay {}", path.display()))?;
                return Ok(path);
            }
            // Taken, on a disk without hard links (exFAT): the next name.
            Err(err) => last_error = Some(err),
        }
    }
    let message = format!(
        "could not find a free name for the replay in {}",
        folder.display()
    );
    match last_error {
        Some(err) => Err(err).context(message),
        None => bail!(message),
    }
}
