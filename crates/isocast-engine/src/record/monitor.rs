//! Audio monitoring: sources heard through an output device while
//! recording, after gain, mute and balance, as soon as they are captured
//! (not 200 ms behind like the program mix).
//!
//! Each monitored source's audio waits in its own buffer and the device
//! takes from all of them at once. A source starts playing (and, after
//! running out, starts again) only once its buffer holds its largest chunk,
//! the device's buffer and a small margin, so sources that deliver in
//! bursts play without gaps; each gap it still hits adds to its margin.
//! Audio piling up beyond that (a device clock running slower than the
//! source's) is dropped, so the delay stays short.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use isocast_protocol::api::MonitorStatus;
use isocast_protocol::{AudioSettings, Monitoring, PanLaw};

use super::SessionControl;
use super::mix::Meter;
use crate::{Platform, SAMPLE_RATE};

/// The margin a source starts with (5 ms), how much each gap adds, and the
/// most it can grow to (100 ms).
const FIRST_MARGIN: usize = SAMPLE_RATE as usize / 200;
const MARGIN_STEP: usize = SAMPLE_RATE as usize / 200;
const MAX_MARGIN: usize = SAMPLE_RATE as usize / 10;
/// How far past what it needs in hand a source's buffer may grow before
/// the oldest audio is dropped (50 ms).
const MAX_EXCESS: usize = SAMPLE_RATE as usize / 20;

/// What the session's monitoring device plays.
pub(super) struct Monitor {
    /// The output device, as the setup names it.
    device: String,
    state: Mutex<State>,
    /// Left and right levels of what was played.
    meters: [Meter; 2],
    /// Sample frames played.
    frames: AtomicU64,
    underruns: AtomicU64,
    /// Play silence (`ISOCAST_MONITOR_SILENT` is set), still taking and
    /// metering everything as if it were heard.
    silent: bool,
}

#[derive(Default)]
struct State {
    /// The buffers of the sources being heard, by id.
    inputs: BTreeMap<String, Buffer>,
    /// Whether opening the device has been tried.
    opened: bool,
    /// The device playing, and its name.
    playback: Option<Box<dyn Send>>,
    output: Option<String>,
    /// Why the device couldn't be opened.
    error: Option<String>,
    /// The most sample frames the device has asked for at once.
    request: usize,
}

/// One source's audio waiting to be played, in stereo after its gains.
struct Buffer {
    frames: VecDeque<[f32; 2]>,
    /// Holding back until it has enough in hand.
    waiting: bool,
    /// The largest chunk the source has delivered, in frames.
    chunk: usize,
    margin: usize,
}

impl Buffer {
    fn new() -> Self {
        Self {
            frames: VecDeque::new(),
            waiting: true,
            chunk: 0,
            margin: FIRST_MARGIN,
        }
    }

    /// What it keeps in hand when the device asks for `request` frames at
    /// a time.
    fn target(&self, request: usize) -> usize {
        self.chunk + request + self.margin
    }
}

impl Monitor {
    /// A monitor playing through `device`, which opens once a source is
    /// heard.
    pub(super) fn new(device: &str) -> Self {
        Self {
            device: device.to_string(),
            state: Mutex::default(),
            meters: Default::default(),
            frames: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            silent: std::env::var_os("ISOCAST_MONITOR_SILENT")
                .is_some_and(|value| !value.is_empty() && value != "0"),
        }
    }

    /// The output device, as the setup names it.
    pub(super) fn device(&self) -> &str {
        &self.device
    }

    /// Open the device, unless that has been tried. Returns why it couldn't
    /// be opened, the first time only.
    pub(super) fn open<P: Platform>(self: &Arc<Self>, platform: &P) -> Option<String> {
        {
            let mut state = self.state.lock().unwrap();
            if std::mem::replace(&mut state.opened, true) {
                return None;
            }
        }
        // The device holds only a weak reference, or the monitor would keep
        // its own playback alive.
        let monitor = Arc::downgrade(self);
        let render = Box::new(move |out: &mut [f32]| match monitor.upgrade() {
            Some(monitor) => monitor.render(out),
            None => out.fill(0.0),
        });
        let opened = platform.play_audio(&self.device, render);
        let mut state = self.state.lock().unwrap();
        match opened {
            Ok((playback, name)) => {
                state.playback = Some(Box::new(playback));
                state.output = Some(name);
                None
            }
            Err(err) => {
                let message = format!("{err:#}");
                state.error = Some(message.clone());
                Some(message)
            }
        }
    }

    /// Stop the device.
    fn close(&self) {
        let playback = self.state.lock().unwrap().playback.take();
        drop(playback);
    }

    /// Queue `samples` of source `id` (interleaved, `channels` per frame),
    /// scaled by left and right `gains`.
    fn push(&self, id: &str, samples: &[f32], channels: usize, gains: [f32; 2]) {
        let channels = channels.max(1);
        let mut state = self.state.lock().unwrap();
        let request = state.request;
        let buffer = state
            .inputs
            .entry(id.to_string())
            .or_insert_with(Buffer::new);
        buffer.chunk = buffer.chunk.max(samples.len() / channels);
        buffer
            .frames
            .extend(samples.chunks_exact(channels).map(|frame| match frame {
                [mono] => [mono * gains[0], mono * gains[1]],
                [left, right, ..] => [left * gains[0], right * gains[1]],
                [] => [0.0; 2],
            }));
        let target = buffer.target(request);
        if buffer.frames.len() > target + buffer.chunk + MAX_EXCESS {
            let excess = buffer.frames.len() - target;
            buffer.frames.drain(..excess);
        }
    }

    /// Stop playing source `id`.
    fn remove(&self, id: &str) {
        self.state.lock().unwrap().inputs.remove(id);
    }

    /// Fill `out` (interleaved stereo) with the sources being heard. Called
    /// by the device.
    fn render(&self, out: &mut [f32]) {
        out.fill(0.0);
        let count = out.len() / 2;
        let mut underruns = 0;
        {
            let mut state = self.state.lock().unwrap();
            state.request = state.request.max(count);
            let request = state.request;
            for buffer in state.inputs.values_mut() {
                if buffer.waiting {
                    let target = buffer.target(request);
                    if buffer.frames.len() < target {
                        continue;
                    }
                    // Start with just what it needs in hand, however far
                    // the chunk that got it there went over.
                    let excess = buffer.frames.len() - target;
                    buffer.frames.drain(..excess);
                    buffer.waiting = false;
                }
                let available = buffer.frames.len().min(count);
                let frames = out.as_chunks_mut::<2>().0.iter_mut();
                for (frame, [left, right]) in frames.zip(buffer.frames.drain(..available)) {
                    frame[0] += left;
                    frame[1] += right;
                }
                if available < count {
                    buffer.waiting = true;
                    buffer.margin = (buffer.margin + MARGIN_STEP).min(MAX_MARGIN);
                    underruns += 1;
                }
            }
        }
        self.meters[0].add_all(out.iter().step_by(2).copied());
        self.meters[1].add_all(out.iter().skip(1).step_by(2).copied());
        self.frames.fetch_add(count as u64, Ordering::Relaxed);
        self.underruns.fetch_add(underruns, Ordering::Relaxed);
        if self.silent {
            out.fill(0.0);
        }
    }

    /// Live status, once a source has been heard; resets the levels.
    pub(super) fn status(&self) -> Option<MonitorStatus> {
        let state = self.state.lock().unwrap();
        if !state.opened {
            return None;
        }
        let [left, right] = [self.meters[0].take(), self.meters[1].take()];
        let buffered = state.inputs.values().map(|b| b.frames.len()).max();
        Some(MonitorStatus {
            device: self.device.clone(),
            output: state.output.clone(),
            error: state.error.clone(),
            sources: state.inputs.keys().cloned().collect(),
            peak_db: [left.0, right.0],
            rms_db: [left.1, right.1],
            frames: self.frames.load(Ordering::Relaxed),
            underruns: self.underruns.load(Ordering::Relaxed),
            buffered_ms: buffered.unwrap_or(0) as f64 * 1000.0 / f64::from(SAMPLE_RATE),
            silent: self.silent,
        })
    }
}

/// Stops a monitor's device when dropped, on the session's own thread
/// (never the device's, which could otherwise hold the last reference).
pub(super) struct Closing(pub(super) Arc<Monitor>);

impl Drop for Closing {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// The monitor sources are heard through, which changes when the setup
/// picks another device.
pub(super) type MonitorSlot = Mutex<Arc<Monitor>>;

/// Feeds one audio source to the monitor whenever its settings say it is
/// heard.
pub(super) struct Tap {
    monitor: Arc<MonitorSlot>,
    /// The monitor it last played through.
    playing: Option<Arc<Monitor>>,
    id: String,
    channels: usize,
    /// Its settings and the pan law; followed live when the session has a
    /// control.
    settings: AudioSettings,
    law: PanLaw,
    heard: bool,
}

impl Tap {
    pub(super) fn new(
        monitor: Arc<MonitorSlot>,
        id: &str,
        channels: u32,
        settings: AudioSettings,
        law: PanLaw,
    ) -> Self {
        Self {
            monitor,
            playing: None,
            id: id.to_string(),
            channels: channels as usize,
            settings,
            law,
            heard: false,
        }
    }

    /// Pass on a chunk of the source's audio, if it is being heard.
    pub(super) fn play<P: Platform>(
        &mut self,
        platform: &P,
        control: Option<&SessionControl>,
        samples: &[f32],
    ) {
        if let Some(control) = control {
            if let Some(settings) = control.audio_settings(&self.id) {
                self.settings = settings;
            }
            self.law = control.pan_law();
        }
        let heard = self.settings.monitor != Monitoring::Off;
        let monitor = self.monitor.lock().unwrap().clone();
        // The setup changed the device: stop playing through the old one.
        if let Some(old) = self.playing.take_if(|old| !Arc::ptr_eq(old, &monitor)) {
            old.remove(&self.id);
        }
        if heard {
            if let Some(err) = monitor.open(platform) {
                isocast_diagnostics::diagnostic!("warn";
                    "isocastd: could not play monitored audio on {:?}: {err}",
                    monitor.device
                );
            }
            let gains = self.settings.gains(self.law);
            monitor.push(&self.id, samples, self.channels, gains);
            self.playing = Some(monitor);
        } else if self.heard {
            monitor.remove(&self.id);
            self.playing = None;
        }
        self.heard = heard;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_play_once_they_have_enough_and_wait_again_after_a_gap() {
        // Heard, even when the tests run with ISOCAST_MONITOR_SILENT set.
        let monitor = Monitor {
            silent: false,
            ..Monitor::new("default")
        };
        let mut out = vec![0.0; 2 * 128];
        // The device asks for 128 frames at a time; the source delivers
        // 480-frame chunks (mono, at half gain on the right).
        monitor.render(&mut out);
        let target = 480 + 128 + FIRST_MARGIN;
        let chunk = vec![0.5; 480];
        monitor.push("m", &chunk, 1, [1.0, 0.5]);
        monitor.render(&mut out);
        assert!(out.iter().all(|&s| s == 0.0), "not enough in hand yet");
        monitor.push("m", &chunk, 1, [1.0, 0.5]);
        assert!(960 >= target);
        monitor.render(&mut out);
        assert!(out.chunks(2).all(|f| f == [0.5, 0.25]), "{:?}", &out[..4]);
        // It started with just its target in hand.
        let left = monitor.state.lock().unwrap().inputs["m"].frames.len();
        assert_eq!(left, target - 128);
        // Drain it: the last render runs short, counts a gap, and the
        // source waits for more, with a larger margin.
        for _ in 0..7 {
            monitor.render(&mut out);
        }
        assert_eq!(monitor.underruns.load(Ordering::Relaxed), 1);
        let state = monitor.state.lock().unwrap();
        let buffer = &state.inputs["m"];
        assert!(buffer.waiting && buffer.frames.is_empty());
        assert_eq!(buffer.margin, FIRST_MARGIN + MARGIN_STEP);
    }

    #[test]
    fn audio_piling_up_is_dropped_down_to_what_is_needed() {
        // Heard, even when the tests run with ISOCAST_MONITOR_SILENT set.
        let monitor = Monitor {
            silent: false,
            ..Monitor::new("default")
        };
        let mut out = vec![0.0; 2 * 512];
        monitor.render(&mut out);
        for _ in 0..100 {
            monitor.push("m", &[0.1; 960], 2, [1.0, 1.0]);
        }
        let state = monitor.state.lock().unwrap();
        let buffer = &state.inputs["m"];
        assert!(buffer.frames.len() <= buffer.target(512) + 480 + MAX_EXCESS);
        assert!(buffer.frames.len() >= buffer.target(512));
    }
}
