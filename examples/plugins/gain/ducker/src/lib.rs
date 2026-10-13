//! Ducker: turns this source down while its sidechain source (a voice,
//! say) makes sound, like Isocast's built-in compressor with a
//! `sidechain_source`, written as a plugin. The level of the sidechain is
//! its loudest channel, held for 10 ms and then falling, followed with
//! attack and release time constants; above the threshold this source is
//! reduced by the excess times 1 − 1/ratio.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

#[path = "../../support.rs"]
mod support;

wit_bindgen::generate!({
    world: "audio-filter",
    path: "../../../../wit/plugin.wit",
});

use support::Global;

/// A peak level is held this long (10 ms at 48 kHz) before it falls.
const HOLD: u32 = 480;
/// How fast a held peak falls after that, as a time constant.
const PEAK_FALL_MS: f32 = 10.0;

struct State {
    sample_rate: f32,
    channels: usize,
    ratio: f32,
    threshold_db: f32,
    /// How far the level moves towards a louder or quieter one each frame.
    attack: f32,
    release: f32,
    /// What a held peak is multiplied by each frame once the hold is over.
    fall: f32,
    peak: f32,
    held: u32,
    /// The followed level, as an amplitude.
    level: f32,
}

static STATE: Global<State> = Global::new(State {
    sample_rate: 48_000.0,
    channels: 1,
    ratio: 10.0,
    threshold_db: -18.0,
    attack: 1.0,
    release: 1.0,
    fall: 0.0,
    peak: 0.0,
    held: 0,
    level: 0.0,
});

impl State {
    /// What a level is multiplied by each frame to fall by 63% in `ms`.
    fn falloff(&self, ms: f32) -> f32 {
        let frames = (ms * self.sample_rate / 1000.0).max(1.0);
        support::exp(f64::from(-1.0 / frames)) as f32
    }

    /// The level after a frame whose loudest sample is `peak`.
    fn hold(&mut self, peak: f32) -> f32 {
        if peak >= self.peak {
            self.peak = peak;
            self.held = 0;
        } else if self.held < HOLD {
            self.held += 1;
        } else {
            self.peak = (self.peak * self.fall).max(peak);
        }
        self.peak
    }
}

struct Ducker;

impl Guest for Ducker {
    fn configure(sample_rate: u32, channels: u32, params: Vec<Param>) -> Result<(), String> {
        let state = STATE.get();
        let number = |name, default: f64| support::number(&params, name).unwrap_or(default) as f32;
        state.sample_rate = sample_rate as f32;
        state.channels = channels.max(1) as usize;
        state.ratio = number("ratio", 10.0);
        state.threshold_db = number("threshold_db", -18.0);
        state.attack = 1.0 - state.falloff(number("attack_ms", 6.0));
        state.release = 1.0 - state.falloff(number("release_ms", 60.0));
        state.fall = state.falloff(PEAK_FALL_MS);
        Ok(())
    }

    fn latency() -> u32 {
        0
    }

    fn process(mut samples: Vec<f32>, sidechain: Option<Vec<f32>>) -> Vec<f32> {
        let state = STATE.get();
        let key = sidechain.as_deref().unwrap_or(&[]);
        for (i, frame) in samples.chunks_exact_mut(state.channels).enumerate() {
            // The sidechain's loudest channel; silence while it has none.
            let loudest = match key.get(2 * i..2 * i + 2) {
                Some([left, right]) => left.abs().max(right.abs()),
                _ => 0.0,
            };
            let level = state.hold(loudest);
            let speed = if level > state.level {
                state.attack
            } else {
                state.release
            };
            state.level += (level - state.level) * speed;
            let over = support::decibels(state.level) - state.threshold_db;
            if over > 0.0 {
                let gain = support::amplitude(-over * (1.0 - 1.0 / state.ratio));
                frame.iter_mut().for_each(|s| *s *= gain);
            }
        }
        samples
    }

    fn reset() {
        let state = STATE.get();
        (state.peak, state.held, state.level) = (0.0, 0, 0.0);
    }
}

export!(Ducker);
