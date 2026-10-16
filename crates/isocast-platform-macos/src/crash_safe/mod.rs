//! Crash-safe recording (PLAN G.3b, GitHub #3): Isocast's own encoders
//! feeding a fragmented QuickTime/MP4 file that is playable up to its last
//! fragment at every moment, committed on demand (a pause) and finished
//! into an ordinary movie at Stop or, after a crash, at the next launch.
//!
//! On by default: `ISOCAST_CRASH_SAFE=0` temporarily selects the legacy
//! writer for every file of a recording. `ISOCAST_CRASH_SAFE_INTERVAL_MS` (default 500) is
//! the fragment length in media time, the crash-loss bound while recording;
//! `ISOCAST_CRASH_SAFE_FSYNC_SECONDS` (default 0: only on commit and Stop)
//! also flushes the file through to the drive at that cadence, checked
//! whenever a fragment is written (so it is a cadence between fragments,
//! not a wall-clock heartbeat). Every flush is `F_FULLFSYNC` (Rust's
//! `sync_data` on Apple platforms).
//!
//! Timing: a picture lasts until the next one. The newest encoded picture
//! is held back until its successor arrives (or a commit/finish releases it
//! with its nominal duration), so every fragment's `trun` carries true
//! durations and a gap where frames were dropped stays in the file, in the
//! fragments and in the finished table alike.

mod encoder;
pub mod mp4;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use isocast_engine::{
    AudioEncoder, FileWriter, Finished, LiveAudioSpec, RepairedFile, SAMPLE_RATE, Time, WriterSpec,
};
use isocast_protocol::Codec;
use objc2::rc::autoreleasepool;

use self::encoder::{SampleFormat, VideoSession};
use self::mp4::{AAC_PRIMING, FragmentTrack, MOVIE_TIMESCALE, Track, TrackKind};
use crate::PixelBuffer;
use crate::stream::AacEncoder;
use crate::writer::{
    EncoderPressure, ISO_SHARE, PENDING_ISO, PENDING_VIDEO, PRIORITY_OUTPUTS, PRIORITY_PRESSURE,
    cancel_encoder_start, claim_encoder_start, encoder_start_rank, request_encoder_start,
};

/// Frames submitted to the encoder at once.
const MAX_IN_FLIGHT: u64 = 8;
/// Bound how long queued inputs may remain stalled during Stop.
const FINISH_WAIT: Duration = Duration::from_secs(10);
/// A fragment is written when it holds this much, whatever its length
/// (ProRes at 4K60 fills it in a fraction of a second).
const FRAGMENT_BYTES: usize = 24 << 20;
/// Samples in one AAC packet.
const AAC_FRAME: u64 = 1024;
/// How long a commit waits for the encoder to take the pictures still
/// queued for it before it gives up on them (they stay queued).
const COMMIT_DRAIN: Duration = Duration::from_secs(2);

/// Whether recordings use this writer (`ISOCAST_CRASH_SAFE`).
pub fn enabled() -> bool {
    std::env::var("ISOCAST_CRASH_SAFE")
        .map(|v| !matches!(v.trim(), "0" | "false" | "no" | "off"))
        .unwrap_or(true)
}

#[derive(Clone, Copy, Debug)]
struct Config {
    interval: Duration,
    fsync_every: Option<Duration>,
}

impl Config {
    fn from_env() -> Self {
        let number = |name: &str| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|v| v.is_finite())
        };
        Self {
            interval: Duration::from_millis(
                number("ISOCAST_CRASH_SAFE_INTERVAL_MS")
                    .map_or(500, |ms| ms.clamp(50.0, 10_000.0) as u64),
            ),
            fsync_every: number("ISOCAST_CRASH_SAFE_FSYNC_SECONDS")
                .filter(|s| *s > 0.0)
                .map(|s| Duration::from_secs_f64(s.min(86_400.0))),
        }
    }
}

/// One AAC track: its encoder and what it has produced.
struct AudioTrack {
    encoder: AacEncoder,
    channels: usize,
    /// Sample frames fed to the encoder so far.
    fed: u64,
    /// Packets written to the file so far.
    written_packets: u64,
    /// Packets encoded but not yet in a fragment.
    pending: Vec<Vec<u8>>,
}

/// Samples waiting for the next fragment.
#[derive(Default)]
struct Fragment {
    /// (size, time, duration, sync) and the bytes, in order.
    video: Vec<(u32, u64, u32, bool)>,
    video_data: Vec<u8>,
    bytes: usize,
}

/// The newest encoded picture, whose duration is known only once the next
/// one arrives.
struct Held {
    data: Vec<u8>,
    time: u64,
    sync: bool,
}

/// One .mov or .mp4 file written by Isocast's own encoders and muxer.
pub struct CrashSafeWriter {
    file: Option<mp4::Writer>,
    video: VideoSession,
    audio: Vec<AudioTrack>,
    path: PathBuf,
    spec: WriterSpec,
    config: Config,
    /// Video media timescale; frame times are exact multiples of it.
    timescale: i32,
    frame_ticks: u32,
    start: Option<Time>,
    pending_video: VecDeque<(PixelBuffer, i64)>,
    /// A tail repeat preserves audio if the final video slots were refused.
    last_video: Option<(PixelBuffer, i64)>,
    priority: Option<EncoderPressure>,
    /// A native video submission exceeded its frame budget. Keep this
    /// pressure through audio-only drains until the next video submission.
    submission_stalled: bool,
    iso_credit: u32,
    fragment: Fragment,
    held: Option<Held>,
    format: Option<SampleFormat>,
    /// End of the last video sample written to the file, in ticks.
    committed_video: Option<u64>,
    /// Decode time of the newest encoded video sample.
    newest_video: Option<u64>,
    last_sync: Instant,
    fragments: u64,
    failed: Option<String>,
    /// Encoder-side drops (null or FrameDropped callbacks) already reported
    /// through `lost_frames`.
    reported_drops: u64,
    /// The first picture places this session on a hardware encoder. Use
    /// the same start gate as AVAssetWriter and the shared stream encoder.
    encoding: bool,
    start_ticket: Option<u64>,
    /// Do not refuse the pictures queued while waiting for the start gate.
    backlog: bool,
    /// The initial keyframe can return well before the following pictures.
    /// Keep the startup queue until a second encoded picture has returned.
    encoded_frames: u64,
}

// SAFETY: the VideoToolbox session and AudioConverters are used by one
// thread at a time; the file is a plain descriptor.
unsafe impl Send for CrashSafeWriter {}

impl Drop for CrashSafeWriter {
    fn drop(&mut self) {
        if !self.encoding
            && let Some(ticket) = self.start_ticket
        {
            cancel_encoder_start(ticket);
        }
    }
}

impl CrashSafeWriter {
    pub fn create(spec: &WriterSpec) -> Result<Self> {
        autoreleasepool(|_| Self::make(spec))
    }

    fn make(spec: &WriterSpec) -> Result<Self> {
        if let Some(problem) = spec.video.encoder.codec.container_problem(spec.container) {
            bail!("encoder.container: {problem}");
        }
        let timescale = crate::writer::video_timescale(spec.video.fps);
        let video = VideoSession::new(&spec.video, timescale)?;
        let mut audio = Vec::new();
        for track in &spec.audio {
            let channels = track.channels.clamp(1, 2);
            let encoder = AacEncoder::new(&LiveAudioSpec {
                channels,
                bitrate_kbps: 128 * channels,
            })?;
            // Recovery has no encoder to ask, so the priming the edit lists
            // and fragment timing assume must be the one this encoder has.
            if u64::from(encoder.delay()) != AAC_PRIMING {
                bail!(
                    "the AAC encoder primes with {} samples, not the {AAC_PRIMING} crash-safe \
                     files assume",
                    encoder.delay()
                );
            }
            audio.push(AudioTrack {
                encoder,
                channels: channels as usize,
                fed: 0,
                written_packets: 0,
                pending: Vec::new(),
            });
        }
        let file = mp4::Writer::create(&spec.path, spec.container, spec.created)?;
        Ok(Self {
            file: Some(file),
            video,
            audio,
            path: spec.path.clone(),
            spec: spec.clone(),
            config: Config::from_env(),
            timescale,
            frame_ticks: (timescale as u32) / spec.video.fps,
            start: None,
            pending_video: VecDeque::new(),
            last_video: None,
            priority: None,
            submission_stalled: false,
            iso_credit: 0,
            fragment: Fragment::default(),
            held: None,
            format: None,
            committed_video: None,
            newest_video: None,
            last_sync: Instant::now(),
            fragments: 0,
            failed: None,
            reported_drops: 0,
            encoding: false,
            start_ticket: None,
            backlog: false,
            encoded_frames: 0,
        })
    }

    fn ticks(&self, at: Time, timescale: u32) -> i64 {
        let start = self.start.expect("started");
        at.ticks_since(start, timescale)
    }

    fn append_frame(&mut self, frame: &PixelBuffer, pts: Time) -> Result<bool> {
        let limit = if self.encoding
            && !self.backlog
            && self.priority.is_none()
            && PRIORITY_OUTPUTS.load(Ordering::Relaxed) > 0
        {
            PENDING_ISO
        } else {
            PENDING_VIDEO
        };
        if self.priority.is_none()
            && self.last_video.is_some()
            && PRIORITY_OUTPUTS.load(Ordering::Relaxed) > 0
        {
            if PRIORITY_PRESSURE.load(Ordering::Relaxed) > 0 {
                self.drain_ready(false)?;
                return Ok(false);
            }
            self.iso_credit += ISO_SHARE.load(Ordering::Relaxed);
            if self.iso_credit < 1000 {
                self.drain_ready(false)?;
                return Ok(false);
            }
            self.iso_credit -= 1000;
        }
        if self.pending_video.len() >= limit {
            self.drain_ready(false)?;
            if self.pending_video.len() >= limit {
                return Ok(false);
            }
        }
        let time = self.ticks(pts, self.timescale as u32);
        self.pending_video.push_back((frame.clone(), time));
        self.last_video = Some((frame.clone(), time));
        self.drain_ready(false)?;
        Ok(true)
    }

    fn append_samples(&mut self, track: usize, samples: &[f32], _pts: Time) -> Result<bool> {
        let audio = &mut self.audio[track];
        let packets = audio.encoder.encode(samples)?;
        audio.fed += (samples.len() / audio.channels) as u64;
        for packet in packets {
            self.fragment.bytes += packet.len();
            audio.pending.push(packet);
        }
        self.drain_ready(false)?;
        Ok(true)
    }

    fn may_start(&mut self) -> bool {
        if !matches!(self.spec.video.encoder.codec, Codec::H264 | Codec::Hevc) {
            return true;
        }
        let ticket = *self.start_ticket.get_or_insert_with(|| {
            request_encoder_start(encoder_start_rank(
                self.priority.is_some(),
                self.spec.video.width,
                self.spec.video.height,
            ))
        });
        claim_encoder_start(ticket)
    }

    /// Feed the encoder what it can take, collect what it has produced, and
    /// write a fragment when one is due. A commit/finalization must drain
    /// accepted pictures even while another protected output is stalled.
    fn drain_ready(&mut self, force_video: bool) -> Result<()> {
        if let Some(failed) = &self.failed {
            bail!("the file writer failed: {failed}");
        }
        while !self.pending_video.is_empty()
            && self.video.in_flight() < MAX_IN_FLIGHT
            // Admission also applies to pictures queued for startup. Feeding
            // that backlog under pressure can block an ISO's audio for >500 ms.
            // Keep the accepted pictures; audio and encoded output still drain.
            && (force_video
                || self.priority.is_some()
                || PRIORITY_PRESSURE.load(Ordering::Relaxed) == 0)
            && (self.encoding || self.may_start())
        {
            // The first picture initializes the native session even with
            // spare capacity; as with the stream encoder, don't time it.
            let submitted = (self.encoding && self.priority.is_some()).then(Instant::now);
            if !self.encoding {
                self.encoding = true;
                self.backlog = true;
            }
            let (frame, time) = self.pending_video.pop_front().expect("not empty");
            self.video.encode(&frame, time)?;
            if let Some(submitted) = submitted {
                // VideoToolbox can block before MAX_IN_FLIGHT is reached.
                // Our pending queue stays empty while the upstream mixed A/V
                // channel fills, losing audio as well as program pictures.
                self.submission_stalled =
                    submitted.elapsed() >= Duration::from_secs(1) / self.spec.video.fps;
            }
        }
        // Waiting for placement is not encoder pressure (#41).
        if let Some(priority) = &mut self.priority
            && self.encoding
        {
            priority.set(self.submission_stalled || self.pending_video.len() > 2);
        }
        let encoded = self.video.drain()?;
        self.collect(encoded)?;
        // The first keyframe alone does not mean startup has drained: on
        // M4 a session can hold eight following pictures for ~200 ms. Do
        // not turn that startup latency into ISO refusals at the four-frame
        // steady-state limit. The normal 32-picture bound still applies.
        if self.backlog && self.encoded_frames >= 2 && self.pending_video.len() < PENDING_ISO {
            self.backlog = false;
        }
        if self.fragment_due() {
            self.flush()?;
        }
        Ok(())
    }

    /// Take the encoder's output: each picture goes into the fragment once
    /// the next one says how long it lasted; the newest waits in `held`.
    fn collect(&mut self, encoded: Vec<encoder::Encoded>) -> Result<()> {
        for sample in encoded {
            self.encoded_frames += 1;
            if self.format.is_none() {
                self.format = Some(encoder::sample_format(&sample.format)?);
            }
            let time = sample.time.max(0) as u64;
            if let Some(held) = self.held.take() {
                let duration = if time > held.time {
                    (time - held.time).min(u64::from(u32::MAX)) as u32
                } else {
                    self.frame_ticks
                };
                self.push_video(held, duration);
            }
            self.held = Some(Held {
                data: sample.data,
                time,
                sync: sample.sync,
            });
            self.newest_video = Some(time);
        }
        Ok(())
    }

    fn push_video(&mut self, held: Held, duration: u32) {
        self.fragment.bytes += held.data.len();
        self.fragment
            .video
            .push((held.data.len() as u32, held.time, duration, held.sync));
        self.fragment.video_data.extend_from_slice(&held.data);
    }

    /// Put the held picture into the fragment with its nominal duration:
    /// at a commit the cut is on the frame grid (and the picture after a
    /// resume continues exactly there), at the finish the tail is padded.
    fn release_held(&mut self) {
        if let Some(held) = self.held.take() {
            self.push_video(held, self.frame_ticks);
        }
    }

    fn fragment_due(&self) -> bool {
        if self.format.is_none() {
            return false;
        }
        // The first fragment goes out as soon as it has a picture (which
        // takes the second picture's arrival to fix the first one's
        // duration): a take is on disk from its second frame, not its
        // first interval, and even its first picture carries true timing.
        if (self.fragments == 0 && !self.fragment.video.is_empty())
            || self.fragment.bytes >= FRAGMENT_BYTES
        {
            return true;
        }
        let interval = self.config.interval.as_secs_f64();
        if let (Some((_, first, ..)), Some(newest)) =
            (self.fragment.video.first(), self.newest_video)
            && (newest - first) as f64 / f64::from(self.timescale) >= interval
        {
            return true;
        }
        self.audio
            .iter()
            .any(|a| a.pending.len() as f64 * AAC_FRAME as f64 / f64::from(SAMPLE_RATE) >= interval)
    }

    /// Write the samples held so far as one fragment.
    fn flush(&mut self) -> Result<()> {
        let Some(format) = &self.format else {
            return Ok(());
        };
        let file = self.file.as_mut().context("the file is finished")?;
        if !file.has_tracks() {
            let name = match self.spec.video.encoder.codec {
                Codec::H264 => "H.264",
                Codec::Hevc => "HEVC",
                Codec::Prores422Proxy => "Apple ProRes 422 Proxy",
                Codec::Prores422Lt => "Apple ProRes 422 LT",
                Codec::Prores422 => "Apple ProRes 422",
                Codec::Prores422Hq => "Apple ProRes 422 HQ",
                Codec::Prores4444 => "Apple ProRes 4444",
            };
            let mut tracks = vec![Track {
                kind: TrackKind::Video {
                    width: format.width,
                    height: format.height,
                },
                timescale: self.timescale as u32,
                sample_entry: mp4::video_sample_entry(
                    format.fourcc,
                    format.width,
                    format.height,
                    format.depth,
                    name,
                    &format.atoms,
                ),
                skip: 0,
            }];
            for audio in &self.audio {
                tracks.push(Track {
                    kind: TrackKind::Audio {
                        channels: audio.channels as u16,
                        sample_rate: SAMPLE_RATE,
                    },
                    timescale: SAMPLE_RATE,
                    sample_entry: mp4::aac_sample_entry(
                        self.spec.container,
                        audio.channels as u16,
                        SAMPLE_RATE,
                    ),
                    skip: AAC_PRIMING,
                });
            }
            file.set_tracks(tracks);
        }
        let shift = if self.audio.is_empty() {
            0
        } else {
            mp4::video_shift(self.timescale as u32)
        };
        let mut audio_samples: Vec<Vec<(u32, u64, u32, bool)>> = Vec::new();
        let mut audio_data: Vec<Vec<u8>> = Vec::new();
        for audio in &self.audio {
            let mut samples = Vec::with_capacity(audio.pending.len());
            let mut data = Vec::new();
            for (i, packet) in audio.pending.iter().enumerate() {
                samples.push((
                    packet.len() as u32,
                    (audio.written_packets + i as u64) * AAC_FRAME,
                    AAC_FRAME as u32,
                    true,
                ));
                data.extend_from_slice(packet);
            }
            audio_samples.push(samples);
            audio_data.push(data);
        }
        let mut tracks = vec![FragmentTrack {
            samples: &self.fragment.video,
            data: &self.fragment.video_data,
        }];
        for (samples, data) in audio_samples.iter().zip(&audio_data) {
            tracks.push(FragmentTrack { samples, data });
        }
        let written = file.write_fragment(&tracks, shift);
        if let Err(err) = written {
            self.failed = Some(format!("{err:#}"));
            return Err(err)
                .with_context(|| format!("could not write a fragment of {}", self.path.display()));
        }
        if let Some((_, time, duration, _)) = self.fragment.video.last() {
            self.committed_video = Some(time + u64::from(*duration));
        }
        for audio in &mut self.audio {
            audio.written_packets += audio.pending.len() as u64;
            audio.pending.clear();
        }
        self.fragment = Fragment::default();
        self.fragments += 1;
        if let Some(every) = self.config.fsync_every
            && self.last_sync.elapsed() >= every
        {
            file.sync()?;
            self.last_sync = Instant::now();
        }
        Ok(())
    }

    /// How much of the file's timeline is on disk: the shortest track.
    fn committed_duration(&self) -> Option<Duration> {
        self.start?;
        let video = self
            .committed_video
            .map_or(0.0, |end| end as f64 / f64::from(self.timescale));
        let audio = self
            .audio
            .iter()
            .map(|a| {
                (a.written_packets * AAC_FRAME).saturating_sub(AAC_PRIMING) as f64
                    / f64::from(SAMPLE_RATE)
            })
            .fold(f64::INFINITY, f64::min);
        Some(Duration::from_secs_f64(video.min(audio).max(0.0)))
    }

    /// Put everything received so far on the disk: the queued pictures go
    /// to the encoder (waiting briefly for its slots), the encoder gives
    /// up everything in flight, the held picture is released, and the
    /// fragment is written and flushed through. The returned duration is
    /// what is on disk after all that: a fact, not the cut.
    fn commit_now(&mut self) -> Result<Option<Duration>> {
        let deadline = Instant::now() + COMMIT_DRAIN;
        while !self.pending_video.is_empty() {
            self.drain_ready(true)?;
            if self.pending_video.is_empty() || Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        let encoded = self.video.complete()?;
        self.collect(encoded)?;
        self.release_held();
        self.flush()?;
        if let Some(file) = &self.file {
            file.sync()?;
            self.last_sync = Instant::now();
        }
        Ok(self.committed_duration())
    }

    /// Give the file up after a write error, finishing it from its
    /// fragments if the disk still allows that (the next launch retries
    /// otherwise).
    fn fail(mut self, error: anyhow::Error) -> anyhow::Error {
        if let Some(file) = self.file.take() {
            file.abandon();
        }
        let (Some(folder), Some(name)) = (self.path.parent(), self.path.file_name()) else {
            return error;
        };
        match recover_file(folder, name) {
            Ok(Some(recovered)) => error.context(RepairedFile {
                fragments: recovered.fragments,
                frames: recovered.frames,
                seconds: recovered.seconds,
                summary: recovered.summary,
            }),
            Ok(None) => error,
            Err(repair) => error.context(format!(
                "and it could not be finished from its fragments ({repair:#}); the next launch \
                 will try again"
            )),
        }
    }

    fn finish_at(mut self, end: Time) -> Result<Finished> {
        if let Some(failed) = self.failed.clone() {
            return Err(self.fail(anyhow::anyhow!("the file writer failed: {failed}")));
        }
        match self.finish_inner(end) {
            Ok(finished) => Ok(finished),
            Err(err) => Err(self.fail(err)),
        }
    }

    fn finish_inner(&mut self, end: Time) -> Result<Finished> {
        let end_ticks = self.ticks(end, self.timescale as u32).max(0) as u64;
        let last = end_ticks.saturating_sub(u64::from(self.frame_ticks));
        let mut repeated = 0;
        if let Some((frame, time)) = self.last_video.take()
            && (time.max(0) as u64) < last
        {
            // This one extra slot exists only at finalization. Preserve every
            // already accepted picture and all audio; don't run admission or
            // back-pressure dropping on the frame that closes the timeline.
            self.pending_video.push_back((frame, last as i64));
            repeated = 1;
        }
        let deadline = Instant::now() + FINISH_WAIT;
        while !self.pending_video.is_empty() {
            self.drain_ready(true)?;
            if self.pending_video.is_empty() {
                break;
            }
            if Instant::now() >= deadline {
                bail!(
                    "the file encoder did not drain within 10 seconds: {}",
                    self.path.display()
                );
            }
            thread::sleep(Duration::from_millis(1));
        }
        let encoded = self.video.complete()?;
        self.collect(encoded)?;
        self.release_held();
        // Pictures the encoder dropped while completing count too.
        let lost_frames = self.lost_frames();
        // Flush the AAC encoders: enough silence for the priming delay, so
        // the last real samples come out, rounded up to whole packets. An
        // encoder that delivers fewer packets than its input covers has
        // dropped sound; say so.
        let mut lost_audio_tracks = 0;
        for track in 0..self.audio.len() {
            let audio = &mut self.audio[track];
            let packets = (audio.fed + AAC_PRIMING).div_ceil(AAC_FRAME);
            let zeros = (packets * AAC_FRAME).saturating_sub(audio.fed) as usize * audio.channels;
            let tail = vec![0.0f32; zeros];
            let encoded = audio.encoder.encode(&tail)?;
            audio.fed += (zeros / audio.channels) as u64;
            for packet in encoded {
                self.fragment.bytes += packet.len();
                audio.pending.push(packet);
            }
            if audio.written_packets + (audio.pending.len() as u64) < packets {
                lost_audio_tracks += 1;
            }
        }
        self.flush()?;
        let file = self.file.take().context("the file is finished")?;
        if !file.has_tracks() {
            bail!(
                "{}: the encoder produced no picture, so nothing was written",
                self.path.display()
            );
        }
        let duration = end_ticks * u64::from(MOVIE_TIMESCALE) / self.timescale as u64;
        file.finish(duration)
            .with_context(|| format!("could not finish {}", self.path.display()))?;
        Ok(Finished {
            repeated,
            lost_frames,
            lost_audio_tracks,
        })
    }
}

impl FileWriter for CrashSafeWriter {
    type Frame = PixelBuffer;

    fn prioritize_video(&mut self) {
        self.priority.get_or_insert_with(EncoderPressure::new);
    }

    fn start(&mut self, at: Time) -> Result<()> {
        self.start = Some(at);
        self.last_sync = Instant::now();
        Ok(())
    }

    fn append_video(&mut self, frame: &PixelBuffer, pts: Time) -> Result<bool> {
        autoreleasepool(|_| self.append_frame(frame, pts))
    }

    fn queue_depth(&self) -> usize {
        self.pending_video.len()
    }

    fn append_audio(&mut self, track: usize, samples: &[f32], pts: Time) -> Result<bool> {
        autoreleasepool(|_| self.append_samples(track, samples, pts))
    }

    fn finish(self, end: Time) -> Result<Finished> {
        autoreleasepool(|_| self.finish_at(end))
    }

    fn cancel(mut self) {
        self.file.take();
        let _ = std::fs::remove_file(&self.path);
    }

    fn crash_safe(&self) -> bool {
        true
    }

    fn commit(&mut self) -> Result<Option<Duration>> {
        autoreleasepool(|_| self.commit_now())
    }

    fn lost_frames(&mut self) -> u64 {
        let dropped = self.video.dropped();
        let new = dropped.saturating_sub(self.reported_drops);
        self.reported_drops = dropped;
        new
    }

    fn committed(&self) -> Option<Duration> {
        self.committed_duration()
    }
}

/// Finish the interrupted crash-safe file `name` in `folder` from its
/// fragments (see [`mp4::recover`]); `None` if the file isn't one or is
/// already finished. Only a regular file inside the folder is opened
/// ([`mp4::open_for_repair`]).
pub fn recover_file(
    folder: &std::path::Path,
    name: &std::ffi::OsStr,
) -> Result<Option<isocast_engine::RecoveredFile>> {
    let mut file = mp4::open_for_repair(folder, name)?;
    Ok(
        mp4::recover(&mut file)?.map(|r| isocast_engine::RecoveredFile {
            fragments: r.fragments,
            frames: r.pictures(),
            seconds: r.shortest(),
            summary: r.summary(),
        }),
    )
}
