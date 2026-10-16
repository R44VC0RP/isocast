//! Joining files end to end (a timeline's export) with AVAssetReader and
//! AVAssetWriter. Each part is the stretch of a take's file from its clip's
//! in point for its clip's length (see [`JoinPart`]), so files at
//! different frame rates joined from the same clips switch clips at the
//! same moments.
//!
//! Video is copied as recorded (no decoding or encoding), retimed to follow
//! on, except where it can't be (C.3, `recode`): when a part starts between
//! two keyframes, the frame on screen at its in point and the frames up to
//! the next keyframe are decoded and encoded again (a smart render), so the
//! cut is frame-accurate and every other frame is the recorded one; and a
//! part whose file has another codec, size or frame rate than the joined
//! file (an ISO angle in the edited file) is decoded, scaled and encoded
//! again whole, on the joined file's frame grid. A part ends with its last
//! frame that starts before its end (recordings have no reordered frames,
//! so cutting there needs nothing encoded again).
//!
//! Each audio track is decoded and encoded again (AAC, as recordings are)
//! as one continuous track, each part's from its in point, cut or padded
//! with silence to the part's length so it lines up with the video at
//! every join, and cross-faded over 10 ms into the next part (the outgoing
//! part's audio just past its end under the incoming part's first samples)
//! so a join never clicks.
//!
//! Why not AVMutableComposition with a passthrough export: every take's AAC
//! starts with its own encoder priming, so a passthrough join needs an edit
//! list entry per join to skip it. QuickTime plays that right, but FFmpeg
//! (and the editors built on it) placed the second take's audio ~20 ms
//! early (measured 2026-10-13), more than a frame at 60 fps.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use block2::RcBlock;
use isocast_engine::{JoinPart, JoinSpec, Joined, JoinedPart, OnJoinProgress, SAMPLE_RATE};
use isocast_protocol::Container;
use objc2::AllocAnyThread;
use objc2::rc::{Retained, autoreleasepool};
use objc2_av_foundation::{
    AVAssetReader, AVAssetReaderStatus, AVAssetReaderTrackOutput, AVAssetTrack, AVAssetWriter,
    AVAssetWriterInput, AVAssetWriterStatus, AVFileTypeMPEG4, AVFileTypeQuickTimeMovie,
    AVMediaTypeAudio, AVMediaTypeVideo, AVURLAsset,
};
use objc2_core_foundation::{CFRetained, Type};
use objc2_core_media::{
    CMAudioFormatDescriptionGetStreamBasicDescription, CMFormatDescription, CMSampleBuffer,
    CMSampleTimingInfo, CMTime, CMTimeFlags, CMTimeRange, CMTimeRoundingMethod,
    CMVideoFormatDescriptionGetDimensions, kCMTimePositiveInfinity,
};
use objc2_core_video::CVPixelBuffer;
use objc2_foundation::{NSArray, NSString, NSURL};

use crate::audio::{interleaved_f32, pcm_settings};
use crate::recode::{Decoder, Encoder, fourcc, is_sync, ten_bit};
use crate::writer::{
    aac_settings, add_input, audio_sample_buffer, catch, float_format, ns_error, writer_error,
};

/// Silence is added in pieces of at most this many samples (0.1 s).
const PAD_CHUNK: i64 = 4800;
/// How often progress is reported.
const PROGRESS_EVERY: Duration = Duration::from_millis(200);
/// The audio cross-fade at each join, in samples (10 ms).
const CROSSFADE: usize = 480;
/// How far before a part's in point its video is read from, to find the
/// keyframe before it (recordings have one at least every second).
const SEEK_BACK_SECONDS: f64 = 3.0;
/// How far before a part's in point its audio is read from.
const AUDIO_SEEK_BACK_SECONDS: f64 = 0.5;

/// The joined file's video format.
struct Format {
    codec: u32,
    size: (i32, i32),
    /// Its frame rate, rounded to whole frames per second (0: unknown).
    fps: i32,
    timescale: i32,
    hint: CFRetained<CMFormatDescription>,
}

/// Where a part's audio comes from: its own file, or (an edited file's
/// angle) the program file of the same take.
struct AudioSource {
    path: PathBuf,
    asset: Retained<AVURLAsset>,
    tracks: Vec<Retained<AVAssetTrack>>,
    /// That file's video start (its audio is timed against it) and where
    /// that start sits within the clip.
    start: CMTime,
    offset: CMTime,
}

/// One file to join.
struct Part {
    path: PathBuf,
    asset: Retained<AVURLAsset>,
    video: Retained<AVAssetTrack>,
    /// The video track's start and length in the file.
    start: CMTime,
    duration: CMTime,
    /// How long it lasts in the joined file.
    length: CMTime,
    /// Where the file starts within its clip, and where the part starts on
    /// the take's clock.
    offset: CMTime,
    in_point: CMTime,
    audio: AudioSource,
    /// Its video is in the joined file's format, so it can be copied.
    copy: bool,
}

pub fn join(spec: &JoinSpec, progress: OnJoinProgress) -> Result<Joined> {
    if spec.parts.is_empty() {
        bail!("nothing to join");
    }
    if spec.output.exists() {
        bail!("{} already exists", spec.output.display());
    }
    let partial = partial_path(&spec.output);
    let result = autoreleasepool(|_| write(spec, &partial, progress));
    match result {
        Ok(joined) => {
            rename_new(&partial, &spec.output).with_context(|| {
                format!(
                    "could not rename {} to {}",
                    partial.display(),
                    spec.output.display()
                )
            })?;
            Ok(joined)
        }
        Err(err) => {
            // Only what this join wrote.
            let _ = std::fs::remove_file(&partial);
            Err(err)
        }
    }
}

/// Rename `from` to `to` unless `to` exists (a file made there while the
/// join ran is never replaced).
fn rename_new(from: &Path, to: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = |path: &Path| std::ffi::CString::new(path.as_os_str().as_bytes());
    let (from_c, to_c) = (c(from)?, c(to)?);
    if unsafe { libc::renamex_np(from_c.as_ptr(), to_c.as_ptr(), libc::RENAME_EXCL) } == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        // A file system without exclusive renames (exFAT, SMB): check
        // first instead.
        Some(libc::ENOTSUP) | Some(libc::EINVAL) if !to.exists() => {
            std::fs::rename(from, to)?;
            Ok(())
        }
        Some(libc::EEXIST) => bail!(
            "{} appeared while the export ran; the joined file is kept as {}",
            to.display(),
            from.display()
        ),
        _ => Err(err.into()),
    }
}

/// `name.mov` → `name.partial.mov`, written until the join is complete.
fn partial_path(output: &Path) -> PathBuf {
    let stem = output
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = match output.extension() {
        Some(extension) => format!("{stem}.partial.{}", extension.to_string_lossy()),
        None => format!("{stem}.partial"),
    };
    output.with_file_name(name)
}

fn open_asset(path: &Path) -> Result<Retained<AVURLAsset>> {
    if !path.exists() {
        bail!("{} does not exist", path.display());
    }
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    Ok(unsafe { AVURLAsset::URLAssetWithURL_options(&url, None) })
}

#[allow(deprecated)] // The synchronous track lists are fine off the main thread.
fn tracks(asset: &AVURLAsset, audio: bool) -> Result<Vec<Retained<AVAssetTrack>>> {
    let kind = match audio {
        true => unsafe { AVMediaTypeAudio }.context("missing AVMediaTypeAudio")?,
        false => unsafe { AVMediaTypeVideo }.context("missing AVMediaTypeVideo")?,
    };
    let list: Retained<NSArray<AVAssetTrack>> = unsafe { asset.tracksWithMediaType(kind) };
    Ok(list.to_vec())
}

fn open_part(spec: &JoinPart, format: Option<&Format>) -> Result<Part> {
    let path = &spec.path;
    let asset = open_asset(path)?;
    let video = tracks(&asset, false)?
        .into_iter()
        .next()
        .with_context(|| format!("{} has no video macOS can read", path.display()))?;
    let range = unsafe { video.timeRange() };
    if !valid(range.duration) || range.duration.value <= 0 {
        bail!("{} has no video frames", path.display());
    }
    let timescale = unsafe { video.naturalTimeScale() }.max(1);
    let length = match spec.length {
        Some((frames, fps)) if fps > 0 => unsafe { CMTime::new(frames as i64, fps as i32) },
        _ => range.duration,
    };
    let at = |seconds: f64| unsafe { CMTime::with_seconds(seconds, timescale) };
    let offset = at(spec.offset_seconds.max(0.0));
    let audio = match &spec.audio {
        None => AudioSource {
            path: path.clone(),
            asset: asset.clone(),
            tracks: tracks(&asset, true)?,
            start: range.start,
            offset,
        },
        Some((audio_path, audio_offset)) => {
            let audio_asset = open_asset(audio_path)?;
            let start = tracks(&audio_asset, false)?
                .first()
                .map_or(unsafe { CMTime::new(0, 1) }, |track| unsafe {
                    track.timeRange().start
                });
            AudioSource {
                path: audio_path.clone(),
                tracks: tracks(&audio_asset, true)?,
                asset: audio_asset,
                start,
                offset: at(audio_offset.max(0.0)),
            }
        }
    };
    let copy = match format {
        Some(format) => matches(&video, spec.fps, format),
        None => true,
    };
    Ok(Part {
        path: path.to_path_buf(),
        asset,
        start: range.start,
        duration: range.duration,
        length,
        offset,
        in_point: at(spec.in_seconds.max(0.0)),
        audio,
        copy,
        video,
    })
}

/// A track's frame rate: `recorded` (the manifest's) if known, else its
/// average, rounded (which dropped frames pull down).
fn rate(track: &AVAssetTrack, recorded: u32) -> i32 {
    match recorded {
        0 => unsafe { track.nominalFrameRate() }.round() as i32,
        fps => fps as i32,
    }
}

/// The format of `track`'s video, at `fps` frames a second.
fn format_of(track: &AVAssetTrack, path: &Path, fps: u32) -> Result<Format> {
    let hint =
        format(track).with_context(|| format!("{}'s video has no format", path.display()))?;
    let size = unsafe { CMVideoFormatDescriptionGetDimensions(&hint) };
    Ok(Format {
        codec: unsafe { hint.media_sub_type() },
        size: (size.width, size.height),
        fps: rate(track, fps),
        timescale: unsafe { track.naturalTimeScale() }.max(1),
        hint,
    })
}

/// Whether `track`'s video can be copied into a file of `format`.
fn matches(track: &AVAssetTrack, recorded: u32, format: &Format) -> bool {
    let Some(own) = self::format(track) else {
        return false;
    };
    let size = unsafe { CMVideoFormatDescriptionGetDimensions(&own) };
    let fps = rate(track, recorded);
    let codec = unsafe { own.media_sub_type() };
    codec == format.codec
        && (size.width, size.height) == format.size
        && (fps == format.fps || fps == 0 || format.fps == 0)
}

fn valid(time: CMTime) -> bool {
    time.flags.contains(CMTimeFlags::Valid)
}

fn seconds(time: CMTime) -> f64 {
    unsafe { time.seconds() }
}

fn add(a: CMTime, b: CMTime) -> CMTime {
    unsafe { a.add(b) }
}

fn sub(a: CMTime, b: CMTime) -> CMTime {
    unsafe { a.subtract(b) }
}

fn cmp(a: CMTime, b: CMTime) -> i32 {
    unsafe { a.compare(b) }
}

fn max(a: CMTime, b: CMTime) -> CMTime {
    if cmp(a, b) >= 0 { a } else { b }
}

/// A track's first format description.
fn format(track: &AVAssetTrack) -> Option<CFRetained<CMFormatDescription>> {
    let descriptions = unsafe { track.formatDescriptions() };
    let first = descriptions.firstObject()?;
    // SAFETY: a track's format descriptions are CMFormatDescriptions.
    let description = unsafe { &*(Retained::as_ptr(&first) as *const CMFormatDescription) };
    Some(description.retain())
}

/// An audio track's channel count.
fn channels(track: &AVAssetTrack) -> Option<u32> {
    let format = format(track)?;
    let asbd = unsafe { CMAudioFormatDescriptionGetStreamBasicDescription(&format).as_ref() }?;
    Some(asbd.mChannelsPerFrame)
}

/// A reader of `track` (in `asset`, at `path`), as PCM if `pcm` gives the
/// channels, over `range` of the track's time if given.
fn reader(
    asset: &AVURLAsset,
    path: &Path,
    track: &AVAssetTrack,
    pcm: Option<u32>,
    range: Option<CMTimeRange>,
) -> Result<Reader> {
    let settings = pcm.map(pcm_settings);
    let output = unsafe {
        AVAssetReaderTrackOutput::assetReaderTrackOutputWithTrack_outputSettings(
            track,
            settings.as_deref(),
        )
    };
    unsafe { output.setAlwaysCopiesSampleData(false) };
    let reader = unsafe { AVAssetReader::initWithAsset_error(AVAssetReader::alloc(), asset) }
        .map_err(|err| ns_error(&err).context(format!("could not read {}", path.display())))?;
    let started = catch(|| unsafe {
        if let Some(range) = range {
            reader.setTimeRange(range);
        }
        reader.addOutput(&output);
        reader.startReading()
    })?;
    if !started {
        return Err(reader_error(&reader).context(format!("could not read {}", path.display())));
    }
    Ok(Reader { reader, output })
}

struct Reader {
    reader: Retained<AVAssetReader>,
    output: Retained<AVAssetReaderTrackOutput>,
}

impl Reader {
    /// The next sample buffer, `None` at the end.
    fn next(&self, path: &Path) -> Result<Option<Retained<CMSampleBuffer>>> {
        let sample = catch(|| unsafe { self.output.copyNextSampleBuffer() })?;
        if sample.is_none() && unsafe { self.reader.status() } == AVAssetReaderStatus::Failed {
            return Err(
                reader_error(&self.reader).context(format!("could not read {}", path.display()))
            );
        }
        Ok(sample)
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        unsafe { self.reader.cancelReading() };
    }
}

fn reader_error(reader: &AVAssetReader) -> anyhow::Error {
    match unsafe { reader.error() } {
        Some(err) => ns_error(&err),
        None => anyhow!("AVAssetReader status {:?}", unsafe { reader.status() }),
    }
}

/// A video sample of the current part, and when it shows in the joined
/// file.
#[derive(Clone)]
struct Sample {
    sample: Retained<CMSampleBuffer>,
    pts: CMTime,
    sync: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Copying samples until the part's end.
    Copy,
    /// Decoding and encoding the whole part again.
    Render,
    Done,
}

/// The parts' video, one after another, retimed to the joined file.
struct VideoStream<'a> {
    parts: &'a [Part],
    starts: &'a [CMTime],
    format: &'a Format,
    index: Option<usize>,
    reader: Option<Reader>,
    /// What to add to the current part's times, and where it starts and
    /// ends in the joined file.
    shift: CMTime,
    start: CMTime,
    end: CMTime,
    mode: Mode,
    /// Samples ready for the writer.
    queue: VecDeque<CFRetained<CMSampleBuffer>>,
    decoder: Option<Decoder>,
    encoder: Option<Encoder>,
    /// Frames past the part's end are counted (the part runs to its
    /// file's end, so they are frames the file holds beyond its clip).
    count_tail: bool,
    /// Rendering a whole part: the next output frame, how many it has,
    /// and the picture on screen.
    render_frame: i64,
    render_frames: i64,
    picture: Option<CFRetained<CVPixelBuffer>>,
    /// A keyframe of the part has been read (decoding starts there).
    synced: bool,
    /// Frames passed on from each part, left out of it, and encoded again.
    frames: Vec<u64>,
    left_out: Vec<u64>,
    reencoded: Vec<u64>,
}

impl VideoStream<'_> {
    fn next(&mut self) -> Result<Option<CFRetained<CMSampleBuffer>>> {
        loop {
            if let Some(sample) = self.queue.pop_front() {
                return Ok(Some(sample));
            }
            match self.mode {
                Mode::Copy => match self.read()? {
                    Some(sample) => self.copy(sample)?,
                    None => self.mode = Mode::Done,
                },
                Mode::Render => self.render_step()?,
                Mode::Done => {
                    let next = self.index.map_or(0, |i| i + 1);
                    if next == self.parts.len() {
                        return Ok(None);
                    }
                    self.begin(next)?;
                }
            }
        }
    }

    fn part(&self) -> &Part {
        &self.parts[self.index.expect("a part is open")]
    }

    /// Open part `index`: copy (from the cut) or render it.
    fn begin(&mut self, index: usize) -> Result<()> {
        self.index = Some(index);
        let part = &self.parts[index];
        self.start = self.starts[index];
        self.end = add(self.start, part.length);
        self.shift = sub(add(self.start, part.offset), add(part.in_point, part.start));
        // The part's in point on the file's video clock (it may be before
        // the file's start: the file began late).
        let file_in = seconds(sub(part.in_point, part.offset));
        let file_end = seconds(add(part.offset, part.duration));
        self.count_tail = seconds(add(part.in_point, part.length)) >= file_end - 0.5e-3;
        self.render_frame = 0;
        self.render_frames = match self.format.fps {
            0 => 0,
            fps => (seconds(part.length) * f64::from(fps)).round() as i64,
        };
        self.picture = None;
        self.synced = false;
        self.open_reader(file_in - SEEK_BACK_SECONDS)?;
        if part.copy {
            self.mode = Mode::Copy;
            self.head(file_in > SEEK_BACK_SECONDS)
        } else {
            if self.format.fps == 0 {
                bail!(
                    "{} is in another format than the joined file, whose frame rate isn't known",
                    part.path.display()
                );
            }
            self.mode = Mode::Render;
            Ok(())
        }
    }

    /// Read the current part's video from `from` seconds into its file.
    fn open_reader(&mut self, from: f64) -> Result<()> {
        let part = self.part();
        let range = (from > 0.0).then(|| CMTimeRange {
            start: add(part.start, unsafe {
                CMTime::with_seconds(from, self.format.timescale)
            }),
            duration: unsafe { kCMTimePositiveInfinity },
        });
        let reader = reader(&part.asset, &part.path, &part.video, None, range)?;
        self.reader = Some(reader);
        Ok(())
    }

    /// The current part's next sample, with its time in the joined file.
    fn read(&mut self) -> Result<Option<Sample>> {
        loop {
            let Some(reader) = &self.reader else {
                return Ok(None);
            };
            let path = &self.parts[self.index.expect("open")].path;
            let Some(sample) = reader.next(path)? else {
                self.reader = None;
                return Ok(None);
            };
            if unsafe { sample.num_samples() } <= 0 {
                continue;
            }
            let pts = unsafe { sample.presentation_time_stamp() };
            let dts = unsafe { sample.decode_time_stamp() };
            if valid(dts) && valid(pts) && cmp(dts, pts) != 0 {
                bail!(
                    "{} has reordered frames (B-frames), which exports can't cut yet; Isocast \
                     records without them",
                    path.display()
                );
            }
            return Ok(Some(Sample {
                sync: is_sync(&sample),
                pts: add(pts, self.shift),
                sample,
            }));
        }
    }

    /// Pass `sample` on as it is, if it starts before the part's end.
    fn copy(&mut self, sample: Sample) -> Result<()> {
        let index = self.index.expect("open");
        if cmp(sample.pts, self.end) >= 0 {
            self.left_out[index] += u64::from(self.count_tail);
            if !self.count_tail {
                self.reader = None;
                self.mode = Mode::Done;
            }
            return Ok(());
        }
        let copy = retimed(&sample.sample, self.shift).with_context(|| {
            format!(
                "could not retime a frame of {}",
                self.parts[index].path.display()
            )
        })?;
        self.queue.push_back(copy);
        self.frames[index] += 1;
        Ok(())
    }

    /// The start of a copied part: the frame on screen at its in point,
    /// copied if it is a keyframe, else encoded again with the frames up
    /// to the next keyframe; then copying carries on from there.
    fn head(&mut self, may_reread: bool) -> Result<()> {
        let index = self.index.expect("open");
        // The keyframe interval on screen at the start: its keyframe first.
        let mut interval: Vec<Sample> = Vec::new();
        let mut skipped = false;
        let after = loop {
            match self.read()? {
                None => break None,
                Some(sample) if cmp(sample.pts, self.start) <= 0 => {
                    if sample.sync {
                        interval.clear();
                    } else if interval.is_empty() {
                        // Before the first keyframe read: can't be decoded.
                        skipped = true;
                        continue;
                    }
                    interval.push(sample);
                }
                Some(sample) => break Some(sample),
            }
        };
        if interval.is_empty() && skipped {
            // The reader began after the keyframe before the in point.
            if may_reread {
                self.open_reader(0.0)?;
                return self.head(false);
            }
            bail!(
                "{} has no keyframe before {:.3} s",
                self.part().path.display(),
                seconds(sub(self.part().in_point, self.part().offset))
            );
        }
        let Some(on_screen) = interval.last().cloned() else {
            // The file starts after the part does (it began late): its
            // first frame is a keyframe, copied where it falls.
            if let Some(sample) = after {
                if !sample.sync {
                    bail!(
                        "{} doesn't start with a keyframe",
                        self.part().path.display()
                    );
                }
                self.copy(sample)?;
            }
            return Ok(());
        };
        if on_screen.sync {
            // A keyframe: copied, shown from the part's start.
            let copy = retimed(&on_screen.sample, sub(self.start, on_screen.sample_pts()))?;
            self.queue.push_back(copy);
            self.frames[index] += 1;
            if let Some(sample) = after {
                self.copy(sample)?;
            }
            return Ok(());
        }
        // Encode again from the frame on screen to the next keyframe.
        let mut again = vec![on_screen];
        let mut next = after;
        let keyframe = loop {
            match next.take() {
                None => break None,
                Some(sample) if sample.sync || cmp(sample.pts, self.end) >= 0 => {
                    break Some(sample);
                }
                Some(sample) => {
                    again.push(sample);
                    next = self.read()?;
                }
            }
        };
        let decode_only = interval.len() - 1;
        let frame = self.frame_duration();
        let mut first = true;
        for (i, sample) in interval[..decode_only].iter().chain(&again).enumerate() {
            let pictures = self.decode(&sample.sample)?;
            if i < decode_only {
                continue;
            }
            let Some((picture, _)) = pictures.into_iter().last() else {
                bail!(
                    "{}: a frame decoded to no picture",
                    self.part().path.display()
                );
            };
            let pts = max(sample.pts, self.start);
            self.encode(&picture, pts, frame, first)?;
            first = false;
            self.frames[index] += 1;
            self.reencoded[index] += 1;
        }
        self.finish_encoding()?;
        if let Some(sample) = keyframe {
            self.copy(sample)?;
        }
        Ok(())
    }

    /// One frame's duration in the joined file.
    fn frame_duration(&self) -> CMTime {
        unsafe { CMTime::new(1, self.format.fps.max(1)) }
    }

    fn decode(
        &mut self,
        sample: &CMSampleBuffer,
    ) -> Result<Vec<(CFRetained<CVPixelBuffer>, CMTime)>> {
        let format = unsafe { sample.format_description() }.context("a frame without a format")?;
        if !self
            .decoder
            .as_ref()
            .is_some_and(|decoder| decoder.accepts(&format))
        {
            self.decoder = Some(Decoder::new(&format)?);
        }
        let path = &self.parts[self.index.expect("open")].path;
        self.decoder
            .as_mut()
            .expect("made")
            .decode(sample)
            .with_context(|| format!("could not decode {}", path.display()))
    }

    fn encode(
        &mut self,
        picture: &CVPixelBuffer,
        pts: CMTime,
        duration: CMTime,
        keyframe: bool,
    ) -> Result<()> {
        let ten = ten_bit(picture);
        if self
            .encoder
            .as_ref()
            .is_some_and(|encoder| encoder.ten_bit != ten)
        {
            // Another bit depth: its frames so far go first.
            self.finish_encoding()?;
            self.encoder = None;
        }
        if self.encoder.is_none() {
            let size = (self.format.size.0 as u32, self.format.size.1 as u32);
            self.encoder = Some(Encoder::new(
                self.format.codec,
                size,
                f64::from(self.format.fps.max(1)),
                ten,
            )?);
        }
        let encoder = self.encoder.as_mut().expect("made");
        encoder.encode(picture, pts, duration, keyframe)?;
        self.queue.extend(encoder.take());
        Ok(())
    }

    /// Wait for every frame given to the encoder, and queue them.
    fn finish_encoding(&mut self) -> Result<()> {
        if let Some(encoder) = &mut self.encoder {
            encoder.flush()?;
            self.queue.extend(encoder.take());
        }
        Ok(())
    }

    /// Render the whole part on the joined file's frame grid: each output
    /// frame is the picture on screen at its time (the first picture
    /// before the file starts, the last after it ends).
    fn render_step(&mut self) -> Result<()> {
        if self.render_frame >= self.render_frames {
            self.reader = None;
            self.mode = Mode::Done;
            return self.finish_encoding();
        }
        let Some(sample) = self.read()? else {
            // The file ended: its last picture holds to the part's end.
            match self.picture.clone() {
                Some(picture) => {
                    while self.render_frame < self.render_frames {
                        self.render_picture(&picture)?;
                    }
                }
                None => bail!("{} has no frames in its clip", self.part().path.display()),
            }
            return Ok(());
        };
        if !self.synced && !sample.sync {
            // Before the first keyframe read: can't be decoded.
            return Ok(());
        }
        self.synced = true;
        let pictures = self.decode(&sample.sample)?;
        let quarter = unsafe { CMTime::new(1, 4 * self.format.fps.max(1)) };
        for (picture, _) in pictures {
            // Frames before this picture show the one before it.
            loop {
                let at = self.render_time();
                if self.render_frame >= self.render_frames || cmp(add(at, quarter), sample.pts) >= 0
                {
                    break;
                }
                let shown = self.picture.clone().unwrap_or_else(|| picture.clone());
                self.render_picture(&shown)?;
            }
            self.picture = Some(picture);
        }
        Ok(())
    }

    /// The joined-file time of the next rendered frame.
    fn render_time(&self) -> CMTime {
        add(self.start, unsafe {
            CMTime::new(self.render_frame, self.format.fps.max(1))
        })
    }

    fn render_picture(&mut self, picture: &CVPixelBuffer) -> Result<()> {
        let index = self.index.expect("open");
        let (at, frame, first) = (
            self.render_time(),
            self.frame_duration(),
            self.render_frame == 0,
        );
        self.encode(picture, at, frame, first)?;
        self.render_frame += 1;
        self.frames[index] += 1;
        self.reencoded[index] += 1;
        Ok(())
    }
}

impl Sample {
    /// Its time in its file.
    fn sample_pts(&self) -> CMTime {
        unsafe { self.sample.presentation_time_stamp() }
    }
}

/// `sample` with every time moved by `shift`.
fn retimed(sample: &CMSampleBuffer, shift: CMTime) -> Result<CFRetained<CMSampleBuffer>> {
    let mut needed = 0;
    unsafe { sample.sample_timing_info_array(0, ptr::null_mut(), &mut needed) };
    let entries = needed.max(1) as usize;
    let mut timing = vec![
        CMSampleTimingInfo {
            duration: unsafe { CMTime::new(0, 1) },
            presentationTimeStamp: unsafe { CMTime::new(0, 1) },
            decodeTimeStamp: unsafe { CMTime::new(0, 1) },
        };
        entries
    ];
    let status = unsafe {
        sample.sample_timing_info_array(entries as isize, timing.as_mut_ptr(), &mut needed)
    };
    if status != 0 {
        bail!("no timing (CoreMedia error {status})");
    }
    timing.truncate(needed.max(0) as usize);
    for entry in &mut timing {
        if valid(entry.presentationTimeStamp) {
            entry.presentationTimeStamp = add(entry.presentationTimeStamp, shift);
        }
        if valid(entry.decodeTimeStamp) {
            entry.decodeTimeStamp = add(entry.decodeTimeStamp, shift);
        }
    }
    let mut out = ptr::null_mut();
    let status = unsafe {
        CMSampleBuffer::create_copy_with_new_timing(
            None,
            sample,
            timing.len() as isize,
            timing.as_ptr(),
            NonNull::from(&mut out),
        )
    };
    match NonNull::new(out) {
        Some(out) if status == 0 => Ok(unsafe { CFRetained::from_raw(out) }),
        _ => bail!("CoreMedia error {status}"),
    }
}

/// One audio track of every part, one after another, as continuous audio:
/// each part's from its in point, cut or padded with silence to the part's
/// length, and cross-faded into the next.
struct AudioStream<'a> {
    parts: &'a [Part],
    /// Where each part starts and the joined file ends, in samples.
    bounds: &'a [i64],
    track: usize,
    channels: u32,
    format: CFRetained<CMFormatDescription>,
    index: Option<usize>,
    reader: Option<Reader>,
    /// Samples written so far, in all.
    written: i64,
    /// Where the current part's in point is on its track's clock, in
    /// samples, until its first samples are read (then `pad` and `skip`
    /// say how they line up).
    target: Option<i64>,
    /// Silence still to add before the current part's audio, and samples
    /// of it still to skip (its audio starts after or before its in point).
    pad: i64,
    skip: i64,
    /// Samples read and not yet written (interleaved).
    pending: Vec<f32>,
    /// The previous part's audio just past its end, faded out under the
    /// current part's first [`CROSSFADE`] samples, and how many of those
    /// have been written.
    tail: Vec<f32>,
    faded: usize,
}

impl AudioStream<'_> {
    fn next(&mut self) -> Result<Option<CFRetained<CMSampleBuffer>>> {
        let channels = self.channels as usize;
        loop {
            if let Some(index) = self.index {
                let end = self.bounds[index + 1];
                if self.written < end {
                    let room = end - self.written;
                    if self.pad > 0 {
                        // Before the part's audio.
                        let count = room.min(PAD_CHUNK).min(self.pad);
                        self.pad -= count;
                        let silence = vec![0.0; count as usize * channels];
                        return self.emit(silence).map(Some);
                    }
                    if self.pending.is_empty() {
                        if !self.fill()? {
                            // After the part's audio ran out.
                            let count = room.min(PAD_CHUNK);
                            let silence = vec![0.0; count as usize * channels];
                            return self.emit(silence).map(Some);
                        }
                        continue;
                    }
                    let take = ((self.pending.len() / channels) as i64).min(room) as usize;
                    let rest = self.pending.split_off(take * channels);
                    let samples = std::mem::replace(&mut self.pending, rest);
                    return self.emit(samples).map(Some);
                }
                // The part is done: keep what follows its end, to fade out
                // under the next part's start.
                self.tail.clear();
                if index + 1 < self.parts.len() && self.pad == 0 {
                    while self.pending.len() < CROSSFADE * channels && self.fill()? {}
                    self.pending.truncate(CROSSFADE * channels);
                    self.tail = std::mem::take(&mut self.pending);
                }
                self.pending.clear();
                self.reader = None;
            }
            let next = self.index.map_or(0, |i| i + 1);
            if next == self.parts.len() {
                return Ok(None);
            }
            self.begin(next)?;
        }
    }

    fn begin(&mut self, index: usize) -> Result<()> {
        self.index = Some(index);
        self.faded = 0;
        self.pad = 0;
        self.skip = 0;
        let part = &self.parts[index];
        let source = &part.audio;
        let track = &source.tracks[self.track];
        // The in point on the track's clock: the file's video start is at
        // its offset in the clip.
        let in_point = add(source.start, sub(part.in_point, source.offset));
        self.target = Some(in_samples(in_point));
        let from = sub(in_point, unsafe {
            CMTime::with_seconds(AUDIO_SEEK_BACK_SECONDS, SAMPLE_RATE as i32)
        });
        let track_start = unsafe { track.timeRange() }.start;
        let range = (valid(track_start) && cmp(from, track_start) > 0).then_some(CMTimeRange {
            start: from,
            duration: unsafe { kCMTimePositiveInfinity },
        });
        self.reader = Some(reader(
            &source.asset,
            &source.path,
            track,
            Some(self.channels),
            range,
        )?);
        Ok(())
    }

    /// Read the next samples into `pending`, skipping what comes before
    /// the in point; false at the end of the part's audio.
    fn fill(&mut self) -> Result<bool> {
        let channels = self.channels as usize;
        loop {
            let Some(reader) = &self.reader else {
                return Ok(false);
            };
            let path = &self.parts[self.index.expect("open")].audio.path;
            let Some(sample) = reader.next(path)? else {
                self.reader = None;
                return Ok(false);
            };
            let pts = unsafe { sample.presentation_time_stamp() };
            let Some(mut samples) = interleaved_f32(&sample, self.channels)? else {
                continue;
            };
            if let Some(target) = self.target.take() {
                // The first samples read: line them up with the in point.
                let first = if valid(pts) { in_samples(pts) } else { target };
                self.pad = (first - target).max(0);
                self.skip = (target - first).max(0);
            }
            if self.skip > 0 {
                let skipped = (self.skip as usize).min(samples.len() / channels);
                samples.drain(..skipped * channels);
                self.skip -= skipped as i64;
            }
            if samples.is_empty() {
                continue;
            }
            self.pending.extend_from_slice(&samples);
            return Ok(true);
        }
    }

    fn emit(&mut self, mut samples: Vec<f32>) -> Result<CFRetained<CMSampleBuffer>> {
        let channels = self.channels as usize;
        // Over the outgoing audio there is (10 ms, or less where its file
        // ends sooner), so the incoming part never fades in over nothing.
        let fade = self.tail.len() / channels;
        if fade > 0 && self.faded < fade {
            let frames = samples.len() / channels;
            for frame in 0..frames {
                let n = self.faded + frame;
                if n >= fade {
                    break;
                }
                let gain = (n as f32 + 0.5) / fade as f32;
                for c in 0..channels {
                    let before = self.tail.get(n * channels + c).copied().unwrap_or(0.0);
                    let sample = &mut samples[frame * channels + c];
                    *sample = *sample * gain + before * (1.0 - gain);
                }
            }
            self.faded += frames;
        }
        let pts = unsafe { CMTime::new(self.written, SAMPLE_RATE as i32) };
        let buffer = audio_sample_buffer(&self.format, channels, &samples, pts)?;
        self.written += (samples.len() / channels) as i64;
        Ok(buffer)
    }
}

/// `time` in audio samples.
fn in_samples(time: CMTime) -> i64 {
    unsafe {
        time.convert_scale(
            SAMPLE_RATE as i32,
            CMTimeRoundingMethod::RoundHalfAwayFromZero,
        )
    }
    .value
}

fn write(spec: &JoinSpec, path: &Path, progress: OnJoinProgress) -> Result<Joined> {
    // The joined file's video format: the format file's, else the first
    // part's.
    let format = match &spec.format {
        Some(file) => {
            let asset = open_asset(file)?;
            let video = tracks(&asset, false)?
                .into_iter()
                .next()
                .with_context(|| format!("{} has no video macOS can read", file.display()))?;
            // The program's: the take's frame grid.
            format_of(&video, file, spec.parts[0].length.map_or(0, |(_, fps)| fps))?
        }
        None => {
            let first = &spec.parts[0].path;
            let asset = open_asset(first)?;
            let video = tracks(&asset, false)?
                .into_iter()
                .next()
                .with_context(|| format!("{} has no video macOS can read", first.display()))?;
            format_of(&video, first, spec.parts[0].fps)?
        }
    };
    let parts = spec
        .parts
        .iter()
        .map(|part| open_part(part, Some(&format)))
        .collect::<Result<Vec<_>>>()?;
    let first = &parts[0];
    let tracks = first.audio.tracks.len();
    let mut track_channels = Vec::new();
    for track in &first.audio.tracks {
        track_channels.push(channels(track).filter(|c| *c > 0).with_context(|| {
            format!(
                "{}: an audio track without channels",
                first.audio.path.display()
            )
        })?);
    }
    for part in &parts[1..] {
        if part.audio.tracks.len() != tracks {
            bail!(
                "{} has {} audio tracks, {} has {tracks}; files joined need the same tracks",
                part.audio.path.display(),
                part.audio.tracks.len(),
                first.audio.path.display()
            );
        }
    }
    for part in parts.iter().filter(|part| !part.copy) {
        isocast_diagnostics::debug!(
            "join: {} is encoded again ({} at {}x{}, {} fps, like the joined file)",
            part.path.display(),
            fourcc(format.codec),
            format.size.0,
            format.size.1,
            format.fps
        );
    }
    // Where each part starts in the joined file: the parts' lengths added
    // up, exactly.
    let timescale = format.timescale;
    let mut starts = Vec::new();
    let mut cursor = unsafe { CMTime::new(0, timescale) };
    for part in &parts {
        starts.push(cursor);
        cursor = add(cursor, part.length);
    }
    let total = cursor;
    let mut bounds: Vec<i64> = starts.iter().map(|t| in_samples(*t)).collect();
    bounds.push(in_samples(total));

    let url = NSURL::fileURLWithPath(&NSString::from_str(
        path.to_str().context("the file path is not valid UTF-8")?,
    ));
    let file_type = match spec.container {
        Container::Mov => unsafe { AVFileTypeQuickTimeMovie },
        Container::Mp4 => unsafe { AVFileTypeMPEG4 },
    }
    .context("AVFoundation is missing a file type constant")?;
    let writer = unsafe { AVAssetWriter::assetWriterWithURL_fileType_error(&url, file_type) }
        .map_err(|err| ns_error(&err))?;
    let video_type = unsafe { AVMediaTypeVideo }.context("missing AVMediaTypeVideo")?;
    let video_input = catch(|| unsafe {
        let input =
            AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings_sourceFormatHint(
                video_type,
                None,
                Some(&format.hint),
            );
        input.setExpectsMediaDataInRealTime(false);
        input.setMediaTimeScale(timescale);
        input
    })?;
    add_input(&writer, &video_input)?;
    let audio_type = unsafe { AVMediaTypeAudio }.context("missing AVMediaTypeAudio")?;
    let mut audio_inputs = Vec::new();
    for channels in &track_channels {
        let settings = aac_settings(*channels);
        let input = catch(|| unsafe {
            let input = AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(
                audio_type,
                Some(&settings),
            );
            input.setExpectsMediaDataInRealTime(false);
            input
        })?;
        add_input(&writer, &input)?;
        audio_inputs.push(input);
    }
    if !unsafe { writer.startWriting() } {
        return Err(writer_error(&writer).context("could not start writing"));
    }
    catch(|| unsafe { writer.startSessionAtSourceTime(CMTime::new(0, timescale)) })?;

    let zero = unsafe { CMTime::new(0, 1) };
    let mut video = VideoStream {
        parts: &parts,
        starts: &starts,
        format: &format,
        index: None,
        reader: None,
        shift: zero,
        start: zero,
        end: zero,
        mode: Mode::Done,
        queue: VecDeque::new(),
        decoder: None,
        encoder: None,
        count_tail: false,
        render_frame: 0,
        render_frames: 0,
        picture: None,
        synced: false,
        frames: vec![0; parts.len()],
        left_out: vec![0; parts.len()],
        reencoded: vec![0; parts.len()],
    };
    let mut audio = track_channels
        .iter()
        .enumerate()
        .map(|(track, channels)| {
            Ok(AudioStream {
                parts: &parts,
                bounds: &bounds,
                track,
                channels: *channels,
                format: float_format(*channels)?,
                index: None,
                reader: None,
                written: 0,
                target: None,
                pad: 0,
                skip: 0,
                pending: Vec::new(),
                tail: Vec::new(),
                faded: 0,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let result = (|| -> Result<()> {
        let mut video_done = false;
        let mut audio_done = vec![false; audio.len()];
        let mut appended = zero;
        let mut reported = Instant::now();
        let total_seconds = seconds(total).max(1e-9);
        if !progress(0.0) {
            bail!("the join was stopped");
        }
        while !(video_done && audio_done.iter().all(|done| *done)) {
            let progressed = autoreleasepool(|_| -> Result<bool> {
                let mut progressed = false;
                if !video_done && unsafe { video_input.isReadyForMoreMediaData() } {
                    match video.next()? {
                        Some(sample) => {
                            appended = unsafe { sample.presentation_time_stamp() };
                            if !unsafe { video_input.appendSampleBuffer(&sample) } {
                                return Err(
                                    writer_error(&writer).context("could not append a video frame")
                                );
                            }
                        }
                        None => {
                            catch(|| unsafe { video_input.markAsFinished() })?;
                            video_done = true;
                        }
                    }
                    progressed = true;
                }
                for ((stream, input), done) in
                    audio.iter_mut().zip(&audio_inputs).zip(&mut audio_done)
                {
                    if !*done && unsafe { input.isReadyForMoreMediaData() } {
                        match stream.next()? {
                            Some(sample) => {
                                if !unsafe { input.appendSampleBuffer(&sample) } {
                                    return Err(
                                        writer_error(&writer).context("could not append audio")
                                    );
                                }
                            }
                            None => {
                                catch(|| unsafe { input.markAsFinished() })?;
                                *done = true;
                            }
                        }
                        progressed = true;
                    }
                }
                Ok(progressed)
            })?;
            if !progressed {
                if unsafe { writer.status() } == AVAssetWriterStatus::Failed {
                    return Err(writer_error(&writer).context("the file writer failed"));
                }
                thread::sleep(Duration::from_millis(1));
            }
            if reported.elapsed() >= PROGRESS_EVERY {
                reported = Instant::now();
                if !progress((seconds(appended) / total_seconds).clamp(0.0, 1.0)) {
                    bail!("the join was stopped");
                }
            }
        }
        let (done, wait) = mpsc::channel();
        let handler = RcBlock::new(move || {
            let _ = done.send(());
        });
        catch(|| unsafe {
            writer.endSessionAtSourceTime(total);
            writer.finishWritingWithCompletionHandler(&handler);
        })?;
        wait.recv()
            .context("AVAssetWriter never reported finishing")?;
        if unsafe { writer.status() } != AVAssetWriterStatus::Completed {
            return Err(writer_error(&writer).context("could not finish the file"));
        }
        progress(1.0);
        Ok(())
    })();
    if let Err(err) = result {
        if unsafe { writer.status() } == AVAssetWriterStatus::Writing {
            unsafe { writer.cancelWriting() };
        }
        return Err(err);
    }
    Ok(Joined {
        duration_seconds: seconds(total),
        frames: video.frames.iter().sum(),
        parts: parts
            .iter()
            .enumerate()
            .map(|(i, part)| JoinedPart {
                start_seconds: seconds(starts[i]),
                duration_seconds: seconds(part.length),
                frames: video.frames[i],
                frames_left_out: video.left_out[i],
                reencoded: video.reencoded[i],
                converted: !part.copy,
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_joined_file_never_replaces_one_made_meanwhile() {
        let folder =
            std::env::temp_dir().join(format!("isocast-join-rename-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let (partial, output) = (folder.join("a.partial.mov"), folder.join("a.mov"));
        std::fs::write(&partial, b"joined").unwrap();
        std::fs::write(&output, b"someone else's").unwrap();
        let err = rename_new(&partial, &output).unwrap_err();
        assert!(
            err.to_string().contains("appeared while the export ran"),
            "{err}"
        );
        assert_eq!(std::fs::read(&output).unwrap(), b"someone else's");
        assert_eq!(std::fs::read(&partial).unwrap(), b"joined");
        std::fs::remove_file(&output).unwrap();
        rename_new(&partial, &output).unwrap();
        assert_eq!(std::fs::read(&output).unwrap(), b"joined");
        assert!(!partial.exists());
        let _ = std::fs::remove_dir_all(&folder);
    }
}
