//! Joining files end to end (a timeline's export) with AVAssetReader and
//! AVAssetWriter: the video samples are copied as they are (no decoding or
//! encoding), retimed to follow each other; each audio track is decoded and
//! encoded again (AAC, as recordings are) as one continuous track, cut or
//! padded with silence to each part's length, so it lines up with the
//! video at every join. Each part fills exactly its clip's length (see
//! [`JoinPart::length`]), so files at different frame rates joined from the
//! same clips switch clips at the same moments.
//!
//! Why not AVMutableComposition with a passthrough export: every take's AAC
//! starts with its own encoder priming, so a passthrough join needs an edit
//! list entry per join to skip it. QuickTime plays that right, but FFmpeg
//! (and the editors built on it) placed the second take's audio ~20 ms
//! early (measured 2026-10-13), more than a frame at 60 fps.

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
    CMSampleTimingInfo, CMTime, CMTimeFlags, CMTimeRoundingMethod,
};
use objc2_foundation::{NSArray, NSString, NSURL};

use crate::audio::{interleaved_f32, pcm_settings};
use crate::writer::{
    aac_settings, add_input, audio_sample_buffer, catch, float_format, ns_error, writer_error,
};

/// Silence is added in pieces of at most this many samples (0.1 s).
const PAD_CHUNK: i64 = 4800;
/// How often progress is reported.
const PROGRESS_EVERY: Duration = Duration::from_millis(200);

/// One file to join.
struct Part {
    path: PathBuf,
    asset: Retained<AVURLAsset>,
    video: Retained<AVAssetTrack>,
    audio: Vec<Retained<AVAssetTrack>>,
    /// The video track's start in the file.
    start: CMTime,
    /// How long it lasts in the joined file.
    length: CMTime,
    /// Where the file starts within that.
    offset: CMTime,
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

#[allow(deprecated)] // The synchronous track list is fine off the main thread.
fn open_part(spec: &JoinPart) -> Result<Part> {
    let path = &spec.path;
    if !path.exists() {
        bail!("{} does not exist", path.display());
    }
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    let asset = unsafe { AVURLAsset::URLAssetWithURL_options(&url, None) };
    let video_type = unsafe { AVMediaTypeVideo }.context("missing AVMediaTypeVideo")?;
    let audio_type = unsafe { AVMediaTypeAudio }.context("missing AVMediaTypeAudio")?;
    let video: Retained<NSArray<AVAssetTrack>> = unsafe { asset.tracksWithMediaType(video_type) };
    let video = video
        .firstObject()
        .with_context(|| format!("{} has no video macOS can read", path.display()))?;
    let audio: Retained<NSArray<AVAssetTrack>> = unsafe { asset.tracksWithMediaType(audio_type) };
    let range = unsafe { video.timeRange() };
    if !valid(range.duration) || range.duration.value <= 0 {
        bail!("{} has no video frames", path.display());
    }
    let timescale = unsafe { video.naturalTimeScale() }.max(1);
    let length = match spec.length {
        Some((frames, fps)) if fps > 0 => unsafe { CMTime::new(frames as i64, fps as i32) },
        _ => range.duration,
    };
    Ok(Part {
        path: path.to_path_buf(),
        asset,
        start: range.start,
        length,
        offset: unsafe { CMTime::with_seconds(spec.offset_seconds.max(0.0), timescale) },
        video,
        audio: audio.to_vec(),
    })
}

fn valid(time: CMTime) -> bool {
    time.flags.contains(CMTimeFlags::Valid)
}

fn seconds(time: CMTime) -> f64 {
    unsafe { time.seconds() }
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

fn reader(part: &Part, track: &AVAssetTrack, pcm: Option<u32>) -> Result<Reader> {
    let settings = pcm.map(pcm_settings);
    let output = unsafe {
        AVAssetReaderTrackOutput::assetReaderTrackOutputWithTrack_outputSettings(
            track,
            settings.as_deref(),
        )
    };
    unsafe { output.setAlwaysCopiesSampleData(false) };
    let reader = unsafe { AVAssetReader::initWithAsset_error(AVAssetReader::alloc(), &part.asset) }
        .map_err(|err| ns_error(&err).context(format!("could not read {}", part.path.display())))?;
    let started = catch(|| unsafe {
        reader.addOutput(&output);
        reader.startReading()
    })?;
    if !started {
        return Err(
            reader_error(&reader).context(format!("could not read {}", part.path.display()))
        );
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

/// The parts' video, one after another, retimed to the joined file.
struct VideoStream<'a> {
    parts: &'a [Part],
    starts: &'a [CMTime],
    index: Option<usize>,
    reader: Option<Reader>,
    /// What to add to the current part's times, and where it ends.
    shift: CMTime,
    end: CMTime,
    /// Frames passed on from each part, and left out of it.
    frames: Vec<u64>,
    left_out: Vec<u64>,
}

impl VideoStream<'_> {
    fn next(&mut self) -> Result<Option<CFRetained<CMSampleBuffer>>> {
        loop {
            if let (Some(reader), Some(index)) = (&self.reader, self.index) {
                let part = &self.parts[index];
                match reader.next(&part.path)? {
                    Some(sample) => {
                        let count = unsafe { sample.num_samples() };
                        if count <= 0 {
                            continue;
                        }
                        let copy = retimed(&sample, self.shift).with_context(|| {
                            format!("could not retime a frame of {}", part.path.display())
                        })?;
                        // A frame starting past the clip's end isn't in it.
                        let pts = unsafe { copy.presentation_time_stamp() };
                        if valid(pts) && unsafe { pts.compare(self.end) } >= 0 {
                            self.left_out[index] += count as u64;
                            continue;
                        }
                        self.frames[index] += count as u64;
                        return Ok(Some(copy));
                    }
                    None => self.reader = None,
                }
            }
            let next = self.index.map_or(0, |i| i + 1);
            if next == self.parts.len() {
                return Ok(None);
            }
            let part = &self.parts[next];
            self.index = Some(next);
            self.reader = Some(reader(part, &part.video, None)?);
            self.shift = unsafe { self.starts[next].add(part.offset).subtract(part.start) };
            self.end = unsafe { self.starts[next].add(part.length) };
        }
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
            entry.presentationTimeStamp = unsafe { entry.presentationTimeStamp.add(shift) };
        }
        if valid(entry.decodeTimeStamp) {
            entry.decodeTimeStamp = unsafe { entry.decodeTimeStamp.add(shift) };
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
/// each part's cut or padded with silence to the part's length.
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
    /// Silence still to add before the current part's audio, and samples
    /// of it still to skip (its audio starts before or after its video).
    pad: i64,
    skip: i64,
}

impl AudioStream<'_> {
    fn next(&mut self) -> Result<Option<CFRetained<CMSampleBuffer>>> {
        loop {
            if let Some(index) = self.index {
                let end = self.bounds[index + 1];
                if self.written < end {
                    if self.pad > 0 || self.reader.is_none() {
                        // Before the part's audio, or after it ran out.
                        let mut count = (end - self.written).min(PAD_CHUNK);
                        if self.reader.is_some() {
                            count = count.min(self.pad);
                            self.pad -= count;
                        }
                        let silence = vec![0.0; count as usize * self.channels as usize];
                        return self.emit(&silence).map(Some);
                    }
                    let reader = self.reader.as_ref().expect("checked");
                    let part = &self.parts[index];
                    let Some(sample) = reader.next(&part.path)? else {
                        self.reader = None;
                        continue;
                    };
                    let Some(mut samples) = interleaved_f32(&sample, self.channels)? else {
                        continue;
                    };
                    let channels = self.channels as usize;
                    if self.skip > 0 {
                        let skipped = (self.skip as usize).min(samples.len() / channels);
                        samples.drain(..skipped * channels);
                        self.skip -= skipped as i64;
                    }
                    let frames = (samples.len() / channels) as i64;
                    let take = frames.min(end - self.written);
                    if take <= 0 {
                        continue;
                    }
                    samples.truncate(take as usize * channels);
                    return self.emit(&samples).map(Some);
                }
                self.reader = None;
            }
            let next = self.index.map_or(0, |i| i + 1);
            if next == self.parts.len() {
                return Ok(None);
            }
            self.index = Some(next);
            let part = &self.parts[next];
            let track = &part.audio[self.track];
            self.reader = Some(reader(part, track, Some(self.channels))?);
            // Where the part's audio starts: after the file's offset in its
            // clip, and its own start against the file's video.
            let range = unsafe { track.timeRange() };
            let mut lead = in_samples(part.offset);
            if valid(range.start) {
                lead += in_samples(unsafe { range.start.subtract(part.start) });
            }
            self.pad = lead.max(0);
            self.skip = (-lead).max(0);
        }
    }

    fn emit(&mut self, samples: &[f32]) -> Result<CFRetained<CMSampleBuffer>> {
        let pts = unsafe { CMTime::new(self.written, SAMPLE_RATE as i32) };
        let buffer = audio_sample_buffer(&self.format, self.channels as usize, samples, pts)?;
        self.written += (samples.len() / self.channels as usize) as i64;
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
    let parts = spec
        .parts
        .iter()
        .map(open_part)
        .collect::<Result<Vec<_>>>()?;
    let first = &parts[0];
    let tracks = first.audio.len();
    let mut track_channels = Vec::new();
    for track in &first.audio {
        track_channels.push(channels(track).filter(|c| *c > 0).with_context(|| {
            format!("{}: an audio track without channels", first.path.display())
        })?);
    }
    for part in &parts[1..] {
        if part.audio.len() != tracks {
            bail!(
                "{} has {} audio tracks, {} has {tracks}; files joined need the same tracks",
                part.path.display(),
                part.audio.len(),
                first.path.display()
            );
        }
    }
    // Where each part starts in the joined file: the parts' lengths added
    // up, exactly.
    let timescale = unsafe { first.video.naturalTimeScale() };
    let mut starts = Vec::new();
    let mut cursor = unsafe { CMTime::new(0, timescale.max(1)) };
    for part in &parts {
        starts.push(cursor);
        cursor = unsafe { cursor.add(part.length) };
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
    let hint = format(&first.video).context("the first file's video has no format")?;
    let video_input = catch(|| unsafe {
        let input =
            AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings_sourceFormatHint(
                video_type,
                None,
                Some(&hint),
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
    catch(|| unsafe { writer.startSessionAtSourceTime(CMTime::new(0, timescale.max(1))) })?;

    let mut video = VideoStream {
        parts: &parts,
        starts: &starts,
        index: None,
        reader: None,
        shift: unsafe { CMTime::new(0, 1) },
        end: unsafe { CMTime::new(0, 1) },
        frames: vec![0; parts.len()],
        left_out: vec![0; parts.len()],
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
                pad: 0,
                skip: 0,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let result = (|| -> Result<()> {
        let mut video_done = false;
        let mut audio_done = vec![false; audio.len()];
        let mut appended = unsafe { CMTime::new(0, 1) };
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
    let (frames, left_out) = (video.frames.clone(), video.left_out.clone());
    Ok(Joined {
        duration_seconds: seconds(total),
        frames: frames.iter().sum(),
        parts: parts
            .iter()
            .zip(&starts)
            .zip(frames.into_iter().zip(left_out))
            .map(|((part, start), (frames, frames_left_out))| JoinedPart {
                start_seconds: seconds(*start),
                duration_seconds: seconds(part.length),
                frames,
                frames_left_out,
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
