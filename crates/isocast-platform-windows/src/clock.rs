use std::sync::OnceLock;

use isocast_engine::Time;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

/// Absolute QPC time, not time since the daemon started. WGC and WASAPI
/// timestamps use this same origin, including across process boundaries.
pub fn now() -> Time {
    static FREQUENCY: OnceLock<i64> = OnceLock::new();
    let frequency = *FREQUENCY.get_or_init(|| {
        let mut value = 0;
        // QPC is always available on supported Windows versions (Windows 10+).
        unsafe { QueryPerformanceFrequency(&mut value) }.expect("QPC frequency unavailable");
        assert!(value > 0, "QPC frequency must be positive");
        value
    });
    let mut ticks = 0;
    unsafe { QueryPerformanceCounter(&mut ticks) }.expect("QPC unavailable");
    let nanos = i128::from(ticks) * 1_000_000_000 / i128::from(frequency);
    Time::from_nanos(nanos.clamp(0, i128::from(i64::MAX)) as i64)
}

pub(crate) fn from_100ns(ticks: i64) -> Time {
    Time::from_nanos(ticks.saturating_mul(100))
}
