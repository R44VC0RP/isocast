use std::collections::VecDeque;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use block2::RcBlock;
use isocast_engine::{
    FileWriter, Finished, MOVIE_FRAGMENT_SECONDS, SAMPLE_RATE, Time, VideoTrackSpec, WriterSpec,
};
use isocast_protocol::{Codec, Container};
use objc2::Message;
use objc2::exception;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2_av_foundation::{
    AVAssetWriter, AVAssetWriterInput, AVAssetWriterInputPixelBufferAdaptor, AVAssetWriterStatus,
    AVFileTypeMPEG4, AVFileTypeQuickTimeMovie, AVMediaTypeAudio, AVMediaTypeVideo,
    AVVideoAllowFrameReorderingKey, AVVideoCodecKey, AVVideoCodecTypeAppleProRes422,
    AVVideoCodecTypeAppleProRes422HQ, AVVideoCodecTypeAppleProRes422LT,
    AVVideoCodecTypeAppleProRes422Proxy, AVVideoCodecTypeAppleProRes4444, AVVideoCodecTypeH264,
    AVVideoCodecTypeHEVC, AVVideoColorPrimaries_ITU_R_709_2, AVVideoColorPrimariesKey,
    AVVideoColorPropertiesKey, AVVideoCompressionPropertiesKey, AVVideoEncoderSpecificationKey,
    AVVideoExpectedSourceFrameRateKey, AVVideoHeightKey, AVVideoMaxKeyFrameIntervalDurationKey,
    AVVideoProfileLevelKey, AVVideoQualityKey, AVVideoScalingModeKey,
    AVVideoScalingModeResizeAspect, AVVideoTransferFunction_ITU_R_709_2,
    AVVideoTransferFunctionKey, AVVideoWidthKey, AVVideoYCbCrMatrix_ITU_R_709_2,
    AVVideoYCbCrMatrixKey,
};
use objc2_avf_audio::{AVEncoderBitRateKey, AVFormatIDKey, AVNumberOfChannelsKey, AVSampleRateKey};
use objc2_core_audio_types::{
    AudioStreamBasicDescription, kAudioFormatFlagIsFloat, kAudioFormatFlagIsPacked,
    kAudioFormatLinearPCM, kAudioFormatMPEG4AAC,
};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{
    CMAudioFormatDescriptionCreate, CMAudioSampleBufferCreateReadyWithPacketDescriptions,
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMTime, kCMBlockBufferAssureMemoryNowFlag,
};
use objc2_foundation::{NSDictionary, NSError, NSNumber, NSString, NSURL, NSUnderlyingErrorKey};
use objc2_video_toolbox::{
    kVTProfileLevel_HEVC_Main10_AutoLevel,
    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
};

use crate::PixelBuffer;

/// Bound encoder back-pressure without blocking the other inputs of the
/// same file. Video buffers are retained, never copied.
pub(crate) const PENDING_VIDEO: usize = 32;
/// ISOs must not build a long hardware backlog ahead of a protected output.
pub(crate) const PENDING_ISO: usize = 4;
const PENDING_AUDIO: usize = 64;
/// Bound how long queued inputs may remain stalled during Stop.
const FINISH_WAIT: Duration = Duration::from_secs(10);
/// Keyframe at least every second, so editors can scrub the files easily.
const KEYFRAME_INTERVAL_SECONDS: f64 = 1.0;
/// Requested media-time fragment interval, not an acknowledgement that a
/// boundary has reached disk. Queued encoding and AAC timing can lag inputs.
const FRAGMENT_SECONDS: i64 = MOVIE_FRAGMENT_SECONDS;

pub(crate) static PRIORITY_OUTPUTS: AtomicUsize = AtomicUsize::new(0);
pub(crate) static PRIORITY_PRESSURE: AtomicUsize = AtomicUsize::new(0);
/// Fraction of ISO pictures admitted, in thousandths. Reduce promptly under
/// pressure, then probe for spare capacity slowly instead of sending bursts.
pub(crate) static ISO_SHARE: AtomicU32 = AtomicU32::new(1000);

/// How far apart hardware sessions get their first pictures, and in what
/// order. VideoToolbox places a session on one of the chip's video encode
/// engines when it gets its first picture, by what the engines are doing
/// just then, so sessions that start within the same few milliseconds
/// often land together, for good: on an M4 Max (two engines), a 4K60 HEVC
/// program, a 4K60 ISO and two 1080p60 ISOs started together put three
/// sessions on one engine in 10–40% of trials (94% with the program
/// already running), and each of the three then ran at ~45 fps for the
/// whole take while the fourth had an engine to itself (GitHub #41).
/// Started 17 ms or more apart the placement is deterministic and depends
/// on the order of sizes: largest first (4K, 4K, 1080p, 1080p) or
/// alternating was right in 120 of 120 trials, a 4K last after a 1080p
/// wrong in 120 of 120; 10 ms apart 3% went wrong, 5 ms apart 22%. So the
/// program goes first, then the other files from the largest down, this
/// far apart. Pictures a session is waiting for stay queued, so nothing is
/// dropped; the last of four sessions waits three gaps.
const ENCODER_START_GAP: Duration = Duration::from_millis(25);
/// How long a session waits for others of its take to ask before it goes
/// first: their first pictures arrive within a frame of each other.
const ENCODER_START_SETTLE: Duration = Duration::from_millis(8);
/// A session that has waited this long no longer holds others back (its
/// writer was dropped before its first picture, or starved).
const ENCODER_START_STALE: Duration = Duration::from_millis(500);

/// Hardware video sessions of this process waiting for their first
/// picture, and when the last one got it.
struct EncoderStarts {
    last: Option<Instant>,
    next_ticket: u64,
    /// (rank, asked, ticket): lower ranks go first.
    waiting: Vec<((u8, u32), Instant, u64)>,
}

static ENCODER_STARTS: Mutex<EncoderStarts> = Mutex::new(EncoderStarts {
    last: None,
    next_ticket: 0,
    waiting: Vec::new(),
});

fn encoder_starts() -> std::sync::MutexGuard<'static, EncoderStarts> {
    ENCODER_STARTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A session's place in the start order: protected outputs first, then
/// by size, largest first.
pub(crate) fn encoder_start_rank(protected: bool, width: u32, height: u32) -> (u8, u32) {
    (
        u8::from(!protected),
        u32::MAX - width.saturating_mul(height),
    )
}

/// Ask to start a session with `rank`: a ticket for [`claim_encoder_start`].
pub(crate) fn request_encoder_start(rank: (u8, u32)) -> u64 {
    let mut starts = encoder_starts();
    let ticket = starts.next_ticket;
    starts.next_ticket += 1;
    starts.waiting.push((rank, Instant::now(), ticket));
    ticket
}

/// Whether the session holding `ticket` may get its first picture now: it
/// has waited [`ENCODER_START_SETTLE`], the last session to start did so
/// at least [`ENCODER_START_GAP`] ago, and no session ahead of it in the
/// start order is waiting. Claims the moment if so. A session that has
/// waited [`ENCODER_START_STALE`] is held back by nothing but the gap.
pub(crate) fn claim_encoder_start(ticket: u64) -> bool {
    let mut starts = encoder_starts();
    let now = Instant::now();
    if starts
        .last
        .is_some_and(|at| now.duration_since(at) < ENCODER_START_GAP)
    {
        return false;
    }
    let Some(&(rank, asked, _)) = starts.waiting.iter().find(|(.., t)| *t == ticket) else {
        // Never asked, or forgotten as stale: start now.
        starts.last = Some(now);
        return true;
    };
    let waited = now.duration_since(asked);
    if waited < ENCODER_START_SETTLE {
        return false;
    }
    if waited < ENCODER_START_STALE {
        let held_back = starts.waiting.iter().any(|(other, other_asked, t)| {
            *t != ticket
                && (*other, *other_asked) < (rank, asked)
                && now.duration_since(*other_asked) < ENCODER_START_STALE
        });
        if held_back {
            return false;
        }
    }
    // Requests this old no longer hold anyone back; forget them.
    starts.waiting.retain(|(_, other_asked, t)| {
        *t != ticket && now.duration_since(*other_asked) < ENCODER_START_STALE
    });
    starts.last = Some(now);
    true
}

/// Withdraw a request: the session ends without a first picture.
pub(crate) fn cancel_encoder_start(ticket: u64) {
    encoder_starts().waiting.retain(|(.., t)| *t != ticket);
}

/// Admission feedback across the native sessions sharing this process's
/// hardware encoders. Only an output's worker mutates its guard. Capture and
/// the compositor never wait; low-priority writers return a counted refusal.
pub(crate) struct EncoderPressure {
    stalled: bool,
    adjusted: Instant,
}

impl EncoderPressure {
    pub(crate) fn new() -> Self {
        if PRIORITY_OUTPUTS.fetch_add(1, Ordering::Relaxed) == 0 {
            ISO_SHARE.store(1000, Ordering::Relaxed);
        }
        Self {
            stalled: false,
            adjusted: Instant::now(),
        }
    }

    pub(crate) fn set(&mut self, stalled: bool) {
        let now = Instant::now();
        if stalled
            && (!self.stalled || now.duration_since(self.adjusted) >= Duration::from_millis(100))
        {
            let _ = ISO_SHARE.try_update(Ordering::Relaxed, Ordering::Relaxed, |share| {
                Some(share * 3 / 4)
            });
            self.adjusted = now;
        } else if !stalled
            && PRIORITY_PRESSURE.load(Ordering::Relaxed) == 0
            && now.duration_since(self.adjusted) >= Duration::from_millis(500)
        {
            let _ = ISO_SHARE.try_update(Ordering::Relaxed, Ordering::Relaxed, |share| {
                Some((share + 10).min(1000))
            });
            self.adjusted = now;
        }
        if stalled != self.stalled {
            if stalled {
                PRIORITY_PRESSURE.fetch_add(1, Ordering::Relaxed);
            } else {
                PRIORITY_PRESSURE.fetch_sub(1, Ordering::Relaxed);
            }
            self.stalled = stalled;
        }
    }
}

impl Drop for EncoderPressure {
    fn drop(&mut self) {
        self.set(false);
        PRIORITY_OUTPUTS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// One .mov or .mp4 file written by AVAssetWriter.
pub struct AssetWriter {
    writer: Retained<AVAssetWriter>,
    video: Retained<AVAssetWriterInput>,
    adaptor: Retained<AVAssetWriterInputPixelBufferAdaptor>,
    audio: Vec<AudioInput>,
    path: PathBuf,
    /// Video media timescale; frame times are exact multiples of it.
    timescale: i32,
    /// The file's start on the host clock, and as the writer sees it.
    start: Option<(Time, CMTime)>,
    pending_video: VecDeque<(PixelBuffer, CMTime)>,
    /// A tail repeat preserves audio if the final video slots were refused.
    last_video: Option<(PixelBuffer, CMTime)>,
    frame_duration: CMTime,
    priority: Option<EncoderPressure>,
    iso_credit: u32,
    /// The video input has its first picture, so its hardware session has
    /// started; until then pictures wait for their turn in the start order
    /// (`start_ticket` is the request; see [`claim_encoder_start`]).
    encoding: bool,
    start_ticket: Option<u64>,
    pixels: (u32, u32),
    /// The encoder is hardware H.264/HEVC, which shares the chip's video
    /// encode engines; ProRes does not take a turn.
    gated: bool,
    /// Pictures that queued while the session waited for its turn are
    /// still draining; the ISO queue limit waits until they have.
    backlog: bool,
}

struct AudioInput {
    input: Retained<AVAssetWriterInput>,
    format: CFRetained<CMFormatDescription>,
    channels: usize,
    pending: VecDeque<CFRetained<CMSampleBuffer>>,
}

// SAFETY: AVAssetWriter and its inputs may be used from any thread as long as
// calls are not concurrent; an AssetWriter is only ever used by one thread.
unsafe impl Send for AssetWriter {}

impl Drop for AssetWriter {
    /// A writer that never got its first picture (cancelled, or failed
    /// while finishing) holds no one back.
    fn drop(&mut self) {
        if !self.encoding
            && let Some(ticket) = self.start_ticket
        {
            cancel_encoder_start(ticket);
        }
    }
}

impl AssetWriter {
    /// A writer for `spec`. The recording's threads live as long as it does
    /// and have no autorelease pool, so each call into AVFoundation here
    /// drains one of its own: otherwise what it autoreleases (the writer
    /// itself among them) would outlive the file, and every take's writers
    /// and their encoder sessions would pile up until the recording ends.
    pub fn create(spec: &WriterSpec) -> Result<Self> {
        let writer = autoreleasepool(|_| Self::make(spec));
        isocast_diagnostics::debug!(
            "writer: {} {:?} {}x{}@{} ({} audio tracks): {}",
            spec.path.display(),
            spec.video.encoder.codec,
            spec.video.width,
            spec.video.height,
            spec.video.fps,
            spec.audio.len(),
            if writer.is_ok() { "created" } else { "failed" }
        );
        writer
    }

    fn make(spec: &WriterSpec) -> Result<Self> {
        if let Some(problem) = spec.video.encoder.codec.container_problem(spec.container) {
            bail!("encoder.container: {problem}");
        }
        let path = spec
            .path
            .to_str()
            .context("the file path is not valid UTF-8")?;
        // AVAssetWriter takes a path, not a descriptor, and follows a link
        // at it: refuse any entry there (a dangling link a plugin made),
        // and check what it made below.
        if spec.path.symlink_metadata().is_ok() {
            bail!("{path} already exists; not recording over it");
        }
        let url = NSURL::fileURLWithPath(&NSString::from_str(path));
        let file_type = match spec.container {
            Container::Mov => unsafe { AVFileTypeQuickTimeMovie },
            Container::Mp4 => unsafe { AVFileTypeMPEG4 },
        }
        .context("AVFoundation is missing a file type constant")?;
        let writer = unsafe { AVAssetWriter::assetWriterWithURL_fileType_error(&url, file_type) }
            .map_err(|err| ns_error(&err))?;
        unsafe { writer.setMovieFragmentInterval(CMTime::new(FRAGMENT_SECONDS, 1)) };

        let timescale = video_timescale(spec.video.fps);
        let video_type = unsafe { AVMediaTypeVideo }.context("missing AVMediaTypeVideo")?;
        let settings = video_settings(&spec.video);
        if !unsafe { writer.canApplyOutputSettings_forMediaType(Some(&settings), video_type) } {
            bail!(
                "AVAssetWriter rejected {:?} recording settings at {}×{} {} fps in {:?}; check encoder size, rate, bit depth and container compatibility",
                spec.video.encoder.codec,
                spec.video.width,
                spec.video.height,
                spec.video.fps,
                spec.container
            );
        }
        let (video, adaptor) = catch(|| unsafe {
            let input = AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(
                video_type,
                Some(&settings),
            );
            input.setExpectsMediaDataInRealTime(true);
            input.setMediaTimeScale(timescale);
            let adaptor = AVAssetWriterInputPixelBufferAdaptor::assetWriterInputPixelBufferAdaptorWithAssetWriterInput_sourcePixelBufferAttributes(&input, None);
            (input, adaptor)
        })?;
        add_input(&writer, &video)?;

        let mut audio = Vec::new();
        for track in &spec.audio {
            let channels = track.channels as usize;
            let audio_type = unsafe { AVMediaTypeAudio }.context("missing AVMediaTypeAudio")?;
            let settings = aac_settings(track.channels);
            let input = catch(|| unsafe {
                let input = AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(
                    audio_type,
                    Some(&settings),
                );
                input.setExpectsMediaDataInRealTime(true);
                input
            })?;
            add_input(&writer, &input)?;
            audio.push(AudioInput {
                input,
                format: float_format(track.channels)?,
                channels,
                pending: VecDeque::new(),
            });
        }

        if !unsafe { writer.startWriting() } {
            return Err(writer_error(&writer).context("could not start writing"));
        }
        // What it made must be a plain file with one name: a link put in
        // its place meanwhile would send the recording elsewhere.
        if let Some(problem) = not_plain(&spec.path) {
            unsafe { writer.cancelWriting() };
            bail!("{path}: {problem}; not recording into it");
        }
        Ok(Self {
            writer,
            video,
            adaptor,
            audio,
            path: spec.path.clone(),
            timescale,
            start: None,
            pending_video: VecDeque::new(),
            last_video: None,
            frame_duration: unsafe { CMTime::new(1, spec.video.fps as i32) },
            priority: None,
            iso_credit: 0,
            encoding: false,
            start_ticket: None,
            pixels: (spec.video.width, spec.video.height),
            gated: matches!(spec.video.encoder.codec, Codec::H264 | Codec::Hevc),
            backlog: false,
        })
    }

    /// An error if AVAssetWriter has failed (for example, the disk is full);
    /// it then refuses every sample, so waiting is pointless.
    fn check_failed(&self) -> Result<()> {
        if unsafe { self.writer.status() } == AVAssetWriterStatus::Failed {
            return Err(writer_error(&self.writer).context("the file writer failed"));
        }
        Ok(())
    }

    /// `at` on the writer's timeline: an exact number of `timescale` ticks
    /// after the start, so evenly spaced frames get exactly even durations.
    fn media_time(&self, at: Time, timescale: i32) -> CMTime {
        let (start, start_cm) = self.start.expect("started");
        let ticks = at.ticks_since(start, timescale as u32);
        unsafe { start_cm.add(CMTime::new(ticks, timescale)) }
    }
}

impl FileWriter for AssetWriter {
    type Frame = PixelBuffer;

    fn prioritize_video(&mut self) {
        self.priority.get_or_insert_with(EncoderPressure::new);
    }

    fn start(&mut self, at: Time) -> Result<()> {
        autoreleasepool(|_| self.start_at(at))
    }

    fn append_video(&mut self, frame: &PixelBuffer, pts: Time) -> Result<bool> {
        autoreleasepool(|_| self.append_frame(frame, pts))
    }

    // Developer mode (#47): frames waiting for AVAssetWriter's input.
    fn queue_depth(&self) -> usize {
        self.pending_video.len()
    }

    fn append_audio(&mut self, track: usize, samples: &[f32], pts: Time) -> Result<bool> {
        autoreleasepool(|_| self.append_samples(track, samples, pts))
    }

    fn finish(self, end: Time) -> Result<Finished> {
        autoreleasepool(|_| self.finish_at(end)).map(|repeated| Finished {
            repeated,
            ..Finished::default()
        })
    }

    fn cancel(self) {
        let owned = autoreleasepool(|_| unsafe {
            if self.writer.status() == AVAssetWriterStatus::Unknown {
                return false;
            }
            self.writer.cancelWriting();
            true
        });
        // Never remove a path the writer did not open.
        if owned {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

impl AssetWriter {
    fn start_at(&mut self, at: Time) -> Result<()> {
        let ticks = at.ticks_since(Time::from_nanos(0), self.timescale as u32);
        let start_cm = unsafe { CMTime::new(ticks, self.timescale) };
        catch(|| unsafe { self.writer.startSessionAtSourceTime(start_cm) })?;
        self.start = Some((at, start_cm));
        Ok(())
    }

    fn append_frame(&mut self, frame: &PixelBuffer, pts: Time) -> Result<bool> {
        // Pictures waiting for the session's start all fit, and the ISO
        // limit only applies once they have drained: a wait for the start
        // must never turn into refusals.
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
            // Stop adding low-priority work immediately while a protected
            // queue catches up. The reduced steady share avoids repeatedly
            // refilling every encoder at full rate when that pause ends.
            if PRIORITY_PRESSURE.load(Ordering::Relaxed) > 0 {
                self.drain_ready()?;
                return Ok(false);
            }
            self.iso_credit += ISO_SHARE.load(Ordering::Relaxed);
            if self.iso_credit < 1000 {
                self.drain_ready()?;
                return Ok(false);
            }
            self.iso_credit -= 1000;
        }
        if self.pending_video.len() >= limit {
            self.drain_ready()?;
            if self.pending_video.len() >= limit {
                return Ok(false);
            }
        }
        let time = self.media_time(pts, self.timescale);
        self.pending_video.push_back((frame.clone(), time));
        self.last_video = Some((frame.clone(), time));
        self.drain_ready()?;
        Ok(true)
    }

    fn append_samples(&mut self, track: usize, samples: &[f32], pts: Time) -> Result<bool> {
        if self.audio[track].pending.len() == PENDING_AUDIO {
            self.drain_ready()?;
            if self.audio[track].pending.len() == PENDING_AUDIO {
                return Ok(false);
            }
        }
        let time = self.media_time(pts, SAMPLE_RATE as i32);
        let audio = &mut self.audio[track];
        let buffer = audio_sample_buffer(&audio.format, audio.channels, samples, time)?;
        audio.pending.push_back(buffer);
        self.drain_ready()?;
        Ok(true)
    }

    /// Whether the video input may get its first picture now (its turn in
    /// the process's encoder start order; see [`claim_encoder_start`]).
    fn may_start(&mut self) -> bool {
        if !self.gated {
            return true;
        }
        let ticket = *self.start_ticket.get_or_insert_with(|| {
            let (width, height) = self.pixels;
            request_encoder_start(encoder_start_rank(self.priority.is_some(), width, height))
        });
        claim_encoder_start(ticket)
    }

    /// AVAssetWriter's inputs depend on each other for interleaving. Waiting
    /// on video here used to prevent the same thread from feeding audio,
    /// causing 100 ms timeouts and overflowing the shared packet channel
    /// (especially with a microphone's 4 ms chunks). Keep each input ordered,
    /// but let ready audio pass stalled video, and vice versa.
    fn drain_ready(&mut self) -> Result<()> {
        self.check_failed()?;
        loop {
            let mut progressed = false;
            for audio in &mut self.audio {
                if !audio.pending.is_empty() && unsafe { audio.input.isReadyForMoreMediaData() } {
                    let buffer = audio.pending.pop_front().expect("not empty");
                    if !unsafe { audio.input.appendSampleBuffer(&buffer) } {
                        return Err(writer_error(&self.writer).context("could not append audio"));
                    }
                    progressed = true;
                }
            }
            if !self.pending_video.is_empty()
                && unsafe { self.video.isReadyForMoreMediaData() }
                && (self.encoding || self.may_start())
            {
                // The first picture starts the input's hardware session.
                if !self.encoding {
                    self.encoding = true;
                    self.backlog = true;
                }
                let (frame, time) = self.pending_video.pop_front().expect("not empty");
                if !unsafe {
                    self.adaptor
                        .appendPixelBuffer_withPresentationTime(&frame.0, time)
                } {
                    return Err(
                        writer_error(&self.writer).context("could not append a video frame")
                    );
                }
                progressed = true;
            }
            if !progressed {
                if self.backlog && self.pending_video.len() < PENDING_ISO {
                    self.backlog = false;
                }
                // Pictures waiting for the start slot are not a stall.
                if let Some(priority) = &mut self.priority
                    && self.encoding
                {
                    priority.set(self.pending_video.len() > 2);
                }
                return Ok(());
            }
        }
    }

    fn finish_at(mut self, end: Time) -> Result<u64> {
        // A failed writer raises an exception if asked to finish.
        self.check_failed()?;
        let end = self.media_time(end, self.timescale);
        let last = unsafe { end.subtract(self.frame_duration) };
        let mut repeated = 0;
        if let Some((frame, pts)) = self.last_video.take()
            && unsafe { pts.compare(last) } < 0
        {
            // This one extra slot exists only at finalization. Preserve every
            // already accepted picture and all audio; don't run admission or
            // back-pressure dropping on the frame that closes the timeline.
            self.pending_video.push_back((frame, last));
            repeated = 1;
        }
        let deadline = Instant::now() + FINISH_WAIT;
        let mut video_finished = false;
        let mut audio_finished = vec![false; self.audio.len()];
        loop {
            self.drain_ready()?;
            // Mark drained inputs independently: the remaining inputs may
            // need this signal before they can accept their final buffers.
            if self.pending_video.is_empty() && !video_finished {
                catch(|| unsafe { self.video.markAsFinished() })?;
                video_finished = true;
            }
            for (audio, finished) in self.audio.iter().zip(&mut audio_finished) {
                if audio.pending.is_empty() && !*finished {
                    catch(|| unsafe { audio.input.markAsFinished() })?;
                    *finished = true;
                }
            }
            if video_finished && audio_finished.iter().all(|finished| *finished) {
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
        let (done, wait) = mpsc::channel();
        let handler = RcBlock::new(move || {
            let _ = done.send(());
        });
        catch(|| unsafe {
            self.writer.endSessionAtSourceTime(end);
            self.writer.finishWritingWithCompletionHandler(&handler);
        })?;
        wait.recv()
            .context("AVAssetWriter never reported finishing")?;
        if unsafe { self.writer.status() } != AVAssetWriterStatus::Completed {
            return Err(writer_error(&self.writer).context("could not finish the file"));
        }
        isocast_diagnostics::debug!("writer: finished, {repeated} tail frames repeated");
        Ok(repeated)
    }
}

/// A media timescale in which every frame time is a whole number of ticks.
pub(crate) fn video_timescale(fps: u32) -> i32 {
    if 60_000 % fps == 0 {
        60_000
    } else {
        fps as i32 * 1000
    }
}

fn video_settings(video: &VideoTrackSpec) -> Retained<NSDictionary<NSString, AnyObject>> {
    let codec = unsafe {
        match video.encoder.codec {
            Codec::H264 => AVVideoCodecTypeH264,
            Codec::Hevc => AVVideoCodecTypeHEVC,
            Codec::Prores422Proxy => AVVideoCodecTypeAppleProRes422Proxy,
            Codec::Prores422Lt => AVVideoCodecTypeAppleProRes422LT,
            Codec::Prores422 => AVVideoCodecTypeAppleProRes422,
            Codec::Prores422Hq => AVVideoCodecTypeAppleProRes422HQ,
            Codec::Prores4444 => AVVideoCodecTypeAppleProRes4444,
        }
    }
    .expect("AVFoundation codec constant");
    let width = NSNumber::new_u32(video.width);
    let height = NSNumber::new_u32(video.height);
    // The engine draws sRGB-like content; tag it as Rec. 709, which is what
    // editors assume for HD and 4K SDR video.
    let color = unsafe {
        NSDictionary::<NSString, AnyObject>::from_slices(
            &[
                key(AVVideoColorPrimariesKey),
                key(AVVideoTransferFunctionKey),
                key(AVVideoYCbCrMatrixKey),
            ],
            &[
                key(AVVideoColorPrimaries_ITU_R_709_2),
                key(AVVideoTransferFunction_ITU_R_709_2),
                key(AVVideoYCbCrMatrix_ITU_R_709_2),
            ],
        )
    };
    // Frames of another size (the program drawn at the canvas size for a
    // smaller file) are scaled to fit, keeping their shape.
    let mut keys = unsafe {
        vec![
            key(AVVideoCodecKey),
            key(AVVideoWidthKey),
            key(AVVideoHeightKey),
            key(AVVideoColorPropertiesKey),
            key(AVVideoScalingModeKey),
        ]
    };
    let mut values: Vec<&AnyObject> = vec![codec, &width, &height, &color, unsafe {
        key(AVVideoScalingModeResizeAspect)
    }];
    let compression;
    let specification;
    if matches!(video.encoder.codec, Codec::H264 | Codec::Hevc) {
        // Keep H.264 and HEVC on the hardware encoders.
        let require: &NSString = unsafe {
            &*(kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder as *const _
                as *const NSString)
        };
        specification = NSDictionary::<NSString, AnyObject>::from_slices(
            &[require],
            &[&**NSNumber::new_bool(true)],
        );
        let quality = NSNumber::new_f64(f64::from(video.encoder.quality.value()) / 100.0);
        let fps = NSNumber::new_u32(video.fps);
        let interval = NSNumber::new_f64(KEYFRAME_INTERVAL_SECONDS);
        // With gaps under overload, AVAssetWriter's unfinished fragments can
        // put B-pictures from the next fragment before pictures already shown
        // (reproduced after SIGKILL). Preserve presentation order in fragments
        // as well as finalized movies. Quality and keyframe settings stay set
        // above; this trades compression efficiency for a recoverable timeline.
        let reorder = NSNumber::new_bool(false);
        let lookahead = NSNumber::new_u32(0);
        // The M4 Max's automatic look-ahead can batch concurrent sessions
        // below real time even though its default reads back as zero. An
        // explicit zero fixes that measured mode. The base M2 regresses with
        // this hint, so leave other chips on their default until qualified.
        // Use the public VT key's string value to keep loading on macOS 14.
        let lookahead_key = NSString::from_str("LookAheadFrames");
        let mut properties = unsafe {
            vec![
                key(AVVideoQualityKey),
                key(AVVideoExpectedSourceFrameRateKey),
                key(AVVideoMaxKeyFrameIntervalDurationKey),
                key(AVVideoAllowFrameReorderingKey),
            ]
        };
        let mut property_values: Vec<&AnyObject> = vec![&quality, &fps, &interval, &reorder];
        if video.encoder.codec == Codec::Hevc
            && objc2::available!(macos = 15.0)
            && crate::system::sysctl_string("machdep.cpu.brand_string").as_deref()
                == Some("Apple M4 Max")
        {
            properties.push(&lookahead_key);
            property_values.push(&lookahead);
        }
        if video.encoder.codec == Codec::Hevc && video.encoder.effective_bit_depth() == 10 {
            properties.push(unsafe { key(AVVideoProfileLevelKey) });
            let main10: &NSString =
                unsafe { &*(kVTProfileLevel_HEVC_Main10_AutoLevel as *const _ as *const NSString) };
            property_values.push(main10);
        }
        compression = NSDictionary::from_slices(&properties, &property_values);
        keys.extend(unsafe {
            [
                key(AVVideoEncoderSpecificationKey),
                key(AVVideoCompressionPropertiesKey),
            ]
        });
        values.extend([&*specification as &AnyObject, &*compression as &AnyObject]);
    }
    NSDictionary::from_slices(&keys, &values)
}

pub(crate) fn aac_settings(channels: u32) -> Retained<NSDictionary<NSString, AnyObject>> {
    let format = NSNumber::new_u32(kAudioFormatMPEG4AAC);
    let rate = NSNumber::new_f64(SAMPLE_RATE as f64);
    let channel_count = NSNumber::new_u32(channels);
    let bit_rate = NSNumber::new_u32(128_000 * channels.min(2));
    unsafe {
        NSDictionary::from_slices(
            &[
                key(AVFormatIDKey),
                key(AVSampleRateKey),
                key(AVNumberOfChannelsKey),
                key(AVEncoderBitRateKey),
            ],
            &[&*format, &*rate, &*channel_count, &*bit_rate],
        )
    }
}

/// The engine's audio format: 48 kHz interleaved 32-bit float.
pub(crate) fn float_format(channels: u32) -> Result<CFRetained<CMFormatDescription>> {
    let asbd = AudioStreamBasicDescription {
        mSampleRate: SAMPLE_RATE as f64,
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
        mBytesPerPacket: 4 * channels,
        mFramesPerPacket: 1,
        mBytesPerFrame: 4 * channels,
        mChannelsPerFrame: channels,
        mBitsPerChannel: 32,
        mReserved: 0,
    };
    let mut format = ptr::null();
    let status = unsafe {
        CMAudioFormatDescriptionCreate(
            None,
            NonNull::from(&asbd),
            0,
            ptr::null(),
            0,
            ptr::null(),
            None,
            NonNull::from(&mut format),
        )
    };
    match NonNull::new(format.cast_mut()) {
        Some(format) if status == 0 => Ok(unsafe { CFRetained::from_raw(format) }),
        _ => bail!("could not describe the audio format (CoreMedia error {status})"),
    }
}

pub(crate) fn audio_sample_buffer(
    format: &CMFormatDescription,
    channels: usize,
    samples: &[f32],
    pts: CMTime,
) -> Result<CFRetained<CMSampleBuffer>> {
    let len = std::mem::size_of_val(samples);
    let mut block = ptr::null_mut();
    let status = unsafe {
        CMBlockBuffer::create_with_memory_block(
            None,
            ptr::null_mut(),
            len,
            None,
            ptr::null(),
            0,
            len,
            kCMBlockBufferAssureMemoryNowFlag,
            NonNull::from(&mut block),
        )
    };
    let Some(block) = NonNull::new(block).filter(|_| status == 0) else {
        bail!("could not allocate audio memory (CoreMedia error {status})");
    };
    let block = unsafe { CFRetained::from_raw(block) };
    let source = NonNull::new(samples.as_ptr().cast_mut()).expect("slice pointer");
    let status = unsafe { CMBlockBuffer::replace_data_bytes(source.cast(), &block, 0, len) };
    if status != 0 {
        bail!("could not copy audio (CoreMedia error {status})");
    }
    let mut buffer = ptr::null_mut();
    let status = unsafe {
        CMAudioSampleBufferCreateReadyWithPacketDescriptions(
            None,
            &block,
            format,
            (samples.len() / channels) as isize,
            pts,
            ptr::null(),
            NonNull::from(&mut buffer),
        )
    };
    match NonNull::new(buffer) {
        Some(buffer) if status == 0 => Ok(unsafe { CFRetained::from_raw(buffer) }),
        _ => bail!("could not wrap audio for the encoder (CoreMedia error {status})"),
    }
}

pub(crate) fn add_input(writer: &AVAssetWriter, input: &AVAssetWriterInput) -> Result<()> {
    if !unsafe { writer.canAddInput(input) } {
        bail!("AVAssetWriter refused a {} track", unsafe {
            input.mediaType()
        });
    }
    catch(|| unsafe { writer.addInput(input) })
}

/// Run `f`, turning an Objective-C exception into an error.
pub(crate) fn catch<R>(f: impl FnOnce() -> R) -> Result<R> {
    exception::catch(AssertUnwindSafe(f)).map_err(|exception| match exception {
        Some(exception) => anyhow!("AVFoundation raised an exception: {exception:?}"),
        None => anyhow!("AVFoundation raised an unknown exception"),
    })
}

pub(crate) fn writer_error(writer: &AVAssetWriter) -> anyhow::Error {
    match unsafe { writer.error() } {
        Some(err) => ns_error(&err),
        None => anyhow!("AVAssetWriter status {:?}", unsafe { writer.status() }),
    }
}

/// An NSError with its underlying errors, which usually hold the real cause
/// (AVFoundation's own message is often just "could not be completed").
pub(crate) fn ns_error(err: &NSError) -> anyhow::Error {
    let mut parts = Vec::new();
    let mut next = Some(err.retain());
    while let Some(err) = next.take() {
        parts.push(format!(
            "{} ({} error {})",
            err.localizedDescription(),
            err.domain(),
            err.code()
        ));
        next = err
            .userInfo()
            .objectForKey(unsafe { NSUnderlyingErrorKey })
            .and_then(|e| e.downcast::<NSError>().ok());
    }
    anyhow!(parts.join(": "))
}

/// Why the file at `path` isn't a plain file with a single name (a
/// symbolic link, or a hard link to another), if it isn't.
fn not_plain(path: &std::path::Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    match path.symlink_metadata() {
        // Not made until the first sample on some systems: nothing there
        // to follow yet.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => Some(format!("it can't be checked ({err})")),
        Ok(meta) if !meta.file_type().is_file() => Some("it isn't a plain file".into()),
        // A second name is a hard link only where the volume can make one;
        // elsewhere (or a count of 0) it's how the volume reports links,
        // and refusing would refuse every take on it (#56).
        Ok(meta) if meta.nlink() > 1 && crate::hard_links_possible(path) => {
            Some(format!("it has {} names", meta.nlink()))
        }
        Ok(meta) if meta.nlink() != 1 => {
            isocast_diagnostics::diagnostic!("warn";
                "isocastd: {} reports {} names, which can't be a hard link there; recording into it",
                path.display(),
                meta.nlink()
            );
            None
        }
        Ok(_) => None,
    }
}

/// AVFoundation constants are weakly linked; every one used here exists on
/// the macOS versions Isocast supports.
fn key(constant: Option<&'static NSString>) -> &'static NSString {
    constant.expect("AVFoundation constant")
}
