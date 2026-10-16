//! A recording-only edit clock. Captures, preview and streaming keep host time.

use std::sync::RwLock;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use isocast_protocol::api::RecordingPause;
use isocast_protocol::manifest::Pause;

use crate::{SAMPLE_RATE, Time};

/// Five milliseconds to/from silence avoids a waveform discontinuity at a cut.
/// These samples stay in place: the fade never changes sync or clip length.
pub(super) const FADE: Duration = Duration::from_millis(5);

pub(super) struct RecordingClock {
    pub state: RwLock<Cuts>,
    /// Last input inspected by any file, including future-stamped audio.
    /// Published under the read lock; a new cut takes the write lock, so it
    /// can never change the meaning of samples a writer already inspected.
    horizon: AtomicI64,
}

pub(super) struct Cuts {
    epoch: Time,
    rate: u32,
    spans: Vec<Span>,
}

struct Span {
    start: Time,
    end: Option<Time>,
    removed_before: Duration,
    /// What the commit barrier at this pause established (see
    /// `RecordingClock::set_recoverable`).
    recoverable: Option<f64>,
}

impl RecordingClock {
    pub fn new(epoch: Time, rates: impl Iterator<Item = u32>) -> Self {
        fn gcd(a: u32, b: u32) -> u32 {
            if b == 0 { a } else { gcd(b, a % b) }
        }
        Self {
            state: RwLock::new(Cuts {
                epoch,
                rate: rates.fold(gcd(1_000_000_000, SAMPLE_RATE), gcd),
                spans: Vec::new(),
            }),
            horizon: AtomicI64::new(epoch.as_nanos()),
        }
    }

    pub fn inspected(&self, until: Time) {
        self.horizon.fetch_max(until.as_nanos(), Ordering::Relaxed);
    }

    /// Record what every file had committed to the disk when the pause cut
    /// at `start` was answered: seconds of the take's saved timeline.
    pub fn set_recoverable(&self, start: Time, seconds: Option<f64>) {
        let mut cuts = self.state.write().unwrap();
        if let Some(span) = cuts.spans.iter_mut().find(|s| s.start == start) {
            span.recoverable = seconds;
        }
    }

    /// The uncut time of the latest pause, if the clock is paused.
    pub fn open_pause(&self) -> Option<Time> {
        let cuts = self.state.read().unwrap();
        cuts.spans
            .last()
            .filter(|s| s.end.is_none())
            .map(|s| s.start)
    }

    pub fn change(&self, paused: bool, now: Time, end: Option<Time>) -> Result<Time, String> {
        let mut cuts = self.state.write().unwrap();
        if !cuts.settled(now) {
            return Err(
                "recording.paused: a pause/resume boundary is pending; wait for its reply".into(),
            );
        }
        let last = cuts.spans.last();
        if last.is_some_and(|s| s.end.is_none()) == paused {
            return Err(format!(
                "recording.paused: already {}; valid action: {}",
                if paused { "paused" } else { "recording" },
                if paused { "resume" } else { "pause" }
            ));
        }
        // Do not schedule backwards under concurrent callers, or cut through
        // an already submitted audio block (including its fade-out).
        let previous = last.map_or(cuts.epoch, |s| s.end.unwrap_or(s.start));
        let after = now
            .max(Time::from_nanos(self.horizon.load(Ordering::Relaxed)))
            .max(previous)
            + FADE;
        let mut tick = after.ticks_since(cuts.epoch, cuts.rate).max(0) as u64;
        if cuts.epoch.add_ticks(tick, cuts.rate) < after {
            tick += 1;
        }
        let at = cuts.epoch.add_ticks(tick, cuts.rate);
        if end.is_some_and(|end| at >= end) {
            return Err("recording: stopping before the pause boundary; valid action: stop".into());
        }
        if paused {
            let removed_before = at.saturating_since(cuts.map(at));
            cuts.spans.push(Span {
                start: at,
                end: None,
                removed_before,
                recoverable: None,
            });
        } else {
            cuts.spans.last_mut().expect("paused").end = Some(at);
        }
        Ok(at)
    }
}

impl Cuts {
    pub fn map(&self, at: Time) -> Time {
        if at == Time::MAX {
            return at;
        }
        let i = self.spans.partition_point(|s| s.start <= at);
        let Some(s) = i.checked_sub(1).map(|i| &self.spans[i]) else {
            return at;
        };
        at - (s.removed_before + at.min(s.end.unwrap_or(at)).saturating_since(s.start))
    }

    pub fn paused(&self, at: Time) -> bool {
        let i = self.spans.partition_point(|s| s.start <= at);
        i.checked_sub(1)
            .is_some_and(|i| self.spans[i].end.is_none_or(|end| at < end))
    }

    pub fn requested_pause(&self) -> bool {
        self.spans.last().is_some_and(|s| s.end.is_none())
    }

    pub fn settled(&self, now: Time) -> bool {
        self.spans
            .last()
            .is_none_or(|s| s.end.unwrap_or(s.start) <= now)
    }

    pub fn status(&self, now: Time) -> RecordingPause {
        let mapped = self.map(now);
        RecordingPause {
            paused: self.paused(now),
            elapsed_seconds: mapped.saturating_since(self.epoch).as_secs_f64(),
            paused_seconds: now.saturating_since(mapped).as_secs_f64(),
            recoverable_seconds: None,
            errors: Vec::new(),
        }
    }

    pub fn manifest(&self, from: Time, until: Time) -> Vec<Pause> {
        self.spans
            .iter()
            .filter(|s| s.start >= from && s.start < until)
            .map(|s| {
                let time_seconds = self
                    .map(s.start)
                    .saturating_since(self.map(from))
                    .as_secs_f64();
                let end_seconds = s
                    .end
                    .or((until != Time::MAX).then_some(until))
                    .map(|end| end.min(until).saturating_since(from).as_secs_f64());
                Pause {
                    start_seconds: s.start.saturating_since(from).as_secs_f64(),
                    // AVAssetWriter accepts queued samples, not committed
                    // fragments, so this stays unknown for its files. A
                    // crash-safe writer's commit barrier fills it in.
                    recoverable_seconds_at_pause: s.recoverable,
                    end_seconds,
                    time_seconds,
                }
            })
            .collect()
    }

    /// Visit the retained parts of a sample block, with fades around cuts.
    /// Normal recording is one borrowed slice, with no allocation or copy.
    pub fn audio<E>(
        &self,
        samples: &[f32],
        channels: usize,
        pts: Time,
        mut append: impl FnMut(&[f32], Time, Time) -> Result<(), E>,
    ) -> Result<(), E> {
        let frames = samples.len() / channels;
        let until = pts.add_ticks(frames as u64, SAMPLE_RATE);
        let Some(relevant) = self.audio_edit(pts, until) else {
            return append(samples, self.map(pts), Time::MAX);
        };
        let sample = |at: Time| at.ticks_since(pts, SAMPLE_RATE).clamp(0, frames as i64) as usize;
        let mut cursor = 0;
        let mut fade_in = None;
        for span in &self.spans[relevant..] {
            let cut = sample(span.start);
            if cut > cursor {
                self.audio_part(
                    samples,
                    channels,
                    pts,
                    cursor..cut,
                    fade_in,
                    Some(span.start),
                    &mut append,
                )?;
            }
            cursor = sample(span.end.unwrap_or(until));
            fade_in = span.end;
            if cursor >= frames {
                return Ok(());
            }
            if span.start >= until {
                return Ok(());
            }
        }
        if cursor < frames {
            self.audio_part(
                samples,
                channels,
                pts,
                cursor..frames,
                fade_in,
                None,
                &mut append,
            )?;
        }
        Ok(())
    }

    /// The common path can borrow its block and release the clock before
    /// entering a native encoder. Only blocks touching cuts need staging.
    pub fn audio_passthrough(&self, pts: Time, frames: u64) -> Option<Time> {
        self.audio_edit(pts, pts.add_ticks(frames, SAMPLE_RATE))
            .is_none()
            .then(|| self.map(pts))
    }

    fn audio_edit(&self, pts: Time, until: Time) -> Option<usize> {
        let index = self
            .spans
            .partition_point(|s| s.end.is_some_and(|end| end + FADE <= pts));
        (index < self.spans.len() && self.spans[index].start - FADE < until).then_some(index)
    }

    #[allow(clippy::too_many_arguments)]
    fn audio_part<E>(
        &self,
        samples: &[f32],
        channels: usize,
        pts: Time,
        range: std::ops::Range<usize>,
        fade_in: Option<Time>,
        fade_out: Option<Time>,
        append: &mut impl FnMut(&[f32], Time, Time) -> Result<(), E>,
    ) -> Result<(), E> {
        let at = pts.add_ticks(range.start as u64, SAMPLE_RATE);
        let until = pts.add_ticks(range.end as u64, SAMPLE_RATE);
        let input = &samples[range.start * channels..range.end * channels];
        let fades =
            fade_in.is_some_and(|t| at < t + FADE) || fade_out.is_some_and(|t| until > t - FADE);
        if !fades {
            return append(
                input,
                self.map(at),
                fade_out.map_or(Time::MAX, |end| self.map(end)),
            );
        }
        let mut output = input.to_vec();
        let fade_samples = (FADE.as_secs_f64() * SAMPLE_RATE as f64) as i64;
        for (i, frame) in output.chunks_exact_mut(channels).enumerate() {
            let t = at.add_ticks(i as u64, SAMPLE_RATE);
            let gain_in = fade_in.map_or(1.0, |start| {
                (t.ticks_since(start, SAMPLE_RATE) as f32 / fade_samples as f32).clamp(0.0, 1.0)
            });
            let gain_out = fade_out.map_or(1.0, |end| {
                ((end.ticks_since(t, SAMPLE_RATE) - 1) as f32 / fade_samples as f32).clamp(0.0, 1.0)
            });
            for value in frame {
                *value *= gain_in.min(gain_out);
            }
        }
        append(
            &output,
            self.map(at),
            fade_out.map_or(Time::MAX, |end| self.map(end)),
        )
    }
}
