//! How the disk holding a recording keeps up: the rate its files reach it,
//! and how long a small test write takes there now compared with when the
//! take started. Drop warnings use it to say when the disk is the cause.

use parking_lot::{Condvar, Mutex};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use isocast_protocol::api::LiveDisk;

use super::FilePlan;
use crate::{Platform, Time};

/// The scratch file test writes go to, in the recording's folder.
const PROBE_FILE: &str = ".isocast-disk-probe";
/// Test writes per measurement; the median counts, as single writes vary.
const PROBES: usize = 3;
/// The least time between measurements while frames are being dropped.
const PROBE_INTERVAL: Duration = Duration::from_secs(2);
/// How far back the files' write rate is measured.
const RATE_WINDOW: Duration = Duration::from_secs(5);

/// Test writes in the recording's folder, on a thread of their own (a slow
/// disk may take seconds to answer): one when the take starts, then more
/// only when asked, while frames are being dropped.
pub(super) struct DiskProbe {
    state: Arc<(Mutex<ProbeState>, Condvar)>,
}

#[derive(Default)]
struct ProbeState {
    at_start: Option<Duration>,
    latest: Option<Duration>,
    wanted: bool,
    done: bool,
}

impl DiskProbe {
    pub(super) fn start<P: Platform>(platform: &P, folder: &Path) -> Self {
        let state = Arc::new((Mutex::new(ProbeState::default()), Condvar::new()));
        let platform = platform.clone();
        let path = folder.join(PROBE_FILE);
        let shared = state.clone();
        // Detached, so a write that never returns (a network share gone
        // away) can't hold up the end of the recording.
        let failed = state.clone();
        let failed_path = path.clone();
        let _ = isocast_diagnostics::spawn(
            "disk probe",
            move || probe(&platform, &path, &shared),
            move |_| {
                *failed.0.lock() = ProbeState {
                    done: true,
                    ..Default::default()
                };
                failed.1.notify_all();
                let _ = std::fs::remove_file(failed_path);
            },
        );
        Self { state }
    }

    /// Measure again (at most every [`PROBE_INTERVAL`]).
    pub(super) fn want(&self) {
        let (state, wake) = &*self.state;
        state.lock().wanted = true;
        wake.notify_one();
    }

    /// The test write times at the start and lately, in ms.
    fn latencies(&self) -> (Option<f64>, Option<f64>) {
        let state = self.state.0.lock();
        let ms = |d: Option<Duration>| d.map(|d| d.as_secs_f64() * 1000.0);
        (ms(state.at_start), ms(state.latest))
    }
}

impl Drop for DiskProbe {
    fn drop(&mut self) {
        let (state, wake) = &*self.state;
        state.lock().done = true;
        wake.notify_one();
    }
}

fn probe<P: Platform>(platform: &P, path: &PathBuf, shared: &(Mutex<ProbeState>, Condvar)) {
    let measure = || {
        let mut times: Vec<Duration> = (0..PROBES)
            .filter_map(|_| platform.write_latency(path))
            .collect();
        times.sort();
        times.get(times.len() / 2).copied()
    };
    let (state, wake) = shared;
    let initial = measure();
    state.lock().at_start = initial;
    loop {
        {
            let mut guard = state.lock();
            while !guard.wanted && !guard.done {
                wake.wait(&mut guard);
            }
            if guard.done {
                break;
            }
            guard.wanted = false;
        }
        let latest = measure();
        state.lock().latest = latest;
        thread::sleep(PROBE_INTERVAL);
    }
    let _ = std::fs::remove_file(path);
}

/// The take's files' sizes over the last [`RATE_WINDOW`].
#[derive(Default)]
pub(super) struct DiskMeter {
    samples: VecDeque<(Time, u64)>,
}

impl DiskMeter {
    /// How the disk is keeping up with `plans` at `now`.
    pub(super) fn sample(
        &mut self,
        plans: &[Arc<FilePlan>],
        probe: &DiskProbe,
        now: Time,
    ) -> LiveDisk {
        let bytes: u64 = plans
            .iter()
            .filter(|plan| plan.start.load(Ordering::Relaxed) != i64::MIN)
            .filter_map(|plan| std::fs::metadata(&plan.spec.path).ok())
            .map(|meta| meta.len())
            .sum();
        // A new take's files start small: start over.
        if self.samples.back().is_some_and(|&(_, last)| bytes < last) {
            self.samples.clear();
        }
        self.samples.push_back((now, bytes));
        while self.samples.len() > 2 && now.saturating_since(self.samples[1].0) >= RATE_WINDOW {
            self.samples.pop_front();
        }
        let (then, before) = self.samples[0];
        let seconds = now.saturating_since(then).as_secs_f64();
        let (latency_at_start_ms, latency_ms) = probe.latencies();
        LiveDisk {
            write_bytes_per_second: if seconds > 0.0 {
                (bytes - before) as f64 / seconds
            } else {
                0.0
            },
            latency_at_start_ms,
            latency_ms,
        }
    }
}
