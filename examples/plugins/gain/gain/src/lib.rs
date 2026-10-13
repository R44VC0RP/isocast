//! Gain: raises or lowers the volume, like Isocast's built-in gain filter
//! (the same formula, so the two sound exactly alike). It can also
//! misbehave on purpose (`fault`), to show that a broken plugin can't hurt
//! a recording: Isocast bypasses it for that block, starts a fresh copy,
//! and turns it off after three failures in a minute.

#![no_std]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

#[path = "../../support.rs"]
mod support;

wit_bindgen::generate!({
    world: "audio-filter",
    path: "../../../../wit/plugin.wit",
});

use support::Global;

#[derive(Clone, Copy, PartialEq)]
enum Fault {
    None,
    /// Crash (a wasm trap).
    Trap,
    /// Never return.
    Loop,
    /// Allocate until memory runs out.
    Memory,
}

struct State {
    factor: f32,
    channels: usize,
    fault: Fault,
    /// Frames of sound until it misbehaves, and frames seen so far.
    fault_after: u64,
    frames: u64,
}

static STATE: Global<State> = Global::new(State {
    factor: 1.0,
    channels: 1,
    fault: Fault::None,
    fault_after: 0,
    frames: 0,
});

struct Gain;

impl Guest for Gain {
    fn configure(sample_rate: u32, channels: u32, params: Vec<Param>) -> Result<(), String> {
        let state = STATE.get();
        let gain_db = support::number(&params, "gain_db").unwrap_or(0.0);
        state.factor = support::amplitude(gain_db as f32);
        state.channels = channels.max(1) as usize;
        state.fault = match support::text(&params, "fault").unwrap_or("none") {
            "none" => Fault::None,
            "trap" => Fault::Trap,
            "loop" => Fault::Loop,
            "memory" => Fault::Memory,
            other => return Err(other.to_string() + " isn't a fault; faults: none, trap, loop, memory"),
        };
        let after_ms = support::number(&params, "fault_after_ms").unwrap_or(1000.0).max(0.0);
        state.fault_after = (after_ms * f64::from(sample_rate) / 1000.0) as u64;
        // A fault counts from its settings.
        state.frames = 0;
        // To the plugin's log (`isocast plugin list` says where). In tenths
        // of a dB: formatting floats would make the component 4 times
        // bigger.
        let tenths = (gain_db * 10.0) as i64;
        let sign = if tenths < 0 { "-" } else { "" };
        isocast::plugin::host::log(
            2,
            &alloc::format!(
                "configured: {channels} channels, {sign}{}.{} dB",
                tenths.abs() / 10,
                tenths.abs() % 10
            ),
        );
        Ok(())
    }

    fn latency() -> u32 {
        0
    }

    fn process(mut samples: Vec<f32>, _sidechain: Option<Vec<f32>>) -> Vec<f32> {
        let state = STATE.get();
        state.frames += (samples.len() / state.channels) as u64;
        if state.fault != Fault::None && state.frames > state.fault_after {
            misbehave(state.fault);
        }
        // The block comes back in its own buffer: no allocation.
        for sample in &mut samples {
            *sample *= state.factor;
        }
        samples
    }

    fn reset() {
        STATE.get().frames = 0;
    }
}

fn misbehave(fault: Fault) {
    match fault {
        Fault::None => {}
        Fault::Trap => core::arch::wasm32::unreachable(),
        Fault::Loop => {
            let mut spins = 0u64;
            loop {
                // SAFETY: a read of a local, kept so the loop isn't removed.
                spins = unsafe { core::ptr::read_volatile(&spins) }.wrapping_add(1);
            }
        }
        Fault::Memory => {
            // Megabyte after megabyte, each touched once (not filled, so
            // it is the memory that runs out, not the fuel).
            let mut kept = Vec::new();
            loop {
                let mut block: Vec<u8> = Vec::with_capacity(1 << 20);
                // SAFETY: within the block's capacity.
                unsafe { block.as_mut_ptr().write_volatile(1) };
                kept.push(block);
            }
        }
    }
}

export!(Gain);
