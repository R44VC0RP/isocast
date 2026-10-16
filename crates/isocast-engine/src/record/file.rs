//! Writing files: one per ISO source per take, with every audio track kept
//! aligned to the video on the session clock.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::thread;

use anyhow::{Context, Result, anyhow};
use isocast_protocol::{Source, SourceKind};

use super::pause::RecordingClock;
use super::{
    AUDIO_CHUNK, AUDIO_JITTER, COMMIT_WAIT, FilePlan, MAX_PENDING_AUDIO, Packet, SPLIT_AUDIO_WAIT,
    SPLIT_FINISH_DELAY, Shared,
};
use crate::record::FileReport;
use crate::{FileWriter, RepairedFile, SAMPLE_RATE, Time};

#[derive(Clone, Copy)]
pub(super) struct AudioTiming {
    /// Media/browser output may legitimately stop, including while on air.
    pub sparse: bool,
    /// Allow late capture/decoding and negative sync offsets before padding.
    wait: std::time::Duration,
}

impl AudioTiming {
    pub(super) fn new(source: Option<&Source>) -> Self {
        Self {
            sparse: source
                .is_some_and(|s| matches!(s.kind, SourceKind::Media(_) | SourceKind::Browser(_))),
            wait: SPLIT_AUDIO_WAIT
                + std::time::Duration::from_millis(
                    source.map_or(0, |s| (-i64::from(s.audio.offset_ms)).max(0)) as u64,
                ),
        }
    }
}

/// Switch a writer thread to a new take's file at `at`.
pub(super) struct Split<W> {
    pub at: Time,
    pub take: usize,
    pub writer: W,
    pub plan: Arc<FilePlan>,
}

/// A finished (or failed) file of take `take`, file `index`.
pub(super) struct Done {
    pub take: usize,
    pub index: usize,
    pub result: Result<FileReport>,
}

/// Write file `index` of each take from the packets its sources send, until
/// they stop. At a split, the current file ends exactly at the split time
/// and the next take's file continues from there; the sources never stop.
#[allow(clippy::too_many_arguments)]
pub(super) fn write_files<W: FileWriter>(
    writer: W,
    plan: Arc<FilePlan>,
    index: usize,
    packets: Receiver<Packet<W::Frame>>,
    splits: Receiver<Split<W>>,
    done: Sender<Done>,
    shared: &Shared,
    clock: Arc<RecordingClock>,
    missed_audio: Arc<AtomicU64>,
) {
    thread::scope(|scope| {
        // Finishing a file can take a while (writing its final movie
        // header), so it happens on its own thread while the next take's
        // file keeps receiving packets.
        let finish = |file: OpenFile<W>, end: Time, take: usize, failure: Result<()>| {
            let done = done.clone();
            scope.spawn(move || {
                let result =
                    isocast_diagnostics::contain("file finalizer", || file.finish(end, failure))
                        .map_err(anyhow::Error::msg)
                        .and_then(|r| r);
                let _ = done.send(Done {
                    take,
                    index,
                    result,
                });
            });
        };
        let mut take = 0;
        let mut file = OpenFile::new(writer, plan, clock.clone());
        let mut next: Option<(Time, usize, OpenFile<W>)> = None;
        // The file a split ended, kept open for its audio up to the split,
        // which can arrive after the next take's first frame.
        let mut closing: Option<Closing<W>> = None;
        let mut failure = Ok(());
        let written = isocast_diagnostics::contain("file writer", || {
            for packet in packets {
                file.lost_packets += missed_audio.swap(0, Ordering::Relaxed);
                if next.is_none()
                    && let Ok(split) = splits.try_recv()
                {
                    next = Some((
                        split.at,
                        split.take,
                        OpenFile::new(split.writer, split.plan, clock.clone()),
                    ));
                }
                let end = shared.end();
                let (result, newest) = match packet {
                    Packet::Video { frame, pts } => {
                        // A split the session ends before stays pending, and is
                        // cancelled below.
                        if let Some((at, ..)) = next
                            && pts >= at
                            && at < end
                        {
                            let (at, next_take, new) = next.take().expect("checked");
                            let mut old = std::mem::replace(&mut file, new);
                            // The ending take's pictures are all in: put them
                            // on disk now rather than when its last audio
                            // arrives, so a crash right after a split keeps
                            // the take up to the split. A commit that fails
                            // fails the file, like any other write error.
                            let failure = if old.crash_safe {
                                let committed = old.writer.commit().map(|_| ());
                                old.publish_committed();
                                committed.with_context(|| {
                                    format!(
                                        "{}: could not commit the take's files at the split",
                                        old.plan.spec.path.display()
                                    )
                                })
                            } else {
                                Ok(())
                            };
                            let ended = Closing {
                                at,
                                take,
                                file: old,
                                failure,
                            };
                            // A split within the previous one's closing window
                            // finishes that file now (its sessions then end as
                            // this take's start; see `SPLIT_FINISH_DELAY`).
                            if let Some(c) = closing.replace(ended) {
                                finish(c.file, c.at, c.take, c.failure);
                            }
                            take = next_take;
                        }
                        (file.video(&frame, pts, end), pts)
                    }
                    Packet::Audio {
                        track,
                        samples,
                        pts,
                    } => {
                        if let Some(c) = &mut closing
                            && c.failure.is_ok()
                            && pts < c.at
                        {
                            c.failure = c.file.audio(track, &samples, pts, c.at.min(end));
                        }
                        // Audio before a split goes to the current file; the next
                        // file holds what it gets until its first frame and trims
                        // it.
                        let until = next.as_ref().map_or(end, |(at, ..)| end.min(*at));
                        let result = file.audio(track, &samples, pts, until);
                        let result = if let Some((.., new)) = &mut next {
                            result.and_then(|()| new.audio(track, &samples, pts, end))
                        } else {
                            result
                        };
                        (result, pts)
                    }
                };
                file.plan.audio_dropouts.store(
                    file.lost_packets + file.tracks.iter().map(|t| t.dropouts).sum::<u64>(),
                    Ordering::Relaxed,
                );
                // A crash-safe writer says how much of the file is on disk,
                // and commits the rest when a pause asks it to, once the
                // samples up to the cut are in (or soon after, if a track
                // stalls): the pause reply then reports a fact.
                let result = if file.crash_safe {
                    file.publish_committed();
                    let requested = file.plan.commit_requested.load(Ordering::Acquire);
                    if requested > file.plan.commit_done.load(Ordering::Relaxed)
                        && requested > file.plan.commit_failed.load(Ordering::Relaxed)
                    {
                        let at = Time::from_nanos(file.plan.commit_at.load(Ordering::Relaxed));
                        let covered =
                            file.newest_input.is_some_and(|n| n >= at) && file.audio_complete(at);
                        if covered || newest >= at + COMMIT_WAIT {
                            match file.writer.commit() {
                                Ok(_) => {
                                    // Only a commit that returned is a
                                    // durable watermark.
                                    file.publish_committed();
                                    file.plan.commit_done.store(requested, Ordering::Release);
                                    result
                                }
                                Err(err) => {
                                    let err = err.context(format!(
                                        "{}: could not commit the file at the pause",
                                        file.plan.spec.path.display()
                                    ));
                                    *file.plan.commit_error.lock().unwrap() =
                                        Some(format!("{err:#}"));
                                    file.plan.commit_failed.store(requested, Ordering::Release);
                                    result.and(Err(err))
                                }
                            }
                        } else {
                            result
                        }
                    } else {
                        result
                    }
                } else {
                    result
                };
                if let Some(c) = &closing
                    && (c.failure.is_err()
                        || (newest >= c.at + SPLIT_FINISH_DELAY
                            && (c.file.audio_complete(c.at) || newest >= c.at + SPLIT_AUDIO_WAIT)))
                {
                    let c = closing.take().expect("checked");
                    finish(c.file, c.at, c.take, c.failure);
                }
                if let Err(err) = result {
                    failure = Err(err);
                    break;
                }
                // The end is immutable once reached. Finish as soon as the last
                // video frame and every audio track are present; retaining the
                // route for the capture timeout only delays finalization. Stalled
                // or missing inputs still use the session's bounded tail wait.
                if end != Time::MAX
                    && let Some(start) = file.start
                    && file.last_video.is_some_and(|pts| {
                        // Frame intervals aren't whole nanoseconds at 60 fps:
                        // adding one to the last PTS can be 1 ns short of the
                        // same boundary computed from the file's start.
                        pts.ticks_since(start, file.fps()) + 1
                            >= file_end(start, clock.state.read().unwrap().map(end), file.fps())
                                .ticks_since(start, file.fps())
                    })
                    && file.audio_complete(end)
                {
                    break;
                }
            }
        });
        if let Err(message) = written {
            failure = Err(anyhow!(message));
        }
        if let Some(c) = closing {
            finish(c.file, c.at.min(shared.end()), c.take, c.failure);
        }
        // Splits that never began, whether or not a packet came after them.
        let never: Vec<_> = next
            .map(|(_, take, file)| (take, file.writer))
            .into_iter()
            .chain(splits.try_iter().map(|split| (split.take, split.writer)))
            .collect();
        for (take, writer) in never {
            writer.cancel();
            let _ = done.send(Done {
                take,
                index,
                result: Err(anyhow!("the recording ended before this take started")),
            });
        }
        file.lost_packets += missed_audio.swap(0, Ordering::Relaxed);
        finish(file, shared.end(), take, failure);
    });
}

/// A file that a split ended at `at`, waiting for the rest of its audio.
struct Closing<W: FileWriter> {
    at: Time,
    take: usize,
    file: OpenFile<W>,
    /// Its audio failed; it is finished with this error.
    failure: Result<()>,
}

/// One file being written.
struct OpenFile<W: FileWriter> {
    writer: W,
    plan: Arc<FilePlan>,
    tracks: Vec<AudioTrack>,
    /// The time of the first frame, which starts the file's timeline.
    start: Option<Time>,
    /// Audio that arrived before the first frame.
    pending_audio: Vec<(usize, Arc<[f32]>, Time)>,
    frames: u64,
    /// Times of the latest frames written, to uncount any that the final end
    /// time (set a moment after they arrived) cuts off.
    recent: VecDeque<Time>,
    /// Latest video delivery, including a frame the encoder refused. Once
    /// the final slot was handled, waiting cannot recover that dropped slot.
    last_video: Option<Time>,
    clock: Arc<RecordingClock>,
    /// Sparse-source packets actually refused by this file's input queue.
    lost_packets: u64,
    /// The writer commits on demand (see `FileWriter::crash_safe`).
    crash_safe: bool,
    /// Latest video delivery on the uncut clock, paused or not.
    newest_input: Option<Time>,
    /// Developer mode's timing trace (see trace.rs), and the age of the
    /// frame being appended.
    trace: Arc<crate::trace::OutputTrace>,
    trace_age: Option<std::time::Duration>,
}

impl<W: FileWriter> OpenFile<W> {
    fn new(writer: W, plan: Arc<FilePlan>, clock: Arc<RecordingClock>) -> Self {
        let crash_safe = writer.crash_safe();
        plan.crash_safe.store(crash_safe, Ordering::Release);
        let tracks = plan
            .spec
            .audio
            .iter()
            .zip(&plan.audio_timing)
            .map(|(t, timing)| AudioTrack {
                channels: t.channels as usize,
                timing: *timing,
                written: 0,
                received: 0,
                dropouts: 0,
            })
            .collect();
        let trace =
            crate::trace::OutputTrace::register(&plan.source, plan.take, plan.spec.video.fps);
        Self {
            trace,
            trace_age: None,
            writer,
            plan,
            tracks,
            start: None,
            pending_audio: Vec::new(),
            frames: 0,
            recent: VecDeque::new(),
            last_video: None,
            clock,
            lost_packets: 0,
            crash_safe,
            newest_input: None,
        }
    }

    fn fps(&self) -> u32 {
        self.plan.spec.video.fps
    }

    fn publish_committed(&self) {
        if let Some(committed) = self.writer.committed() {
            self.plan.committed.store(
                committed.as_nanos().min(i64::MAX as u128) as i64,
                Ordering::Relaxed,
            );
        }
    }

    /// Append a frame; frames at or after `end`, or before the file's start,
    /// are left out.
    fn video(&mut self, frame: &W::Frame, pts: Time, end: Time) -> Result<()> {
        self.newest_input = Some(pts);
        if crate::trace::on() {
            self.trace_age = crate::trace::OutputTrace::age(pts);
        }
        let clock = self.clock.clone();
        let cuts = clock.state.read().unwrap();
        clock.inspected(pts.add_ticks(1, self.fps()));
        if cuts.paused(pts) {
            return Ok(());
        }
        let (pts, end) = (cuts.map(pts), cuts.map(end));
        drop(cuts);
        self.video_mapped(frame, pts, end)
    }

    fn video_mapped(&mut self, frame: &W::Frame, pts: Time, end: Time) -> Result<()> {
        self.last_video = Some(pts);
        if pts >= end || self.start.is_some_and(|start| pts < start) {
            return Ok(());
        }
        if self.start.is_none() {
            self.writer.start(pts)?;
            if crate::trace::on() {
                self.trace.started(pts);
            }
            self.start = Some(pts);
            self.plan.start.store(pts.as_nanos(), Ordering::SeqCst);
            let end = file_end(pts, end, self.fps());
            for (track, samples, at) in std::mem::take(&mut self.pending_audio) {
                self.tracks[track].append(&mut self.writer, track, &samples, at, pts, end)?;
            }
        }
        let submit = crate::trace::on().then(std::time::Instant::now);
        let appended = self.writer.append_video(frame, pts);
        if let Some(at) = submit {
            self.trace.frame(
                self.trace_age.take(),
                at.elapsed(),
                matches!(appended, Ok(true)),
                self.writer.queue_depth(),
            );
        }
        if let Ok(true) = appended {
            self.frames += 1;
            if self.recent.len() == 8 {
                self.recent.pop_front();
            }
            self.recent.push_back(pts);
        }
        // Pictures the encoder took and then discarded on its own are not
        // in the file: they count as dropped, not written.
        let lost = self.writer.lost_frames();
        if lost > 0 {
            self.frames = self.frames.saturating_sub(lost);
        }
        if let Ok(true) = appended {
            self.plan.frames.store(self.frames, Ordering::SeqCst);
        } else if lost > 0 {
            self.plan.frames.store(self.frames, Ordering::SeqCst);
        }
        // Published after the count, so live status never sees a frame
        // that is still being appended as dropped.
        self.plan.newest.store(pts.as_nanos(), Ordering::Release);
        appended?;
        // A silent/off-air source still has a clocked ISO picture. Fill behind
        // that clock instead of bursting minutes of silence into the bounded
        // encoder queue on reactivation or Stop. Work on the pause-mapped
        // timeline and retain the input's late-arrival/sync-offset allowance.
        let start = self.start.expect("started");
        for (i, track) in self.tracks.iter_mut().enumerate() {
            track.pad_to(&mut self.writer, i, start, pts - track.timing.wait)?;
        }
        Ok(())
    }

    /// Append audio to `track`, up to `end`.
    fn audio(&mut self, track: usize, samples: &Arc<[f32]>, pts: Time, end: Time) -> Result<()> {
        let clock = self.clock.clone();
        let cuts = clock.state.read().unwrap();
        let channels = self.tracks[track].channels;
        let frames = (samples.len() / channels) as u64;
        clock.inspected(pts.add_ticks(frames, SAMPLE_RATE));
        let end = cuts.map(end);
        if let Some(pts) = cuts.audio_passthrough(pts, frames) {
            drop(cuts);
            return self.audio_mapped(track, samples, pts, end);
        }
        let mut parts = Vec::new();
        cuts.audio(samples, channels, pts, |samples, pts, cut_end| {
            parts.push((samples.to_vec(), pts, end.min(cut_end)));
            Ok::<_, std::convert::Infallible>(())
        })
        .expect("staging audio cannot fail");
        drop(cuts);
        for (samples, pts, end) in parts {
            self.audio_mapped(track, &samples, pts, end)?;
        }
        Ok(())
    }

    fn audio_mapped(&mut self, track: usize, samples: &[f32], pts: Time, end: Time) -> Result<()> {
        let Some(start) = self.start else {
            if self.pending_audio.len() == MAX_PENDING_AUDIO {
                self.pending_audio.remove(0);
            }
            self.pending_audio.push((track, samples.into(), pts));
            return Ok(());
        };
        let end = file_end(start, end, self.fps());
        self.tracks[track].append(&mut self.writer, track, samples, pts, start, end)?;
        Ok(())
    }

    /// Finish the file at `end` (rounded up to a frame boundary); with
    /// `Time::MAX`, after its last frame. `failure` is an earlier error,
    /// reported after finishing so that what was written stays playable.
    fn finish(mut self, end: Time, failure: Result<()>) -> Result<FileReport> {
        let end = self.clock.state.read().unwrap().map(end);
        let path = self.plan.spec.path.clone();
        let fps = self.fps();
        let Some(start) = self.start else {
            self.writer.cancel();
            failure?;
            // Stopped before the take's first frame was due: the source
            // wasn't at fault.
            let epoch = self.plan.epoch.load(Ordering::SeqCst);
            if epoch != i64::MIN && end <= Time::from_nanos(epoch).add_ticks(1, fps) {
                return Err(anyhow!(
                    "{}: the recording stopped before its first frame, so nothing was written",
                    path.display()
                ));
            }
            return Err(anyhow!(
                "{}: source {:?} delivered no frames, so nothing was written",
                path.display(),
                self.plan.source
            ));
        };
        // The end is unknown if this file stopped on its own (its writer
        // failed); it then ends after its last frame.
        let end = match end {
            Time::MAX => self
                .recent
                .back()
                .map_or(start, |last| last.add_ticks(1, fps)),
            end => end,
        };
        let end = file_end(start, end, fps);
        self.frames -= self.recent.iter().filter(|&&pts| pts >= end).count() as u64;
        let padded = failure.and_then(|()| {
            for (i, track) in self.tracks.iter_mut().enumerate() {
                track.pad_to(&mut self.writer, i, start, end)?;
            }
            Ok(())
        });
        let mut dropouts = self.lost_packets + self.tracks.iter().map(|t| t.dropouts).sum::<u64>();
        // A failed file isn't in the report, so its error says what it holds
        // and what it lost: every frame due up to the end that it lacks. A
        // writer that repaired its file after failing says what the movie
        // holds; the counters then describe that movie, not what was accepted.
        let expected = end.ticks_since(start, fps).max(0) as u64;
        let seconds = end.saturating_since(start).as_secs_f64();
        let finished = match self.writer.finish(end) {
            Ok(finished) => finished,
            Err(err) => {
                let repaired = err.downcast_ref::<RepairedFile>().cloned();
                if let Some(frames) = repaired.as_ref().and_then(|r| r.frames) {
                    self.frames = frames;
                    self.plan.frames.store(frames, Ordering::SeqCst);
                }
                let frames = self.frames;
                let dropped = expected.saturating_sub(frames);
                let holds = match &repaired {
                    Some(RepairedFile {
                        frames: Some(_), ..
                    }) => {
                        format!("the repaired movie holds {frames} of {expected} frames due")
                    }
                    _ => format!("{frames} of {expected} frames were accepted"),
                };
                return Err(err.context(format!(
                    "could not finish {} ({holds} over {seconds:.1} s, {dropped} dropped)",
                    path.display()
                )));
            }
        };
        self.frames = self.frames.saturating_sub(finished.lost_frames);
        self.plan.frames.store(self.frames, Ordering::SeqCst);
        dropouts += finished.lost_audio_tracks;
        let repeated = finished.repeated;
        padded.with_context(|| format!("recording {} stopped early", path.display()))?;

        let epoch = self
            .clock
            .state
            .read()
            .unwrap()
            .map(Time::from_nanos(self.plan.epoch.load(Ordering::SeqCst)));
        Ok(FileReport {
            path,
            take: self.plan.take,
            source: self.plan.source.clone(),
            audio: self.plan.audio.clone(),
            start_offset_seconds: start.saturating_since(epoch).as_secs_f64(),
            duration_seconds: end.saturating_since(start).as_secs_f64(),
            frames: self.frames + repeated,
            repeated_frames: repeated,
            dropped_frames: expected.saturating_sub(self.frames),
            audio_dropouts: dropouts,
        })
    }

    /// Every audio track has reached `end` (rounded up to a frame
    /// boundary).
    fn audio_complete(&self, end: Time) -> bool {
        let end = self.clock.state.read().unwrap().map(end);
        let Some(start) = self.start else {
            return false;
        };
        let limit = file_end(start, end, self.fps()).ticks_since(start, SAMPLE_RATE);
        self.tracks.iter().all(|t| t.written as i64 >= limit)
    }
}

/// Where a file starting at `start` ends when its take ends at `end`: on
/// the next frame boundary, so its last frame lasts a whole frame.
pub(super) fn file_end(start: Time, end: Time, fps: u32) -> Time {
    if end == Time::MAX {
        return end;
    }
    let mut frames = end.ticks_since(start, fps).max(0) as u64;
    if start.add_ticks(frames, fps) < end {
        frames += 1;
    }
    start.add_ticks(frames, fps)
}

/// One audio track of a file, kept continuous and aligned with the video.
struct AudioTrack {
    channels: usize,
    timing: AudioTiming,
    /// Sample frames written since the file's start.
    written: u64,
    /// End of received audio, separate from clock-driven silence.
    received: u64,
    /// Missing continuous input, late sparse input and encoder refusals.
    dropouts: u64,
}

impl AudioTrack {
    fn append<W: FileWriter>(
        &mut self,
        writer: &mut W,
        track: usize,
        samples: &[f32],
        pts: Time,
        start: Time,
        end: Time,
    ) -> Result<()> {
        let frames = samples.len() / self.channels;
        let at = pts.ticks_since(start, SAMPLE_RATE);
        let limit = if end == Time::MAX {
            i64::MAX
        } else {
            end.ticks_since(start, SAMPLE_RATE).max(0)
        };
        // Audio from after the end (like the rest of a chunk that straddled
        // it) is left out; finishing pads up to the end if needed.
        if at >= limit {
            return Ok(());
        }
        let written = self.written as i64;
        // The first audio is placed exactly; after that, small timestamp
        // jitter is ignored to keep the track continuous.
        let tolerance = if written == 0 { 0 } else { AUDIO_JITTER };
        // Clock-driven padding must not hide an actual gap in a capture
        // or program track. Media/browser silence is expected instead.
        if !self.timing.sparse && self.received > 0 && at > self.received as i64 + tolerance {
            self.dropouts += 1;
        } else if at < written - tolerance
            && (at + frames as i64).min(written) > at.max(self.received as i64)
        {
            // Samples delivered after their silence was committed really were
            // lost; don't relabel those (or refused packets) as normal silence.
            self.dropouts += 1;
        }
        let mut skip = 0;
        if at > written + tolerance {
            self.write_silence(writer, track, start, (at - written) as u64)?;
        } else if at < written - tolerance {
            skip = ((written - at) as usize).min(frames);
        }
        let take =
            ((frames - skip) as u64).min((limit as u64).saturating_sub(self.written)) as usize;
        if take > 0 {
            let ch = self.channels;
            let pts = start.add_ticks(self.written, SAMPLE_RATE);
            if !writer.append_audio(track, &samples[skip * ch..(skip + take) * ch], pts)? {
                self.dropouts += 1;
            }
            self.written += take as u64;
        }
        self.received = self.received.max(if take > 0 {
            self.written
        } else {
            (at + frames as i64).clamp(0, limit) as u64
        });
        Ok(())
    }

    /// Fill the track with silence up to `end`, so every track in the file
    /// lasts as long as the video.
    fn pad_to<W: FileWriter>(
        &mut self,
        writer: &mut W,
        track: usize,
        start: Time,
        end: Time,
    ) -> Result<()> {
        let target = end.ticks_since(start, SAMPLE_RATE).max(0) as u64;
        if target > self.written {
            self.write_silence(writer, track, start, target - self.written)?;
        }
        Ok(())
    }

    fn write_silence<W: FileWriter>(
        &mut self,
        writer: &mut W,
        track: usize,
        start: Time,
        frames: u64,
    ) -> Result<()> {
        let chunk = vec![0.0; AUDIO_CHUNK as usize * self.channels];
        let mut left = frames;
        while left > 0 {
            let n = left.min(AUDIO_CHUNK) as usize;
            let pts = start.add_ticks(self.written, SAMPLE_RATE);
            if !writer.append_audio(track, &chunk[..n * self.channels], pts)? {
                self.dropouts += 1;
            }
            self.written += n as u64;
            left -= n as u64;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A writer that remembers how many audio frames it was given.
    #[derive(Default)]
    struct Count(u64);

    impl FileWriter for Count {
        type Frame = ();

        fn start(&mut self, _: Time) -> Result<()> {
            Ok(())
        }

        fn append_video(&mut self, _: &(), _: Time) -> Result<bool> {
            Ok(true)
        }

        fn append_audio(&mut self, _: usize, samples: &[f32], _: Time) -> Result<bool> {
            self.0 += samples.len() as u64;
            Ok(true)
        }

        fn finish(self, _: Time) -> Result<crate::Finished> {
            Ok(crate::Finished::default())
        }

        fn cancel(self) {}
    }

    const CHUNK: usize = 480;
    const ZERO: Time = Time::from_nanos(0);

    fn at(sample: u64) -> Time {
        ZERO.add_ticks(sample, SAMPLE_RATE)
    }

    /// Mono chunks appended at `starts` (in samples) to a track ending at
    /// `end`: the samples written and the dropouts counted.
    fn track(starts: &[u64], end: Time) -> (u64, u64) {
        let mut writer = Count::default();
        let mut track = AudioTrack {
            channels: 1,
            timing: AudioTiming::new(None),
            written: 0,
            received: 0,
            dropouts: 0,
        };
        for &start in starts {
            track
                .append(&mut writer, 0, &[0.1; CHUNK], at(start), ZERO, end)
                .unwrap();
        }
        assert_eq!(writer.0, track.written);
        (track.written, track.dropouts)
    }

    #[test]
    fn contiguous_and_slightly_jittery_audio_is_kept_as_is() {
        assert_eq!(track(&[0, 480, 960], Time::MAX), (1440, 0));
        // 1 ms late is within the jitter allowance.
        assert_eq!(track(&[0, 480 + 48], Time::MAX), (960, 0));
    }

    #[test]
    fn gaps_become_silence_and_overlaps_are_trimmed() {
        assert_eq!(track(&[0, 960], Time::MAX), (1440, 1));
        assert_eq!(track(&[0, 280], Time::MAX), (760, 0));
    }

    #[test]
    fn nothing_is_written_past_the_end() {
        let end = at(600);
        assert_eq!(track(&[0, 480], end), (600, 0));
        // A chunk after a gap beyond the end adds no silence past it.
        assert_eq!(track(&[0, 960], end), (480, 0));
    }

    #[test]
    fn file_ends_fall_on_frame_boundaries() {
        let start = at(0);
        assert_eq!(
            file_end(start, ZERO.add_ticks(10, 60), 60),
            ZERO.add_ticks(10, 60)
        );
        let end = file_end(
            start,
            ZERO.add_ticks(10, 60) + std::time::Duration::from_micros(1),
            60,
        );
        assert_eq!(end, ZERO.add_ticks(11, 60));
    }
}
