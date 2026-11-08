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

/// How a chunk of browser audio relates to the chunk before it.
///
/// `end_us` is the page's Unix wall-clock time (`Date.now()` in microseconds)
/// when the chunk finished buffering plus the output-latency compensation
/// sampled at the start of the run, not the audio hardware render clock.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum BrowserAudioTiming {
    /// Host arrival time only.
    #[default]
    Arrival,
    /// Continuous output from an active tap, ending at `end_us` (Unix epoch µs).
    Continuous { end_us: u64 },
    /// First output after an intentional tap pause or handoff, ending at `end_us`.
    Resumed { end_us: u64 },
}

#[derive(Clone, Copy)]
struct PageClock {
    origin_us: u64,
    origin_host: Time,
    last_us: u64,
    // Tumbling 64-chunk min-filter window on `wall_us - end_us` so NTP slew
    // cannot accumulate across long takes.
    window_count: u8,
    window_best: Option<(u64, Time, i64)>,
}

impl PageClock {
    fn new(end_us: u64, host_end: Time) -> Self {
        Self {
            origin_us: end_us,
            origin_host: host_end,
            last_us: end_us,
            window_count: 0,
            window_best: None,
        }
    }

    fn reset(&mut self, end_us: u64, host_end: Time) {
        self.origin_us = end_us;
        self.origin_host = host_end;
        self.window_count = 0;
        self.window_best = None;
    }

    fn observe_wall(&mut self, end_us: u64, wall_us: Option<u64>, now: Time) {
        const WINDOW: u8 = 64;
        let Some((anchored, signed_us)) = wall_anchor(end_us, wall_us, now) else {
            return;
        };
        if self
            .window_best
            .is_none_or(|(_, _, best_signed)| signed_us < best_signed)
        {
            self.window_best = Some((end_us, anchored, signed_us));
        }
        self.window_count += 1;
        if self.window_count >= WINDOW {
            if let Some((best_us, best_host, _)) = self.window_best.take() {
                self.origin_us = best_us;
                self.origin_host = best_host;
            }
            self.window_count = 0;
        }
    }
}

/// Places page audio on the host clock. Start fresh for each document.
#[derive(Default)]
pub struct BrowserAudioStamper {
    // End of stamped audio and actual arrival of that same chunk.
    previous: Option<(Time, Time)>,
    // Anchor on the page's wall clock (`Date.now()` + output latency in µs) and host clock.
    source: Option<PageClock>,
}

const MAX_OUTPUT_LATENCY: Duration = Duration::from_millis(250);

fn wall_anchor(end_us: u64, wall_us: Option<u64>, now: Time) -> Option<(Time, i64)> {
    const MAX_WALL_LAG_US: i128 = 2_000_000;
    let max_lead_us = MAX_OUTPUT_LATENCY.as_micros() as i128;
    let wall = wall_us?;
    let signed_us = wall as i128 - end_us as i128;
    if !(-max_lead_us..=MAX_WALL_LAG_US).contains(&signed_us) {
        return None;
    }
    let anchored = if signed_us >= 0 {
        now - Duration::from_micros(signed_us as u64)
    } else {
        now + Duration::from_micros((-signed_us) as u64)
    };
    Some((anchored, signed_us as i64))
}

fn anchor_end(end_us: u64, wall_us: Option<u64>, now: Time) -> Time {
    wall_anchor(end_us, wall_us, now).map_or(now, |(anchored, _)| anchored)
}

impl BrowserAudioStamper {
    /// Returns the first sample's time and any missing duration before it.
    pub fn stamp(&mut self, frames: usize, now: Time) -> (Time, Option<Duration>) {
        self.stamp_timed(frames, BrowserAudioTiming::Arrival, None, now)
    }

    /// Places a chunk using the page's wall clock + run-start output latency
    /// when present (`audio.js` samples output-latency compensation once at
    /// the start of each continuous run), keeping `source_end <= now + MAX_OUTPUT_LATENCY`
    /// and chunk starts within [`crate::record::mix::MIX_DELAY`] while
    /// reporting stalls past the mixer's jitter budget as gaps.
    pub fn stamp_timed(
        &mut self,
        frames: usize,
        timing: BrowserAudioTiming,
        wall_us: Option<u64>,
        now: Time,
    ) -> (Time, Option<Duration>) {
        const RESYNC: Duration = Duration::from_millis(150);
        // Below MIX_DELAY (200 ms) so late samples never miss the program mix
        // silently; anything older is clamped forward and reported as a gap.
        const MAX_LAG: Duration = Duration::from_millis(195);
        const MAX_PAUSE: Duration = Duration::from_secs(1);
        const MAX_SPAN: Duration = Duration::from_secs(86_400);
        let duration =
            Duration::from_nanos(frames as u64 * 1_000_000_000 / u64::from(crate::SAMPLE_RATE));
        let made = now - duration;
        let min_at = now - MAX_LAG;
        match timing {
            BrowserAudioTiming::Continuous { end_us } | BrowserAudioTiming::Resumed { end_us } => {
                // Only allow `source_end > now` to the extent this chunk's own
                // `end_us` is ahead of host `wall_us` (output-latency compensation
                // minus IPC delay, plus 5 ms for `Date.now()` rounding), so a prior
                // `min_at` clamp on a stale pre-stall chunk cannot push post-stall
                // audio up to `MAX_OUTPUT_LATENCY` into the future.
                let max_end = match wall_us {
                    Some(w) if end_us > w => {
                        let ahead = Duration::from_micros((end_us - w).saturating_add(5_000));
                        now + ahead.min(MAX_OUTPUT_LATENCY)
                    }
                    Some(w) if w.saturating_sub(end_us) < 5_000 => {
                        let ahead = Duration::from_micros(5_000 - w.saturating_sub(end_us));
                        now + ahead.min(MAX_OUTPUT_LATENCY)
                    }
                    _ => now,
                };
                let max_at = max_end - duration;
                let Some((next, _)) = self.previous else {
                    let desired_end = anchor_end(end_us, wall_us, now);
                    let at = (desired_end - duration).max(min_at).min(max_at);
                    let end = at + duration;
                    self.previous = Some((end, now));
                    self.source = Some(PageClock::new(end_us, end));
                    return (at, None);
                };
                if let Some(mut clock) = self.source.filter(|c| end_us >= c.last_us) {
                    let step = Duration::from_micros(end_us - clock.last_us).min(MAX_SPAN);
                    clock.last_us = end_us;
                    if matches!(timing, BrowserAudioTiming::Continuous { .. }) {
                        clock.observe_wall(end_us, wall_us, now);
                    }
                    let total = Duration::from_micros(end_us - clock.origin_us).min(MAX_SPAN);
                    if matches!(timing, BrowserAudioTiming::Resumed { .. }) {
                        let pause = step.saturating_sub(duration);
                        let source_end = if pause <= MAX_PAUSE && total < MAX_SPAN {
                            let raw = clock.origin_host + total;
                            if raw > max_end {
                                clock.origin_host = max_end - total;
                                max_end
                            } else {
                                raw
                            }
                        } else {
                            let anchored = anchor_end(end_us, wall_us, now);
                            clock.reset(end_us, anchored);
                            anchored
                        };
                        let desired_at = (source_end - duration).max(next.min(max_at));
                        let (at, gap) = if desired_at < min_at {
                            let at = min_at.max(next.min(max_at));
                            let end = (at + duration).min(max_end);
                            clock.reset(end_us, end);
                            (
                                at,
                                Some(at.saturating_since(desired_at)).filter(|d| !d.is_zero()),
                            )
                        } else {
                            (desired_at.min(max_at), None)
                        };
                        let end = at + duration;
                        self.previous = Some((end, now));
                        self.source = Some(clock);
                        return (at, gap);
                    }
                    let source_end = if total < MAX_SPAN {
                        let raw = clock.origin_host + total;
                        if raw > max_end {
                            clock.origin_host = max_end - total;
                            max_end
                        } else {
                            raw
                        }
                    } else {
                        clock.reset(end_us, now);
                        now
                    };
                    let desired_at = source_end - duration;
                    let (mut at, mut gap) = (desired_at, None);
                    if desired_at < min_at {
                        at = min_at.max(next.min(max_at));
                        gap = Some(at.saturating_since(next)).filter(|d| !d.is_zero());
                        let end = (at + duration).min(max_end);
                        clock.reset(end_us, end);
                    } else if desired_at.saturating_since(next) >= RESYNC {
                        gap = Some(desired_at.saturating_since(next));
                    } else if next.saturating_since(desired_at) < RESYNC {
                        // Keep continuous chunks stitched, while preventing a
                        // > MIX_DELAY IPC burst from marching `next` up to
                        // `RESYNC` (150 ms) past `max_end` and locking all
                        // subsequent audio several frames late.
                        at = next.min(max_at + Duration::from_millis(8));
                    }
                    let end = at + duration;
                    self.previous = Some((end, now));
                    self.source = Some(clock);
                    return (at, gap);
                }
                let anchored = anchor_end(end_us, wall_us, now);
                self.source = Some(PageClock::new(end_us, anchored));
            }
            BrowserAudioTiming::Arrival => {
                self.source = None;
            }
        }
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
    fn timed_browser_audio_keeps_bursts_whole_and_preserves_short_pauses() {
        let mut clock = BrowserAudioStamper::default();
        let ms = |n: i64| Time::from_nanos(n * 1_000_000);
        // Startup buffered 110 ms ahead of arrival: wall-clock age places the
        // first 10 ms chunk at ms(0), not ms(110), so the take has zero late shift.
        assert_eq!(
            clock.stamp_timed(
                480,
                BrowserAudioTiming::Continuous { end_us: 10_000 },
                Some(120_000),
                ms(120)
            ),
            (ms(0), None)
        );
        for n in 1..=10 {
            assert_eq!(
                clock.stamp_timed(
                    480,
                    BrowserAudioTiming::Continuous {
                        end_us: (n as u64 + 1) * 10_000
                    },
                    Some(120_000),
                    ms(120)
                ),
                (ms(n * 10), None)
            );
        }
        // A 75 ms suspend followed by 10 ms of resumed sound keeps the 75 ms hole
        // (110 -> 185 ms) with end <= now.
        assert_eq!(
            clock.stamp_timed(
                480,
                BrowserAudioTiming::Resumed { end_us: 195_000 },
                Some(197_000),
                ms(197)
            ),
            (ms(185), None)
        );
        // A 160 ms IPC stall (first sample 170 ms old, within MAX_LAG 195 ms < MIX_DELAY 200 ms)
        // delivers 16 continuous 10 ms chunks at once with zero holes and zero dropouts.
        for n in 0..16 {
            assert_eq!(
                clock.stamp_timed(
                    480,
                    BrowserAudioTiming::Continuous {
                        end_us: 205_000 + n as u64 * 10_000
                    },
                    Some(365_000),
                    ms(365)
                ),
                (ms(195 + n * 10), None)
            );
        }
        // A 150 ms post-resume IPC stall (within MAX_LAG) still places a 75 ms handoff hole at 75 ms.
        assert_eq!(
            clock.stamp_timed(
                480,
                BrowserAudioTiming::Resumed { end_us: 440_000 },
                Some(590_000),
                ms(590)
            ),
            (ms(430), None)
        );
        // A 260 ms Continuous stall (> MAX_LAG 195 ms) clamps to now - 195 ms and reports
        // the excess lag once instead of silently missing MIX_DELAY.
        assert_eq!(
            clock.stamp_timed(
                480,
                BrowserAudioTiming::Continuous { end_us: 450_000 },
                Some(700_000),
                ms(700)
            ),
            (ms(505), Some(Duration::from_millis(65)))
        );
        // A 260 ms Resumed stall past a 75 ms pause also clamps to now - 195 ms and reports
        // only the excess lag beyond the intentional pause (750 - 670 = 80 ms).
        assert_eq!(
            clock.stamp_timed(
                480,
                BrowserAudioTiming::Resumed { end_us: 615_000 },
                Some(945_000),
                ms(945)
            ),
            (ms(750), Some(Duration::from_millis(80)))
        );
        // Lost quanta during a continuous run report one gap; u64::MAX never overflows
        // on either Continuous or Resumed and never places samples after now.
        let mut loss = BrowserAudioStamper::default();
        let mut gaps = Vec::new();
        for n in 0..22 {
            let (_, gap) = loss.stamp_timed(
                480,
                BrowserAudioTiming::Continuous {
                    end_us: (n + 1) * 18_000,
                },
                None,
                ms(100 + n as i64 * 18),
            );
            if let Some(gap) = gap {
                gaps.push(gap);
            }
        }
        assert_eq!(gaps, [Duration::from_millis(152)]);
        assert_eq!(
            loss.stamp_timed(
                480,
                BrowserAudioTiming::Continuous { end_us: u64::MAX },
                None,
                ms(650),
            ),
            (ms(640), Some(Duration::from_millis(178)))
        );
        let mut overflow = BrowserAudioStamper::default();
        let _ = overflow.stamp_timed(
            480,
            BrowserAudioTiming::Continuous { end_us: 10 },
            None,
            ms(20),
        );
        assert_eq!(
            overflow.stamp_timed(
                480,
                BrowserAudioTiming::Resumed { end_us: u64::MAX },
                None,
                ms(600),
            ),
            (ms(590), None)
        );
        // 3,050 chunks of 1024 frames (65.07 s at 21.333... ms/chunk) with
        // millisecond-quantized Date.now() end_us values must not ratchet or report gaps.
        let mut long = BrowserAudioStamper::default();
        let start = Time::from_nanos(1_000_000_000);
        for k in 1..=3_050u64 {
            let exact_ns = k * 1_024 * 1_000_000_000 / 48_000;
            let end_ms = exact_ns / 1_000_000;
            let end_us = (1_700_000_000_000 + end_ms) * 1_000;
            let arrival = start + Duration::from_nanos(exact_ns + 1_500_000);
            let wall_us = 1_700_000_000_000_000 + (exact_ns + 1_500_000) / 1_000;
            let (at, gap) = long.stamp_timed(
                1_024,
                BrowserAudioTiming::Continuous { end_us },
                Some(wall_us),
                arrival,
            );
            assert_eq!(gap, None, "chunk {k} reported gap at {at:?}");
            assert!(
                at + Duration::from_nanos(1_024 * 1_000_000_000 / 48_000)
                    <= arrival + MAX_OUTPUT_LATENCY,
                "chunk {k} ended past output-latency allowance"
            );
        }
        // Output latency (e.g. 50 ms on Windows WASAPI) places chunks at their
        // speaker presentation time (`end_us > wall_us` within MAX_OUTPUT_LATENCY),
        // and the 64-chunk min-filter re-anchor bounds 1,000 ppm NTP slew over 10 min.
        let mut slew = BrowserAudioStamper::default();
        let host_origin = Time::from_nanos(5_000_000_000);
        for k in 1..=28_125u64 {
            let mono_ns = k * 1_024 * 1_000_000_000 / 48_000;
            // Wall clock slews +1,000 ppm (+600 ms over 10 min) relative to host_time.
            let wall_render_us = 1_700_000_000_000_000 + mono_ns / 1_000 + (mono_ns / 1_000_000);
            let end_us = (wall_render_us / 1_000) * 1_000 + 50_000;
            let ipc_us = if k.is_multiple_of(17) { 45_000 } else { 1_000 };
            let arrival = host_origin + Duration::from_nanos(mono_ns + ipc_us * 1_000);
            let wall_us = wall_render_us + ipc_us;
            let (at, gap) = slew.stamp_timed(
                1_024,
                BrowserAudioTiming::Continuous { end_us },
                Some(wall_us),
                arrival,
            );
            assert_eq!(gap, None, "NTP slew chunk {k} falsely reported gap");
            if k == 1 {
                let expected =
                    host_origin + Duration::from_nanos(mono_ns) + Duration::from_millis(50)
                        - Duration::from_nanos(1_024 * 1_000_000_000 / 48_000);
                assert!(
                    (at.as_nanos() - expected.as_nanos()).abs() <= 2_000_000,
                    "first output-compensated chunk at {at:?}, expected {expected:?}"
                );
            }
        }
        // When `next` is already past `min_at` (e.g. a frozen `end_us` while
        // chunks kept arriving), the Continuous lag clamp must not place `at`
        // before `next` and silently trim overlap in the mixer.
        let mut frozen = BrowserAudioStamper::default();
        for t in [10, 20, 30, 40, 50] {
            let (_, gap) = frozen.stamp_timed(
                480,
                BrowserAudioTiming::Continuous { end_us: 10_000 },
                None,
                ms(t),
            );
            assert_eq!(gap, None);
        }
        assert_eq!(
            frozen.stamp_timed(
                480,
                BrowserAudioTiming::Continuous { end_us: 10_000 },
                None,
                ms(210),
            ),
            (ms(50), None)
        );
        // After a > MAX_LAG render stall flushes a stale pre-stall chunk (clamped
        // to `min_at`), the first live post-stall chunk must re-anchor to `now`
        // (+ actual output latency) rather than jumping `MAX_OUTPUT_LATENCY` ahead.
        let mut stall = BrowserAudioStamper::default();
        assert_eq!(
            stall.stamp_timed(
                480,
                BrowserAudioTiming::Continuous { end_us: 100_000 },
                Some(100_000),
                ms(100),
            ),
            (ms(90), None)
        );
        assert_eq!(
            stall.stamp_timed(
                480,
                BrowserAudioTiming::Continuous { end_us: 110_000 },
                Some(600_000),
                ms(600),
            ),
            (ms(405), Some(Duration::from_millis(305)))
        );
        let (post_at, post_gap) = stall.stamp_timed(
            480,
            BrowserAudioTiming::Continuous { end_us: 620_000 },
            Some(620_000),
            ms(620),
        );
        assert!(
            (post_at.as_nanos() - ms(610).as_nanos()).abs() <= 5_000_000,
            "post-stall chunk jumped into the future: {post_at:?}"
        );
        assert!(
            post_gap.is_some_and(|g| g >= Duration::from_millis(190)),
            "expected post-stall resync gap, got {post_gap:?}"
        );
        // A > MIX_DELAY main-thread IPC burst (e.g. 63 buffered 21.33 ms chunks
        // = 1.34 s arriving at once during cold page load) must not march `next`
        // +115..150 ms into the future and leave subsequent live audio late.
        let mut burst = BrowserAudioStamper::default();
        let t0 = Time::from_nanos(2_000_000_000);
        let q_ns = 1_024u64 * 1_000_000_000 / 48_000;
        let _ = burst.stamp_timed(
            1_024,
            BrowserAudioTiming::Continuous {
                end_us: 1_700_000_000_000_000 + q_ns / 1_000,
            },
            Some(1_700_000_000_000_000 + q_ns / 1_000),
            t0 + Duration::from_nanos(q_ns),
        );
        let burst_arrival = t0 + Duration::from_nanos(64 * q_ns);
        let burst_wall = 1_700_000_000_000_000 + (64 * q_ns) / 1_000;
        for k in 2..=64u64 {
            let end_us = 1_700_000_000_000_000 + ((k * q_ns) / 1_000_000) * 1_000;
            let _ = burst.stamp_timed(
                1_024,
                BrowserAudioTiming::Continuous { end_us },
                Some(burst_wall),
                burst_arrival,
            );
        }
        let live_arrival = t0 + Duration::from_nanos(65 * q_ns);
        let live_end_us = 1_700_000_000_000_000 + ((65 * q_ns) / 1_000_000) * 1_000;
        let (live_at, live_gap) = burst.stamp_timed(
            1_024,
            BrowserAudioTiming::Continuous {
                end_us: live_end_us,
            },
            Some(1_700_000_000_000_000 + (65 * q_ns) / 1_000),
            live_arrival,
        );
        assert_eq!(live_gap, None);
        let expected_live = live_arrival - Duration::from_nanos(q_ns);
        assert!(
            (live_at.as_nanos() - expected_live.as_nanos()).abs() <= 15_000_000,
            "post-burst live chunk stayed late: {live_at:?} vs {expected_live:?}"
        );
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
