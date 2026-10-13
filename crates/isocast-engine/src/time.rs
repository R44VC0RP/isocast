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

#[cfg(test)]
mod tests {
    use super::*;

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
