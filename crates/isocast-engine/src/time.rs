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
    // Active after >= 1 s of continuous lag drops fell back to the self-healing
    // `now - MAX_LAG` clamp, keeping `observe_wall` from pulling `origin_host`
    // back below `min_end` until a chunk arrives within `MAX_LAG`.
    clamped_skew: bool,
}

impl PageClock {
    fn new(end_us: u64, host_end: Time) -> Self {
        Self {
            origin_us: end_us,
            origin_host: host_end,
            last_us: end_us,
            window_count: 0,
            window_best: None,
            clamped_skew: false,
        }
    }

    fn reset(&mut self, end_us: u64, host_end: Time) {
        self.origin_us = end_us;
        self.origin_host = host_end;
        self.window_count = 0;
        self.window_best = None;
    }

    fn observe_wall(&mut self, end_us: u64, wall_us: Option<u64>, now: Time, min_end: Time) {
        const WINDOW: u8 = 64;
        let Some((anchored, signed_us)) = wall_anchor(end_us, wall_us, now) else {
            return;
        };
        if anchored >= min_end {
            self.clamped_skew = false;
        } else if self.clamped_skew {
            return;
        }
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

#[derive(Clone, Copy)]
struct DroppedRun {
    from: Time,
    until: Time,
    started_now: Time,
}

/// Places page audio on the host clock. Start fresh for each document.
#[derive(Default)]
pub struct BrowserAudioStamper {
    // End of stamped audio and actual arrival of that same chunk.
    previous: Option<(Time, Time)>,
    // Anchor on the page's wall clock (`Date.now()` + output latency in µs) and host clock.
    source: Option<PageClock>,
    // Ongoing run of chunks dropped because their target fell before
    // `now - MAX_LAG`, reported on the next retained chunk, after `MAX_DROP_RUN`
    // (~1 s, when the stamper falls back to the self-healing clamp), or on `finish`.
    dropped: Option<DroppedRun>,
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
            .expect("arrival chunks are never before the lag limit")
    }

    /// Flush any pending lag-drop gap when the audio stream stops or reloads
    /// before a retained chunk could report it. Returns `(until, duration)` for
    /// [`crate::AudioSink::report_gap`].
    pub fn finish(&mut self, now: Time) -> Option<(Time, Duration)> {
        let drop = self.dropped.take()?;
        let until = drop.until.min(now);
        let gap = until.saturating_since(drop.from);
        (!gap.is_zero()).then_some((until, gap))
    }

    #[allow(clippy::too_many_arguments)]
    fn drop_or_clamp(
        &mut self,
        end_us: u64,
        desired_at: Time,
        loss_from: Time,
        duration: Duration,
        min_at: Time,
        max_at: Time,
        max_end: Time,
        now: Time,
        mut clock: PageClock,
    ) -> Option<(Time, Option<Duration>)> {
        const MAX_DROP_RUN: Duration = Duration::from_secs(1);
        let candidate_end = (desired_at + duration).max(loss_from + duration);
        let drop = match self.dropped {
            Some(mut d) => {
                d.until = (d.until + duration).max(candidate_end);
                d
            }
            None => DroppedRun {
                from: loss_from,
                until: candidate_end,
                started_now: now,
            },
        };
        if now.saturating_since(drop.started_now) >= MAX_DROP_RUN {
            let at = self
                .previous
                .map_or(min_at, |(next, _)| min_at.max(next.min(max_at)))
                .min(max_at);
            let gap = Some(at.saturating_since(drop.from)).filter(|d| !d.is_zero());
            let end = (at + duration).min(max_end);
            clock.reset(end_us, end);
            clock.clamped_skew = true;
            self.dropped = None;
            self.previous = Some((end, now));
            self.source = Some(clock);
            return Some((at, gap));
        }
        self.dropped = Some(drop);
        self.source = Some(clock);
        None
    }

    /// Places a chunk using the page's wall clock + run-start output latency
    /// when present (`audio.js` samples output-latency compensation once at
    /// the start of each continuous run), keeping `source_end <= now + MAX_OUTPUT_LATENCY`.
    /// Chunks whose target falls before the [`crate::record::mix::MIX_DELAY`] lag
    /// limit (`now - MAX_LAG`) are dropped (`None`) for up to ~1 s of real time
    /// so transient bursts retain only their in-sync live tail, and fall back to
    /// the self-healing `min_at` clamp (reporting the accumulated gap) if a
    /// persistent backlog continues for >= 1 s; [`Self::finish`] flushes any
    /// shorter pending gap on stop or reload.
    pub fn stamp_timed(
        &mut self,
        frames: usize,
        timing: BrowserAudioTiming,
        wall_us: Option<u64>,
        now: Time,
    ) -> Option<(Time, Option<Duration>)> {
        const RESYNC: Duration = Duration::from_millis(150);
        // Below MIX_DELAY (200 ms) so retained samples never miss the program
        // mix; chunks older than MAX_LAG are dropped and reported as a gap.
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
                // minus IPC delay, plus 5 ms for `Date.now()` rounding).
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
                if self.source.is_none() {
                    let desired_end = anchor_end(end_us, wall_us, now).min(max_end);
                    let desired_at = desired_end - duration;
                    if desired_at < min_at {
                        let loss_from = self.previous.map_or(desired_at, |(next, _)| next);
                        return self.drop_or_clamp(
                            end_us,
                            desired_at,
                            loss_from,
                            duration,
                            min_at,
                            max_at,
                            max_end,
                            now,
                            PageClock::new(end_us, desired_end),
                        );
                    }
                    let at = self
                        .previous
                        .map_or(desired_at, |(next, _)| desired_at.max(next.min(max_at)))
                        .min(max_at);
                    let gap = self
                        .dropped
                        .take()
                        .and_then(|d| Some(at.saturating_since(d.from)).filter(|g| !g.is_zero()));
                    let end = at + duration;
                    self.previous = Some((end, now));
                    self.source = Some(PageClock::new(end_us, end));
                    return Some((at, gap));
                }
                if let Some(mut clock) = self.source.filter(|c| end_us >= c.last_us) {
                    let step = Duration::from_micros(end_us - clock.last_us).min(MAX_SPAN);
                    clock.last_us = end_us;
                    if matches!(timing, BrowserAudioTiming::Continuous { .. }) {
                        clock.observe_wall(end_us, wall_us, now, min_at + duration);
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
                            let anchored = anchor_end(end_us, wall_us, now).min(max_end);
                            clock.reset(end_us, anchored);
                            clock.clamped_skew = false;
                            anchored
                        };
                        let raw_at = source_end - duration;
                        if raw_at < min_at {
                            clock.clamped_skew = false;
                            return self.drop_or_clamp(
                                end_us, raw_at, raw_at, duration, min_at, max_at, max_end, now,
                                clock,
                            );
                        }
                        let at = self
                            .previous
                            .map_or(raw_at, |(next, _)| raw_at.max(next.min(max_at)))
                            .min(max_at);
                        let gap = self.dropped.take().and_then(|d| {
                            Some(at.saturating_since(d.from)).filter(|g| !g.is_zero())
                        });
                        let end = at + duration;
                        self.previous = Some((end, now));
                        self.source = Some(clock);
                        return Some((at, gap));
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
                        clock.clamped_skew = false;
                        now
                    };
                    let desired_at = source_end - duration;
                    if desired_at < min_at {
                        if clock.clamped_skew {
                            let at = self
                                .previous
                                .map_or(min_at, |(next, _)| min_at.max(next.min(max_at)))
                                .min(max_at);
                            let gap = self.previous.and_then(|(next, _)| {
                                Some(at.saturating_since(next)).filter(|g| !g.is_zero())
                            });
                            let end = (at + duration).min(max_end);
                            clock.reset(end_us, end);
                            self.previous = Some((end, now));
                            self.source = Some(clock);
                            return Some((at, gap));
                        }
                        let loss_from = self.previous.map_or(desired_at, |(next, _)| next);
                        return self.drop_or_clamp(
                            end_us, desired_at, loss_from, duration, min_at, max_at, max_end, now,
                            clock,
                        );
                    }
                    let (at, gap) = if let Some(d) = self.dropped.take() {
                        let at = self
                            .previous
                            .map_or(desired_at, |(next, _)| desired_at.max(next.min(max_at)))
                            .min(max_at);
                        let gap = Some(at.saturating_since(d.from)).filter(|g| !g.is_zero());
                        (at, gap)
                    } else if let Some((next, _)) = self.previous {
                        let (mut at, mut gap) = (desired_at, None);
                        if desired_at.saturating_since(next) >= RESYNC {
                            gap = Some(desired_at.saturating_since(next));
                        } else if next.saturating_since(desired_at) < RESYNC {
                            at = next.min(max_at + Duration::from_millis(8));
                        }
                        (at, gap)
                    } else {
                        (desired_at.min(max_at), None)
                    };
                    let end = at + duration;
                    self.previous = Some((end, now));
                    self.source = Some(clock);
                    return Some((at, gap));
                }
                let anchored = anchor_end(end_us, wall_us, now).min(max_end);
                let desired_at = anchored - duration;
                if desired_at < min_at {
                    let loss_from = self.previous.map_or(desired_at, |(next, _)| next);
                    return self.drop_or_clamp(
                        end_us,
                        desired_at,
                        loss_from,
                        duration,
                        min_at,
                        max_at,
                        max_end,
                        now,
                        PageClock::new(end_us, anchored),
                    );
                }
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
        if gap.is_none()
            && let Some(d) = self.dropped.take()
        {
            gap = Some(at.saturating_since(d.from)).filter(|g| !g.is_zero());
        }
        self.previous = Some((at + duration, now));
        Some((at, gap))
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
            Some((ms(0), None))
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
                Some((ms(n * 10), None))
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
            Some((ms(185), None))
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
                Some((ms(195 + n * 10), None))
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
            Some((ms(430), None))
        );
        // A 260 ms Continuous IPC stall (> MAX_LAG 195 ms) drops the 7 chunks
        // older than `now - MAX_LAG` (`450_000..=510_000`, `desired_at < ms(505)`)
        // and places the live tail (`520_000..=700_000`) at its true timestamps
        // with one coalesced 70 ms gap (`ms(440) -> ms(510)`).
        for end_us in (450_000..=510_000).step_by(10_000) {
            assert_eq!(
                clock.stamp_timed(
                    480,
                    BrowserAudioTiming::Continuous { end_us },
                    Some(700_000),
                    ms(700)
                ),
                None
            );
        }
        assert_eq!(
            clock.stamp_timed(
                480,
                BrowserAudioTiming::Continuous { end_us: 520_000 },
                Some(700_000),
                ms(700)
            ),
            Some((ms(510), Some(Duration::from_millis(70))))
        );
        for (i, end_us) in (530_000..=700_000).step_by(10_000).enumerate() {
            assert_eq!(
                clock.stamp_timed(
                    480,
                    BrowserAudioTiming::Continuous { end_us },
                    Some(700_000),
                    ms(700)
                ),
                Some((ms(520 + i as i64 * 10), None))
            );
        }
        // A 260 ms Resumed stall past an intentional 75 ms pause (`ms(700) -> ms(775)`)
        // drops the 7 post-resume chunks before `min_at = ms(840)` (`775..845 ms`)
        // and reports only the lost post-resume audio (`845 - 775 = 70 ms`), not
        // the intentional 75 ms pause.
        assert_eq!(
            clock.stamp_timed(
                480,
                BrowserAudioTiming::Resumed { end_us: 785_000 },
                Some(1_035_000),
                ms(1_035)
            ),
            None
        );
        for end_us in (795_000..=845_000).step_by(10_000) {
            assert_eq!(
                clock.stamp_timed(
                    480,
                    BrowserAudioTiming::Continuous { end_us },
                    Some(1_035_000),
                    ms(1_035)
                ),
                None
            );
        }
        assert_eq!(
            clock.stamp_timed(
                480,
                BrowserAudioTiming::Continuous { end_us: 855_000 },
                Some(1_035_000),
                ms(1_035)
            ),
            Some((ms(845), Some(Duration::from_millis(70))))
        );
        // Lost quanta during a continuous run report one gap; u64::MAX never overflows
        // on either Continuous or Resumed and never places samples after now.
        let mut loss = BrowserAudioStamper::default();
        let mut gaps = Vec::new();
        for n in 0..22 {
            let (_, gap) = loss
                .stamp_timed(
                    480,
                    BrowserAudioTiming::Continuous {
                        end_us: (n + 1) * 18_000,
                    },
                    None,
                    ms(100 + n as i64 * 18),
                )
                .unwrap();
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
            Some((ms(640), Some(Duration::from_millis(178))))
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
            Some((ms(590), None))
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
            let (at, gap) = long
                .stamp_timed(
                    1_024,
                    BrowserAudioTiming::Continuous { end_us },
                    Some(wall_us),
                    arrival,
                )
                .expect("on-time chunk retained");
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
            let (at, gap) = slew
                .stamp_timed(
                    1_024,
                    BrowserAudioTiming::Continuous { end_us },
                    Some(wall_us),
                    arrival,
                )
                .expect("slew chunk retained");
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
        // A chunk whose `end_us` is frozen past `MAX_LAG` is dropped (`None`)
        // rather than placed before `next` or played stale.
        let mut frozen = BrowserAudioStamper::default();
        for t in [10, 20, 30, 40, 50] {
            let (_, gap) = frozen
                .stamp_timed(
                    480,
                    BrowserAudioTiming::Continuous { end_us: 10_000 },
                    None,
                    ms(t),
                )
                .unwrap();
            assert_eq!(gap, None);
        }
        assert_eq!(
            frozen.stamp_timed(
                480,
                BrowserAudioTiming::Continuous { end_us: 10_000 },
                None,
                ms(210),
            ),
            None
        );
        // After a > MAX_LAG render stall flushes a stale pre-stall chunk, the
        // stale chunk is dropped and the first live post-stall chunk reports a
        // single coalesced gap (`ms(100) -> ms(610)` = 510 ms) at `ms(610)`.
        let mut stall = BrowserAudioStamper::default();
        assert_eq!(
            stall.stamp_timed(
                480,
                BrowserAudioTiming::Continuous { end_us: 100_000 },
                Some(100_000),
                ms(100),
            ),
            Some((ms(90), None))
        );
        assert_eq!(
            stall.stamp_timed(
                480,
                BrowserAudioTiming::Continuous { end_us: 110_000 },
                Some(600_000),
                ms(600),
            ),
            None
        );
        let (post_at, post_gap) = stall
            .stamp_timed(
                480,
                BrowserAudioTiming::Continuous { end_us: 620_000 },
                Some(620_000),
                ms(620),
            )
            .expect("live post-stall chunk retained");
        assert!(
            (post_at.as_nanos() - ms(610).as_nanos()).abs() <= 5_000_000,
            "post-stall chunk jumped into the future: {post_at:?}"
        );
        assert_eq!(post_gap, Some(Duration::from_millis(510)));
    }

    #[test]
    fn burst_past_mix_delay_drops_stale_prefix_and_retains_live_tail_without_overlap() {
        let q_ns = 1_024u64 * 1_000_000_000 / 48_000;
        let q_dur = Duration::from_nanos(q_ns);
        let t0 = Time::from_nanos(2_000_000_000);
        let burst_arrival = t0 + Duration::from_nanos(64 * q_ns);
        let burst_wall = 1_700_000_000_000_000 + (64 * q_ns) / 1_000;
        let min_at = burst_arrival - Duration::from_millis(195);

        // Case 1: True startup burst (`self.previous == None`): all 64 chunks
        // (`1.365 s`) arrive together after cold page load.
        let mut startup = BrowserAudioStamper::default();
        let mut kept = Vec::new();
        for k in 1..=64u64 {
            let end_us = 1_700_000_000_000_000 + ((k * q_ns) / 1_000_000) * 1_000;
            if let Some(res) = startup.stamp_timed(
                1_024,
                BrowserAudioTiming::Continuous { end_us },
                Some(burst_wall),
                burst_arrival,
            ) {
                kept.push((k, res.0, res.1));
            }
        }
        // Chunks 1..=55 (`desired_at < burst_arrival - 195 ms`) are dropped;
        // chunks 56..=64 (the newest ~192 ms) are retained monotonically with
        // zero overlap, and chunk 56 reports the single coalesced startup gap.
        let kept_indices: Vec<u64> = kept.iter().map(|&(k, _, _)| k).collect();
        assert_eq!(kept_indices, (56..=64).collect::<Vec<_>>());
        assert!(
            kept[0].1 >= min_at,
            "first kept chunk before lag limit: {:?}",
            kept[0]
        );
        let first_end_us = 1_700_000_000_000_000 + (q_ns / 1_000_000) * 1_000;
        let expected_gap = kept[0].1.saturating_since(
            burst_arrival - Duration::from_micros(burst_wall - first_end_us) - q_dur,
        );
        assert_eq!(kept[0].2, Some(expected_gap));
        assert!(expected_gap >= Duration::from_millis(1_150));
        for pair in kept.windows(2) {
            assert_eq!(
                pair[1].1,
                pair[0].1 + q_dur,
                "overlap or hole in retained tail: {pair:?}"
            );
            assert_eq!(pair[1].2, None);
        }
        let live_arrival = t0 + Duration::from_nanos(65 * q_ns);
        let live_end_us = 1_700_000_000_000_000 + ((65 * q_ns) / 1_000_000) * 1_000;
        let (live_at, live_gap) = startup
            .stamp_timed(
                1_024,
                BrowserAudioTiming::Continuous {
                    end_us: live_end_us,
                },
                Some(1_700_000_000_000_000 + (65 * q_ns) / 1_000),
                live_arrival,
            )
            .expect("live chunk after startup burst retained");
        assert_eq!(live_gap, None);
        assert_eq!(live_at, kept.last().unwrap().1 + q_dur);
        assert!((live_at.as_nanos() - (live_arrival - q_dur).as_nanos()).abs() <= 2_000_000);

        // Case 2: Mid-take burst after chunk 1 arrived on time (`self.previous` populated).
        let mut mid = BrowserAudioStamper::default();
        let (first_at, first_gap) = mid
            .stamp_timed(
                1_024,
                BrowserAudioTiming::Continuous {
                    end_us: 1_700_000_000_000_000 + q_ns / 1_000,
                },
                Some(1_700_000_000_000_000 + q_ns / 1_000),
                t0 + q_dur,
            )
            .unwrap();
        assert_eq!((first_at, first_gap), (t0, None));
        let mut mid_kept = Vec::new();
        for k in 2..=64u64 {
            let end_us = 1_700_000_000_000_000 + ((k * q_ns) / 1_000_000) * 1_000;
            if let Some(res) = mid.stamp_timed(
                1_024,
                BrowserAudioTiming::Continuous { end_us },
                Some(burst_wall),
                burst_arrival,
            ) {
                mid_kept.push((k, res.0, res.1));
            }
        }
        assert_eq!(
            mid_kept.iter().map(|&(k, _, _)| k).collect::<Vec<_>>(),
            (56..=64).collect::<Vec<_>>()
        );
        assert_eq!(
            mid_kept[0].2,
            Some(mid_kept[0].1.saturating_since(t0 + q_dur))
        );
        for pair in mid_kept.windows(2) {
            assert_eq!(pair[1].1, pair[0].1 + q_dur);
            assert_eq!(pair[1].2, None);
        }
    }

    #[test]
    fn persistent_backlog_reports_gap_and_self_heals_after_one_second() {
        let q_ns = 1_024u64 * 1_000_000_000 / 48_000;
        let q_dur = Duration::from_nanos(q_ns);
        let t0 = Time::from_nanos(5_000_000_000);
        let base_wall = 1_700_000_000_000_000u64;
        let mut stamper = BrowserAudioStamper::default();

        // Chunk 1 arrives live (5 ms IPC) so `previous` and `source` are initialized.
        let (first_at, first_gap) = stamper
            .stamp_timed(
                1_024,
                BrowserAudioTiming::Continuous {
                    end_us: base_wall + q_ns / 1_000,
                },
                Some(base_wall + q_ns / 1_000 + 5_000),
                t0 + q_dur + Duration::from_millis(5),
            )
            .expect("initial live chunk retained");
        assert_eq!(first_gap, None);
        let first_end = first_at + q_dur;

        // Chunks 2..=220 (~4.67 s across multiple 64-chunk `observe_wall` windows)
        // suffer a persistent 250 ms IPC backlog (> MAX_LAG 195 ms + 21.33 ms),
        // with deterministic +/-6 ms arrival jitter.
        let mut dropped_count = 0usize;
        let mut heal_chunk = None;
        let mut post_heal_kept = 0usize;
        for k in 2..=220u64 {
            let render_ns = k * q_ns;
            let end_us = base_wall + ((render_ns / 1_000_000) * 1_000);
            let jitter_us = if k.is_multiple_of(3) { 6_000 } else { 0 };
            let lag_us = 250_000 + jitter_us;
            let arrival = t0 + Duration::from_nanos(render_ns) + Duration::from_micros(lag_us);
            let wall_us = base_wall + render_ns / 1_000 + lag_us;
            match stamper.stamp_timed(
                1_024,
                BrowserAudioTiming::Continuous { end_us },
                Some(wall_us),
                arrival,
            ) {
                None => {
                    assert!(
                        heal_chunk.is_none(),
                        "chunk {k} dropped again after self-healing at {heal_chunk:?}"
                    );
                    dropped_count += 1;
                }
                Some((at, gap)) => {
                    if heal_chunk.is_none() {
                        let reported = gap.expect("1 s drop fallback must report accumulated gap");
                        assert_eq!(reported, at.saturating_since(first_end));
                        assert!(
                            reported >= Duration::from_millis(950)
                                && reported <= Duration::from_millis(1_150),
                            "unexpected 1 s fallback gap: {reported:?}"
                        );
                        heal_chunk = Some(k);
                    } else {
                        post_heal_kept += 1;
                    }
                    assert!(
                        at >= arrival - Duration::from_millis(195),
                        "retained chunk {k} placed before lag limit: {at:?} vs {arrival:?}"
                    );
                }
            }
        }
        assert!(
            (45..=50).contains(&dropped_count),
            "expected ~1 s of drops (~47 chunks), got {dropped_count}"
        );
        assert!(
            post_heal_kept >= 165,
            "expected >= 165 self-healed chunks retained across observe_wall windows, got {post_heal_kept}"
        );
        assert_eq!(stamper.finish(t0 + Duration::from_secs(6)), None);
    }

    #[test]
    fn pending_drop_gap_is_reported_on_stop() {
        let ms = |n: i64| Time::from_nanos(n * 1_000_000);

        // Case 1: Mid-take < 1 s drop run ending at stop without a retained chunk.
        let mut mid = BrowserAudioStamper::default();
        assert_eq!(
            mid.stamp_timed(
                480,
                BrowserAudioTiming::Continuous { end_us: 100_000 },
                Some(100_000),
                ms(100),
            ),
            Some((ms(90), None))
        );
        for step in 1..=25i64 {
            let end_us = 100_000 + step as u64 * 10_000;
            // 250 ms behind host time (< 1 s total duration = 250 ms).
            let now_ms = 100 + step * 10 + 250;
            assert_eq!(
                mid.stamp_timed(
                    480,
                    BrowserAudioTiming::Continuous { end_us },
                    Some(now_ms as u64 * 1_000),
                    ms(now_ms),
                ),
                None
            );
        }
        assert_eq!(
            mid.finish(ms(620)),
            Some((ms(350), Duration::from_millis(250)))
        );
        assert_eq!(mid.finish(ms(620)), None);

        // Case 2: Startup < 1 s drop run (`previous == None`) ending at stop.
        let mut startup = BrowserAudioStamper::default();
        for step in 1..=15i64 {
            let end_us = step as u64 * 10_000;
            let now_ms = step * 10 + 250;
            assert_eq!(
                startup.stamp_timed(
                    480,
                    BrowserAudioTiming::Continuous { end_us },
                    Some(now_ms as u64 * 1_000),
                    ms(now_ms),
                ),
                None
            );
        }
        assert_eq!(
            startup.finish(ms(420)),
            Some((ms(150), Duration::from_millis(150)))
        );
        assert_eq!(startup.finish(ms(420)), None);
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
