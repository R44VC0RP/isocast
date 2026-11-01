//! Writing files: one per ISO source per take, with every audio track kept
//! aligned to the video on the session clock.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SendError, Sender};
use std::thread;

use anyhow::{Context, Result, anyhow};
use isocast_protocol::{Source, SourceKind};
use parking_lot::Mutex;

use super::pause::RecordingClock;
use super::{
    AUDIO_CHUNK, AUDIO_JITTER, COMMIT_WAIT, FilePlan, MAX_PENDING_AUDIO, Packet, SPLIT_AUDIO_WAIT,
    SPLIT_FINISH_DELAY, Shared,
};
use crate::record::FileReport;
use crate::source::FrameSlot;
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

/// How long a file whose capture stopped waits, beyond three frames, for
/// the frames the capture delivered before it stopped.
const DRAIN_AFTER_STOP: std::time::Duration = std::time::Duration::from_millis(50);

/// Switch a writer thread to a new take's file at `at`.
pub(super) struct Split<W> {
    pub at: Time,
    pub take: usize,
    pub writer: W,
    pub plan: Arc<FilePlan>,
}

/// A writer thread's splits. Sending and closing take the same lock, so a
/// split is either received (and answered with a `Done`) or refused at
/// once, never lost with the channel.
pub(super) fn split_channel<W>() -> (SplitSender<W>, SplitReceiver<W>) {
    let (tx, rx) = std::sync::mpsc::channel();
    let closed = Arc::new(Mutex::new(false));
    (
        SplitSender {
            tx,
            closed: closed.clone(),
        },
        SplitReceiver { rx, closed },
    )
}

pub(super) struct SplitSender<W> {
    tx: Sender<Split<W>>,
    closed: Arc<Mutex<bool>>,
}

impl<W> SplitSender<W> {
    /// Hands `split` to the writer thread, or gives it back once that
    /// thread no longer takes splits.
    pub(super) fn send(&self, split: Split<W>) -> Result<(), SendError<Split<W>>> {
        let closed = self.closed.lock();
        if *closed {
            return Err(SendError(split));
        }
        self.tx.send(split)
    }
}

pub(super) struct SplitReceiver<W> {
    rx: Receiver<Split<W>>,
    closed: Arc<Mutex<bool>>,
}

impl<W> SplitReceiver<W> {
    fn try_recv(&self) -> Option<Split<W>> {
        self.rx.try_recv().ok()
    }

    /// Refuses further splits and returns those not yet received.
    fn close(self) -> Vec<Split<W>> {
        let mut closed = self.closed.lock();
        *closed = true;
        self.rx.try_iter().collect()
    }
}

/// A finished (or failed) file of take `take`, file `index`.
pub(super) struct Done {
    pub take: usize,
    pub index: usize,
    pub result: Result<FileReport>,
}

/// From which frame time an ISO file's frame clock must send every picture,
/// whatever admission says (see `IsoOutput`): a file starts on a picture,
/// and so does the next take's at a split. Nanoseconds, the earlier of two
/// demands, so neither side's update can erase the other's.
pub(super) struct IsoDemand {
    /// The writer thread's: every picture until its file starts (`MIN`),
    /// from a split it has received until the next take's file starts, and
    /// none otherwise (`MAX`).
    writer: AtomicI64,
    /// A split sent to the writer thread whose take hasn't started yet,
    /// published by the recording before the thread can receive it, so a
    /// backlogged thread can't leave the split's ticks to admission (#29).
    /// `MAX` if none.
    split: AtomicI64,
}

impl Default for IsoDemand {
    fn default() -> Self {
        Self {
            writer: AtomicI64::new(i64::MIN),
            split: AtomicI64::new(i64::MAX),
        }
    }
}

impl IsoDemand {
    pub(super) fn from(&self) -> i64 {
        self.writer
            .load(Ordering::Acquire)
            .min(self.split.load(Ordering::Acquire))
    }

    /// A split at `at` is about to be sent: call before sending it. A
    /// later split replaces an earlier one still unstarted, which can only
    /// be sent once that one's time is a take's length past.
    pub(super) fn split_sent(&self, at: Time) {
        self.split.store(at.as_nanos(), Ordering::Release);
    }

    /// The split at `at` started its take, or never will: stop asking for
    /// every picture for it, unless a later split has replaced it.
    pub(super) fn split_settled(&self, at: Time) {
        let _ = self.split.compare_exchange(
            at.as_nanos(),
            i64::MAX,
            Ordering::AcqRel,
            Ordering::Relaxed,
        );
    }
}

/// Write file `index` of each take from the packets its sources send, until
/// they stop. At a split, the current file ends exactly at the split time
/// and the next take's file continues from there; the sources never stop.
/// `wanted` says from which frame time on an ISO file's frame clock must
/// send every picture, whatever admission says (see `IsoDemand`).
#[allow(clippy::too_many_arguments)]
pub(super) fn write_files<W: FileWriter>(
    writer: W,
    plan: Arc<FilePlan>,
    index: usize,
    packets: Receiver<Packet<W::Frame>>,
    splits: SplitReceiver<W>,
    done: Sender<Done>,
    shared: &Shared,
    clock: Arc<RecordingClock>,
    missed_audio: Arc<AtomicU64>,
    source: Option<Arc<FrameSlot<W::Frame>>>,
    wanted: Arc<IsoDemand>,
) where
    W::Frame: Clone,
{
    // Explicit isolated, developer-only fault injection: an ISO file's writer thread stalls this
    // long as it receives a split, as one backlogged past the split's lead.
    let split_stall = (!super::is_program(&plan)
        && isocast_diagnostics::developer()
        && std::env::var_os("ISOCAST_SOCKET").is_some())
    .then(|| {
        std::env::var("ISOCAST_DEBUG_SPLIT_STALL_MS")
            .ok()?
            .parse::<u64>()
            .ok()
    })
    .flatten()
    .map(std::time::Duration::from_millis);
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
            // Once the capture has stopped: until when its last frames may
            // still arrive (queued here, or on their way from its frame
            // clock, which delivers each frame a frame after it is due).
            let mut draining: Option<std::time::Instant> = None;
            loop {
                let packet = match draining {
                    None => packets.recv().ok(),
                    Some(until) => packets
                        .recv_timeout(until.saturating_duration_since(std::time::Instant::now()))
                        .ok(),
                };
                let Some(packet) = packet else {
                    break;
                };
                // Audio can keep this receiver alive after its video capture
                // has stopped. Close that file instead of accepting splits
                // whose first video frame can never arrive, once it holds
                // the frames the capture delivered: it ends on the last one
                // (the failure is set after the loop).
                if draining.is_none()
                    && source.as_ref().is_some_and(|slot| slot.stopped().is_some())
                {
                    draining = Some(
                        std::time::Instant::now()
                            + DRAIN_AFTER_STOP
                            + std::time::Duration::from_secs(3) / file.fps(),
                    );
                }
                file.lost_packets += missed_audio.swap(0, Ordering::Relaxed);
                if next.is_none()
                    && draining.is_none()
                    && let Some(split) = splits.try_recv()
                {
                    if let Some(stall) = split_stall {
                        thread::sleep(stall);
                    }
                    next = Some((
                        split.at,
                        split.take,
                        OpenFile::new(split.writer, split.plan, clock.clone()),
                    ));
                }
                let end = shared.end();
                let (result, newest) = match packet {
                    Packet::AudioGap { from, until } => {
                        if let Some(c) = &mut closing {
                            c.file.source_gap(from, until.min(c.at).min(end));
                        }
                        let split = next.as_ref().map_or(end, |(at, ..)| end.min(*at));
                        file.source_gap(from, until.min(split));
                        if let Some((at, _, new)) = &mut next {
                            new.source_gap(from.max(*at), until.min(end));
                        }
                        (Ok(()), until)
                    }
                    Packet::Video { frame, pts } => {
                        // A split the session ends before stays pending, and is
                        // cancelled below.
                        if let Some((at, ..)) = next
                            && pts >= at
                            && at < end
                        {
                            let (at, next_take, new) = next.take().expect("checked");
                            // This picture starts the take; `from` below
                            // stops asking for every one.
                            wanted.split_settled(at);
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
                        (file.video(Some(&frame), pts, end), pts)
                    }
                    // A tick ISO admission left out before drawing it: kept
                    // like a frame the encoder refused. A pending take starts
                    // on a picture, never on a skipped tick (from the split
                    // on, `wanted` asks for every picture).
                    Packet::Skipped { pts } => {
                        let switching = next
                            .as_ref()
                            .is_some_and(|(at, ..)| pts >= *at && *at < end);
                        let result = if switching {
                            Ok(())
                        } else {
                            file.video(None, pts, end)
                        };
                        (result, pts)
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
                // From when ISO admission must hand over every picture: a
                // file starts on its first one, so any tick while it hasn't,
                // and from a pending split's time until the next take's has.
                let from = match &next {
                    _ if file.start.is_none() => i64::MIN,
                    Some((at, ..)) => at.as_nanos(),
                    None => i64::MAX,
                };
                wanted.writer.store(from, Ordering::Release);
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
                                    *file.plan.commit_error.lock() = Some(format!("{err:#}"));
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
                            >= file_end(start, clock.state.read().map(end), file.fps())
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
        if failure.is_ok()
            && let Some(reason) = source.as_ref().and_then(|slot| slot.stopped())
        {
            failure = Err(anyhow!("the capture stopped: {reason}"));
        }
        if let Some(c) = closing {
            finish(c.file, c.at.min(shared.end()), c.take, c.failure);
        }
        // Splits that never began, whether or not a packet came after them.
        // Closing refuses further handoffs while the native finalizers are
        // still closing, so none can enter a receiver never read again.
        let never: Vec<_> = next
            .map(|(at, take, file)| (at, take, file.writer))
            .into_iter()
            .chain(
                splits
                    .close()
                    .into_iter()
                    .map(|split| (split.at, split.take, split.writer)),
            )
            .collect();
        for (at, take, writer) in never {
            wanted.split_settled(at);
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
    fn new(mut writer: W, plan: Arc<FilePlan>, clock: Arc<RecordingClock>) -> Self {
        plan.output_active.store(0, Ordering::Release);
        let active = plan.clone();
        if !writer.on_first_video_packet(Box::new(move || {
            active.output_active.store(1, Ordering::Release);
        })) {
            plan.output_active.store(-1, Ordering::Release);
        }
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
    /// are left out. `None`: admission skipped the tick, which counts as a
    /// refused frame (a file starts only on a picture).
    fn video(&mut self, frame: Option<&W::Frame>, pts: Time, end: Time) -> Result<()> {
        self.newest_input = Some(pts);
        if crate::trace::on() {
            self.trace_age = crate::trace::OutputTrace::age(pts);
        }
        let clock = self.clock.clone();
        let cuts = clock.state.read();
        clock.inspected(pts.add_ticks(1, self.fps()));
        if cuts.paused(pts) {
            return Ok(());
        }
        let (pts, end) = (cuts.map(pts), cuts.map(end));
        drop(cuts);
        self.video_mapped(frame, pts, end)
    }

    fn video_mapped(&mut self, frame: Option<&W::Frame>, pts: Time, end: Time) -> Result<()> {
        self.last_video = Some(pts);
        // Once the file has started it ends on a frame boundary of its own
        // (`file_end`), which can lie up to 1 ns before `end`: a picture at
        // or after that boundary is past the file, so it is refused here
        // rather than written and then cut off by `finish` (which a writer
        // already holding it may refuse).
        let limit = self
            .start
            .map_or(end, |start| file_end(start, end, self.fps()));
        if pts >= limit || self.start.is_some_and(|start| pts < start) {
            return Ok(());
        }
        if self.start.is_none() {
            if frame.is_none() {
                return Ok(());
            }
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
        let intentional_before = self.writer.intentional_video_drops();
        let appended = match frame {
            Some(frame) => self.writer.append_video(frame, pts),
            None => self.writer.skip_video().map(|()| false),
        };
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
        // Every canvas's program file (`program`, `<id>-program`, #84).
        if let Some(canvas) = super::program_canvas(&self.plan) {
            crate::governor::protected_loss(
                canvas,
                lost + u64::from(
                    matches!(appended, Ok(false))
                        && self.writer.intentional_video_drops() == intentional_before,
                ),
            );
        }
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

    /// Count a source-reported interval only if this take retained any of it.
    fn source_gap(&mut self, from: Time, until: Time) {
        let epoch = Time::from_nanos(self.plan.epoch.load(Ordering::Acquire));
        let cuts = self.clock.state.read();
        let start = cuts.map(from.max(epoch));
        if self.start.map_or(start, |picture| start.max(picture)) < cuts.map(until) {
            self.lost_packets += 1;
        }
    }

    /// Append audio to `track`, up to `end`.
    fn audio(&mut self, track: usize, samples: &Arc<[f32]>, pts: Time, end: Time) -> Result<()> {
        let clock = self.clock.clone();
        let cuts = clock.state.read();
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
        let end = self.clock.state.read().map(end);
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
        // A timed take's planned end is not the end of a failed source.
        // Keep everything already accepted (including audio ahead of video),
        // but never stretch its last picture to a future scheduled stop.
        let end = if failure.is_err() || end == Time::MAX {
            let video = self
                .recent
                .back()
                .map_or(start, |last| last.add_ticks(1, fps));
            let audio = self
                .tracks
                .iter()
                .map(|t| start.add_ticks(t.written, SAMPLE_RATE))
                .max()
                .unwrap_or(start);
            end.min(video.max(audio))
        } else {
            end
        };
        let end = file_end(start, end, fps);
        self.frames -= self.recent.iter().filter(|&&pts| pts >= end).count() as u64;
        // Source failure doesn't mean the writer failed. Finish its short
        // audio tail too; a broken writer still returns its write error.
        let padded = (|| {
            for (i, track) in self.tracks.iter_mut().enumerate() {
                track.pad_to(&mut self.writer, i, start, end)?;
            }
            Ok(())
        })()
        .and(failure);
        let mut dropouts = self.lost_packets + self.tracks.iter().map(|t| t.dropouts).sum::<u64>();
        // A failed file isn't in the report, so its error says what it holds
        // and what it lost: every frame due up to the end that it lacks. A
        // writer that repaired its file after failing says what the movie
        // holds; the counters then describe that movie, not what was accepted.
        let epoch = self
            .clock
            .state
            .read()
            .map(Time::from_nanos(self.plan.epoch.load(Ordering::SeqCst)));
        // A program that began after its take also lost the slots before
        // its first picture (#150).
        let head = if self.plan.epoch.load(Ordering::SeqCst) == i64::MIN {
            0
        } else {
            super::head_dropped(&self.plan, epoch, start)
        };
        let expected = end.ticks_since(start, fps).max(0) as u64 + head;
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
        let report = FileReport {
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
            audio_stops: Vec::new(),
        };
        if let Err(error) = padded {
            *self.plan.failed_report.lock() = Some(report);
            return Err(error).with_context(|| {
                format!("recording {} stopped early", self.plan.spec.path.display())
            });
        }
        Ok(report)
    }

    /// Every audio track has reached `end` (rounded up to a frame
    /// boundary).
    fn audio_complete(&self, end: Time) -> bool {
        let end = self.clock.state.read().map(end);
        let Some(start) = self.start else {
            return false;
        };
        let limit = file_end(start, end, self.fps()).ticks_since(start, SAMPLE_RATE);
        self.tracks.iter().all(|t| t.written as i64 >= limit)
    }
}

/// Where a file starting at `start` ends when its take ends at `end`: on
/// the next frame boundary, so its last frame lasts a whole frame.
///
/// `add_ticks` truncates to whole nanoseconds, so a boundary reached from a
/// start that is itself a tick after the epoch can land up to 1 ns before
/// the same boundary computed from the epoch. That is the same boundary, not
/// a take that ends just after one: without the allowance a program that
/// started late (on an epoch tick) would run one frame into the next take.
///
/// The bound rests on one premise: `start` and a split's `end` are both
/// whole ticks from the same epoch (pictures and splits are clocked on the
/// canvas's ticks; pause cuts map them exactly), so floor(a + b) − floor(a)
/// − floor(b) ≤ 1 ns. A stop at an arbitrary host time can therefore end the
/// file up to 1 ns before it: pictures are admitted against this boundary
/// (`video_mapped`), never against the raw end, and writers that recompute
/// the end from their start must round the same way.
pub(super) fn file_end(start: Time, end: Time, fps: u32) -> Time {
    if end == Time::MAX {
        return end;
    }
    let mut frames = end.ticks_since(start, fps).max(0) as u64;
    if start.add_ticks(frames, fps).as_nanos() + TICK_ROUNDING_NS < end.as_nanos() {
        frames += 1;
    }
    start.add_ticks(frames, fps)
}

/// The most two truncated `add_ticks` steps can fall short of one.
pub(super) const TICK_ROUNDING_NS: i64 = 1;

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

    /// A writer that, like the Windows one, refuses to end before a picture
    /// it accepted.
    #[derive(Default)]
    struct Strict {
        last: Option<Time>,
    }

    impl FileWriter for Strict {
        type Frame = ();

        fn start(&mut self, _: Time) -> Result<()> {
            Ok(())
        }

        fn append_video(&mut self, _: &(), pts: Time) -> Result<bool> {
            self.last = Some(pts);
            Ok(true)
        }

        fn append_audio(&mut self, _: usize, _: &[f32], _: Time) -> Result<bool> {
            Ok(true)
        }

        fn finish(self, end: Time) -> Result<crate::Finished> {
            if self.last.is_some_and(|last| end <= last) {
                return Err(anyhow!("video.end: must be after every accepted picture"));
            }
            Ok(crate::Finished::default())
        }

        fn cancel(self) {}
    }

    /// A 30 fps file of `kind` in a take starting at `epoch`, without audio.
    fn plan(kind: &'static str, epoch: Time) -> Arc<FilePlan> {
        use std::sync::atomic::AtomicBool;
        Arc::new(FilePlan {
            recording: 1,
            take: 1,
            epoch: AtomicI64::new(epoch.as_nanos()),
            source: kind.to_string(),
            kind,
            start: AtomicI64::new(i64::MIN),
            frames: AtomicU64::new(0),
            newest: AtomicI64::new(i64::MIN),
            audio_dropouts: AtomicU64::new(0),
            hardware_accelerated: AtomicI64::new(-1),
            output_active: AtomicI64::new(-1),
            crash_safe: AtomicBool::new(false),
            committed: AtomicI64::new(-1),
            commit_requested: AtomicU64::new(0),
            commit_at: AtomicI64::new(i64::MIN),
            commit_done: AtomicU64::new(0),
            commit_failed: AtomicU64::new(0),
            commit_error: Mutex::new(None),
            failed_report: Mutex::new(None),
            encoder: Mutex::new(None),
            audio: Vec::new(),
            audio_timing: Vec::new(),
            spec: crate::WriterSpec {
                path: format!("/nonexistent/{kind}.mov").into(),
                container: Default::default(),
                video: crate::VideoTrackSpec {
                    width: 1280,
                    height: 720,
                    fps: 30,
                    encoder: Default::default(),
                },
                audio: Vec::new(),
                created: std::time::SystemTime::UNIX_EPOCH,
            },
        })
    }

    /// #149 review: a stop at a host time in (B, B + 1 ns], B a picture's
    /// tick, ends the file at B. That picture is refused when it arrives,
    /// not accepted and then cut off where a strict writer can't drop it.
    #[test]
    fn a_stop_just_past_a_picture_refuses_that_picture() {
        let epoch = Time::from_nanos(1_271_969_827_375);
        for late in [0, 4] {
            let start = epoch.add_ticks(late, 30);
            let clock = Arc::new(RecordingClock::new(epoch, [30].into_iter()));
            let mut file = OpenFile::new(Strict::default(), plan("program", epoch), clock);
            let boundary = file_end(start, epoch.add_ticks(60, 30), 30);
            let stop = boundary + std::time::Duration::from_nanos(1);
            for tick in late..=60 {
                file.video(Some(&()), epoch.add_ticks(tick, 30), stop)
                    .unwrap();
            }
            let report = file
                .finish(stop, Ok(()))
                .expect("the writer holds nothing past its end");
            assert_eq!(
                (report.frames, report.dropped_frames),
                // A late program's skipped head counts as dropped (#150).
                (60 - late, late),
                "{late} late"
            );
            assert!((report.duration_seconds - (60 - late) as f64 / 30.0).abs() < 1e-6);
        }
    }

    const CHUNK: usize = 480;

    #[test]
    fn explicit_loss_counts_on_sparse_and_mixed_files_only_inside_the_take() {
        use super::super::engine::Routes;
        use std::sync::mpsc::sync_channel;
        let epoch = at(960);
        let web = Arc::<Routes<()>>::default();
        let program = Arc::<Routes<()>>::default();
        let (tx, _rx) = sync_channel(1);
        web.add_mix_input(tx, 0, true, "web", Arc::default(), program.clone());
        let mut files = Vec::new();
        for (kind, route) in [("browser", &web), ("program", &program)] {
            let mut plan = plan(kind, epoch);
            let p = Arc::get_mut(&mut plan).unwrap();
            p.audio.push(kind.into());
            p.audio_timing.push(AudioTiming {
                sparse: kind == "browser",
                ..AudioTiming::new(None)
            });
            p.spec.audio.push(crate::AudioTrackSpec { channels: 2 });
            let (tx, rx) = sync_channel(4);
            route.add_file_audio(tx, 0, epoch, None, Arc::default(), &plan);
            let clock = Arc::new(RecordingClock::new(epoch, [30].into_iter()));
            files.push((OpenFile::new(Count::default(), plan, clock), rx));
        }
        web.source_gap(epoch, std::time::Duration::from_millis(10));
        web.source_gap(at(8160), std::time::Duration::from_millis(150));
        for (mut file, rx) in files {
            let end = epoch.add_ticks(6, 30);
            file.video(Some(&()), epoch, end).unwrap();
            for packet in rx.try_iter() {
                let Packet::AudioGap { from, until } = packet else {
                    panic!("expected a gap")
                };
                file.source_gap(from, until);
            }
            assert_eq!(file.finish(end, Ok(())).unwrap().audio_dropouts, 1);
        }
    }
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

    /// #150: a file whose first picture came 4 ticks into a 60-tick take.
    fn late_by_four(kind: &'static str) -> FileReport {
        let epoch = Time::from_nanos(1_271_969_827_375);
        let plan = plan(kind, epoch);
        let clock = Arc::new(RecordingClock::new(epoch, [30].into_iter()));
        let mut file = OpenFile::new(Count::default(), plan, clock);
        let end = epoch.add_ticks(60, 30);
        for tick in 4..60 {
            file.video(Some(&()), epoch.add_ticks(tick, 30), end)
                .unwrap();
        }
        file.finish(end, Ok(())).unwrap()
    }

    #[test]
    fn a_late_programs_skipped_head_counts_as_dropped() {
        let report = late_by_four(isocast_protocol::PROGRAM);
        assert_eq!((report.frames, report.repeated_frames), (56, 0));
        assert_eq!(report.dropped_frames, 4);
        assert!((report.start_offset_seconds - 4.0 / 30.0).abs() < 1e-6);
        assert!((report.duration_seconds - 56.0 / 30.0).abs() < 1e-6);
    }

    /// QA on #151: a 30 fps extra canvas ticks on the engine's 30 fps grid
    /// while the take's epoch is a 60 fps tick of the main canvas. On an odd
    /// one its first picture comes half a slot late, with nothing lost.
    #[test]
    fn only_whole_slots_before_the_first_picture_are_dropped() {
        let engine = Time::from_nanos(1_271_969_827_375);
        // A take on an odd 60 fps tick; the 30 fps canvas's ticks fall
        // half a slot after it.
        let odd = engine.add_ticks(61, 60);
        let program = plan(isocast_protocol::PROGRAM, odd);
        let head = |start| super::super::head_dropped(&program, odd, start);
        assert_eq!(head(odd), 0, "on time");
        assert_eq!(head(engine.add_ticks(31, 30)), 0, "half a slot late");
        assert_eq!(
            head(engine.add_ticks(33, 30)),
            2,
            "two and a half slots late"
        );
        // On the canvas's own grid, every slot before the picture counts.
        let program = plan(isocast_protocol::PROGRAM, engine);
        assert_eq!(
            super::super::head_dropped(&program, engine, engine.add_ticks(4, 30)),
            4
        );
    }

    #[test]
    fn a_late_iso_keeps_its_start_offset_without_drops() {
        let report = late_by_four("media");
        assert_eq!((report.frames, report.dropped_frames), (56, 0));
        assert!((report.start_offset_seconds - 4.0 / 30.0).abs() < 1e-6);
    }

    /// CI run 40161187404: a 30 fps program that began 4 ticks into a take
    /// split exactly 2 s after its epoch ended 61 ticks after the epoch, its
    /// last picture a repeat lying in the next take. It ends on the split.
    #[test]
    fn a_late_file_ends_on_its_takes_boundary() {
        let epoch = Time::from_nanos(1_271_969_827_375);
        let split = epoch.add_ticks(60, 30);
        assert_eq!(split.as_nanos() - epoch.as_nanos(), 2_000_000_000);
        for late in 0..30 {
            let start = epoch.add_ticks(late, 30);
            let end = file_end(start, split, 30);
            assert_eq!(end.ticks_since(start, 30), 60 - late as i64, "{late} late");
            assert!(
                end <= split && split.as_nanos() - end.as_nanos() <= 1,
                "{late} late"
            );
        }
        // Every rate the engine records, from any epoch tick.
        for fps in [24, 25, 30, 48, 50, 60, 120] {
            for late in 0..fps as u64 {
                let start = epoch.add_ticks(late, fps);
                let split = epoch.add_ticks(3 * fps as u64, fps);
                assert_eq!(
                    file_end(start, split, fps).ticks_since(start, fps),
                    3 * fps as i64 - late as i64,
                    "{late} late at {fps}"
                );
            }
        }
        // Take 2 of a split: its epoch is the split, itself a tick of take
        // 1's epoch; a file starting late in it ends on take 2's next split.
        let second = epoch.add_ticks(60, 30);
        for late in 0..30 {
            let start = second.add_ticks(late, 30);
            let end = file_end(start, second.add_ticks(45, 30), 30);
            assert_eq!(
                end.ticks_since(start, 30),
                45 - late as i64,
                "take 2, {late} late"
            );
        }
        // An end a microsecond past a boundary still completes its frame.
        assert_eq!(
            file_end(epoch, split + std::time::Duration::from_micros(1), 30),
            epoch.add_ticks(61, 30)
        );
    }
}
