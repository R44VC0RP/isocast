//! Per-media-clock waits, with measured stretching shared across the process.
//!
//! Ordinary sleeps are cheap and sufficient in normal user launch contexts.
//! Only measured stretching selects the platform's critical deadline timer.
//! Subsequent clocks inherit that evidence instead of relearning it while
//! a take is already starting. Keep the evidence until process exit: healthy
//! critical wakes do not prove ordinary sleeps have recovered. This changes
//! neither process scheduling policy nor recording admission/startup waits.

use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use isocast_protocol::api::{ClockPacingInfo, PacingMode, Percentiles};

use crate::{Platform, Time};

const WINDOW: usize = 600;

// Survives engine restarts within the daemon. A new daemon starts ordinary;
// the program/source startup probes can establish evidence before ISO clocks
// are created. Early-created clocks can also adopt it at their next wait;
// allocation can precede the program's first probe on a cold engine.
static PROCESS_STRETCHED: AtomicBool = AtomicBool::new(false);

/// Active clocks only; weak entries disappear when a worker exits. Reopening
/// a source/ISO with the same id replaces its old entry, not another clock.
#[derive(Default)]
pub(crate) struct Clocks(Mutex<BTreeMap<String, Weak<Stats>>>);

impl Clocks {
    pub(crate) fn start(&self, id: String, period: Duration) -> MediaClock {
        let inherited_critical = PROCESS_STRETCHED.load(Ordering::Acquire);
        let stats = Arc::new(Stats {
            id: id.clone(),
            period,
            critical: AtomicBool::new(inherited_critical),
            inherited_critical: AtomicBool::new(inherited_critical),
            wakes: AtomicU64::new(0),
            late_wakes: AtomicU64::new(0),
            max_ns: AtomicU64::new(0),
            times: Box::new(std::array::from_fn(|_| AtomicU64::new(0))),
        });
        let mut clocks = self.0.lock();
        clocks.retain(|_, clock| clock.strong_count() > 0);
        clocks.insert(id, Arc::downgrade(&stats));
        MediaClock {
            period,
            critical: inherited_critical,
            late_streak: 0,
            probes: 2,
            next: 0,
            stats,
        }
    }

    pub(crate) fn info(&self) -> Vec<ClockPacingInfo> {
        let clocks: Vec<_> = self.0.lock().values().filter_map(Weak::upgrade).collect();
        clocks.iter().map(|clock| clock.info()).collect()
    }
}

struct Stats {
    id: String,
    period: Duration,
    critical: AtomicBool,
    inherited_critical: AtomicBool,
    wakes: AtomicU64,
    late_wakes: AtomicU64,
    max_ns: AtomicU64,
    // u64 nanoseconds also preserve multi-second whole-thread blackouts.
    // One writer, lock-free readers: a status/metrics thread suspended by
    // launch policy must never hold up a media clock while sorting samples.
    times: Box<[AtomicU64; WINDOW]>,
}

impl Stats {
    fn record(&self, late: Duration, index: usize) {
        let ns = late.as_nanos().min(u128::from(u64::MAX)) as u64;
        self.times[index].store(ns, Ordering::Relaxed);
        if late > self.period {
            self.late_wakes.fetch_add(1, Ordering::Relaxed);
        }
        self.max_ns.fetch_max(ns, Ordering::Relaxed);
        self.wakes.fetch_add(1, Ordering::Release);
    }

    fn info(&self) -> ClockPacingInfo {
        let wakes = self.wakes.load(Ordering::Acquire);
        // A concurrent tick may replace a sample; these are recent-window
        // diagnostics, not a transactional snapshot or a scheduling input.
        let mut times: Vec<_> = self.times[..wakes.min(WINDOW as u64) as usize]
            .iter()
            .map(|ns| ns.load(Ordering::Relaxed))
            .collect();
        times.sort_unstable();
        let percentile = |p: f64| {
            if times.is_empty() {
                return 0.0;
            }
            let rank = ((times.len() as f64 * p).ceil() as usize).clamp(1, times.len());
            (times[rank - 1] as f64 / 100.0).round() / 10.0
        };
        let critical = self.critical.load(Ordering::Acquire);
        let inherited_critical = self.inherited_critical.load(Ordering::Relaxed);
        ClockPacingInfo {
            id: self.id.clone(),
            period_ms: self.period.as_secs_f64() * 1000.0,
            mode: if critical {
                PacingMode::Critical
            } else {
                PacingMode::Ordinary
            },
            inherited_critical,
            escalations: u64::from(critical && !inherited_critical),
            wakes,
            late_wakes: self.late_wakes.load(Ordering::Relaxed),
            wake_max_us: self.max_ns.load(Ordering::Relaxed) as f64 / 1000.0,
            wake_lateness_us: Percentiles {
                p50: percentile(0.5),
                p99: percentile(0.99),
            },
        }
    }
}

pub(crate) struct MediaClock {
    period: Duration,
    critical: bool,
    late_streak: u8,
    // Learn during the existing pre-roll: split only the first two long
    // waits, so ordinary steady-state pacing has the original sleep cadence.
    probes: u8,
    next: usize,
    stats: Arc<Stats>,
}

impl MediaClock {
    pub(crate) fn sleep_until<P: Platform>(&mut self, platform: &P, at: Time) {
        let mut now = platform.now();
        while now < at {
            if !self.critical && PROCESS_STRETCHED.load(Ordering::Acquire) {
                self.critical = true;
                self.stats.inherited_critical.store(true, Ordering::Relaxed);
                self.stats.critical.store(true, Ordering::Release);
            }
            let deadline = if self.probes > 0 && !self.critical {
                at.min(now + Duration::from_millis(20))
            } else {
                at
            };
            if self.critical {
                platform.sleep_until(deadline);
            } else {
                std::thread::sleep(deadline.saturating_since(now));
            }
            now = platform.now();
            let late = now.saturating_since(deadline);
            if !self.critical {
                // Two-frame slack is too permissive at 30 fps: launchd can
                // stretch each wait by 50–65 ms without crossing it. Ignore
                // normal 10–15 ms jitter, but learn within the pre-roll.
                let stretched = late > (self.period / 2).max(Duration::from_millis(20));
                self.late_streak = if stretched { self.late_streak + 1 } else { 0 };
                // A severe *startup* wake also suffices. Later, isolated
                // blackouts/CPU stalls don't change policy: two real sleeps
                // must stretch. Already-overdue catch-up ticks never vote.
                if self.late_streak >= 2
                    || (self.probes > 0 && late > (self.period * 4).max(Duration::from_millis(40)))
                {
                    PROCESS_STRETCHED.store(true, Ordering::Release);
                    self.critical = true;
                    self.stats.critical.store(true, Ordering::Release);
                }
                self.probes = self.probes.saturating_sub(1);
            }
        }
        self.stats.record(now.saturating_since(at), self.next);
        self.next = (self.next + 1) % WINDOW;
    }
}
