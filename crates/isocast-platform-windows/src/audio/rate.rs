//! Discipline a native endpoint's PCM rate to its packet QPC timestamps.
//! Output timestamps count *rate-corrected* frames, not free-running device
//! frames. Real position gaps, idle periods and discontinuities retain QPC.

use isocast_engine::{SAMPLE_RATE, Time};

const PHASE: u64 = 1 << 32;
const TAPS: usize = 32;
const HALF: usize = TAPS / 2;
const PHASES: usize = 256;
const MAX_RATE: f64 = 200e-6;
const FOLLOW_SECONDS: f64 = 10.0;

pub(super) struct RateFollower {
    pub(super) resets: u64,
    pub(super) clamped: u64,
    pub(super) max_error_nanos: u64,
    max_phase_error: f64,
    channels: usize,
    anchor: Option<Time>,
    previous: Option<(Time, usize)>,
    input_frames: u64,
    output_frames: u64,
    position: u128,
    step: u64,
    integral: f64,
    buffer_start: i128,
    buffer: Vec<f32>,
    output: Vec<f32>,
    kernel: Vec<[f32; TAPS]>,
}

impl RateFollower {
    pub(super) fn new(channels: usize, packet_frames: usize, phase_limit: u32) -> Self {
        let mut kernel = Vec::with_capacity(PHASES + 1);
        for phase in 0..=PHASES {
            let fraction = phase as f64 / PHASES as f64;
            let mut weights = [0.0; TAPS];
            let mut total = 0.0;
            for (tap, weight) in weights.iter_mut().enumerate() {
                let x = tap as f64 - (HALF - 1) as f64 - fraction;
                let window = 0.42
                    + 0.5 * (std::f64::consts::PI * x / HALF as f64).cos()
                    + 0.08 * (2.0 * std::f64::consts::PI * x / HALF as f64).cos();
                let angle = std::f64::consts::PI * 0.96 * x;
                let sinc = if angle == 0.0 {
                    1.0
                } else {
                    angle.sin() / angle
                };
                let value = 0.96 * sinc * window;
                *weight = value as f32;
                total += value;
            }
            for weight in &mut weights {
                *weight /= total as f32;
            }
            kernel.push(weights);
        }
        Self {
            resets: 0,
            clamped: 0,
            max_error_nanos: 0,
            max_phase_error: f64::from(phase_limit),
            channels,
            anchor: None,
            previous: None,
            input_frames: 0,
            output_frames: 0,
            position: 0,
            step: PHASE,
            integral: 0.0,
            buffer_start: 0,
            buffer: Vec::with_capacity((packet_frames + TAPS) * channels),
            output: Vec::with_capacity((packet_frames + TAPS) * channels),
            kernel,
        }
    }

    fn phase_error(&self, time: Time) -> f64 {
        let Some(anchor) = self.anchor else {
            return 0.0;
        };
        let host_frames =
            (time.as_nanos() - anchor.as_nanos()) as f64 * SAMPLE_RATE as f64 / 1_000_000_000.0;
        // Include the fractional input cursor and FIR lookahead. Comparing
        // just emitted integer counts would bias the servo by sixteen frames.
        let next = self.position as f64 / PHASE as f64;
        let predicted = self.output_frames as f64
            + (self.input_frames as f64 - next) / (self.step as f64 / PHASE as f64);
        host_frames - predicted
    }

    fn reset(&mut self, time: Time) {
        self.anchor = Some(time);
        self.previous = None;
        self.input_frames = 0;
        self.output_frames = 0;
        self.position = 0;
        self.buffer_start = -((HALF - 1) as i128);
        self.buffer.clear();
        self.buffer.resize((HALF - 1) * self.channels, 0.0);
        // Idle periods do not change the endpoint's oscillator rate.
        self.step = (PHASE as f64 / (1.0 + self.integral)).round() as u64;
    }

    pub(super) fn push(
        &mut self,
        samples: &[f32],
        time: Time,
        discontinuity: bool,
        emit: &mut impl FnMut(&[f32], Time),
    ) {
        let frames = samples.len() / self.channels;
        let error = self.phase_error(time);
        self.max_error_nanos = self
            .max_error_nanos
            .max((error.abs() * 1_000_000_000.0 / SAMPLE_RATE as f64).ceil() as u64);
        let jumped = self.previous.is_some_and(|(previous, count)| {
            let elapsed = (time.as_nanos() - previous.as_nanos()) as f64 * SAMPLE_RATE as f64
                / 1_000_000_000.0;
            (elapsed - count as f64).abs() > self.max_phase_error
        });
        // Position gaps/flags always reset, including gaps below phase_limit.
        // The limit only permits bounded timestamp quantization to be slewed.
        if self.anchor.is_none() || discontinuity || jumped || error.abs() > self.max_phase_error {
            if self.anchor.is_some() {
                self.resets += 1;
            }
            self.finish(emit);
            self.reset(time);
        } else if let Some((previous, _)) = self.previous {
            let seconds = (time.as_nanos() - previous.as_nanos()) as f64 / 1_000_000_000.0;
            // Critically damped phase/rate follower, ten-second time constant.
            // QPC noise cannot cause a per-packet sample insertion/deletion;
            // a fractional cursor spreads the correction across every sample.
            self.integral = (self.integral
                + error * seconds / (SAMPLE_RATE as f64 * FOLLOW_SECONDS.powi(2)))
            .clamp(-MAX_RATE, MAX_RATE);
            let requested = self.integral + 2.0 * error / (SAMPLE_RATE as f64 * FOLLOW_SECONDS);
            self.clamped += u64::from(requested.abs() >= MAX_RATE);
            let rate = requested.clamp(-MAX_RATE, MAX_RATE);
            self.step = (PHASE as f64 / (1.0 + rate)).round() as u64;
        }
        self.previous = Some((time, frames));
        self.append(samples, emit);
    }

    fn append(&mut self, samples: &[f32], emit: &mut impl FnMut(&[f32], Time)) {
        self.buffer.extend_from_slice(samples);
        self.input_frames += (samples.len() / self.channels) as u64;
        self.render(self.input_frames.saturating_sub(HALF as u64), emit);
        let retain = (self.position >> 32) as i128 - (HALF - 1) as i128;
        let discard = (retain - self.buffer_start).max(0) as usize;
        // The rate bound keeps the cursor within the sixteen-frame lookahead.
        self.buffer.drain(..discard * self.channels);
        self.buffer_start += discard as i128;
    }

    fn render(&mut self, end: u64, emit: &mut impl FnMut(&[f32], Time)) {
        let limit = u128::from(end) << 32;
        if self.position >= limit {
            return;
        }
        let count = (limit - self.position).div_ceil(u128::from(self.step)) as usize;
        self.output.clear();
        self.output.resize(count * self.channels, 0.0);
        if self.buffer.iter().any(|&sample| sample != 0.0) {
            for frame in self.output.chunks_exact_mut(self.channels) {
                let center = (self.position >> 32) as i128;
                let start = (center - (HALF - 1) as i128 - self.buffer_start) as usize;
                let fraction = self.position as u32;
                let phase = (fraction >> 24) as usize;
                let blend = (fraction & 0x00ff_ffff) as f32 / 16_777_216.0;
                for tap in 0..TAPS {
                    let a = self.kernel[phase][tap];
                    let weight = a + (self.kernel[phase + 1][tap] - a) * blend;
                    for (channel, sample) in frame.iter_mut().enumerate() {
                        *sample += self.buffer[(start + tap) * self.channels + channel] * weight;
                    }
                }
                self.position += u128::from(self.step);
            }
        } else {
            self.position += count as u128 * u128::from(self.step);
        }
        let time = self
            .anchor
            .unwrap()
            .add_ticks(self.output_frames, SAMPLE_RATE);
        emit(&self.output, time);
        self.output_frames += count as u64;
    }

    pub(super) fn finish(&mut self, emit: &mut impl FnMut(&[f32], Time)) {
        if self.anchor.is_some() {
            // Deliver the last sixteen real frames before a real gap. Zero
            // padding is only FIR context, never extra output or a gap bridge.
            self.buffer
                .resize(self.buffer.len() + HALF * self.channels, 0.0);
            self.render(self.input_frames, emit);
            self.anchor = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::MAX_PHASE_ERROR;

    #[test]
    fn drifting_endpoint_stays_continuous_and_tracks_qpc_for_ten_minutes() {
        for ppm in [-40_i64, 0, 40] {
            let mut rate = RateFollower::new(2, 480, MAX_PHASE_ERROR);
            let base = Time::from_nanos(1_000_000_000);
            let mut received = 0;
            let mut emit = |samples: &[f32], time: Time| {
                assert_eq!(time.ticks_since(base, SAMPLE_RATE), received as i64);
                received += (samples.len() / 2) as u64;
            };
            let mut maximum_error = 0.0_f64;
            for packet in 0..60_000_i64 {
                // Deterministic +/-6 us timestamp noise, no wall clock or I/O.
                let noise = if packet == 0 {
                    0
                } else {
                    ((packet * 17) % 13 - 6) * 1000
                };
                let time =
                    Time::from_nanos(base.as_nanos() + packet * (10_000_000 + ppm * 10) + noise);
                maximum_error = maximum_error.max(rate.phase_error(time).abs());
                if packet > 6000 {
                    assert!(rate.phase_error(time).abs() < 1.0);
                }
                rate.push(&[0.0; 960], time, false, &mut emit);
            }
            rate.finish(&mut emit);
            assert_eq!(rate.resets, 0);
            assert_eq!(rate.clamped, 0);
            assert!(
                maximum_error < 8.0,
                "{ppm} ppm: {maximum_error} frames of timestamp error"
            );
            let expected = 28_800_000 + 28_800_000 * ppm / 1_000_000;
            assert!(
                (received as i64 - expected).abs() <= 2,
                "{received} vs {expected}"
            );
        }
    }

    #[test]
    fn real_gaps_and_sparse_endpoint_time_are_not_rate_corrected_away() {
        for (gap, explicit, limit) in [
            (100_000_000, false, MAX_PHASE_ERROR),
            (416_667, true, MAX_PHASE_ERROR),
            (100_000_000, false, crate::audio::MIC_PHASE_ERROR),
            (416_667, true, crate::audio::MIC_PHASE_ERROR),
        ] {
            let mut rate = RateFollower::new(2, 480, limit);
            let base = Time::from_nanos(1_000_000_000);
            let mut spans = Vec::new();
            let mut emit = |samples: &[f32], time| spans.push((time, samples.len() / 2));
            for packet in 0..10 {
                rate.push(
                    &[0.0; 960],
                    base.add_ticks(packet * 480, SAMPLE_RATE),
                    false,
                    &mut emit,
                );
            }
            let resumed = Time::from_nanos(base.as_nanos() + 100_000_000 + gap);
            rate.push(&[0.0; 960], resumed, explicit, &mut emit);
            rate.finish(&mut emit);
            let index = spans.iter().position(|&(time, _)| time == resumed).unwrap();
            let (previous, frames) = spans[index - 1];
            assert_eq!(
                previous.ticks_since(base, SAMPLE_RATE) + frames as i64,
                4800
            );
            assert_eq!(resumed.as_nanos() - (base.as_nanos() + 100_000_000), gap);
        }
    }

    #[test]
    fn fractional_rate_conversion_preserves_high_frequency_pcm() {
        for frequency in [10_000.0, 15_000.0, 20_000.0] {
            let mut rate = RateFollower::new(1, 480, MAX_PHASE_ERROR);
            rate.reset(Time::from_nanos(0));
            rate.step = (PHASE as f64 / 1.00004).round() as u64;
            let step = rate.step as f64 / PHASE as f64;
            let omega = 2.0 * std::f64::consts::PI * frequency / SAMPLE_RATE as f64;
            let mut output = Vec::new();
            for packet in 0..200 {
                let samples: Vec<_> = (0..480)
                    .map(|i| ((packet * 480 + i) as f64 * omega).sin() as f32)
                    .collect();
                rate.append(&samples, &mut |samples, _| {
                    output.extend_from_slice(samples)
                });
            }
            rate.finish(&mut |samples, _| output.extend_from_slice(samples));
            let mut error = 0.0;
            let mut signal = 0.0;
            for (index, &sample) in output
                .iter()
                .enumerate()
                .skip(TAPS)
                .take(output.len() - 2 * TAPS)
            {
                let expected = (index as f64 * step * omega).sin();
                error += (f64::from(sample) - expected).powi(2);
                signal += expected.powi(2);
            }
            // Includes gain error: the 20 kHz passband edge is within 0.1 dB.
            let limit = if frequency < 20_000.0 { 0.0001 } else { 0.011 };
            assert!(
                (error / signal).sqrt() < limit,
                "{frequency} Hz: error power {error}, signal {signal}"
            );
        }
    }

    #[test]
    fn idle_endpoint_keeps_its_learned_rate_but_not_a_false_timeline() {
        let mut rate = RateFollower::new(2, 480, MAX_PHASE_ERROR);
        let mut end = None;
        let mut gaps = Vec::new();
        let mut emit = |samples: &[f32], time: Time| {
            if let Some(previous) = end {
                let gap = time.ticks_since(previous, SAMPLE_RATE);
                if gap != 0 {
                    gaps.push(gap);
                }
            }
            end = Some(time.add_ticks((samples.len() / 2) as u64, SAMPLE_RATE));
        };
        for packet in 0..20_000 {
            let idle = if packet >= 10_000 { 3_000_000_000 } else { 0 };
            let time = Time::from_nanos(1_000_000_000 + packet * 10_000_400 + idle);
            rate.push(&[0.0; 960], time, false, &mut emit);
            if packet > 10_000 {
                assert!(rate.phase_error(time.add_ticks(480, SAMPLE_RATE)).abs() < 2.0);
            }
        }
        rate.finish(&mut emit);
        assert_eq!(rate.resets, 1);
        assert_eq!(gaps.len(), 1);
        assert!((gaps[0] - 144_000).abs() <= 2);
    }

    #[test]
    fn sub_millisecond_qpc_noise_does_not_reset_or_step_the_pcm() {
        let mut clean = RateFollower::new(2, 480, MAX_PHASE_ERROR);
        let mut noisy = RateFollower::new(2, 480, MAX_PHASE_ERROR);
        for packet in 0..10_003 {
            let nanos = 1_000_000_000 + packet * 10_000_400;
            let noise = if packet == 10_000 { 900_000 } else { 0 };
            clean.push(&[0.0; 960], Time::from_nanos(nanos), false, &mut |_, _| {});
            noisy.push(
                &[0.0; 960],
                Time::from_nanos(nanos + noise),
                false,
                &mut |_, _| {},
            );
            let correction =
                |r: &RateFollower| r.position as f64 / PHASE as f64 - r.output_frames as f64;
            assert!((correction(&clean) - correction(&noisy)).abs() < 0.2);
        }
        assert_eq!(noisy.resets, 0);
    }
}
