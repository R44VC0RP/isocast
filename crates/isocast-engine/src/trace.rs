//! Developer mode's frame timing traces for recording outputs.
//!
//! Each file being written has an [`OutputTrace`]: while developer mode is
//! on, its writer adds every video frame's timing to it (lock-free atomics,
//! no allocation); while off, a writer pays one relaxed atomic load per
//! frame ([`on`]). isocastd drains the counters once a second
//! ([`drain`]) into one JSONL record per output, so the trace's volume is
//! per output per second, not per frame.

use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};

use crate::Time;

/// Developer mode is on: trace. One relaxed atomic load.
#[inline]
pub fn on() -> bool {
    isocast_diagnostics::developer()
}

static CLOCK: OnceLock<fn() -> Time> = OnceLock::new();

/// The host clock frames are stamped on, set by the platform's process
/// (isocastd) once; without it, latencies from a frame's time are left out.
pub fn set_clock(now: fn() -> Time) {
    let _ = CLOCK.set(now);
}

fn now() -> Option<Time> {
    CLOCK.get().map(|now| now())
}

static OUTPUTS: Mutex<Vec<Weak<OutputTrace>>> = Mutex::new(Vec::new());
/// First frames of outputs that ended before a drain listed them (their
/// trace's drop moves them here), until the next drain.
static STARTS: Mutex<Vec<(u64, Value)>> = Mutex::new(Vec::new());
/// The order writers started their files in (the encoder start order).
static NEXT_START: AtomicU64 = AtomicU64::new(0);
/// The most first frames one drain lists.
const MAX_STARTS: usize = 64;

/// A file's first frame, as its writer started it.
struct Start {
    order: u64,
    pts_ns: i64,
    wall: SystemTime,
    age: Option<Duration>,
}

impl Start {
    fn record(&self, trace: &OutputTrace) -> (u64, Value) {
        let wall = chrono::DateTime::<chrono::Utc>::from(self.wall)
            .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
        (
            self.order,
            json!({"source":trace.source,"take":trace.take,"first_pts_ns":self.pts_ns,
                "wall":wall,"age_us":self.age.map(|a| a.as_micros() as u64)}),
        )
    }
}

/// One recording output's (file's) counters since the last drain.
pub struct OutputTrace {
    source: String,
    take: u32,
    fps: u32,
    frames: AtomicU64,
    /// Frames the encoder refused (dropped).
    dropped: AtomicU64,
    /// Frames that reached the writer more than two frame periods after
    /// their time (captured, or the canvas tick that drew them).
    late: AtomicU64,
    /// From a frame's time to its writer taking it: capture (or the
    /// program's tick) to encode submit, queues included. Nanoseconds.
    age_sum: AtomicU64,
    age_max: AtomicU64,
    /// Time in the encoder's append (submit, including waiting for a busy
    /// encoder). Nanoseconds.
    submit_sum: AtomicU64,
    submit_max: AtomicU64,
    /// Frames waiting inside the writer for the encoder, most seen.
    queue_max: AtomicU64,
    /// The first frame, set once by the writer (no lock, no allocation),
    /// and whether a drain has listed it.
    start: OnceLock<Start>,
    start_listed: AtomicBool,
}

impl OutputTrace {
    /// A file's trace, listed for [`drain`] until it is dropped.
    pub(crate) fn register(source: &str, take: u32, fps: u32) -> Arc<Self> {
        let trace = Arc::new(Self {
            source: source.to_string(),
            take,
            fps,
            frames: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            late: AtomicU64::new(0),
            age_sum: AtomicU64::new(0),
            age_max: AtomicU64::new(0),
            submit_sum: AtomicU64::new(0),
            submit_max: AtomicU64::new(0),
            queue_max: AtomicU64::new(0),
            start: OnceLock::new(),
            start_listed: AtomicBool::new(false),
        });
        let mut outputs = OUTPUTS.lock();
        outputs.retain(|o| o.strong_count() > 0);
        outputs.push(Arc::downgrade(&trace));
        trace
    }

    /// How old a frame stamped `pts` is now (call when the writer takes it).
    pub(crate) fn age(pts: Time) -> Option<Duration> {
        let now = now()?;
        Some(Duration::from_nanos(
            now.as_nanos().saturating_sub(pts.as_nanos()).max(0) as u64,
        ))
    }

    /// A frame the writer handled: its age when taken, the time its append
    /// took, whether the encoder took it, and the writer's queue after.
    pub(crate) fn frame(
        &self,
        age: Option<Duration>,
        submit: Duration,
        appended: bool,
        queue: usize,
    ) {
        let relaxed = Ordering::Relaxed;
        if appended {
            self.frames.fetch_add(1, relaxed);
        } else {
            self.dropped.fetch_add(1, relaxed);
        }
        if let Some(age) = age {
            let ns = age.as_nanos() as u64;
            self.age_sum.fetch_add(ns, relaxed);
            self.age_max.fetch_max(ns, relaxed);
            if ns > 2_000_000_000 / u64::from(self.fps.max(1)) {
                self.late.fetch_add(1, relaxed);
            }
        }
        let ns = submit.as_nanos() as u64;
        self.submit_sum.fetch_add(ns, relaxed);
        self.submit_max.fetch_max(ns, relaxed);
        self.queue_max.fetch_max(queue as u64, relaxed);
    }

    /// The writer started the file with its first frame, stamped `pts`.
    /// Lock-free and allocation-free on the writer's thread (#55): the
    /// record is built when [`drain`] lists it.
    pub(crate) fn started(&self, pts: Time) {
        let _ = self.start.set(Start {
            order: NEXT_START.fetch_add(1, Ordering::Relaxed),
            pts_ns: pts.as_nanos(),
            wall: SystemTime::now(),
            age: Self::age(pts),
        });
    }

    /// The first frame's record, once, if the writer has started.
    fn take_start(&self) -> Option<(u64, Value)> {
        let start = self.start.get()?;
        (!self.start_listed.swap(true, Ordering::Relaxed)).then(|| start.record(self))
    }

    fn take(&self) -> Value {
        let relaxed = Ordering::Relaxed;
        let frames = self.frames.swap(0, relaxed);
        let dropped = self.dropped.swap(0, relaxed);
        let handled = (frames + dropped).max(1) as f64;
        let us = |ns: u64| (ns as f64 / 100.0).round() / 10.0;
        json!({"source":self.source,"take":self.take,"fps":self.fps,"frames":frames,"dropped":dropped,
            "late":self.late.swap(0, relaxed),
            "age_us":{"mean":us((self.age_sum.swap(0, relaxed) as f64 / handled) as u64),"max":us(self.age_max.swap(0, relaxed))},
            "submit_us":{"mean":us((self.submit_sum.swap(0, relaxed) as f64 / handled) as u64),"max":us(self.submit_max.swap(0, relaxed))},
            "writer_queue_max":self.queue_max.swap(0, relaxed)})
    }
}

/// Every live output's counters since the last drain (and resets them),
/// and the outputs started since then in start order.
pub fn drain() -> (Vec<Value>, Vec<Value>) {
    let outputs: Vec<Arc<OutputTrace>> = {
        let mut outputs = OUTPUTS.lock();
        outputs.retain(|o| o.strong_count() > 0);
        outputs.iter().filter_map(Weak::upgrade).collect()
    };
    let records = outputs.iter().map(|o| o.take()).collect();
    let mut starts = std::mem::take(&mut *STARTS.lock());
    starts.extend(outputs.iter().filter_map(|o| o.take_start()));
    starts.sort_by_key(|(order, _)| *order);
    starts.truncate(MAX_STARTS);
    (
        records,
        starts.into_iter().map(|(_, start)| start).collect(),
    )
}

impl Drop for OutputTrace {
    /// A file that ended before a drain listed its start: keep the start
    /// for the next drain (at the file's end, off its frame path).
    fn drop(&mut self) {
        if let Some(start) = self.take_start() {
            let mut starts = STARTS.lock();
            if starts.len() < MAX_STARTS {
                starts.push(start);
            }
        }
    }
}
