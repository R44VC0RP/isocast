//! The process's own CPU time and peak memory, for the frame-rate and
//! idle measurements (`--bench`, `--soak`) and the status bar.

use std::time::Duration;

/// CPU time used by this process so far (user and system, all threads).
#[cfg(unix)]
pub fn cpu_time() -> Duration {
    let usage = rusage();
    let time = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
    time(usage.ru_utime) + time(usage.ru_stime)
}

/// The most memory this process has had resident, in bytes.
#[cfg(unix)]
pub fn peak_memory() -> u64 {
    let max = rusage().ru_maxrss as u64;
    // Bytes on macOS, kilobytes elsewhere.
    if cfg!(target_vendor = "apple") {
        max
    } else {
        max * 1024
    }
}

#[cfg(unix)]
fn rusage() -> libc::rusage {
    // SAFETY: getrusage fills the struct it is given.
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        usage
    }
}

#[cfg(not(unix))]
pub fn cpu_time() -> Duration {
    Duration::ZERO
}

#[cfg(not(unix))]
pub fn peak_memory() -> u64 {
    0
}

/// CPU use between two readings, in percent of one core.
pub struct Meter {
    wall: std::time::Instant,
    cpu: Duration,
}

impl Meter {
    pub fn start() -> Self {
        Self {
            wall: std::time::Instant::now(),
            cpu: cpu_time(),
        }
    }

    /// Percent of one core since the last reading, and restart.
    pub fn lap(&mut self) -> f64 {
        let percent = self.percent();
        *self = Self::start();
        percent
    }

    pub fn percent(&self) -> f64 {
        let wall = self.wall.elapsed().as_secs_f64();
        if wall <= 0.0 {
            return 0.0;
        }
        (cpu_time() - self.cpu).as_secs_f64() / wall * 100.0
    }

    pub fn seconds(&self) -> f64 {
        self.wall.elapsed().as_secs_f64()
    }
}
