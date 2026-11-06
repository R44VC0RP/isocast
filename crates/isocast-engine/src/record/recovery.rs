//! Recording-scoped transitions and retry limits. This ledger never opens a
//! device. Native owners keep their resources; only evidence crosses here.

use isocast_protocol::manifest::{RolloverState, VideoRollover};
use isocast_protocol::manifest::{
    VideoFaultKind, VideoIncident, VideoRecovery, VideoRecoveryScope as Scope,
    VideoRecoveryState as State,
};
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use super::{file::file_end, pause::Cuts};
use crate::Time;

/// A writer's view of its take, including its pause mapping. The movie uses
/// saved PTS; retry deadlines use the uncut host clock. Do not use delivery
/// time as the end of a media gap (the IDR may arrive several frames late).
#[derive(Clone)]
pub struct OutputRecovery {
    pub(super) clock: Arc<super::pause::RecordingClock>,
    pub(super) plan: Arc<super::FilePlan>,
}

impl OutputRecovery {
    /// Called only across an actual gap in a writer's accepted picture PTS.
    /// Map both sides through the same pause cuts as the emitted packets.
    pub fn source_resumed_between(&self, previous: Time, next: Time) -> bool {
        let cuts = self.clock.state.read();
        self.clock.recovery.entries.lock().iter().rev().any(|e| {
            e.scope == Scope::Source
                && e.owner == self.plan.source
                && e.resumed.is_some_and(|r| {
                    let at = cuts.map(r);
                    previous < at && at <= next
                })
        })
    }

    /// Queue the name/offset for the sole manifest writer. The file worker
    /// must obtain admission from the returned reservation before writing media.
    pub fn reserve_part(
        &self,
        ticket: Attempt,
        reservation: ReservedMovie,
        saved: Time,
        now: Time,
        backend: String,
        preset: Option<u32>,
    ) -> anyhow::Result<PendingPart> {
        let ReservedMovie { path, identity } = reservation;
        anyhow::ensure!(
            self.can_resume(ticket, saved, now),
            "stale rollover generation"
        );
        let epoch = self
            .clock
            .state
            .read()
            .map(Time::from_nanos(self.plan.epoch.load(Ordering::Acquire)));
        let offset = saved.saturating_since(epoch);
        let first_name = self
            .plan
            .spec
            .path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow::anyhow!("part one needs a UTF-8 file name"))?;
        let mut row = PhysicalPart::new(path.clone(), offset.as_secs_f64());
        row.audio_loss_base = self.plan.audio_dropouts.load(Ordering::Acquire);
        row.encoder = self.plan.encoder.lock().clone().map(|mut e| {
            e.backend = backend;
            e.applied_preset = preset;
            e.planned_preset = preset;
            e.admission = "replanned".into();
            e.calibrated = false;
            e.estimated_load = None;
            e.reason = Some("replacement selected by mid-take recording preflight".into());
            e
        });
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let ledger = &self.clock.recovery;
        let mut requests = ledger.part_commits.lock();
        let mut parts = self.plan.parts.lock();
        let number = parts.len().max(1) as u32 + 1;
        let changed = ledger.update(ticket.incident, now, |e| {
            if e.attempts.last() != Some(&ticket) || e.rollover.is_some() {
                return false;
            }
            e.rollover = Some(VideoRollover {
                reservation_id: Some(identity),
                state: RolloverState::Pending,
                part: number,
                path,
                take_offset_ns: offset.as_nanos() as u64,
            });
            true
        });
        anyhow::ensure!(
            changed,
            "cannot publish a part for a closed recovery incident"
        );
        if parts.is_empty() {
            let start = Time::from_nanos(self.plan.start.load(Ordering::Acquire));
            parts.push(PhysicalPart::new(
                first_name.into(),
                start.saturating_since(epoch).as_secs_f64(),
            ));
        }
        parts.push(row);
        drop(parts);
        requests.push((self.plan.take, tx));
        drop(requests);
        ledger.revision.fetch_add(1, Ordering::Release);
        Ok(PendingPart {
            owner: self.clone(),
            ticket,
            number,
            saved,
            ack: rx,
            refused: None,
            deadline: ticket.deadline.min(now + Duration::from_secs(3)),
            retain: false,
        })
    }

    pub fn finish_part(&self, number: u32, facts: PartFacts) {
        if let Some(index) = number.checked_sub(1)
            && let Some(row) = self.plan.parts.lock().get_mut(index as usize)
        {
            row.facts = Some(facts);
        }
        self.clock.recovery.revision.fetch_add(1, Ordering::Release);
    }

    pub fn prior_finish_failed(&self, ticket: Attempt, number: u32, why: &str) {
        let mut parts = self.plan.parts.lock();
        let row = &mut parts[(number - 1) as usize];
        let error = format!(
            "{}: {}",
            row.path,
            why.chars().take(1024).collect::<String>()
        );
        row.finish_error = Some(error.clone());
        let ledger = &self.clock.recovery;
        // This incident has resumed. Do not mark the healthy output inactive
        // or create a new encoder fault for a different physical file.
        ledger.entries.lock()[ticket.incident as usize - 1]
            .reason
            .push_str(&format!("; part {number} finalization failed: {error}"));
        ledger.revision.fetch_add(1, Ordering::Release);
    }
    pub fn interrupted(&self, first_missing: Time, now: Time, reason: &str) -> Option<u64> {
        let from = self.clock.state.read().host_time(first_missing);
        self.plan.output_active.store(0, Ordering::Release);
        self.clock.recovery.interrupted(
            (Scope::Output, &self.plan.source),
            self.plan.spec.video.fps,
            from,
            now,
            VideoFaultKind::EncoderFailed,
            reason,
        )
    }
    pub fn attempt(&self, id: u64, now: Time) -> Result<Attempt, RetryRefused> {
        self.clock.recovery.begin_attempt(
            id,
            now,
            Time::from_nanos(self.plan.epoch.load(Ordering::Acquire)),
        )
    }
    /// A busy native admission gate is not a failed encoder open. Reserve
    /// it without blocking, then mint a ticket only if that succeeded.
    pub fn attempt_when<T>(
        &self,
        id: u64,
        now: Time,
        reserve: impl FnOnce() -> Option<T>,
    ) -> Result<Option<(Attempt, T)>, RetryRefused> {
        self.clock.recovery.begin_when(
            id,
            now,
            Time::from_nanos(self.plan.epoch.load(Ordering::Acquire)),
            reserve,
        )
    }
    pub fn failed_attempt(&self, ticket: Attempt, now: Time) {
        self.clock.recovery.finish_attempt(ticket, now, false);
    }
    pub fn can_resume(&self, ticket: Attempt, saved_pts: Time, now: Time) -> bool {
        let picture = self.clock.state.read().host_time(saved_pts);
        let ledger = &self.clock.recovery;
        now < ticket.deadline
            && now >= ticket.started
            && picture < ledger.end()
            && now < ledger.end()
            && ledger
                .entries
                .lock()
                .get(ticket.incident as usize - 1)
                .is_some_and(|e| {
                    e.attempts.last() == Some(&ticket)
                        && e.ended.is_none()
                        && e.resumed.is_none()
                        && picture >= e.from
                })
    }
    /// Publish only *after* the first real IDR's fragment is committed.
    pub fn resumed(
        &self,
        ticket: Attempt,
        saved_pts: Time,
        checked_at: Time,
        backend: &str,
        hardware: bool,
    ) {
        let picture = self.clock.state.read().host_time(saved_pts);
        let changed = self
            .clock
            .recovery
            .update(ticket.incident, checked_at, |e| {
                if e.attempts.last() != Some(&ticket)
                    || checked_at >= ticket.deadline
                    || picture < e.from
                {
                    return false;
                }
                e.resumed = Some(picture);
                if let Some(roll) = e.rollover.as_mut() {
                    roll.state = RolloverState::Committed;
                    e.reason.push_str("; committed_seconds stays at the continuous prefix before the picture gap; resumed in a new output part with ");
                } else {
                    e.reason.push_str("; resumed in the same file with ");
                }
                e.reason.extend(backend.chars().take(512));
                true
            });
        if !changed {
            return;
        }
        self.plan
            .hardware_accelerated
            .store(i64::from(hardware), Ordering::Release);
        self.plan.output_active.store(1, Ordering::Release);
    }
    pub fn ended_early(&self, id: u64, now: Time, why: &str) {
        self.clock.recovery.update(id, now, |e| {
            e.ended = Some(now);
            e.reason.push_str("; ");
            e.reason.extend(why.chars().take(512));
            true
        });
    }
    pub fn storage_failed(&self, id: u64, now: Time, why: &str) {
        self.clock.recovery.storage_failed(id, now, why);
    }
}

pub struct ReservedMovie {
    pub path: String,
    pub identity: String,
}

/// Owned by the single file worker until the first picture commits.
/// Dropping an unadmitted part retracts its row, not the physical header: the
/// last durable checkpoint may still name it, even if the ack never arrived.
#[must_use]
pub struct PendingPart {
    owner: OutputRecovery,
    ticket: Attempt,
    number: u32,
    saved: Time,
    ack: std::sync::mpsc::Receiver<Result<(), String>>,
    // Inclusive saved PTS. No video can be admitted between these refusals.
    refused: Option<(Time, Time)>,
    deadline: Time,
    retain: bool,
}

impl PendingPart {
    pub fn refused_picture(&mut self, pts: Time) {
        self.refused.get_or_insert((pts, pts)).1 = pts;
    }

    /// Publish the admission gap only after the retained IDR commits. On
    /// cancellation the original incident already spans the whole outage.
    pub fn committed(mut self) {
        self.retain = true;
        let Some((first, last)) = self.refused else {
            return;
        };
        let owner = &self.owner;
        let fps = owner.plan.spec.video.fps;
        let cuts = owner.clock.state.read();
        let from = cuts.host_time(first);
        let end = cuts.host_time(last.add_ticks(1, fps));
        let ledger = &owner.clock.recovery;
        let mut entries = ledger.entries.lock();
        let original = &entries[self.ticket.incident as usize - 1];
        if original.resumed.is_none_or(|idr| idr >= from) {
            return;
        }
        ledger.push(
            &mut entries,
            Incident {
                scope: Scope::Output,
                owner: owner.plan.source.clone(),
                fps,
                kind: VideoFaultKind::RolloverAckWait,
                reason: format!(
                    "pictures refused while waiting for part {} manifest acknowledgment",
                    self.number
                ),
                from,
                detected: from,
                resumed: Some(end),
                ended: None,
                split_end: None,
                attempts: Vec::new(),
                source_retry: None,
                rollover: None,
            },
        );
    }

    pub fn poll_ack(
        &mut self,
        expected: u32,
        now: Time,
        skipped: Option<Time>,
    ) -> anyhow::Result<bool> {
        use std::sync::mpsc::TryRecvError;
        anyhow::ensure!(self.number == expected, "unexpected physical part ordinal");
        anyhow::ensure!(
            self.owner.can_resume(self.ticket, self.saved, now),
            "recovery expired before the first picture"
        );
        // An upstream-skipped picture cannot be submitted even if this poll
        // consumes the ack. Account for it before the handoff can commit.
        if let Some(pts) = skipped {
            self.refused_picture(pts);
        }
        match self.ack.try_recv() {
            Ok(Ok(())) => Ok(true),
            Ok(Err(msg)) => Err(anyhow::Error::msg(msg).context(PartMetadataWrite)),
            Err(TryRecvError::Empty) if now < self.deadline => Ok(false),
            Err(_) => anyhow::bail!("rollover intent was not durably acknowledged"),
        }
    }
    /// Once a write starts even unwinding must leave the row discoverable.
    pub fn writing(&mut self) {
        self.retain = true;
    }
    /// The owned mux handle still contains *only* its original header.
    pub fn retract_empty(mut self) {
        self.retain = false;
    }
}

impl Drop for PendingPart {
    fn drop(&mut self) {
        if self.retain {
            return;
        }
        let mut parts = self.owner.plan.parts.lock();
        let ledger = &self.owner.clock.recovery;
        let mut entries = ledger.entries.lock();
        let incident = &mut entries[self.ticket.incident as usize - 1];
        let roll = incident.rollover.as_mut().expect("queued rollover");
        debug_assert_eq!(parts.last().map(|p| &p.path), Some(&roll.path));
        parts.pop();
        if parts.len() == 1 {
            parts.clear();
        } // back to the ordinary single-file plan
        roll.state = RolloverState::ReservedNoMedia;
        ledger.revision.fetch_add(1, Ordering::Release);
    }
}

pub struct PartFacts {
    pub frames: u64,
    pub repeated: u64,
    pub dropped: u64,
    pub duration: Duration,
    pub video: Duration,
}

#[derive(Debug)]
pub struct PartMetadataWrite;
impl std::fmt::Display for PartMetadataWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("writing durable output-part metadata")
    }
}

pub(super) struct PhysicalPart {
    pub finish_error: Option<String>,
    pub encoder: Option<isocast_protocol::manifest::ManifestEncoder>,
    pub audio_loss_base: u64,
    pub path: String,
    pub offset: f64,
    pub facts: Option<PartFacts>,
}
impl PhysicalPart {
    fn new(path: String, offset: f64) -> Self {
        Self {
            path,
            encoder: None,
            finish_error: None,
            audio_loss_base: 0,
            offset,
            facts: None,
        }
    }
}

struct Incident {
    rollover: Option<VideoRollover>,
    scope: Scope,
    owner: String,
    reason: String,
    fps: u32,
    from: Time,
    detected: Time,
    kind: VideoFaultKind,
    attempts: Vec<Attempt>,
    /// A platform source exists before its recording ledger. It admits its
    /// one verified-target reopen locally; report it, don't issue it twice.
    source_retry: Option<Time>,
    resumed: Option<Time>,
    ended: Option<Time>,
    /// Media boundary, distinct from observing failure/teardown (which can
    /// arrive late). Unlike a terminal output, the next file is independent.
    split_end: Option<Time>,
}

/// A native result must carry this ticket, not just the source id: a late
/// success from an expired first attempt cannot resurrect the second one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attempt {
    incident: u64,
    number: u8,
    started: Time,
    pub deadline: Time,
    failed: Option<Time>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryRefused {
    WaitUntil(Time),
    Ended,
    BudgetExhausted,
    StorageFailed,
}

/// Per-recording transition ledger for capture and recording workers. A token
/// belongs to one incident in this ledger; stale callbacks cannot close a
/// later one. All event times are uncut host times, never since a retry.
pub struct VideoRecoveryLedger {
    pub(super) part_commits: Mutex<Vec<PartCheckpoint>>,
    entries: Mutex<Vec<Incident>>,
    revision: AtomicU64,
    stop: AtomicI64,
    limited_from: AtomicI64,
}

type PartCheckpoint = (u32, std::sync::mpsc::SyncSender<Result<(), String>>);

impl Default for VideoRecoveryLedger {
    fn default() -> Self {
        Self {
            part_commits: Default::default(),
            entries: Default::default(),
            revision: Default::default(),
            stop: AtomicI64::new(i64::MAX),
            limited_from: AtomicI64::new(i64::MAX),
        }
    }
}

impl VideoRecoveryLedger {
    pub(super) fn stop_at(&self, end: Time) {
        self.stop.fetch_min(end.as_nanos(), Ordering::AcqRel);
    }

    fn end(&self) -> Time {
        Time::from_nanos(self.stop.load(Ordering::Acquire))
    }

    /// `from` is the first known unavailable slot/bound, not the last frame
    /// arrival of a static desktop. Preserve the first cause on repeat signals.
    /// None means the recording's evidence budget is exhausted; do not retry.
    pub fn interrupted(
        &self,
        (scope, owner): (Scope, &str),
        fps: u32,
        from: Time,
        detected: Time,
        kind: VideoFaultKind,
        reason: &str,
    ) -> Option<u64> {
        assert!(fps > 0, "an interruption needs a video slot rate");
        let mut entries = self.entries.lock();
        let mut from = from;
        if let Some(i) = entries.iter().rposition(|e| {
            e.scope == scope && e.owner == owner && e.kind != VideoFaultKind::RolloverAckWait
        }) {
            match entries[i].split_end.or(entries[i].resumed) {
                Some(at) if detected >= at => from = from.max(at),
                _ => return Some(i as u64 + 1),
            }
        }
        self.push(
            &mut entries,
            Incident {
                rollover: None,
                scope,
                owner: owner.into(),
                fps,
                from,
                reason: reason.chars().take(512).collect(),
                detected,
                kind,
                attempts: Vec::new(),
                source_retry: None,
                resumed: None,
                ended: None,
                split_end: None,
            },
        )
    }

    fn push(&self, entries: &mut Vec<Incident>, entry: Incident) -> Option<u64> {
        if entries.len() >= 256 {
            let from = entry.from.as_nanos();
            if from < self.limited_from.fetch_min(from, Ordering::AcqRel) {
                self.revision.fetch_add(1, Ordering::Release);
            }
            return None;
        }
        entries.push(entry);
        self.revision.fetch_add(1, Ordering::Release);
        Some(entries.len() as u64)
    }

    fn update(&self, id: u64, at: Time, f: impl FnOnce(&mut Incident) -> bool) -> bool {
        let mut entries = self.entries.lock();
        if let Some(entry) = id
            .checked_sub(1)
            .and_then(|n| usize::try_from(n).ok())
            .and_then(|i| entries.get_mut(i))
            && entry.resumed.is_none()
            && entry.ended.is_none()
            && at >= entry.detected
            && f(entry)
        {
            self.revision.fetch_add(1, Ordering::Release);
            return true;
        }
        false
    }

    /// Uncut host deadlines: a user pause cannot extend them.
    pub fn begin_attempt(
        &self,
        id: u64,
        now: Time,
        take_epoch: Time,
    ) -> Result<Attempt, RetryRefused> {
        self.begin_when(id, now, take_epoch, || Some(()))
            .map(|r| r.expect("unconditional reservation").0)
    }

    fn begin_when<T>(
        &self,
        id: u64,
        now: Time,
        take_epoch: Time,
        reserve: impl FnOnce() -> Option<T>,
    ) -> Result<Option<(Attempt, T)>, RetryRefused> {
        let mut entries = self.entries.lock();
        let i = id
            .checked_sub(1)
            .and_then(|n| usize::try_from(n).ok())
            .ok_or(RetryRefused::Ended)?;
        let e = entries.get(i).ok_or(RetryRefused::Ended)?;
        if e.resumed.is_some() || e.ended.is_some() || now >= self.end() {
            return Err(RetryRefused::Ended);
        }
        if e.kind == VideoFaultKind::StorageFailed {
            return Err(RetryRefused::StorageFailed);
        }
        let end = (e.detected + Duration::from_secs(15)).min(self.end());
        let count = e.attempts.len();
        let prior = entries[..i]
            .iter()
            .filter(|p| {
                p.scope == e.scope
                    && p.owner == e.owner
                    && p.attempts.iter().any(|a| a.started >= take_epoch)
            })
            .count();
        if now >= end || prior >= 3 {
            return Err(RetryRefused::BudgetExhausted);
        }
        if let Some(last) = e.attempts.last() {
            let finished = last.failed.unwrap_or(last.deadline);
            if now < finished {
                return Err(RetryRefused::WaitUntil(finished.min(end)));
            }
            if count >= 3 {
                return Err(RetryRefused::BudgetExhausted);
            }
            let next = finished + Duration::from_millis(if count == 1 { 250 } else { 1000 });
            if now < next {
                return Err(RetryRefused::WaitUntil(next.min(end)));
            }
        } else if now < e.detected {
            return Err(RetryRefused::WaitUntil(e.detected));
        }
        let Some(reservation) = reserve() else {
            return Ok(None);
        };
        let attempt = Attempt {
            incident: id,
            number: count as u8 + 1,
            started: now,
            deadline: end.min(now + Duration::from_secs(5)),
            failed: None,
        };
        entries[i].attempts.push(attempt);
        self.revision.fetch_add(1, Ordering::Release);
        Ok(Some((attempt, reservation)))
    }

    /// `resumed` requires a fresh source picture or validated replacement
    /// output, not open success. Other/expired generations are ignored.
    pub fn finish_attempt(&self, ticket: Attempt, at: Time, resumed: bool) {
        self.update(ticket.incident, at, |e| {
            if at >= self.end() {
                return false;
            }
            let Some(a) = e.attempts.last_mut() else {
                return false;
            };
            if a.number != ticket.number
                || a.failed.is_some()
                || at < a.started
                || at >= a.deadline
                || (resumed && at < e.from)
            {
                return false;
            }
            if resumed {
                e.resumed = Some(at);
            } else {
                a.failed = Some(at);
            }
            true
        });
    }

    /// Terminal for this owner, not for the take. Its unavailable tail still
    /// accrues through the take's end, even if another output records for hours.
    pub fn ended_early(&self, id: u64, at: Time) {
        self.update(id, at, |e| {
            e.ended = Some(at);
            true
        });
    }

    pub(super) fn end_source(&self, id: u64, at: Time, reason: &str) {
        self.update(id, at, |e| {
            if e.reason != reason {
                e.reason.push_str("; ");
                e.reason.extend(reason.chars().take(512));
            }
            e.ended = Some(at);
            true
        });
    }

    pub(super) fn observe_source_retry(&self, id: u64, started: Option<Time>, now: Time) {
        if started.is_some() {
            self.update(id, now, |e| {
                if e.source_retry.is_some() {
                    return false;
                }
                e.source_retry = started;
                true
            });
        }
    }

    /// A write/sync failure while sealing or resuming the prefix supersedes
    /// the codec retry, without creating a second overlapping interval.
    pub fn storage_failed(&self, id: u64, at: Time, why: &str) {
        self.update(id, at, |e| {
            e.kind = VideoFaultKind::StorageFailed;
            e.ended = Some(at);
            e.reason.push_str("; storage failed: ");
            e.reason.extend(why.chars().take(512));
            true
        });
    }

    pub(super) fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    pub(super) fn reopening_output(&self) -> bool {
        self.entries.lock().iter().any(|e| {
            e.scope == Scope::Output
                && e.kind == VideoFaultKind::EncoderFailed
                && e.ended.is_none()
                && e.resumed.is_none()
        })
    }

    /// Called synchronously by the old file's sole packet worker *after*
    /// committing it and *before* sending the first picture to its successor.
    /// The async finalizer is too late: the same output may have failed again.
    pub(super) fn output_split(&self, owner: &str, cut: Time) {
        let mut entries = self.entries.lock();
        if let Some(e) = entries.iter_mut().rev().find(|e| {
            e.scope == Scope::Output
                && e.owner == owner
                && e.kind != VideoFaultKind::RolloverAckWait
        }) && e.resumed.is_none()
            && e.split_end.is_none()
        {
            e.split_end = Some(cut);
            if e.ended.is_none() {
                e.ended = Some(cut.max(e.detected));
                e.reason.push_str("; file ended at split during recovery");
            }
            self.revision.fetch_add(1, Ordering::Release);
        }
    }

    pub(super) fn observe_reopened_source(&self, owner: &str, first: Time) {
        let mut entries = self.entries.lock();
        if let Some(e) = entries
            .iter_mut()
            .rev()
            .find(|e| e.scope == Scope::Source && e.owner == owner)
            && e.resumed.is_none()
            && e.ended.is_none()
            && e.attempts.is_empty()
            && first < self.end()
            && first >= e.from
        {
            e.resumed = Some(first);
            self.revision.fetch_add(1, Ordering::Release);
        }
    }

    pub(super) fn snapshot(&self, cuts: &Cuts, epoch: Time, until: Time) -> VideoRecovery {
        let stop = self.end();
        let until = until.min(stop);
        let saved_epoch = cuts.map(epoch);
        let incidents: Vec<_> = self
            .entries
            .lock()
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                let until = e.split_end.unwrap_or(until).min(until);
                let media_end = e.split_end.or(e.resumed);
                if e.from >= until || until <= epoch || media_end.is_some_and(|at| at <= epoch) {
                    return None;
                }
                let slot = |at| {
                    // Same half-open video grid and one-nanosecond floor tolerance
                    // as a file's Stop, including e.g. 60 fps' fractional period.
                    file_end(saved_epoch, cuts.map(at).max(saved_epoch), e.fps)
                        .ticks_since(saved_epoch, e.fps) as u64
                };
                let first_lost_slot = slot(e.from.max(epoch));
                let end_slot_exclusive = slot(media_end.unwrap_or(until).min(until));
                let by_cut = |at: Option<Time>| at.is_some_and(|at| at <= until);
                Some(VideoIncident {
                    rollover: e.rollover.clone(),
                    incident_id: i as u64 + 1,
                    scope: e.scope,
                    owner_id: e.owner.clone(),
                    reason: e.reason.clone(),
                    fault_kind: e.kind,
                    attempts_started: e.attempts.iter().filter(|a| a.started <= until).count()
                        as u8
                        + u8::from(by_cut(e.source_retry)),
                    fps: e.fps,
                    state: if by_cut(e.resumed) {
                        State::Resumed
                    } else if by_cut(e.ended) || by_cut(e.split_end) {
                        State::EndedEarly
                    } else if until == stop {
                        State::Stopped
                    } else if by_cut(e.source_retry)
                        || e.attempts.first().is_some_and(|a| a.started <= until)
                    {
                        State::Recovering
                    } else {
                        State::Interrupted
                    },
                    first_lost_slot,
                    end_slot_exclusive,
                    lost_slots: end_slot_exclusive - first_lost_slot,
                })
            })
            .collect();
        VideoRecovery {
            incident_count: incidents.len() as u64,
            history_limited: self.limited_from.load(Ordering::Acquire) < until.as_nanos(),
            incidents,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_restart_counts_once_and_ends_on_the_first_eligible_iso_tick() {
        let zero = Time::from_nanos(0);
        let clock = super::super::pause::RecordingClock::new(zero, [30].into_iter());
        let ledger = &clock.recovery;
        let at = |n| zero.add_ticks(n, 30);
        let id = ledger
            .interrupted(
                (Scope::Source, "window"),
                30,
                at(4),
                at(4),
                VideoFaultKind::CaptureStopped,
                "helper exited",
            )
            .unwrap();
        for tick in 5..9 {
            ledger.observe_source_retry(id, Some(at(4)), at(tick));
        }
        let report = |tick| {
            ledger
                .snapshot(&clock.state.read(), zero, at(tick))
                .incidents
                .remove(0)
        };
        assert_eq!(report(7).state, State::Recovering);
        ledger.observe_reopened_source("window", at(10));
        let e = report(12);
        assert_eq!(
            (
                e.state,
                e.attempts_started,
                e.first_lost_slot,
                e.end_slot_exclusive
            ),
            (State::Resumed, 1, 4, 10)
        );
        let id = ledger
            .interrupted(
                (Scope::Source, "window"),
                30,
                at(15),
                at(15),
                VideoFaultKind::CaptureStopped,
                "helper exited",
            )
            .unwrap();
        ledger.end_source(id, at(16), "original window closed");
        ledger.observe_reopened_source("window", at(17));
        let e = ledger
            .snapshot(&clock.state.read(), zero, at(19))
            .incidents
            .remove(1);
        assert_eq!((e.state, e.lost_slots), (State::EndedEarly, 4));
        assert!(e.reason.contains("original window closed"));
    }

    #[test]
    fn admission_contention_waits_within_incident_time_without_spending_tickets() {
        let now = |s| Time::from_nanos(s * 1_000_000_000i64);
        let ledger = VideoRecoveryLedger::default();
        let fault = |id| {
            ledger
                .interrupted(
                    (Scope::Output, id),
                    30,
                    now(0),
                    now(0),
                    VideoFaultKind::EncoderFailed,
                    "encoder",
                )
                .unwrap()
        };
        let id = fault("program");
        for s in 0..12 {
            assert!(
                ledger
                    .begin_when(id, now(s), now(0), || None::<()>)
                    .unwrap()
                    .is_none()
            );
        }
        let (first, ()) = ledger
            .begin_when(id, now(12), now(0), || Some(()))
            .unwrap()
            .unwrap();
        assert_eq!((first.number, first.deadline), (1, now(15)));
        let other = fault("camera");
        assert_eq!(
            ledger.begin_when(other, now(15), now(0), || None::<()>),
            Err(RetryRefused::BudgetExhausted)
        );
    }

    #[test]
    fn handing_off_a_recovering_file_ends_its_gap_not_the_next_take() {
        let epoch = Time::from_nanos(0);
        let clock = super::super::pause::RecordingClock::new(epoch, [30].into_iter());
        let ledger = &clock.recovery;
        let at = |n| epoch.add_ticks(n, 30);
        // A split already scheduled at slot 30; the old file's queued work
        // reports the missing slot 24 after its cut, at slot 31.
        let fault = |from, detected| {
            ledger
                .interrupted(
                    (Scope::Output, "bars"),
                    30,
                    at(from),
                    at(detected),
                    VideoFaultKind::EncoderFailed,
                    "submit failed",
                )
                .unwrap()
        };
        let old = fault(24, 31);
        let ticket = ledger.begin_attempt(old, at(32), epoch).unwrap();
        assert!(ledger.reopening_output());
        ledger.output_split("bars", at(30));
        ledger.finish_attempt(ticket, at(33), true);
        assert!(!ledger.reopening_output());
        let snap = |from, to| ledger.snapshot(&clock.state.read(), at(from), at(to));
        let first = snap(0, 30);
        assert_eq!(
            (first.incidents[0].state, first.incidents[0].lost_slots),
            (State::EndedEarly, 6)
        );
        assert!(
            first.incidents[0]
                .reason
                .contains("file ended at split during recovery")
        );
        assert!(snap(30, 40).incidents.is_empty());
        let new = fault(36, 40);
        assert_ne!(new, old);
        assert!(ledger.begin_attempt(new, at(40), at(30)).is_ok());
        assert_eq!(snap(30, 45).incidents[0].lost_slots, 9);
        ledger.stop_at(at(45));
        assert_eq!(snap(30, 50).incidents[0].state, State::Stopped);
    }

    #[test]
    fn failed_prefix_sync_terminates_the_original_open_codec_incident() {
        let zero = Time::from_nanos(0);
        let clock = super::super::pause::RecordingClock::new(zero, [30].into_iter());
        let ledger = &clock.recovery;
        let at = |n| zero.add_ticks(n, 30);
        let id = ledger
            .interrupted(
                (Scope::Output, "bars"),
                30,
                at(12),
                at(14),
                VideoFaultKind::EncoderFailed,
                "MF ProcessOutput failed",
            )
            .unwrap();
        let ticket = ledger.begin_attempt(id, at(14), zero).unwrap();
        assert!(ledger.reopening_output());
        ledger.storage_failed(id, at(15), "sync: disk full");
        ledger.finish_attempt(ticket, at(16), true);
        assert!(!ledger.reopening_output());
        assert!(ledger.begin_attempt(id, at(17), zero).is_err());
        let snapshot = ledger.snapshot(&clock.state.read(), zero, at(30));
        let [entry] = snapshot.incidents.as_slice() else {
            panic!("one continuous interval")
        };
        assert_eq!(
            (
                entry.fault_kind,
                entry.state,
                entry.attempts_started,
                entry.lost_slots
            ),
            (VideoFaultKind::StorageFailed, State::EndedEarly, 1, 18)
        );
        assert!(
            entry
                .reason
                .contains("MF ProcessOutput failed; storage failed: sync: disk full")
        );
    }

    #[test]
    fn picture_pts_invert_the_pause_mapping_without_rebasing_the_retry_clock() {
        let epoch = Time::from_nanos(0);
        let at = |n| epoch.add_ticks(n, 30);
        let clock = super::super::pause::RecordingClock::new(epoch, [30].into_iter());
        let cut = clock.change(true, at(20), None).unwrap();
        let resume = clock.change(false, at(50), None).unwrap();
        let cuts = clock.state.read();
        assert_eq!(cuts.host_time(cuts.map(at(15))), at(15));
        assert_eq!(cuts.host_time(cuts.map(cut)), resume);
        assert_eq!(cuts.host_time(cuts.map(at(70))), at(70));
    }

    #[test]
    fn full_history_returns_no_ticket_but_preserves_existing_incidents() {
        let clock = super::super::pause::RecordingClock::new(Time::from_nanos(0), [30].into_iter());
        let ledger = &clock.recovery;
        let stop = |owner: &str| {
            ledger.interrupted(
                (Scope::Source, owner),
                30,
                Time::from_nanos(0),
                Time::from_nanos(0),
                VideoFaultKind::CaptureStopped,
                "capture stopped",
            )
        };
        for i in 1..=256 {
            assert_eq!(stop(&i.to_string()), Some(i));
        }
        assert_eq!(stop("additional source"), None);
        assert_eq!(stop("256"), Some(256));
        let snapshot = ledger.snapshot(
            &clock.state.read(),
            Time::from_nanos(0),
            Time::from_nanos(100_000_000),
        );
        assert!(snapshot.history_limited);
        assert_eq!(snapshot.incident_count, 256);
        assert_eq!(snapshot.incidents.last().unwrap().lost_slots, 3);
    }

    #[test]
    fn retry_limit_is_per_owner_and_take_and_stop_beats_a_late_success() {
        let zero = Time::from_nanos(0);
        let clock = super::super::pause::RecordingClock::new(zero, [30].into_iter());
        let ledger = &clock.recovery;
        let at = |n| zero.add_ticks(n, 30);
        for i in 0..3 {
            let id = ledger
                .interrupted(
                    (Scope::Source, "screen"),
                    30,
                    at(3 * i),
                    at(3 * i),
                    VideoFaultKind::DeviceLost,
                    "lost",
                )
                .unwrap();
            let t = ledger.begin_attempt(id, at(3 * i), zero).unwrap();
            ledger.finish_attempt(t, at(3 * i + 1), true);
        }
        let id = ledger
            .interrupted(
                (Scope::Source, "screen"),
                30,
                at(9),
                at(9),
                VideoFaultKind::DeviceLost,
                "again",
            )
            .unwrap();
        assert_eq!(
            ledger.begin_attempt(id, at(9), zero),
            Err(RetryRefused::BudgetExhausted)
        );
        let t = ledger.begin_attempt(id, at(30), at(30)).unwrap();
        ledger.stop_at(at(32));
        ledger.finish_attempt(t, at(33), true);
        assert_eq!(
            ledger.begin_attempt(id, at(34), at(30)),
            Err(RetryRefused::Ended)
        );
        let snapshot = ledger.snapshot(&clock.state.read(), at(30), at(100));
        assert_eq!(
            (
                snapshot.incidents[0].state,
                snapshot.incidents[0].lost_slots
            ),
            (State::Stopped, 2)
        );
    }

    #[test]
    fn retries_are_bounded_in_host_time_and_only_the_current_ticket_can_resume() {
        let ms = |n: i64| Time::from_nanos(n * 1_000_000);
        let ledger = VideoRecoveryLedger::default();
        let id = ledger
            .interrupted(
                (Scope::Output, "program"),
                30,
                ms(500),
                ms(1000),
                VideoFaultKind::EncoderFailed,
                "codec rejected frame",
            )
            .unwrap();
        let attempt = |at| ledger.begin_attempt(id, ms(at), ms(0));
        let first = attempt(1000).unwrap();
        assert_eq!(attempt(2000), Err(RetryRefused::WaitUntil(ms(6000))));
        ledger.finish_attempt(first, ms(2000), false);
        assert_eq!(attempt(2200), Err(RetryRefused::WaitUntil(ms(2250))));
        let second = attempt(2250).unwrap();
        ledger.finish_attempt(first, ms(2300), true);
        assert_eq!(attempt(7250), Err(RetryRefused::WaitUntil(ms(8250))));
        let last = attempt(14000).unwrap();
        assert_eq!(last.deadline, ms(16000));
        ledger.finish_attempt(second, ms(14500), true);
        ledger.finish_attempt(last, ms(15000), false);
        assert_eq!(attempt(15001), Err(RetryRefused::BudgetExhausted));
        assert_eq!(attempt(17000), Err(RetryRefused::BudgetExhausted));
        let disk = ledger
            .interrupted(
                (Scope::Output, "camera"),
                60,
                ms(1000),
                ms(1000),
                VideoFaultKind::StorageFailed,
                "disk full",
            )
            .unwrap();
        assert_eq!(
            ledger.begin_attempt(disk, ms(1000), ms(0)),
            Err(RetryRefused::StorageFailed)
        );
    }
}
