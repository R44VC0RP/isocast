//! What the program's compositor costs per canvas tick, measured on its
//! own thread and reported in the engine's status.

use std::time::Duration;

use isocast_protocol::api::{CompositorInfo, Percentiles};

/// Ticks the CPU time percentiles are taken over: 10 s at 60 fps.
const WINDOW: usize = 600;

/// The compositor's recent ticks. Recording one never allocates.
pub(crate) struct CompositorTimings {
    draw: Samples,
    gpu_wait: Samples,
    /// CPU time of each recent tick that gave the program a frame, in
    /// nanoseconds; a ring of the last [`WINDOW`].
    cpu_ns: Box<[u32; WINDOW]>,
    /// How many of `cpu_ns` hold a time, and where the next one goes.
    len: usize,
    next: usize,
    reused: u64,
    draws: u32,
    animated: u32,
}

impl Default for CompositorTimings {
    fn default() -> Self {
        Self {
            draw: Samples::default(),
            gpu_wait: Samples::default(),
            cpu_ns: Box::new([0; WINDOW]),
            len: 0,
            next: 0,
            reused: 0,
            draws: 0,
            animated: 0,
        }
    }
}

impl CompositorTimings {
    pub(crate) fn draw_wall(&mut self, wall: Duration) {
        self.draw.push(wall);
    }

    pub(crate) fn gpu(&mut self, elapsed: Duration) {
        self.gpu_wait.push(elapsed);
    }

    /// A tick that gave the program a frame took `cpu` of the thread's
    /// time; `reused` if it sent the last frame again rather than drawing,
    /// else `draws` were drawn with `animated` properties evaluated.
    pub(crate) fn tick(&mut self, cpu: Duration, reused: bool, draws: u32, animated: u32) {
        self.cpu_ns[self.next] = cpu.as_nanos().min(u128::from(u32::MAX)) as u32;
        self.next = (self.next + 1) % WINDOW;
        self.len = (self.len + 1).min(WINDOW);
        if reused {
            self.reused += 1;
        } else {
            self.draws = draws;
            self.animated = animated;
        }
    }

    pub(crate) fn info(&self) -> CompositorInfo {
        let mut times: Vec<u32> = self.cpu_ns[..self.len].to_vec();
        times.sort_unstable();
        CompositorInfo {
            // Filled from the program's adaptive clock by Engine::info.
            wake_lateness_us: Percentiles::default(),
            draw_wall_us: self.draw.info(),
            gpu_wait_us: self.gpu_wait.info(),
            cpu_us: Percentiles {
                p50: percentile(&times, 0.50),
                p99: percentile(&times, 0.99),
            },
            reused: self.reused,
            draws: self.draws,
            animated: self.animated,
        }
    }
}

struct Samples {
    ns: Box<[u32; WINDOW]>,
    len: usize,
    next: usize,
}

impl Default for Samples {
    fn default() -> Self {
        Self {
            ns: Box::new([0; WINDOW]),
            len: 0,
            next: 0,
        }
    }
}

impl Samples {
    fn push(&mut self, time: Duration) {
        self.ns[self.next] = time.as_nanos().min(u128::from(u32::MAX)) as u32;
        self.next = (self.next + 1) % WINDOW;
        self.len = (self.len + 1).min(WINDOW);
    }

    fn info(&self) -> Percentiles {
        let mut times = self.ns[..self.len].to_vec();
        times.sort_unstable();
        Percentiles {
            p50: percentile(&times, 0.50),
            p99: percentile(&times, 0.99),
        }
    }
}

/// The `p` percentile (nearest rank) of sorted nanoseconds, in
/// microseconds rounded to 0.1; 0 if there are none.
fn percentile(sorted: &[u32], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (p * sorted.len() as f64).ceil() as usize;
    let ns = sorted[rank.clamp(1, sorted.len()) - 1];
    (f64::from(ns) / 100.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_cover_the_last_ticks_only() {
        let mut timings = CompositorTimings::default();
        assert_eq!(timings.info().cpu_us, Percentiles::default());
        // 1..=100 µs: the median is 50, the 99th percentile 99.
        for us in 1..=100 {
            timings.tick(Duration::from_micros(us), false, 3, 0);
        }
        let info = timings.info();
        assert_eq!((info.cpu_us.p50, info.cpu_us.p99), (50.0, 99.0));
        assert_eq!((info.reused, info.draws), (0, 3));
        // A full window of 7 µs ticks pushes them all out.
        for _ in 0..WINDOW {
            timings.tick(Duration::from_micros(7), true, 0, 0);
        }
        let info = timings.info();
        assert_eq!((info.cpu_us.p50, info.cpu_us.p99), (7.0, 7.0));
        assert_eq!((info.reused, info.draws), (WINDOW as u64, 3));
    }
}
