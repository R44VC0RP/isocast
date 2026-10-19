use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow, bail};
use isocast_engine::Time;
use windows::Win32::Foundation::{HANDLE, WAIT_FAILED, WAIT_OBJECT_0};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, INFINITE, SetWaitableTimerEx,
    TIMER_ALL_ACCESS, WaitForSingleObject,
};
use windows::core::{Owned, PCWSTR};

/// Absolute QPC time, not time since the daemon started. WGC and WASAPI
/// timestamps use this same origin, including across process boundaries.
pub fn now() -> Time {
    let mut ticks = 0;
    unsafe { QueryPerformanceCounter(&mut ticks) }.expect("QPC unavailable");
    from_counter(ticks)
}

/// DXGI timestamps are raw QPC ticks, unlike WGC's 100-nanosecond units.
pub(crate) fn from_counter(ticks: i64) -> Time {
    static FREQUENCY: OnceLock<i64> = OnceLock::new();
    let frequency = *FREQUENCY.get_or_init(|| {
        let mut value = 0;
        // QPC is always available on supported Windows versions (Windows 10+).
        unsafe { QueryPerformanceFrequency(&mut value) }.expect("QPC frequency unavailable");
        assert!(value > 0, "QPC frequency must be positive");
        value
    });
    let nanos = i128::from(ticks) * 1_000_000_000 / i128::from(frequency);
    Time::from_nanos(nanos.clamp(0, i128::from(i64::MAX)) as i64)
}

pub(crate) fn from_100ns(ticks: i64) -> Time {
    Time::from_nanos(ticks.saturating_mul(100))
}

/// Wait for an absolute QPC deadline without spinning or raising the process
/// timer resolution. Each media thread owns one high-resolution, one-shot
/// timer, released when that thread exits. Windows 10 1803+ is required.
pub fn sleep_until(at: Time) -> Result<()> {
    thread_local! {
        static TIMER: std::result::Result<Owned<HANDLE>, String> = unsafe {
            CreateWaitableTimerExW(None, PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS.0)
                .map(|handle| Owned::new(handle))
                .map_err(|error| format!("create high-resolution media timer: {error}"))
        };
    }
    TIMER.with(|timer| {
        let timer = timer.as_ref().map_err(|error| anyhow!("{error}"))?;
        loop {
            let remaining = at.as_nanos().saturating_sub(now().as_nanos());
            if remaining <= 0 {
                return Ok(());
            }
            // A positive Windows due-time is UTC FILETIME, NOT QPC. Recompute
            // a relative interval from the absolute QPC deadline every tick;
            // round upward to 100 ns so quantization cannot request an early
            // wake. Re-arm if an OS wake is early, never busy-wait until `at`.
            let due = -(remaining / 100 + i64::from(remaining % 100 != 0));
            unsafe { SetWaitableTimerEx(**timer, &due, 0, None, None, None, 0) }?;
            let result = unsafe { WaitForSingleObject(**timer, INFINITE) };
            if result == WAIT_FAILED {
                return Err(windows::core::Error::from_thread())
                    .context("wait for high-resolution media timer");
            }
            if result != WAIT_OBJECT_0 {
                bail!("media timer wait returned unexpected status {}", result.0);
            }
        }
    })
}
