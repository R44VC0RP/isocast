//! The interface's clock. Normally the wall clock; the motion harness
//! (src/motion) switches it to the virtual display's, so everything the
//! interface times (edit pacing, gesture tails, held frames, the LCD,
//! meters) moves with the frames it draws and a run doesn't depend on how
//! fast the machine is. Slint's own timers follow the platform's clock,
//! which the harness drives the same way.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

static VIRTUAL: AtomicBool = AtomicBool::new(false);
static BASE: OnceLock<Instant> = OnceLock::new();
static OFFSET_NS: AtomicU64 = AtomicU64::new(0);

/// Now, as the interface sees it.
pub fn now() -> Instant {
    if VIRTUAL.load(Ordering::Relaxed) {
        *BASE.get_or_init(Instant::now) + Duration::from_nanos(OFFSET_NS.load(Ordering::Relaxed))
    } else {
        Instant::now()
    }
}

/// How long since `then`, on the interface's clock.
pub fn since(then: Instant) -> Duration {
    now().saturating_duration_since(then)
}

/// The harness: from now on the clock reads `at` past a fixed start.
pub fn set_virtual(at: Duration) {
    BASE.get_or_init(Instant::now);
    OFFSET_NS.store(at.as_nanos() as u64, Ordering::Relaxed);
    VIRTUAL.store(true, Ordering::Relaxed);
}
