//! Early warning for a loss of encoding headroom while our workload is stable.
//! macOS does not expose other applications' encoder sessions. A latency step
//! is evidence of contention, not proof of which process caused it.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use isocast_protocol::api::LoadWarning;
use objc2::rc::autoreleasepool;
use objc2_foundation::NSProcessInfo;

static ENCODERS: Mutex<Vec<Weak<EncodeHealth>>> = Mutex::new(Vec::new());
static WARNING: Mutex<Option<LoadWarning>> = Mutex::new(None);
static FAILURES: AtomicU64 = AtomicU64::new(0);
static HEADROOM: Mutex<Option<(Instant, isocast_engine::governor::Headroom)>> = Mutex::new(None);

pub fn headroom() -> Option<isocast_engine::governor::Headroom> {
    let value = HEADROOM.lock().unwrap_or_else(|e| e.into_inner());
    value.as_ref().map(|(at, sample)| {
        let mut sample = sample.clone();
        sample.age_ms = at.elapsed().as_secs_f64() * 1000.0;
        sample
    })
}

pub fn warning() -> Option<LoadWarning> {
    WARNING.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub(crate) fn session_failed(status: i32) {
    // -12908 also means descriptor exhaustion; callers exclude that first.
    if status == -12908 {
        FAILURES.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn monotonic_ns() -> u64 {
    let mut time = std::mem::MaybeUninit::<libc::timespec>::zeroed();
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC_RAW, time.as_mut_ptr()) } != 0 {
        return 0;
    }
    let time = unsafe { time.assume_init() };
    time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64
}

/// Callback-side counters only: no locks, allocations, or system sampling on
/// the encoder threads. The daemon samples them every 100 ms.
pub(crate) struct EncodeHealth {
    protected: AtomicBool,
    pending: AtomicU64,
    began: AtomicU64,
    completed: AtomicU64,
    peak_ns: AtomicU64,
    frame_ns: u64,
}

impl EncodeHealth {
    pub(crate) fn new(fps: u32, protected: bool) -> Arc<Self> {
        let health = Arc::new(Self {
            protected: AtomicBool::new(protected),
            pending: AtomicU64::new(0),
            began: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            peak_ns: AtomicU64::new(0),
            frame_ns: 1_000_000_000 / u64::from(fps.max(1)),
        });
        ENCODERS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::downgrade(&health));
        health
    }

    pub(crate) fn protect(&self, protected: bool) {
        self.protected.store(protected, Ordering::Relaxed);
    }

    pub(crate) fn submit(&self) -> u64 {
        let now = monotonic_ns();
        if self.pending.load(Ordering::Relaxed) == 0 {
            self.began.store(now, Ordering::Relaxed);
        }
        // Publish the timestamp before the pending picture. Otherwise a poll
        // at a parked encoder's first submission could see the old idle gap.
        self.pending.fetch_add(1, Ordering::Release);
        now
    }

    pub(crate) fn complete(&self, submitted: u64) {
        let now = monotonic_ns();
        self.completed.store(now, Ordering::Relaxed);
        if submitted != 0 {
            self.peak_ns
                .fetch_max(now.saturating_sub(submitted), Ordering::Relaxed);
        }
        self.pending.fetch_sub(1, Ordering::Release);
    }
}

struct Baseline {
    encoder: Weak<EncodeHealth>,
    started: bool,
    latency_ns: u64,
}

/// Called by the daemon's diagnostics worker, never from a capture thread.
pub struct LoadMonitor {
    encoders: Vec<Baseline>,
    changed: Instant,
    cpu_ticks: Option<[u32; 4]>,
    cpu_baseline: f64,
    failures: u64,
    threatened_since: Option<Instant>,
    quiet_since: Instant,
    last_announced: HashMap<&'static str, Instant>,
}

impl Default for LoadMonitor {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            encoders: Vec::new(),
            changed: now,
            cpu_ticks: None,
            cpu_baseline: 0.0,
            failures: 0,
            threatened_since: None,
            quiet_since: now,
            last_announced: HashMap::new(),
        }
    }
}

impl Drop for LoadMonitor {
    fn drop(&mut self) {
        *WARNING.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *HEADROOM.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

impl LoadMonitor {
    /// Returns a warning on an episode's rising edge, at most once per cause
    /// every three minutes. Status follows every episode, including a muted
    /// one, until five quiet seconds have elapsed. Splits reset neither limit.
    pub fn poll(&mut self) -> Option<LoadWarning> {
        let now = Instant::now();
        let clock = monotonic_ns();
        let encoders = {
            let mut registered = ENCODERS.lock().unwrap_or_else(|e| e.into_inner());
            registered.retain(|e| e.strong_count() > 0);
            registered
                .iter()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>()
        };
        let changed = self.encoders.len() != encoders.len()
            || self.encoders.iter().zip(&encoders).any(|(a, b)| {
                a.encoder.as_ptr() != Arc::as_ptr(b)
                    || a.started != (b.began.load(Ordering::Relaxed) != 0)
            });
        if changed {
            self.changed = now;
            self.cpu_baseline = 0.0;
            self.encoders = encoders
                .iter()
                .map(|e| Baseline {
                    encoder: Arc::downgrade(e),
                    started: e.began.load(Ordering::Relaxed) != 0,
                    latency_ns: e.frame_ns * 2,
                })
                .collect();
        }
        let settled = now.duration_since(self.changed);
        let ready = settled >= Duration::from_secs(5);
        let cpu = self.cpu();
        if !ready && let Some(cpu) = cpu {
            self.cpu_baseline = self.cpu_baseline.max(cpu);
        }
        let mut threatened = false;
        let mut latency_ns = 0;
        let mut pending = 0;
        let mut active = false;
        let mut recovery_ratio = 0.0f64;
        for (baseline, encoder) in self.encoders.iter_mut().zip(&encoders) {
            let peak = encoder.peak_ns.swap(0, Ordering::Relaxed);
            if !encoder.protected.load(Ordering::Relaxed) {
                continue;
            }
            let queued = encoder.pending.load(Ordering::Acquire);
            let completed = encoder.completed.load(Ordering::Relaxed);
            // A parked stream has no pending picture and no recent callback.
            active |= queued > 0
                || (completed != 0
                    && clock.saturating_sub(completed) < (encoder.frame_ns * 3).max(500_000_000));
            let stalled = if queued > 0 {
                clock.saturating_sub(completed.max(encoder.began.load(Ordering::Relaxed)))
            } else {
                0
            };
            let latency = peak.max(stalled);
            if !ready && settled >= Duration::from_secs(1) {
                baseline.latency_ns = baseline.latency_ns.max(latency);
            }
            // Both native encoders admit eight pictures. Normal ISO recovery
            // can fill that window even with unchanged work; a fast startup
            // baseline must not label this as another app's load (#90).
            let limit = (baseline.latency_ns * 3 / 2)
                .max(baseline.latency_ns + encoder.frame_ns * 2)
                .max(encoder.frame_ns * 8);
            recovery_ratio = recovery_ratio.max(latency as f64 / limit.max(1) as f64);
            threatened |= ready && latency > limit;
            latency_ns = latency_ns.max(latency);
            pending = pending.max(queued);
        }
        let thermal = autoreleasepool(|_| NSProcessInfo::processInfo().thermalState().0);
        let failures = FAILURES.load(Ordering::Relaxed);
        let failed = failures != self.failures;
        self.failures = failures;
        let sustained = if threatened {
            now.duration_since(*self.threatened_since.get_or_insert(now))
                >= Duration::from_millis(150)
        } else {
            self.threatened_since = None;
            false
        };
        let cause = if active && thermal >= 2 {
            Some((
                "thermal",
                "The Mac is hot; recording and streaming may lose frames",
            ))
        } else if active && ready && cpu.is_some_and(|c| c >= 90.0 && c > self.cpu_baseline + 15.0)
        {
            Some((
                "system_load",
                "The Mac is under heavy load; recording and streaming may lose frames",
            ))
        } else if active && (sustained || failed) {
            Some((
                "encoder_contention",
                "Video encoders slowed down; another app may be using them",
            ))
        } else {
            None
        };
        *HEADROOM.lock().unwrap_or_else(|e| e.into_inner()) = Some((
            now,
            isocast_engine::governor::Headroom {
                age_ms: 0.0,
                ready,
                // Allocation failures remain warnings. Thinning existing
                // files does not release an encoder session slot.
                encoder_threatened: sustained,
                pending_frames: pending,
                recovery_ratio,
                cpu_percent: cpu,
                thermal_state: thermal as u32,
            },
        ));
        let mut warning = WARNING.lock().unwrap_or_else(|e| e.into_inner());
        let mut event = None;
        if let Some((cause, message)) = cause {
            self.quiet_since = now;
            if warning.is_none() {
                let next = LoadWarning {
                    cause: cause.into(),
                    message: message.into(),
                    encode_latency_ms: latency_ns as f64 / 1e6,
                    pending_frames: pending,
                    system_cpu_percent: cpu,
                    thermal_state: thermal as u32,
                };
                *warning = Some(next.clone());
                // A five-second quiet gap ends the status episode, but must
                // not turn a flapping load into a stream of warning toasts.
                if self
                    .last_announced
                    .get(cause)
                    .is_none_or(|at| now.duration_since(*at) >= Duration::from_secs(180))
                {
                    self.last_announced.insert(cause, now);
                    event = Some(next);
                }
            }
        } else if !active || now.duration_since(self.quiet_since) >= Duration::from_secs(5) {
            *warning = None;
        }
        event
    }

    fn cpu(&mut self) -> Option<f64> {
        // Public Mach HOST_CPU_LOAD_INFO: user, system, idle, nice ticks.
        unsafe extern "C" {
            fn mach_host_self() -> u32;
            fn host_statistics(host: u32, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
        }
        let mut ticks = [0u32; 4];
        let mut count = 4;
        // Keep one host-port send right for the process, not one per sample.
        static HOST: OnceLock<u32> = OnceLock::new();
        let host = *HOST.get_or_init(|| unsafe { mach_host_self() });
        if unsafe { host_statistics(host, 3, ticks.as_mut_ptr().cast(), &mut count) } != 0
            || count != 4
        {
            self.cpu_ticks = None;
            return None;
        }
        let previous = self.cpu_ticks.replace(ticks)?;
        let delta: [u64; 4] =
            std::array::from_fn(|i| u64::from(ticks[i].wrapping_sub(previous[i])));
        let total = delta.iter().sum::<u64>();
        (total > 0).then(|| 100.0 * (total - delta[2]) as f64 / total as f64)
    }
}
