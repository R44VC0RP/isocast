//! Recording-scoped transitions and retry limits. This ledger never opens a
//! device. Native owners keep their resources; only evidence crosses here.

use isocast_protocol::manifest::{
    VideoFaultKind, VideoIncident, VideoRecovery, VideoRecoveryScope as Scope,
    VideoRecoveryState as State,
};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use super::{file::file_end, pause::Cuts};
use crate::Time;

struct Incident {
    scope: Scope,
    owner: String,
    reason: String,
    fps: u32,
    from: Time,
    detected: Time,
    kind: VideoFaultKind,
    attempts: Vec<Attempt>,
    resumed: Option<Time>,
    ended: Option<Time>,
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
    entries: Mutex<Vec<Incident>>,
    revision: AtomicU64,
    stop: AtomicI64,
    limited_from: AtomicI64,
}

impl Default for VideoRecoveryLedger {
    fn default() -> Self {
        Self {
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
        if let Some(i) = entries
            .iter()
            .rposition(|e| e.scope == scope && e.owner == owner)
        {
            match entries[i].resumed {
                Some(at) if detected >= at => from = from.max(at),
                _ => return Some(i as u64 + 1),
            }
        }
        if entries.len() >= 256 {
            if from.as_nanos()
                < self
                    .limited_from
                    .fetch_min(from.as_nanos(), Ordering::AcqRel)
            {
                self.revision.fetch_add(1, Ordering::Release);
            }
            return None;
        }
        entries.push(Incident {
            scope,
            owner: owner.into(),
            fps,
            from,
            reason: reason.chars().take(512).collect(),
            detected,
            kind,
            attempts: Vec::new(),
            resumed: None,
            ended: None,
        });
        self.revision.fetch_add(1, Ordering::Release);
        Some(entries.len() as u64)
    }

    fn update(&self, id: u64, at: Time, f: impl FnOnce(&mut Incident) -> bool) {
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
        }
    }

    /// Called only by a future recovery-capable owner. These are uncut host
    /// deadlines: a user pause cannot extend them. No existing adapter opts in.
    pub fn begin_attempt(
        &self,
        id: u64,
        now: Time,
        take_epoch: Time,
    ) -> Result<Attempt, RetryRefused> {
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
        let attempt = Attempt {
            incident: id,
            number: count as u8 + 1,
            started: now,
            deadline: end.min(now + Duration::from_secs(5)),
            failed: None,
        };
        entries[i].attempts.push(attempt);
        self.revision.fetch_add(1, Ordering::Release);
        Ok(attempt)
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

    pub(super) fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
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
                if e.from >= until || until <= epoch || e.resumed.is_some_and(|at| at <= epoch) {
                    return None;
                }
                let slot = |at| {
                    // Same half-open video grid and one-nanosecond floor tolerance
                    // as a file's Stop, including e.g. 60 fps' fractional period.
                    file_end(saved_epoch, cuts.map(at).max(saved_epoch), e.fps)
                        .ticks_since(saved_epoch, e.fps) as u64
                };
                let first_lost_slot = slot(e.from.max(epoch));
                let end_slot_exclusive = slot(e.resumed.unwrap_or(until).min(until));
                let by_cut = |at: Option<Time>| at.is_some_and(|at| at <= until);
                Some(VideoIncident {
                    incident_id: i as u64 + 1,
                    scope: e.scope,
                    owner_id: e.owner.clone(),
                    reason: e.reason.clone(),
                    fault_kind: e.kind,
                    attempts_started: e.attempts.iter().filter(|a| a.started <= until).count()
                        as u8,
                    fps: e.fps,
                    state: if by_cut(e.resumed) {
                        State::Resumed
                    } else if by_cut(e.ended) {
                        State::EndedEarly
                    } else if until == stop {
                        State::Stopped
                    } else if e.attempts.first().is_some_and(|a| a.started <= until) {
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
