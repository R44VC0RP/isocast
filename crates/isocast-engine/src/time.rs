use std::ops::{Add, Sub};
use std::time::Duration;

const NANOS_PER_SEC: i128 = 1_000_000_000;

/// A point on the master clock (the host clock), in nanoseconds. Every frame
/// and audio buffer in a session is stamped on this one clock, which is what
/// lets separately written files line up exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Time(i64);

impl Time {
    pub const MAX: Time = Time(i64::MAX);

    pub const fn from_nanos(nanos: i64) -> Self {
        Self(nanos)
    }

    pub const fn as_nanos(self) -> i64 {
        self.0
    }

    /// The time `count` periods of `1/rate` seconds after `self`, computed
    /// without accumulating rounding error.
    pub fn add_ticks(self, count: u64, rate: u32) -> Self {
        Self(self.0 + (count as i128 * NANOS_PER_SEC / rate as i128) as i64)
    }

    /// Whole periods of `1/rate` seconds from `earlier` to `self`, rounded to
    /// the nearest period. Negative if `self` is earlier.
    pub fn ticks_since(self, earlier: Time, rate: u32) -> i64 {
        let scaled = (self.0 as i128 - earlier.0 as i128) * rate as i128;
        let half = NANOS_PER_SEC / 2;
        let rounded = if scaled >= 0 {
            (scaled + half) / NANOS_PER_SEC
        } else {
            (scaled - half) / NANOS_PER_SEC
        };
        rounded as i64
    }

    /// The time from `earlier` to `self`, or zero if `earlier` is later.
    pub fn saturating_since(self, earlier: Time) -> Duration {
        Duration::from_nanos(self.0.saturating_sub(earlier.0).max(0) as u64)
    }
}

impl Add<Duration> for Time {
    type Output = Time;

    fn add(self, rhs: Duration) -> Time {
        Time(self.0 + rhs.as_nanos() as i64)
    }
}

impl Sub<Duration> for Time {
    type Output = Time;

    fn sub(self, rhs: Duration) -> Time {
        Time(self.0 - rhs.as_nanos() as i64)
    }
}

/// Places page audio on the host clock. Start fresh for each document.
#[derive(Default)]
pub struct BrowserAudioStamper {
    // End of stamped audio and actual arrival of that same chunk.
    previous: Option<(Time, Time)>,
}

impl BrowserAudioStamper {
    /// Returns the first sample's time and any missing duration before it.
    pub fn stamp(&mut self, frames: usize, now: Time) -> (Time, Option<Duration>) {
        const RESYNC: Duration = Duration::from_millis(150);
        let duration =
            Duration::from_nanos(frames as u64 * 1_000_000_000 / u64::from(crate::SAMPLE_RATE));
        let made = now - duration;
        let (mut at, mut gap) = (made, None);
        if let Some((next, arrived)) = self.previous {
            // The bridge has samples but no page play/pause signal. If no
            // callback covered a whole resync window, this can be a paused
            // page: start a new run. Otherwise a forward snap means the
            // sample clock lost time while callbacks kept arriving (#203).
            if made.saturating_since(arrived) < RESYNC {
                if made.saturating_since(next) >= RESYNC {
                    gap = Some(made.saturating_since(next));
                } else if next.saturating_since(made) < RESYNC {
                    at = next;
                }
            }
        }
        self.previous = Some((at + duration, now));
        (at, gap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_audio_counts_accumulated_lag_but_not_a_page_resuming() {
        let mut clock = BrowserAudioStamper::default();
        let ms = |n: i64| Time::from_nanos(n * 1_000_000);
        let mut gaps = Vec::new();
        // Ten ms of sound delivered every 17 ms: after 22 intervals the
        // clock snaps forward by 154 ms, then stitches continuously again.
        for n in 0..30 {
            let (at, gap) = clock.stamp(480, ms(100 + n * 17));
            if let Some(gap) = gap {
                gaps.push((at, gap));
            }
        }
        assert_eq!(gaps, [(ms(464), Duration::from_millis(154))]);
        assert_eq!(clock.stamp(480, ms(900)), (ms(890), None));
        assert_eq!(clock.stamp(480, ms(910)), (ms(900), None));
        clock = Default::default();
        assert_eq!(clock.stamp(480, ms(920)), (ms(910), None));
    }

    #[test]
    fn ticks_round_trip_without_drift() {
        let start = Time::from_nanos(123_456_789);
        for rate in [24, 30, 60, 48_000, 60_000] {
            for count in [0, 1, 59, 60, 3_600 * 60, 10 * 3_600 * 48_000] {
                let later = start.add_ticks(count, rate);
                assert_eq!(
                    later.ticks_since(start, rate),
                    count as i64,
                    "{count} at {rate}"
                );
            }
        }
    }

    #[test]
    fn an_hour_of_ticks_is_exactly_an_hour() {
        let zero = Time::from_nanos(0);
        assert_eq!(zero.add_ticks(3_600 * 60, 60).as_nanos(), 3_600_000_000_000);
        assert_eq!(
            zero.add_ticks(3_600 * 48_000, 48_000).as_nanos(),
            3_600_000_000_000
        );
    }

    #[test]
    fn ticks_since_rounds_to_the_nearest_tick_both_ways() {
        let zero = Time::from_nanos(0);
        let half_frame = 1_000_000_000 / 60 / 2;
        assert_eq!(Time::from_nanos(half_frame - 1).ticks_since(zero, 60), 0);
        assert_eq!(Time::from_nanos(half_frame + 1).ticks_since(zero, 60), 1);
        assert_eq!(Time::from_nanos(-half_frame - 1).ticks_since(zero, 60), -1);
        assert_eq!(zero.saturating_since(Time::from_nanos(5)), Duration::ZERO);
    }
}
