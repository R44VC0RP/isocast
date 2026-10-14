//! Host-clock media deadlines, independent of launchd's timer slack.
//!
//! QoS alone does not override a daemon/background task's coalescing (a
//! 10 ms nanosleep can take 80/200 ms). A critical, absolute kqueue timer
//! bounds coalescing without spinning or raising thread priority. A little
//! explicit leeway retains power-saving wakeup batching on ordinary launches.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};

use isocast_engine::Time;
use objc2_core_media::{CMClock, CMTime};

thread_local! {
    // A queue per pacing thread, reused each tick and closed on thread exit.
    static TIMER: Option<OwnedFd> = {
        // SAFETY: creates a new descriptor owned only by this thread.
        let fd = unsafe { libc::kqueue() };
        (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) })
    };
}

pub(super) fn sleep_until(at: Time) {
    let now = super::host_time();
    if at <= now {
        return;
    }
    // SAFETY: a finite nonnegative host-clock time. CoreMedia converts back
    // to the same Mach absolute epoch used by NOTE_MACHTIME, including its
    // platform-specific timebase (not wall or continuous time).
    let deadline = unsafe {
        CMClock::convert_host_time_to_system_units(CMTime::new(at.as_nanos().max(0), 1_000_000_000))
    }
    .min(i64::MAX as u64) as i64;
    let leeway = (at.saturating_since(now) / 4).min(std::time::Duration::from_millis(5));
    // SAFETY: converting a finite duration to the timer's native units.
    let leeway = unsafe {
        CMClock::convert_host_time_to_system_units(CMTime::new(
            leeway.as_nanos() as i64,
            1_000_000_000,
        ))
    };
    let ok = TIMER.with(|timer| {
        let Some(timer) = timer else { return false };
        let change = libc::kevent64_s {
            ident: 1,
            filter: libc::EVFILT_TIMER,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            fflags: libc::NOTE_ABSOLUTE
                | libc::NOTE_MACHTIME
                | libc::NOTE_CRITICAL
                | libc::NOTE_LEEWAY,
            data: deadline,
            udata: 0,
            ext: [0, leeway],
        };
        loop {
            let mut event = change;
            // SAFETY: valid one-element input/output arrays, a thread-local
            // descriptor and no timeout. EV_ONESHOT removes the fired timer.
            let count = unsafe {
                libc::kevent64(
                    timer.as_raw_fd(),
                    &change,
                    1,
                    &mut event,
                    1,
                    0,
                    std::ptr::null(),
                )
            };
            if count == 1 {
                return event.flags & libc::EV_ERROR == 0;
            }
            if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                return false;
            }
            // A signal must not restart a relative sleep and move the frame
            // deadline; re-arm the same absolute deadline instead.
        }
    });
    if !ok {
        static WARNED: AtomicBool = AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::Relaxed) {
            eprintln!("media deadline timer unavailable; falling back to coalescible sleeps");
        }
        std::thread::sleep(at.saturating_since(super::host_time()));
    }
}
