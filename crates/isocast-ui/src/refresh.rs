//! The display's refresh while the editor plays (#94): a CVDisplayLink
//! asks the editor window for a frame at every refresh, and says when that
//! frame will be on screen, so each frame shows the picture (and the sound
//! plays the moment) due at that time. Pacing from a timer drifted against
//! the display (a 30 fps picture held 1, 2 or 3 refreshes). Dropping a
//! [`Refresh`] stops it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

/// What the display link's thread and the window's share.
#[derive(Default)]
struct Shared {
    /// The host time (mach ticks) the next frame shows at.
    output: AtomicU64,
    /// A frame was asked for and not drawn yet: don't ask again.
    asked: AtomicBool,
}

pub struct Refresh {
    shared: Arc<Shared>,
    /// When a frame was last drawn (the window's thread only).
    drawn: std::cell::Cell<Instant>,
    #[cfg(target_os = "macos")]
    _link: mac::Link,
}

impl Refresh {
    /// Call `ask` (on the display link's thread) at every refresh while a
    /// frame isn't already asked for; none if there is no display link.
    pub fn start(ask: impl Fn() + Send + Sync + 'static) -> Option<Self> {
        let shared = Arc::new(Shared::default());
        #[cfg(target_os = "macos")]
        {
            let tick = {
                let shared = shared.clone();
                move |output: u64| {
                    shared.output.store(output, Ordering::Relaxed);
                    if !shared.asked.swap(true, Ordering::AcqRel) {
                        ask();
                    }
                }
            };
            let link = mac::Link::start(Box::new(tick))?;
            Some(Self {
                shared,
                drawn: std::cell::Cell::new(Instant::now()),
                _link: link,
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (shared, ask);
            None
        }
    }

    /// A frame is being drawn: when it will be on screen (now, if the link
    /// hasn't said yet). Asking for the next frame is allowed again.
    pub fn drawing(&self) -> Instant {
        self.drawn.set(Instant::now());
        self.shared.asked.store(false, Ordering::Release);
        let output = self.shared.output.load(Ordering::Relaxed);
        let now = Instant::now();
        #[cfg(target_os = "macos")]
        if output != 0 {
            return mac::instant_of(output, now);
        }
        let _ = output;
        now
    }
}

impl Refresh {
    /// No frame was drawn for `limit` though one was asked for (the window
    /// is minimised or covered, or the redraw was dropped): ask again from
    /// the next refresh on. True if so; the caller steps without drawing.
    pub fn stalled(&self, limit: std::time::Duration) -> bool {
        if self.drawn.get().elapsed() < limit {
            return false;
        }
        self.drawn.set(Instant::now());
        self.shared.asked.store(false, Ordering::Release);
        true
    }
}

#[cfg(target_os = "macos")]
pub mod mac {
    use std::ffi::c_void;
    use std::ptr;
    use std::time::{Duration, Instant};

    /// The start of CoreVideo's CVTimeStamp (the fields read).
    #[repr(C)]
    struct TimeStamp {
        version: u32,
        video_time_scale: i32,
        video_time: i64,
        host_time: u64,
    }

    type Callback = unsafe extern "C" fn(
        link: *mut c_void,
        now: *const TimeStamp,
        output: *const TimeStamp,
        flags_in: u64,
        flags_out: *mut u64,
        context: *mut c_void,
    ) -> i32;

    #[link(name = "CoreVideo", kind = "framework")]
    unsafe extern "C" {
        fn CVDisplayLinkCreateWithActiveCGDisplays(link: *mut *mut c_void) -> i32;
        fn CVDisplayLinkSetOutputCallback(
            link: *mut c_void,
            callback: Callback,
            context: *mut c_void,
        ) -> i32;
        fn CVDisplayLinkStart(link: *mut c_void) -> i32;
        fn CVDisplayLinkStop(link: *mut c_void) -> i32;
        fn CVDisplayLinkRelease(link: *mut c_void);
    }

    /// mach_timebase_info_data_t.
    #[repr(C)]
    struct Timebase {
        numer: u32,
        denom: u32,
    }

    unsafe extern "C" {
        fn mach_absolute_time() -> u64;
        fn mach_timebase_info(info: *mut Timebase) -> i32;
    }

    /// The host clock now, in its ticks, and the ticks' (numer, denom) in
    /// nanoseconds.
    pub fn host_clock() -> (u64, u128, u128) {
        let mut base = Timebase { numer: 0, denom: 0 };
        // SAFETY: fills `base`; a pure clock read.
        let now = unsafe {
            mach_timebase_info(&mut base);
            mach_absolute_time()
        };
        (
            now,
            u128::from(base.numer.max(1)),
            u128::from(base.denom.max(1)),
        )
    }

    /// Host clock ticks in `duration`.
    pub fn ticks(duration: Duration) -> u64 {
        let (_, numer, denom) = host_clock();
        (duration.as_nanos() * denom / numer) as u64
    }

    type Tick = Box<dyn Fn(u64) + Send + Sync>;

    /// A running display link; dropping it stops it before its callback's
    /// state goes.
    pub struct Link {
        link: *mut c_void,
        tick: *mut Tick,
    }

    impl Link {
        pub fn start(tick: Tick) -> Option<Self> {
            let mut link = ptr::null_mut();
            // SAFETY: CoreVideo's documented calls; the context outlives
            // the link (it is freed after the link stops, in drop).
            unsafe {
                if CVDisplayLinkCreateWithActiveCGDisplays(&mut link) != 0 || link.is_null() {
                    return None;
                }
                let tick = Box::into_raw(Box::new(tick));
                let this = Self { link, tick };
                if CVDisplayLinkSetOutputCallback(link, callback, tick.cast()) != 0
                    || CVDisplayLinkStart(link) != 0
                {
                    return None;
                }
                Some(this)
            }
        }
    }

    impl Drop for Link {
        fn drop(&mut self) {
            // SAFETY: stopping waits for a callback in progress; then
            // nothing uses the context.
            unsafe {
                CVDisplayLinkStop(self.link);
                CVDisplayLinkRelease(self.link);
                drop(Box::from_raw(self.tick));
            }
        }
    }

    unsafe extern "C" fn callback(
        _link: *mut c_void,
        _now: *const TimeStamp,
        output: *const TimeStamp,
        _flags_in: u64,
        _flags_out: *mut u64,
        context: *mut c_void,
    ) -> i32 {
        // SAFETY: the context is the link's boxed tick (see `start`);
        // `output` is CoreVideo's valid timestamp.
        let (tick, output) = unsafe { (&*context.cast::<Tick>(), (*output).host_time) };
        // A panic must not unwind into CoreVideo.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tick(output)));
        0
    }

    /// The instant of host time `host` (mach ticks), with `now` as now.
    pub fn instant_of(host: u64, now: Instant) -> Instant {
        let (current, numer, denom) = host_clock();
        let nanos = |ticks: u64| Duration::from_nanos((u128::from(ticks) * numer / denom) as u64);
        match host >= current {
            true => now + nanos(host - current),
            false => now.checked_sub(nanos(current - host)).unwrap_or(now),
        }
    }
}
