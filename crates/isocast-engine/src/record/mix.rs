//! The program mix: audio sources summed into one stereo track with each
//! source's audio filters, gain, mute and balance (leaving out those
//! monitored only, and those only heard as sidechains), plus level meters.

use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::time::Duration;

use isocast_protocol::{AudioSettings, PanLaw};

use super::engine::Routes;
use super::{AUDIO_CHUNK, AUDIO_JITTER, Packet, SessionControl, Shared};
use crate::audio_filter::{AudioChain, LiveAudioFilters};
use crate::{Platform, SAMPLE_RATE, Time};

/// Live jitter budget, retained even when all input has already arrived.
/// Only a stopping recording or an ending mixer can drain complete blocks
/// early; missing input still gets this allowance while draining.
pub(super) const MIX_DELAY: Duration = Duration::from_millis(200);

/// Monitoring must not inherit the recording jitter budget: late input is
/// omitted from the monitor rather than holding every audible source back.
pub(super) const MONITOR_DELAY: Duration = Duration::from_millis(40);
const MONITOR_DELAY_SAMPLES: i64 = MONITOR_DELAY.as_millis() as i64 * SAMPLE_RATE as i64 / 1000;

/// The most blocks one pass of the monitor mix ([`Mixer::run_monitor`])
/// sends at once: twice the capture allowance (one more for rounding).
/// Filter lookahead can retain older blocks, but never grows a catch-up
/// batch or its channel; the next pass handles the rest.
pub(super) const MONITOR_BURST: usize =
    (2 * MONITOR_DELAY_SAMPLES / AUDIO_CHUNK as i64) as usize + 1;

/// Peak and power of the samples since it was last read.
#[derive(Default)]
pub(super) struct Meter(Mutex<(f32, f64, u64)>);

impl Meter {
    pub(super) fn add(&self, samples: &[f32]) {
        self.add_all(samples.iter().copied());
    }

    /// Add samples one by one, like one side of interleaved audio.
    pub(super) fn add_all(&self, samples: impl Iterator<Item = f32>) {
        let mut state = self.0.lock();
        for sample in samples {
            state.0 = state.0.max(sample.abs());
            state.1 += f64::from(sample) * f64::from(sample);
            state.2 += 1;
        }
    }

    /// Peak and RMS level in dBFS since the last call, and reset.
    pub(super) fn take(&self) -> (f64, f64) {
        let (peak, power, count) = std::mem::take(&mut *self.0.lock());
        let rms = if count == 0 {
            0.0
        } else {
            (power / count as f64).sqrt()
        };
        (decibels(f64::from(peak)), decibels(rms))
    }
}

/// The program mix's meter: interleaved stereo, each channel on its own
/// as well as both together (as `Meter` measures them).
#[derive(Default)]
pub(super) struct StereoMeter(Mutex<([f32; 2], [f64; 2], u64)>);

impl StereoMeter {
    /// Add interleaved stereo frames (left, right, left, right…).
    pub(super) fn add(&self, samples: &[f32]) {
        let mut state = self.0.lock();
        for frame in samples.as_chunks::<2>().0 {
            for (channel, sample) in frame.iter().enumerate() {
                state.0[channel] = state.0[channel].max(sample.abs());
                state.1[channel] += f64::from(*sample) * f64::from(*sample);
            }
            state.2 += 1;
        }
    }

    /// Peak and RMS in dBFS since the last call, both channels together
    /// and each (left, right), and reset.
    pub(super) fn take(&self) -> ((f64, f64), [(f64, f64); 2]) {
        let (peak, power, frames) = std::mem::take(&mut *self.0.lock());
        let rms = |power: f64, count: u64| {
            if count == 0 {
                0.0
            } else {
                (power / count as f64).sqrt()
            }
        };
        let both = (
            decibels(f64::from(peak[0].max(peak[1]))),
            decibels(rms(power[0] + power[1], frames * 2)),
        );
        let each = [0, 1].map(|c| {
            (
                decibels(f64::from(peak[c])),
                decibels(rms(power[c], frames)),
            )
        });
        (both, each)
    }
}

fn decibels(level: f64) -> f64 {
    if level <= 1e-6 {
        -120.0
    } else {
        (20.0 * level.log10()).max(-120.0)
    }
}

/// One source feeding the mix: its audio as stereo frames from sample
/// `start` of the session timeline.
struct Input {
    id: String,
    channels: usize,
    settings: AudioSettings,
    frames: VecDeque<[f32; 2]>,
    start: i64,
    /// Its audio filters, run on its frames just before they are mixed.
    chain: AudioChain,
    /// The next session sample its filters take, ahead of the block being
    /// mixed by their latency; `None` to start afresh.
    fed: Option<i64>,
    /// Heard in the mix; otherwise only by other inputs' sidechains.
    mixed: bool,
}

impl Input {
    fn new(id: String, channels: u32, settings: AudioSettings, mixed: bool) -> Self {
        Self {
            id,
            channels: channels as usize,
            settings,
            frames: VecDeque::new(),
            start: 0,
            chain: AudioChain::new((channels as usize).clamp(1, 2)),
            fed: None,
            mixed,
        }
    }

    /// The frame at session sample `at`, if it has one.
    fn frame(&self, at: i64) -> Option<&[f32; 2]> {
        let index = usize::try_from(at - self.start).ok()?;
        self.frames.get(index)
    }

    fn frame_mut(&mut self, at: i64) -> Option<&mut [f32; 2]> {
        let index = usize::try_from(at - self.start).ok()?;
        self.frames.get_mut(index)
    }

    /// Its `count` frames from `at` as stereo samples, as a sidechain
    /// hears it (silence where it has none): `None` while it is muted.
    fn sound(&self, at: i64, count: usize) -> Option<Vec<f32>> {
        if self.settings.muted {
            return None;
        }
        let mut sound = Vec::with_capacity(count * 2);
        for t in at..at + count as i64 {
            sound.extend_from_slice(self.frame(t).unwrap_or(&[0.0, 0.0]));
        }
        Some(sound)
    }

    /// Run its filters, in place, on its `count` frames from session
    /// sample `at`. A chain that delays the sound (noise suppression) is
    /// fed that much further ahead, so what comes out lands where it was
    /// captured. A muted input is skipped and starts afresh when unmuted.
    fn filter<'k>(&mut self, at: i64, count: usize, key: &dyn Fn(&str) -> Option<&'k [f32]>) {
        if self.chain.is_empty() || self.settings.muted {
            self.fed = None;
            return;
        }
        let latency = self.chain.latency() as i64;
        let end = at + count as i64 + latency;
        let fed = self.fed.filter(|fed| (at..=end).contains(fed));
        if fed.is_none() {
            // Starting afresh (first, after a gap or a mute).
            self.chain.reset();
        }
        let from = fed.unwrap_or(at);
        let channels = self.channels.clamp(1, 2);
        let mut samples = vec![0.0; (end - from) as usize * channels];
        for (k, frame) in samples.chunks_exact_mut(channels).enumerate() {
            if let Some(stereo) = self.frame(from + k as i64) {
                frame.copy_from_slice(&stereo[..channels]);
            }
        }
        self.chain.process(&mut samples, key);
        for (k, frame) in samples.chunks_exact(channels).enumerate() {
            let t = from + k as i64 - latency;
            if t >= at
                && let Some(stereo) = self.frame_mut(t)
            {
                *stereo = [frame[0], frame[channels - 1]];
            }
        }
        self.fed = Some(end);
    }

    /// Add a chunk captured at session sample `at`, keeping the input
    /// continuous: small timestamp jitter is ignored, gaps become silence,
    /// overlaps are trimmed.
    fn push(&mut self, samples: &[f32], at: i64) {
        let channels = self.channels.max(1);
        let stereo = samples.chunks_exact(channels).map(|frame| match frame {
            [mono] => [*mono, *mono],
            [left, right, ..] => [*left, *right],
            [] => [0.0, 0.0],
        });
        let next = self.start + self.frames.len() as i64;
        if self.frames.is_empty() {
            self.start = at;
            self.frames.extend(stereo);
            return;
        }
        if at > next + AUDIO_JITTER {
            let gap = (at - next) as usize;
            self.frames.extend(std::iter::repeat_n([0.0, 0.0], gap));
            self.frames.extend(stereo);
        } else if at < next - AUDIO_JITTER {
            self.frames.extend(stereo.skip((next - at) as usize));
        } else {
            self.frames.extend(stereo);
        }
    }

    /// Drop frames before session sample `at`.
    fn release(&mut self, at: i64) {
        let old = (at - self.start).clamp(0, self.frames.len() as i64) as usize;
        self.frames.drain(..old);
        self.start += old as i64;
        if self.frames.is_empty() {
            self.start = self.start.max(at);
        }
    }
}

/// Which inputs the monitor mix must wait for: those its output depends
/// on. That's every unmuted input heard in the program, and every unmuted
/// input one of those (or of these, in turn) hears through a sidechain of
/// its filters. A muted input is neither mixed, filtered nor heard by a
/// sidechain, and the rest only feed sound that is thrown away.
fn monitor_waits_for(inputs: &[Input]) -> Vec<bool> {
    let mut waits: Vec<bool> = inputs
        .iter()
        .map(|input| !input.settings.muted && input.mixed && input.settings.in_program())
        .collect();
    let mut changed = true;
    while changed {
        changed = false;
        for (i, input) in inputs.iter().enumerate() {
            if !waits[i] {
                continue;
            }
            for id in input.chain.sidechains() {
                for (j, key) in inputs.iter().enumerate() {
                    if key.id == id && !key.settings.muted && !waits[j] {
                        waits[j] = true;
                        changed = true;
                    }
                }
            }
        }
    }
    waits
}

pub(super) struct Mixer<F> {
    inputs: Vec<Input>,
    /// How balance pans the inputs.
    law: PanLaw,
    /// Audio from the inputs, `track` being the input's index.
    packets: Receiver<Packet<F>>,
    meter: Arc<StereoMeter>,
    /// Every source's audio filters, and the revision the inputs have.
    filters: Option<(Arc<LiveAudioFilters>, Option<u64>)>,
}

impl<F: Clone> Mixer<F> {
    /// A mix of the sources `inputs` (id, channels, settings), panned by
    /// `law`, receiving their audio on `packets`.
    pub(super) fn new(
        inputs: Vec<(String, u32, AudioSettings)>,
        law: PanLaw,
        packets: Receiver<Packet<F>>,
        meter: Arc<StereoMeter>,
    ) -> Self {
        Self {
            inputs: inputs
                .into_iter()
                .map(|(id, channels, settings)| Input::new(id, channels, settings, true))
                .collect(),
            law,
            packets,
            meter,
            filters: None,
        }
    }

    /// Also take source `id`'s audio, only for other inputs' sidechains
    /// (it isn't mixed). Returns its track.
    pub(super) fn add_key(&mut self, id: String, channels: u32, settings: AudioSettings) -> usize {
        self.inputs.push(Input::new(id, channels, settings, false));
        self.inputs.len() - 1
    }

    /// Run the inputs' audio filters, taken from `filters` whenever they
    /// change.
    pub(super) fn set_filters(&mut self, filters: Arc<LiveAudioFilters>) {
        self.filters = Some((filters, None));
        self.update_filters();
    }

    fn update_filters(&mut self) {
        let Some((live, revision)) = &mut self.filters else {
            return;
        };
        let current = live.revision();
        if *revision == Some(current) {
            return;
        }
        *revision = Some(current);
        for input in &mut self.inputs {
            let latency = input.chain.latency();
            input.chain.set_place(live.place(&input.id, "mix"));
            input.chain.update(&live.of(&input.id));
            if input.chain.latency() != latency {
                input.fed = None;
            }
        }
    }

    /// Mix from session sample `from` until the session ends, sending each
    /// block to `outputs`.
    pub(super) fn run<P: Platform>(
        &mut self,
        platform: &P,
        shared: &Shared,
        from: i64,
        control: Option<&SessionControl>,
        outputs: &Routes<F>,
    ) {
        let epoch = shared.epoch;
        // The next session sample to mix.
        let mut out: i64 = from;
        // Stop may compute at most MIX_DELAY of audio ahead of live delivery.
        // Only recordings get that tail early; a concurrent stream must not
        // receive a burst when another output stops. Keep the same PTS/order.
        let mut pending: VecDeque<(Time, Time, Arc<[f32]>)> = VecDeque::new();
        let deliver_pending = |pending: &mut VecDeque<(Time, Time, Arc<[f32]>)>, now| {
            while pending.front().is_some_and(|(due, ..)| *due <= now) {
                let (_, pts, samples) = pending.pop_front().expect("checked");
                outputs.audio_live(&samples, pts);
            }
        };
        loop {
            let end = shared.end();
            let last = if end == Time::MAX {
                i64::MAX
            } else {
                end.ticks_since(epoch, SAMPLE_RATE)
            };
            if !pending.is_empty() {
                deliver_pending(&mut pending, platform.now());
            }
            if out >= last && (pending.is_empty() || outputs.is_empty()) {
                break;
            }
            match self.packets.recv_timeout(Duration::from_millis(5)) {
                Ok(Packet::Audio {
                    track,
                    samples,
                    pts,
                }) => {
                    if let Some(input) = self.inputs.get_mut(track) {
                        input.push(&samples, pts.ticks_since(epoch, SAMPLE_RATE));
                    }
                }
                Ok(Packet::Video { .. } | Packet::Skipped { .. } | Packet::AudioGap { .. })
                | Err(RecvTimeoutError::Timeout) => {}
                // Every source has stopped: mix silence until the end.
                Err(RecvTimeoutError::Disconnected) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            if let Some(control) = control {
                self.law = control.pan_law();
                for input in &mut self.inputs {
                    if let Some(settings) = control.audio_settings(&input.id) {
                        input.settings = settings;
                    }
                }
            }
            self.update_filters();
            let block = AUDIO_CHUNK as i64;
            while out < last {
                let count = block.min(last - out) as usize;
                let until = out + count as i64;
                let due = epoch.add_ticks(until as u64, SAMPLE_RATE);
                let now = platform.now();
                // Flush older live blocks before publishing any newer one,
                // including after a scheduler stall while receiving input.
                deliver_pending(&mut pending, now);
                if now < due + MIX_DELAY {
                    let draining = end != Time::MAX
                        || control.is_some_and(|control| {
                            // Include the block straddling the final video
                            // boundary; the writer trims its excess samples.
                            epoch.add_ticks(out as u64, SAMPLE_RATE).as_nanos()
                                < control.mix_drain_until.load(Ordering::Acquire)
                        });
                    // Never generate future audio or discard missing input
                    // to finish sooner. Include sidechains and lookahead.
                    let ready = draining
                        && now >= due
                        && !self.inputs.is_empty()
                        && self.inputs.iter().all(|input| {
                            input.start + input.frames.len() as i64
                                >= until + input.chain.latency() as i64
                        });
                    if !ready {
                        break;
                    }
                }
                let samples = self.mix(out, count);
                self.meter.add(&samples);
                let samples: Arc<[f32]> = samples.into();
                let pts = epoch.add_ticks(out as u64, SAMPLE_RATE);
                if now < due + MIX_DELAY {
                    if outputs.audio_recordings(&samples, pts) {
                        pending.push_back((due + MIX_DELAY, pts, samples));
                    }
                } else {
                    outputs.audio(&samples, pts);
                }
                out += count as i64;
            }
        }
    }

    /// Mix a low-latency copy of the program for live monitoring (WHEP)
    /// from session sample `from`, sending each 10 ms block to `output` as
    /// track 0's audio, until `alive` says to stop or `output` is closed.
    /// A full output skips only this monitor's block. Never used for recordings:
    /// run it on its own `Mixer`, with
    /// its own inputs, filters and meter, so it can't change their mix.
    ///
    /// A block goes out as soon as it is due and every input it depends on
    /// (sidechains and filter lookahead included) has arrived; a late one
    /// gets [`MONITOR_DELAY`] of capture slack in addition to that input's
    /// filter lookahead before missing input becomes silence.
    /// The recording mix retains its separate [`MIX_DELAY`]. Never waits on `output`.
    pub(super) fn run_monitor<P: Platform>(
        &mut self,
        platform: &P,
        epoch: Time,
        from: i64,
        control: &SessionControl,
        output: SyncSender<Packet<F>>,
        mut alive: impl FnMut() -> bool,
    ) {
        // The next session sample to mix.
        let mut out = from;
        while alive() {
            match self.packets.recv_timeout(Duration::from_millis(5)) {
                Ok(Packet::Audio {
                    track,
                    samples,
                    pts,
                }) => {
                    if let Some(input) = self.inputs.get_mut(track) {
                        input.push(&samples, pts.ticks_since(epoch, SAMPLE_RATE));
                    }
                }
                Ok(Packet::Video { .. } | Packet::Skipped { .. } | Packet::AudioGap { .. })
                | Err(RecvTimeoutError::Timeout) => {}
                // Every source has stopped: mix silence until told to stop.
                Err(RecvTimeoutError::Disconnected) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            self.law = control.pan_law();
            for input in &mut self.inputs {
                if let Some(settings) = control.audio_settings(&input.id) {
                    input.settings = settings;
                }
            }
            self.update_filters();
            if !self.monitor_due(platform.now(), epoch, &mut out, &output) {
                break;
            }
        }
    }

    /// Mix and send the monitor's blocks from session sample `out` that
    /// are ready at `now` (see [`Mixer::run_monitor`]), advancing `out`.
    /// False once `output` is closed; full queues lose monitor audio only.
    fn monitor_due(
        &mut self,
        now: Time,
        epoch: Time,
        out: &mut i64,
        output: &SyncSender<Packet<F>>,
    ) -> bool {
        let block = AUDIO_CHUNK as i64;
        let waits = monitor_waits_for(&self.inputs);
        let lookahead = self
            .inputs
            .iter()
            .zip(&waits)
            .filter(|(_, waits)| **waits)
            .map(|(input, _)| input.chain.latency() as i64)
            .max()
            .unwrap_or(0);
        // After a stall of more than MONITOR_DELAY past the late-input
        // allowance AND required lookahead, skip ahead. Normal filter latency
        // is not a stall and must not discard otherwise timely capture.
        let behind = now.ticks_since(epoch, SAMPLE_RATE) - MONITOR_DELAY_SAMPLES - lookahead;
        if *out < behind - MONITOR_DELAY_SAMPLES {
            *out += (behind - *out) / block * block;
            for input in &mut self.inputs {
                input.release(*out);
            }
        }
        for _ in 0..MONITOR_BURST {
            let until = *out + block;
            let due = epoch.add_ticks(until as u64, SAMPLE_RATE);
            // Never mix ahead of the clock.
            if now < due {
                return true;
            }
            if self.inputs.iter().zip(&waits).any(|(input, waits)| {
                let needed = until + input.chain.latency() as i64;
                *waits
                    && input.start + (input.frames.len() as i64) < needed
                    && now < epoch.add_ticks(needed as u64, SAMPLE_RATE) + MONITOR_DELAY
            }) {
                return true;
            }
            let samples = self.mix(*out, block as usize);
            let packet = Packet::Audio {
                track: 0,
                samples: samples.into(),
                pts: epoch.add_ticks(*out as u64, SAMPLE_RATE),
            };
            *out = until;
            match output.try_send(packet) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => return false,
            }
        }
        true
    }

    /// `count` stereo frames of the mix from session sample `at`.
    fn mix(&mut self, at: i64, count: usize) -> Vec<f32> {
        self.filter(at, count);
        let mut mixed = vec![0.0f32; count * 2];
        for input in &mut self.inputs {
            // Sources monitored only, or only heard as sidechains, are not
            // mixed.
            let gains = match input.mixed && input.settings.in_program() {
                true => input.settings.gains(self.law),
                false => [0.0; 2],
            };
            if gains != [0.0; 2] {
                for (i, frame) in mixed.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                    let index = at + i as i64 - input.start;
                    if let Some([left, right]) = usize::try_from(index)
                        .ok()
                        .and_then(|index| input.frames.get(index))
                    {
                        let (left, right) = if input.settings.mono {
                            let mono = (left + right) * 0.5;
                            (mono, mono)
                        } else {
                            (*left, *right)
                        };
                        frame[0] += left * gains[0];
                        frame[1] += right * gains[1];
                    }
                }
            }
            input.release(at + count as i64);
        }
        mixed
    }

    /// Run every input's filters on its `count` frames from session sample
    /// `at` (see [`Input::filter`]). Sidechains hear their source after its
    /// own filters, so those go first; where sidechains form a loop, one
    /// hears its source unfiltered.
    fn filter(&mut self, at: i64, count: usize) {
        if self.inputs.iter().all(|input| input.chain.is_empty()) {
            return;
        }
        let ids: Vec<String> = self.inputs.iter().map(|input| input.id.clone()).collect();
        let index = |id: &str| ids.iter().position(|other| other == id);
        // The sound sidechains hear, by input.
        let mut keys: Vec<Option<Vec<f32>>> = vec![None; self.inputs.len()];
        for input in &self.inputs {
            for j in input.chain.sidechains().filter_map(index) {
                keys[j] = self.inputs[j].sound(at, count);
            }
        }
        let mut done = vec![false; self.inputs.len()];
        let ready = |i: usize, done: &[bool], inputs: &[Input]| {
            !done[i]
                && inputs[i]
                    .chain
                    .sidechains()
                    .all(|id| index(id).is_none_or(|j| done[j] || j == i))
        };
        while let Some(i) = (0..done.len())
            .find(|&i| ready(i, &done, &self.inputs))
            .or_else(|| done.iter().position(|done| !done))
        {
            done[i] = true;
            let key = |id: &str| index(id).and_then(|j| keys[j].as_deref());
            self.inputs[i].filter(at, count, &key);
            if keys[i].is_some() {
                keys[i] = self.inputs[i].sound(at, count);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::sync_channel;

    use super::*;

    fn mixer(inputs: Vec<(String, u32, AudioSettings)>) -> Mixer<()> {
        let (_, rx) = sync_channel(1);
        Mixer::new(inputs, PanLaw::default(), rx, Arc::default())
    }

    #[test]
    fn inputs_are_mixed_with_gain_and_mute_mono_in_both_channels() {
        let half = AudioSettings {
            gain_db: -6.0,
            ..AudioSettings::default()
        };
        let muted = AudioSettings {
            muted: true,
            ..AudioSettings::default()
        };
        let mut mixer = mixer(vec![
            ("mono".into(), 1, half),
            ("stereo".into(), 2, AudioSettings::default()),
            ("off".into(), 2, muted),
        ]);
        mixer.inputs[0].push(&[1.0; 480], 0);
        mixer.inputs[1].push(&[0.25, -0.25].repeat(480), 0);
        mixer.inputs[2].push(&[1.0; 960], 0);
        let mixed = mixer.mix(0, 480);
        let gain = half.factor();
        assert!((gain - 0.501).abs() < 0.001);
        for frame in mixed.chunks(2) {
            assert!((frame[0] - (gain + 0.25)).abs() < 1e-6, "{frame:?}");
            assert!((frame[1] - (gain - 0.25)).abs() < 1e-6, "{frame:?}");
        }
    }

    #[test]
    fn balance_lowers_the_far_side_under_the_pan_law() {
        let at = |balance| AudioSettings {
            balance,
            ..AudioSettings::default()
        };
        let mut mixer = mixer(vec![
            ("right".into(), 1, at(0.5)),
            ("left".into(), 2, at(-1.0)),
        ]);
        mixer.law = PanLaw::Linear;
        mixer.inputs[0].push(&[0.5; 480], 0);
        mixer.inputs[1].push(&[0.0, 0.25].repeat(480), 0);
        // The mono source at half right: left at half (linear), right as
        // is; the stereo one fully left: its right side gone.
        for frame in mixer.mix(0, 480).chunks(2) {
            assert_eq!(frame, [0.25, 0.5]);
        }
    }

    #[test]
    fn gaps_are_silent_and_mixed_audio_is_released() {
        let mut mixer = mixer(vec![("m".into(), 1, AudioSettings::default())]);
        mixer.inputs[0].push(&[1.0; 480], 0);
        mixer.inputs[0].push(&[1.0; 480], 960);
        assert_eq!(mixer.mix(0, 480), vec![1.0; 960]);
        assert_eq!(mixer.mix(480, 480), vec![0.0; 960]);
        assert_eq!(mixer.mix(960, 480), vec![1.0; 960]);
        assert!(mixer.inputs[0].frames.is_empty());
        // Audio for a block already mixed is too late and is dropped.
        mixer.inputs[0].push(&[1.0; 480], 0);
        assert_eq!(mixer.mix(1440, 480), vec![0.0; 960]);
    }

    #[test]
    fn filters_run_before_mixing_and_sidechains_duck_without_being_mixed() {
        let setup = isocast_protocol::Setup::from_json(
            r#"{"version": 1, "name": "t", "canvas": {"width": 8, "height": 8, "fps": 30},
                "sources": [
                  {"id": "music", "kind": "tone", "audio_filters": [
                    {"kind": "gain", "gain_db": -6},
                    {"kind": "compressor", "ratio": 32, "threshold_db": -36, "attack_ms": 1,
                     "release_ms": 1, "sidechain_source": "voice"}]},
                  {"id": "voice", "kind": "tone"}],
                "recording": {"folder": ".", "program": {"encoder": {"codec": "hevc"}}}}"#,
        )
        .unwrap();
        let live = Arc::new(LiveAudioFilters::default());
        live.set(&setup);
        let mut mixer = mixer(vec![("music".into(), 1, AudioSettings::default())]);
        let voice = mixer.add_key("voice".into(), 2, AudioSettings::default());
        mixer.set_filters(live);
        // Half a second of steady music, the voice speaking in the second
        // quarter only.
        mixer.inputs[0].push(&[0.5; 24_000], 0);
        let speaking = |i: usize| (6_000..12_000).contains(&(i / 2));
        let voice_samples: Vec<f32> = (0..48_000)
            .map(|i| if speaking(i) { 0.25 } else { 0.0 })
            .collect();
        mixer.inputs[voice].push(&voice_samples, 0);
        let mixed: Vec<f32> = (0..50).flat_map(|n| mixer.mix(n * 480, 480)).collect();
        let music = 0.5 * 10f32.powf(-6.0 / 20.0);
        // The voice isn't heard; while it speaks (0.25, about 24 dB over
        // the threshold) the music goes down by the excess times 31/32.
        let excess = 20.0 * 0.25f32.log10() + 36.0;
        let ducked = music * 10f32.powf(-excess * 31.0 / 32.0 / 20.0);
        for (at, expected) in [(5_000, music), (11_000, ducked), (23_000, music)] {
            let [left, right] = [mixed[at * 2], mixed[at * 2 + 1]];
            assert!(
                (left - expected).abs() < 1e-4,
                "{at}: {left} not {expected}"
            );
            assert_eq!(left, right);
        }
    }

    /// Every source's audio filters from `sources` (setup JSON).
    fn live_filters(sources: &str) -> Arc<LiveAudioFilters> {
        let setup = isocast_protocol::Setup::from_json(&format!(
            r#"{{"version": 1, "name": "t", "canvas": {{"width": 8, "height": 8, "fps": 30}},
                "sources": {sources},
                "recording": {{"folder": ".", "program": {{"encoder": {{"codec": "hevc"}}}}}}}}"#
        ))
        .unwrap();
        let live = Arc::new(LiveAudioFilters::default());
        live.set(&setup);
        live
    }

    const EPOCH: Time = Time::from_nanos(1_000_000_000);

    fn at_ms(ms: u64) -> Time {
        EPOCH + Duration::from_millis(ms)
    }

    /// The monitor's blocks sent so far, as (pts, samples).
    fn sent(rx: &Receiver<Packet<()>>) -> Vec<(Time, Vec<f32>)> {
        rx.try_iter()
            .map(|packet| match packet {
                Packet::Audio {
                    track,
                    samples,
                    pts,
                } => {
                    assert_eq!(track, 0);
                    (pts, samples.to_vec())
                }
                _ => panic!("the monitor sends only audio"),
            })
            .collect()
    }

    #[test]
    fn monitor_sends_a_block_once_it_is_due_and_every_input_has_it() {
        let mut mixer = mixer(vec![
            ("a".into(), 1, AudioSettings::default()),
            ("b".into(), 1, AudioSettings::default()),
        ]);
        let (tx, rx) = sync_channel(MONITOR_BURST);
        let mut out = 0;
        mixer.inputs[0].push(&[0.25; 960], 0);
        // Due, but "b" hasn't arrived: it waits.
        assert!(mixer.monitor_due(at_ms(10), EPOCH, &mut out, &tx));
        assert!(sent(&rx).is_empty());
        assert_eq!(out, 0);
        mixer.inputs[1].push(&[0.5; 960], 0);
        // Both have 20 ms, but at 15 ms only the first block is due: the
        // monitor never mixes ahead of the clock.
        assert!(mixer.monitor_due(at_ms(15), EPOCH, &mut out, &tx));
        let blocks = sent(&rx);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].0, EPOCH);
        assert_eq!(blocks[0].1, vec![0.75; 960]);
        assert_eq!(out, 480);
        assert!(mixer.monitor_due(at_ms(20), EPOCH, &mut out, &tx));
        let blocks = sent(&rx);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].0, at_ms(10));
        assert_eq!(out, 960);
    }

    #[test]
    fn monitor_drops_late_input_after_its_own_short_allowance() {
        let mut mixer = mixer(vec![
            ("on time".into(), 1, AudioSettings::default()),
            ("late".into(), 1, AudioSettings::default()),
        ]);
        let (tx, rx) = sync_channel(MONITOR_BURST);
        let mut out = 0;
        mixer.inputs[0].push(&[0.25; 960], 0);
        let deadline = at_ms(10) + MONITOR_DELAY;
        assert!(mixer.monitor_due(deadline - Duration::from_nanos(1), EPOCH, &mut out, &tx));
        assert!(sent(&rx).is_empty());
        assert!(mixer.monitor_due(deadline, EPOCH, &mut out, &tx));
        let blocks = sent(&rx);
        // Only the first block's allowance is over.
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].1, vec![0.25; 960]);
        // What arrives for it afterwards is too late; the next block mixes
        // as soon as it has everything.
        mixer.inputs[1].push(&[0.5; 960], 0);
        assert!(mixer.monitor_due(deadline, EPOCH, &mut out, &tx));
        let blocks = sent(&rx);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].0, at_ms(10));
        assert_eq!(blocks[0].1, vec![0.75; 960]);
    }

    #[test]
    fn monitor_does_not_wait_for_sources_it_does_not_hear() {
        let muted = AudioSettings {
            muted: true,
            ..AudioSettings::default()
        };
        let monitored = AudioSettings {
            monitor: isocast_protocol::Monitoring::MonitorOnly,
            ..AudioSettings::default()
        };
        let mut mixer = mixer(vec![
            ("heard".into(), 1, AudioSettings::default()),
            ("muted".into(), 1, muted),
            ("monitored".into(), 1, monitored),
        ]);
        let (tx, rx) = sync_channel(MONITOR_BURST);
        let mut out = 0;
        mixer.inputs[0].push(&[0.25; 480], 0);
        assert!(mixer.monitor_due(at_ms(10), EPOCH, &mut out, &tx));
        assert_eq!(sent(&rx), vec![(EPOCH, vec![0.25; 960])]);
    }

    #[test]
    fn monitor_waits_for_filter_lookahead_and_sidechain_keys() {
        // Noise suppression works 20 ms (960 samples) behind.
        let live = live_filters(
            r#"[{"id": "mic", "kind": "tone", "audio_filters": [{"kind": "noise_suppression"}]},
                {"id": "music", "kind": "tone", "audio_filters": [
                  {"kind": "compressor", "sidechain_source": "voice"}]},
                {"id": "voice", "kind": "tone"}]"#,
        );
        let mut mixer = mixer(vec![
            ("mic".into(), 1, AudioSettings::default()),
            ("music".into(), 1, AudioSettings::default()),
        ]);
        let voice = mixer.add_key("voice".into(), 1, AudioSettings::default());
        mixer.set_filters(live);
        assert_eq!(mixer.inputs[0].chain.latency(), 960);
        let (tx, rx) = sync_channel(MONITOR_BURST);
        let mut out = 0;
        let now = at_ms(10);
        mixer.inputs[0].push(&[0.1; 480], 0);
        mixer.inputs[1].push(&[0.1; 480], 0);
        assert!(mixer.monitor_due(now, EPOCH, &mut out, &tx));
        assert!(sent(&rx).is_empty(), "waits for the mic's lookahead");
        mixer.inputs[0].push(&[0.1; 960], 480);
        assert!(mixer.monitor_due(now, EPOCH, &mut out, &tx));
        assert!(sent(&rx).is_empty(), "waits for the music's sidechain");
        mixer.inputs[voice].push(&[0.1; 480], 0);
        assert!(mixer.monitor_due(now, EPOCH, &mut out, &tx));
        assert_eq!(sent(&rx).len(), 1);
        // A muted key isn't heard, so it isn't waited for.
        mixer.inputs[voice].settings.muted = true;
        mixer.inputs[0].push(&[0.1; 480], 1440);
        mixer.inputs[1].push(&[0.1; 480], 480);
        assert!(mixer.monitor_due(at_ms(20), EPOCH, &mut out, &tx));
        assert_eq!(sent(&rx).len(), 1);
    }

    #[test]
    fn monitor_keeps_lookahead_audio_with_thirty_ms_capture_delay() {
        // One denoiser needs 20 ms; three also exercise lookahead longer
        // than the monitor's 40 ms capture budget / original stall window.
        for count in [1, 3] {
            let filters = vec![r#"{"kind":"noise_suppression"}"#; count].join(",");
            let live = live_filters(&format!(
                r#"[{{"id":"mic","kind":"tone","audio_filters":[{filters}]}}]"#
            ));
            let make = || {
                let mut mix = mixer(vec![("mic".into(), 1, AudioSettings::default())]);
                mix.set_filters(live.clone());
                mix
            };
            let (mut monitor, mut reference) = (make(), make());
            let latency = monitor.inputs[0].chain.latency();
            assert_eq!(latency, count * 960);
            let mut seed = 17_u32;
            let samples: Vec<_> = (0..57_600)
                .map(|_| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (seed >> 8) as f32 / (1 << 24) as f32 - 0.5
                })
                .collect();
            reference.inputs[0].push(&samples, 0);
            let (tx, rx) = sync_channel(MONITOR_BURST);
            let (mut out, mut next, mut blocks, mut audible) = (0, 0, 0, false);
            for ms in (0..=1000).step_by(5) {
                while (next + 480) / 48 + 30 <= ms {
                    monitor.inputs[0].push(&samples[next..next + 480], next as i64);
                    next += 480;
                }
                assert!(monitor.monitor_due(at_ms(ms as u64), EPOCH, &mut out, &tx));
                for (pts, actual) in sent(&rx) {
                    let at = pts.ticks_since(EPOCH, SAMPLE_RATE);
                    assert_eq!(at, blocks * 480, "no valid capture blocks skipped");
                    assert!(
                        at_ms(ms as u64)
                            >= pts.add_ticks((480 + latency) as u64, SAMPLE_RATE)
                                + Duration::from_millis(30),
                        "lookahead was committed before capture arrived"
                    );
                    let expected = reference.mix(at, 480);
                    assert_eq!(
                        actual.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
                        expected.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
                    );
                    audible |= actual.iter().any(|v| v.abs() > 1e-8);
                    blocks += 1;
                }
            }
            assert!(
                blocks >= 90 && audible,
                "the comparison must include real filtered audio"
            );
            // Longer lookahead must not overflow the fixed output queue
            // when a worker catches up: leave remaining blocks for next pass.
            let mut stalled = make();
            stalled.inputs[0].push(&samples, 0);
            let mut out = 0;
            assert!(stalled.monitor_due(at_ms(200), EPOCH, &mut out, &tx));
            let batch = sent(&rx);
            assert!(!batch.is_empty() && batch.len() <= MONITOR_BURST);
            assert_eq!(
                out,
                batch.last().unwrap().0.ticks_since(EPOCH, SAMPLE_RATE) + 480
            );
        }
    }

    #[test]
    fn monitor_does_not_extend_plain_capture_for_another_chains_lookahead() {
        let mut monitor = mixer(vec![
            ("mic".into(), 1, AudioSettings::default()),
            ("late".into(), 1, AudioSettings::default()),
        ]);
        monitor.set_filters(live_filters(
            r#"[{"id":"mic","kind":"tone","audio_filters":[{"kind":"noise_suppression"}]}]"#,
        ));
        monitor.inputs[0].push(&[0.25; 4800], 0);
        let (tx, rx) = sync_channel(MONITOR_BURST);
        let mut out = 0;
        assert!(monitor.monitor_due(at_ms(50), EPOCH, &mut out, &tx));
        let batch = sent(&rx);
        assert_eq!(batch.len(), 1, "unfiltered input still expires after 40 ms");
        assert_eq!(batch[0].0, EPOCH);
    }

    #[test]
    fn monitor_skips_ahead_after_a_stall_instead_of_bursting() {
        let mut mixer = mixer(vec![("m".into(), 1, AudioSettings::default())]);
        let (tx, rx) = sync_channel(MONITOR_BURST);
        let mut out = 0;
        mixer.inputs[0].push(&[0.25; 48_000], 0);
        // A second without running: what is more than MONITOR_DELAY behind
        // its own allowance is dropped, and what's left fits the channel.
        assert!(mixer.monitor_due(at_ms(1000), EPOCH, &mut out, &tx));
        let blocks = sent(&rx);
        assert!(
            !blocks.is_empty() && blocks.len() <= MONITOR_BURST,
            "{}",
            blocks.len()
        );
        assert!(blocks[0].0 >= at_ms(1000) - MONITOR_DELAY - Duration::from_millis(10));
        assert_eq!(out, 48_000);
        assert!(
            blocks
                .iter()
                .all(|(_, samples)| *samples == vec![0.25; 960])
        );
        // The longest hold within the allowance fits as well.
        let mut mixer = self::mixer(vec![("m".into(), 1, AudioSettings::default())]);
        let mut out = 0;
        mixer.inputs[0].push(&[0.25; 48_000], 0);
        assert!(mixer.monitor_due(at_ms(80), EPOCH, &mut out, &tx));
        assert_eq!(sent(&rx).len(), MONITOR_BURST - 1);
    }

    #[test]
    fn monitor_skips_full_output_but_stops_when_closed() {
        let mut mixer = mixer(vec![("m".into(), 1, AudioSettings::default())]);
        mixer.inputs[0].push(&[0.25; 4800], 0);
        let (tx, rx) = sync_channel(1);
        let mut out = 0;
        assert!(mixer.monitor_due(at_ms(20), EPOCH, &mut out, &tx));
        assert_eq!(sent(&rx).len(), 1);
        assert!(mixer.monitor_due(at_ms(30), EPOCH, &mut out, &tx));
        assert_eq!(sent(&rx)[0].0, at_ms(20));
        drop(rx);
        assert!(!mixer.monitor_due(at_ms(40), EPOCH, &mut out, &tx));
    }

    /// The program mix of a session with filters (lookahead, sidechain),
    /// late and out-of-order capture and a gain change, mixed as the
    /// recording mixer does (each block at its deadline), keeps every
    /// sample bit for bit while an early monitor mix of the same capture
    /// runs beside it. They share filter definitions, not filter instances
    /// or input buffers; feeding the same immutable capture trace catches
    /// hidden global DSP state that could otherwise couple their output.
    #[test]
    fn an_early_monitor_mix_leaves_the_program_mix_bit_for_bit() {
        let live = live_filters(
            r#"[{"id": "mic", "kind": "tone", "audio_filters": [
                  {"kind": "gain", "gain_db": -3}, {"kind": "noise_suppression"}]},
                {"id": "music", "kind": "tone", "audio_filters": [
                  {"kind": "compressor", "ratio": 8, "threshold_db": -30, "attack_ms": 1,
                   "release_ms": 20, "sidechain_source": "voice"}]},
                {"id": "voice", "kind": "tone"}]"#,
        );
        let make = |meter: Arc<StereoMeter>| {
            let (_, rx) = sync_channel(1);
            let mut mixer: Mixer<()> = Mixer::new(
                vec![
                    ("mic".into(), 1, AudioSettings::default()),
                    ("music".into(), 2, AudioSettings::default()),
                ],
                PanLaw::default(),
                rx,
                meter,
            );
            mixer.add_key("voice".into(), 2, AudioSettings::default());
            mixer.set_filters(live.clone());
            mixer
        };
        let (reference_meter, program_meter) = (Arc::default(), Arc::default());
        let mut reference = make(Arc::clone(&reference_meter));
        let mut program = make(Arc::clone(&program_meter));
        let mut monitor = make(Arc::default());
        // Deterministic noise, shared as capture is.
        let mut seed = 0x2545_f491_u32;
        let mut noise = |n: usize| -> Arc<[f32]> {
            (0..n)
                .map(|_| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (seed >> 8) as f32 / (1 << 24) as f32 - 0.5
                })
                .collect()
        };
        // (input, session sample, samples, delivered at session sample):
        // the mic in odd-sized chunks, the voice 30 ms late with a stretch
        // 300 ms late (past the allowance), the music on time.
        let mut capture = Vec::new();
        for j in 0..150 {
            let at = j * 336;
            capture.push((0, at, noise(336), at + 336));
        }
        for j in 0..100 {
            let at = j * 480;
            let late = if (40..60).contains(&j) { 14_400 } else { 1440 };
            capture.push((2, at, noise(960), at + 480 + late));
            capture.push((1, at, noise(960), at + 480));
        }
        capture.sort_by_key(|(_, _, _, delivered)| *delivered);
        let (tx, rx) = sync_channel(10_000);
        let (mut out, mut mixed, mut early) = (0, 0, 0);
        let (mut reference_blocks, mut program_blocks) = (Vec::new(), Vec::new());
        let mut next = 0;
        for now in (0..60_000).step_by(96) {
            while capture
                .get(next)
                .is_some_and(|(_, _, _, delivered)| *delivered <= now)
            {
                let (input, at, samples, _) = &capture[next];
                for mixer in [&mut reference, &mut program, &mut monitor] {
                    mixer.inputs[*input].push(samples, *at);
                }
                next += 1;
            }
            if now == 24_000 {
                for mixer in [&mut reference, &mut program, &mut monitor] {
                    mixer.inputs[0].settings.gain_db = -10.0;
                    mixer.law = PanLaw::Linear;
                }
            }
            let time = EPOCH.add_ticks(now as u64, SAMPLE_RATE);
            assert!(monitor.monitor_due(time, EPOCH, &mut out, &tx));
            for (pts, _) in sent(&rx) {
                let due = pts.add_ticks(480, SAMPLE_RATE) + MIX_DELAY;
                early += usize::from(time < due);
            }
            // The recording mixer's schedule: each block at its deadline.
            while EPOCH.add_ticks(mixed as u64 + 480, SAMPLE_RATE) + MIX_DELAY <= time {
                reference_blocks.push(reference.mix(mixed, 480));
                program_blocks.push(program.mix(mixed, 480));
                program.meter.add(program_blocks.last().unwrap());
                reference.meter.add(reference_blocks.last().unwrap());
                mixed += 480;
            }
        }
        assert!(program_blocks.len() > 100, "{}", program_blocks.len());
        assert!(early > 50, "the monitor mixed early only {early} times");
        let bits = |blocks: &[Vec<f32>]| -> Vec<u32> {
            blocks
                .iter()
                .flatten()
                .map(|sample| sample.to_bits())
                .collect()
        };
        assert_eq!(bits(&program_blocks), bits(&reference_blocks));
        assert!(bits(&program_blocks).iter().any(|&b| b != 0));
        let ((peak, rms), each) = program_meter.take();
        let ((ref_peak, ref_rms), ref_each) = reference_meter.take();
        assert_eq!(
            [peak, rms, each[0].0, each[0].1, each[1].0, each[1].1].map(f64::to_bits),
            [
                ref_peak,
                ref_rms,
                ref_each[0].0,
                ref_each[0].1,
                ref_each[1].0,
                ref_each[1].1
            ]
            .map(f64::to_bits)
        );
        for (program, reference) in program.inputs.iter().zip(&reference.inputs) {
            assert_eq!(
                (program.start, program.fed),
                (reference.start, reference.fed)
            );
            let frames = |input: &Input| -> Vec<u32> {
                input.frames.iter().flatten().map(|s| s.to_bits()).collect()
            };
            assert_eq!(frames(program), frames(reference));
        }
    }

    #[test]
    fn meters_report_peak_and_rms_in_dbfs() {
        let meter = Meter::default();
        meter.add(&[0.5, -0.5, 0.5, -0.5]);
        let (peak, rms) = meter.take();
        assert!(
            (peak + 6.02).abs() < 0.01 && (rms + 6.02).abs() < 0.01,
            "{peak} {rms}"
        );
        assert_eq!(meter.take(), (-120.0, -120.0));
    }

    #[test]
    fn stereo_meter_reports_each_channel_and_both() {
        let meter = StereoMeter::default();
        meter.add(&[0.5, 0.0, -0.5, 0.0]);
        let ((peak, rms), [left, right]) = meter.take();
        assert!((peak - -6.02).abs() < 0.01);
        assert!((rms - -9.03).abs() < 0.01);
        assert!((left.0 - -6.02).abs() < 0.01 && (left.1 - -6.02).abs() < 0.01);
        assert_eq!(right, (-120.0, -120.0));
    }
}
