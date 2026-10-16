//! Audio filters: each source's chain of processors, run on its sound at
//! 48 kHz before the program mix (and for filtered ISO files). The
//! formulas are ours; OBS documents only the settings.
//!
//! Level-driven filters (gate, compressor, limiter, expander, upward
//! compressor) measure each frame by its loudest channel and change every
//! channel alike, so a stereo image stays put. Their peak level is held for
//! 10 ms before it falls, so a steady tone reads as its peak without
//! ripple, and attack and release are time constants of the level they
//! follow.
//!
//! To add a filter: its settings in `isocast_protocol::audio_filter` and
//! its arms in [`Processor::new`], [`Processor::set`] and
//! [`Processor::process`].

use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::f64::consts::{PI, SQRT_2};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use isocast_protocol::{AudioFilter, AudioFilterKind, Detection, Setup};
use nnnoiseless::DenoiseState;

use crate::SAMPLE_RATE;
use crate::wasm::{Place, PluginFilter, Reports};

/// How long a peak level is held before it falls: a tone above 50 Hz
/// peaks again within it, so it reads steady.
const HOLD: u32 = SAMPLE_RATE / 100;
/// How fast a held peak falls after that, as a time constant.
const PEAK_FALL_MS: f32 = 10.0;
/// How many frames RMS detection averages (10 ms).
const RMS_WINDOW: usize = SAMPLE_RATE as usize / 100;
/// The quietest level filters tell apart, so silence has a level in dB.
const FLOOR_DB: f32 = -120.0;
/// The upward compressor raises sound under this no more than sound at it.
const UPWARD_FLOOR_DB: f32 = -60.0;
/// Where the equalizer's bands meet, in Hz.
const LOW_CROSSOVER: f64 = 250.0;
const HIGH_CROSSOVER: f64 = 4000.0;
/// RNNoise works on frames of 10 ms, in 16-bit sample values.
const DENOISE_FRAME: usize = DenoiseState::FRAME_SIZE;
const DENOISE_SCALE: f32 = 32768.0;

/// Every source's audio filters, by source id, which can change while
/// recording; the mixer and filtered ISO audio notice by the revision.
#[derive(Default)]
pub(crate) struct LiveAudioFilters {
    /// Each source's place in the setup and its filters.
    filters: Mutex<HashMap<String, (usize, Vec<AudioFilter>)>>,
    revision: AtomicU64,
    /// Where plugins' filters report (warnings, counters).
    pub(crate) reports: Arc<Reports>,
}

impl LiveAudioFilters {
    /// Take `setup`'s sources' audio filters.
    pub(crate) fn set(&self, setup: &Setup) {
        let filters: HashMap<String, (usize, Vec<AudioFilter>)> = setup
            .sources
            .iter()
            .enumerate()
            .filter(|(_, source)| !source.audio_filters.is_empty())
            .map(|(i, source)| (source.id.clone(), (i, source.audio_filters.clone())))
            .collect();
        let mut current = self.filters.lock();
        if *current != filters {
            *current = filters;
            self.revision.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Bumped by every change, and whenever the installed plugins' audio
    /// filters change (so a plugin added later is picked up).
    pub(crate) fn revision(&self) -> u64 {
        self.revision.load(Ordering::SeqCst) + crate::wasm::generation()
    }

    /// Source `id`'s filters.
    pub(crate) fn of(&self, id: &str) -> Vec<AudioFilter> {
        let filters = self.filters.lock();
        filters.get(id).map(|(_, f)| f.clone()).unwrap_or_default()
    }

    /// Where source `id`'s chain that filters for `runs_in` (`mix` or
    /// `iso`) is, for its plugin filters' reports.
    pub(crate) fn place(&self, id: &str, runs_in: &'static str) -> Place {
        let filters = self.filters.lock();
        Place {
            source: filters.get(id).map(|(i, _)| *i),
            runs_in,
            reports: self.reports.clone(),
        }
    }
}

/// A source's audio filters with their state, for interleaved audio of a
/// fixed number of channels.
pub(crate) struct AudioChain {
    channels: usize,
    links: Vec<Link>,
    /// Where it is, for plugin filters' warnings.
    place: Place,
}

struct Link {
    key: String,
    kind: String,
    enabled: bool,
    processor: Processor,
}

impl AudioChain {
    pub(crate) fn new(channels: usize) -> Self {
        Self {
            channels: channels.max(1),
            links: Vec::new(),
            place: Place::default(),
        }
    }

    /// Say where the chain is (before [`AudioChain::update`]).
    pub(crate) fn set_place(&mut self, place: Place) {
        self.place = place;
    }

    /// Take a new list of filters. Filters that stay (same key and kind)
    /// keep their state, so changing a setting doesn't click; a filter
    /// switched back on starts afresh.
    pub(crate) fn update(&mut self, filters: &[AudioFilter]) {
        let mut old = std::mem::take(&mut self.links);
        for (index, filter) in filters.iter().enumerate() {
            let (key, kind) = (filter.key(), filter.kind.label());
            let kept = old
                .iter()
                .position(|link| link.key == key && link.kind == kind)
                .map(|i| old.remove(i));
            let processor = match kept {
                Some(mut link) if link.enabled || !filter.enabled => {
                    link.processor.set(&filter.kind);
                    link.processor
                }
                // A plugin's filter that is off makes no instance until on.
                _ if !filter.enabled && matches!(filter.kind, AudioFilterKind::Plugin(_)) => {
                    Processor::Gain(1.0)
                }
                _ => Processor::new(&filter.kind, self.channels, &self.place, index),
            };
            self.links.push(Link {
                key: key.to_string(),
                kind: kind.to_string(),
                enabled: filter.enabled,
                processor,
            });
        }
    }

    /// The sound starts afresh after a gap: plugin filters forget what
    /// came before (built-in ones carry on, as they always have).
    pub(crate) fn reset(&mut self) {
        for link in self.links.iter_mut().filter(|link| link.enabled) {
            if let Processor::Plugin(plugin) = &mut link.processor {
                plugin.reset();
            }
        }
    }

    /// No filter is on.
    pub(crate) fn is_empty(&self) -> bool {
        !self.links.iter().any(|link| link.enabled)
    }

    /// How many frames late the filtered sound comes out.
    pub(crate) fn latency(&self) -> usize {
        self.enabled().map(|link| link.processor.latency()).sum()
    }

    /// The sources whose sound drives filters that are on.
    pub(crate) fn sidechains(&self) -> impl Iterator<Item = &str> {
        self.enabled().filter_map(|link| link.processor.sidechain())
    }

    fn enabled(&self) -> impl Iterator<Item = &Link> {
        self.links.iter().filter(|link| link.enabled)
    }

    /// Filter interleaved `samples` in place. `key` gives a sidechain
    /// source's sound for the same frames as stereo samples, or `None` when
    /// it is muted or not at hand.
    pub(crate) fn process<'k>(
        &mut self,
        samples: &mut [f32],
        key: &dyn Fn(&str) -> Option<&'k [f32]>,
    ) {
        for link in self.links.iter_mut().filter(|link| link.enabled) {
            let key = link.processor.sidechain().map(key);
            link.processor.process(samples, self.channels, key);
        }
    }
}

/// A source's filters for its ISO files (`recording.iso.filtered`), run on
/// its chunks as they arrive. Sidechains are left out: they need the other
/// source's sound at the same moment, which only the mixer has.
pub(crate) struct AudioStage {
    id: String,
    chain: AudioChain,
    live: Arc<LiveAudioFilters>,
    revision: Option<u64>,
}

impl AudioStage {
    pub(crate) fn new(id: &str, channels: usize, live: Arc<LiveAudioFilters>) -> Self {
        Self {
            id: id.to_string(),
            chain: AudioChain::new(channels),
            live,
            revision: None,
        }
    }

    /// `samples` filtered, and how many frames earlier than they were
    /// captured they belong (the chain's latency).
    pub(crate) fn process(&mut self, samples: &[f32]) -> (Vec<f32>, usize) {
        let revision = self.live.revision();
        if self.revision != Some(revision) {
            self.revision = Some(revision);
            self.chain.set_place(self.live.place(&self.id, "iso"));
            self.chain.update(&self.live.of(&self.id));
        }
        let mut samples = samples.to_vec();
        if !self.chain.is_empty() {
            self.chain.process(&mut samples, &|_| None);
        }
        (samples, self.chain.latency())
    }
}

/// One filter's settings and state.
enum Processor {
    /// A gain factor.
    Gain(f32),
    Invert,
    Gate(Gate),
    Suppressor(Suppressor),
    Dynamics(Dynamics),
    Limiter(Limiter),
    Equalizer(Equalizer),
    /// A plugin's filter: a WebAssembly component's instance.
    Plugin(Box<PluginFilter>),
}

impl Processor {
    /// A new processor for `kind`, filter `index` of a chain at `place`.
    fn new(kind: &AudioFilterKind, channels: usize, place: &Place, index: usize) -> Self {
        let mut processor = match kind {
            AudioFilterKind::Plugin(plugin) => {
                return Self::Plugin(Box::new(PluginFilter::new(plugin, channels, place, index)));
            }
            AudioFilterKind::Gain(_) => Self::Gain(1.0),
            AudioFilterKind::InvertPolarity(_) => Self::Invert,
            AudioFilterKind::NoiseGate(_) => Self::Gate(Gate::default()),
            AudioFilterKind::NoiseSuppression(_) => Self::Suppressor(Suppressor::new(channels)),
            AudioFilterKind::Compressor(_)
            | AudioFilterKind::Expander(_)
            | AudioFilterKind::UpwardCompressor(_) => Self::Dynamics(Dynamics::default()),
            AudioFilterKind::Limiter(_) => Self::Limiter(Limiter::default()),
            AudioFilterKind::Equalizer(_) => Self::Equalizer(Equalizer::new(channels)),
        };
        processor.set(kind);
        processor
    }

    /// Take new settings for the same kind, keeping the state.
    fn set(&mut self, kind: &AudioFilterKind) {
        match (self, kind) {
            (Self::Gain(factor), AudioFilterKind::Gain(gain)) => *factor = amplitude(gain.gain_db),
            (Self::Gate(gate), AudioFilterKind::NoiseGate(settings)) => {
                gate.open_at = amplitude(settings.open_threshold_db);
                gate.close_at = amplitude(settings.close_threshold_db);
                gate.attack = 1.0 / frames(settings.attack_ms).max(1.0);
                gate.release = 1.0 / frames(settings.release_ms).max(1.0);
                gate.hold = frames(settings.hold_ms) as u32;
                gate.fall = falloff(PEAK_FALL_MS);
            }
            (Self::Dynamics(dynamics), AudioFilterKind::Compressor(settings)) => {
                dynamics.curve = Curve::Compress {
                    threshold: settings.threshold_db,
                    ratio: settings.ratio,
                };
                dynamics.detection = Detection::Peak;
                dynamics.sidechain = settings.sidechain_source.clone();
                // Ducking leaves the level alone while the other source is
                // quiet, as OBS does.
                let output = match dynamics.sidechain {
                    Some(_) => 0.0,
                    None => settings.output_gain_db,
                };
                dynamics.times(settings.attack_ms, settings.release_ms, output);
            }
            (Self::Dynamics(dynamics), AudioFilterKind::Expander(settings)) => {
                dynamics.curve = Curve::Expand {
                    threshold: settings.threshold_db,
                    ratio: settings.ratio,
                };
                dynamics.detection = settings.detection;
                dynamics.sidechain = None;
                dynamics.times(
                    settings.attack_ms,
                    settings.release_ms,
                    settings.output_gain_db,
                );
            }
            (Self::Dynamics(dynamics), AudioFilterKind::UpwardCompressor(settings)) => {
                dynamics.curve = Curve::Raise {
                    threshold: settings.threshold_db,
                    ratio: settings.ratio,
                };
                dynamics.detection = settings.detection;
                dynamics.sidechain = None;
                dynamics.times(
                    settings.attack_ms,
                    settings.release_ms,
                    settings.output_gain_db,
                );
            }
            (Self::Limiter(limiter), AudioFilterKind::Limiter(settings)) => {
                limiter.threshold = amplitude(settings.threshold_db);
                limiter.fall = falloff(settings.release_ms);
            }
            (Self::Equalizer(equalizer), AudioFilterKind::Equalizer(settings)) => {
                equalizer.gains = [settings.low_db, settings.mid_db, settings.high_db]
                    .map(|db| f64::from(amplitude(db)));
            }
            (Self::Plugin(filter), AudioFilterKind::Plugin(plugin)) => filter.set(plugin),
            _ => {}
        }
    }

    fn latency(&self) -> usize {
        match self {
            Self::Suppressor(_) => Suppressor::LATENCY,
            Self::Plugin(filter) => filter.latency(),
            _ => 0,
        }
    }

    fn sidechain(&self) -> Option<&str> {
        match self {
            Self::Dynamics(dynamics) => dynamics.sidechain.as_deref(),
            Self::Plugin(filter) => filter.sidechain(),
            _ => None,
        }
    }

    /// `key` is `Some` for a filter with a sidechain: the other source's
    /// sound as stereo samples, if it isn't muted.
    fn process(&mut self, samples: &mut [f32], channels: usize, key: Option<Option<&[f32]>>) {
        match self {
            Self::Gain(factor) => samples.iter_mut().for_each(|s| *s *= *factor),
            Self::Invert => samples.iter_mut().for_each(|s| *s = -*s),
            Self::Gate(gate) => gate.process(samples, channels),
            Self::Suppressor(suppressor) => suppressor.process(samples, channels),
            Self::Dynamics(dynamics) => dynamics.process(samples, channels, key),
            Self::Limiter(limiter) => limiter.process(samples, channels),
            Self::Equalizer(equalizer) => equalizer.process(samples, channels),
            Self::Plugin(filter) => filter.process(samples, key.flatten()),
        }
    }
}

/// A level in dB as an amplitude factor.
fn amplitude(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// An amplitude in dB, no lower than [`FLOOR_DB`].
fn decibels(level: f32) -> f32 {
    (20.0 * level.max(1e-6).log10()).max(FLOOR_DB)
}

/// Milliseconds in frames.
fn frames(ms: f32) -> f32 {
    ms * SAMPLE_RATE as f32 / 1000.0
}

/// How far to move towards a target each frame to get 63% of the way
/// there in `ms` (a time constant).
fn smoothing(ms: f32) -> f32 {
    1.0 - falloff(ms)
}

/// What a level is multiplied by each frame to fall by 63% in `ms`.
fn falloff(ms: f32) -> f32 {
    (-1.0 / frames(ms).max(1.0)).exp()
}

/// The loudest sample of a frame.
fn peak(frame: &[f32]) -> f32 {
    frame.iter().fold(0.0, |peak: f32, s| peak.max(s.abs()))
}

/// A frame's peak level held for [`HOLD`], then falling.
#[derive(Default)]
struct PeakHold {
    level: f32,
    held: u32,
}

impl PeakHold {
    /// The level after a frame whose loudest sample is `peak`; once the
    /// hold is over it is multiplied by `fall` each frame, never going
    /// below the frame's peak.
    fn next(&mut self, peak: f32, fall: f32) -> f32 {
        if peak >= self.level {
            self.level = peak;
            self.held = 0;
        } else if self.held < HOLD {
            self.held += 1;
        } else {
            self.level = (self.level * fall).max(peak);
        }
        self.level
    }
}

/// The RMS level over the last [`RMS_WINDOW`] frames.
#[derive(Default)]
struct Rms {
    squares: Vec<f32>,
    next: usize,
    sum: f64,
}

impl Rms {
    fn next(&mut self, frame: &[f32]) -> f32 {
        if self.squares.is_empty() {
            self.squares = vec![0.0; RMS_WINDOW];
        }
        let square = frame.iter().map(|s| s * s).sum::<f32>() / frame.len().max(1) as f32;
        self.sum += f64::from(square) - f64::from(self.squares[self.next]);
        self.squares[self.next] = square;
        self.next = (self.next + 1) % RMS_WINDOW;
        if self.next == 0 {
            // Start the running sum afresh so rounding can't build up.
            self.sum = self.squares.iter().map(|s| f64::from(*s)).sum();
        }
        (self.sum.max(0.0) / RMS_WINDOW as f64).sqrt() as f32
    }
}

/// Opens when the level rises above one threshold and closes, after a hold,
/// when it falls below another, fading in and out linearly.
#[derive(Default)]
struct Gate {
    open_at: f32,
    close_at: f32,
    /// Gain change per frame while opening and closing.
    attack: f32,
    release: f32,
    /// Frames to stay open once under the close threshold.
    hold: u32,
    fall: f32,
    peak: PeakHold,
    open: bool,
    held: u32,
    gain: f32,
}

impl Gate {
    fn process(&mut self, samples: &mut [f32], channels: usize) {
        for frame in samples.chunks_exact_mut(channels) {
            let level = self.peak.next(peak(frame), self.fall);
            if !self.open {
                if level >= self.open_at {
                    self.open = true;
                    self.held = 0;
                }
            } else if level < self.close_at {
                if self.held >= self.hold {
                    self.open = false;
                } else {
                    self.held += 1;
                }
            } else {
                self.held = 0;
            }
            self.gain = match self.open {
                true => (self.gain + self.attack).min(1.0),
                false => (self.gain - self.release).max(0.0),
            };
            frame.iter_mut().for_each(|s| *s *= self.gain);
        }
    }
}

/// How a level-driven filter changes the gain for a level, in dB.
#[derive(Default, Clone, Copy)]
enum Curve {
    #[default]
    Flat,
    /// Above the threshold, reduce by the excess times 1 − 1/ratio.
    Compress { threshold: f32, ratio: f32 },
    /// Below the threshold, reduce by the shortfall times ratio − 1.
    Expand { threshold: f32, ratio: f32 },
    /// Below the threshold, raise by the shortfall times 1 − 1/ratio.
    Raise { threshold: f32, ratio: f32 },
}

impl Curve {
    fn gain_db(self, level: f32) -> f32 {
        match self {
            Self::Compress { threshold, ratio } if level > threshold => {
                -(level - threshold) * (1.0 - 1.0 / ratio)
            }
            Self::Expand { threshold, ratio } if level < threshold => {
                ((level - threshold) * (ratio - 1.0)).max(FLOOR_DB)
            }
            Self::Raise { threshold, ratio } if level < threshold => {
                (threshold - level.max(UPWARD_FLOOR_DB)) * (1.0 - 1.0 / ratio)
            }
            _ => 0.0,
        }
    }
}

/// The compressor, expander and upward compressor: a level followed with
/// attack (rising) and release (falling) smoothing, put through a curve.
#[derive(Default)]
struct Dynamics {
    curve: Curve,
    detection: Detection,
    attack: f32,
    release: f32,
    /// Output gain, as a factor.
    output: f32,
    sidechain: Option<String>,
    fall: f32,
    peak: PeakHold,
    rms: Rms,
    /// The smoothed level, as an amplitude.
    level: f32,
}

impl Dynamics {
    fn times(&mut self, attack_ms: f32, release_ms: f32, output_db: f32) {
        self.attack = smoothing(attack_ms);
        self.release = smoothing(release_ms);
        self.output = amplitude(output_db);
        self.fall = falloff(PEAK_FALL_MS);
    }

    fn process(&mut self, samples: &mut [f32], channels: usize, key: Option<Option<&[f32]>>) {
        for (i, frame) in samples.chunks_exact_mut(channels).enumerate() {
            let level = match key {
                // A silent or missing sidechain reads as silence; its level
                // is its loudest channel.
                Some(key) => {
                    let level = key
                        .and_then(|key| key.get(2 * i..2 * i + 2))
                        .map_or(0.0, |lr| lr[0].abs().max(lr[1].abs()));
                    self.peak.next(level, self.fall)
                }
                None if self.detection == Detection::Rms => self.rms.next(frame),
                None => self.peak.next(peak(frame), self.fall),
            };
            let speed = if level > self.level {
                self.attack
            } else {
                self.release
            };
            self.level += (level - self.level) * speed;
            let gain = amplitude(self.curve.gain_db(decibels(self.level))) * self.output;
            frame.iter_mut().for_each(|s| *s *= gain);
        }
    }
}

/// Never lets a sample past the threshold: the gain follows the held peak
/// at once and recovers with the release time.
#[derive(Default)]
struct Limiter {
    threshold: f32,
    fall: f32,
    peak: PeakHold,
}

impl Limiter {
    fn process(&mut self, samples: &mut [f32], channels: usize) {
        for frame in samples.chunks_exact_mut(channels) {
            // At least the frame's own peak, so the result is at most the
            // threshold.
            let level = self.peak.next(peak(frame), self.fall);
            if level > self.threshold {
                let gain = self.threshold / level;
                frame.iter_mut().for_each(|s| *s *= gain);
            }
        }
    }
}

/// Three bands split by Linkwitz-Riley crossovers (two Butterworth
/// sections each), which add back up to the input's level at every
/// frequency; the low band passes an all-pass to keep in phase with the
/// others.
struct Equalizer {
    /// Low, mid and high gain factors.
    gains: [f64; 3],
    /// Each channel's filters.
    channels: Vec<Bands>,
}

impl Equalizer {
    fn new(channels: usize) -> Self {
        Self {
            gains: [1.0; 3],
            channels: (0..channels).map(|_| Bands::new()).collect(),
        }
    }

    fn process(&mut self, samples: &mut [f32], channels: usize) {
        for frame in samples.chunks_exact_mut(channels) {
            for (sample, bands) in frame.iter_mut().zip(&mut self.channels) {
                let split = bands.split(f64::from(*sample));
                *sample = (0..3).map(|i| split[i] * self.gains[i]).sum::<f64>() as f32;
            }
        }
    }
}

struct Bands {
    low: [Biquad; 2],
    low_phase: Biquad,
    above_low: [Biquad; 2],
    mid: [Biquad; 2],
    high: [Biquad; 2],
}

impl Bands {
    fn new() -> Self {
        let pair = |pass, frequency| [Biquad::new(pass, frequency); 2];
        Self {
            low: pair(Pass::Low, LOW_CROSSOVER),
            low_phase: Biquad::new(Pass::All, HIGH_CROSSOVER),
            above_low: pair(Pass::High, LOW_CROSSOVER),
            mid: pair(Pass::Low, HIGH_CROSSOVER),
            high: pair(Pass::High, HIGH_CROSSOVER),
        }
    }

    /// Low, mid and high parts of `x`.
    fn split(&mut self, x: f64) -> [f64; 3] {
        let run = |pair: &mut [Biquad; 2], x| {
            let first = pair[0].run(x);
            pair[1].run(first)
        };
        let low = self.low_phase.run(run(&mut self.low, x));
        let rest = run(&mut self.above_low, x);
        [low, run(&mut self.mid, rest), run(&mut self.high, rest)]
    }
}

#[derive(Clone, Copy)]
enum Pass {
    Low,
    High,
    All,
}

/// A second-order Butterworth section (Q = 1/√2) from the bilinear
/// transform, run in transposed direct form II.
#[derive(Clone, Copy)]
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    z: [f64; 2],
}

impl Biquad {
    fn new(pass: Pass, frequency: f64) -> Self {
        let w = 2.0 * PI * frequency / f64::from(SAMPLE_RATE);
        let (sin, cos) = w.sin_cos();
        let alpha = sin / SQRT_2;
        let b = match pass {
            Pass::Low => [(1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0],
            Pass::High => [(1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0],
            Pass::All => [1.0 - alpha, -2.0 * cos, 1.0 + alpha],
        };
        let a0 = 1.0 + alpha;
        Self {
            b: b.map(|b| b / a0),
            a: [-2.0 * cos / a0, (1.0 - alpha) / a0],
            z: [0.0; 2],
        }
    }

    fn run(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

/// RNNoise on each channel. Its output trails its input by one 10 ms
/// frame, and input is gathered into whole frames, so the sound comes out
/// exactly [`Suppressor::LATENCY`] late whatever size the chunks are.
struct Suppressor {
    states: Vec<Box<DenoiseState<'static>>>,
    input: Vec<Vec<f32>>,
    output: Vec<VecDeque<f32>>,
    scratch: Vec<f32>,
}

impl Suppressor {
    const LATENCY: usize = 2 * DENOISE_FRAME;

    fn new(channels: usize) -> Self {
        Self {
            states: (0..channels).map(|_| DenoiseState::new()).collect(),
            input: vec![Vec::with_capacity(DENOISE_FRAME); channels],
            // One frame of silence first: output never runs short.
            output: vec![VecDeque::from(vec![0.0; DENOISE_FRAME]); channels],
            scratch: vec![0.0; DENOISE_FRAME],
        }
    }

    fn process(&mut self, samples: &mut [f32], channels: usize) {
        for frame in samples.chunks_exact(channels) {
            for (c, sample) in frame.iter().enumerate() {
                let input = &mut self.input[c];
                input.push(sample * DENOISE_SCALE);
                if input.len() == DENOISE_FRAME {
                    self.states[c].process_frame(&mut self.scratch, input);
                    self.output[c].extend(self.scratch.iter().map(|s| s / DENOISE_SCALE));
                    input.clear();
                }
            }
        }
        for frame in samples.chunks_exact_mut(channels) {
            for (c, sample) in frame.iter_mut().enumerate() {
                *sample = self.output[c].pop_front().unwrap_or(0.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use isocast_protocol::{
        Compressor, Equalizer as EqualizerSettings, Expander, Gain, Limiter as LimiterSettings,
        NoiseGate, UpwardCompressor,
    };

    use super::*;

    fn chain(channels: usize, kinds: Vec<AudioFilterKind>) -> AudioChain {
        let mut chain = AudioChain::new(channels);
        let filters: Vec<AudioFilter> = kinds
            .into_iter()
            .map(|kind| AudioFilter {
                id: None,
                enabled: true,
                kind,
                unknown: Default::default(),
            })
            .collect();
        chain.update(&filters);
        chain
    }

    /// `seconds` of a mono sine at `frequency` Hz with peak `level`.
    fn sine(frequency: f64, level: f32, seconds: f64) -> Vec<f32> {
        let count = (seconds * f64::from(SAMPLE_RATE)) as usize;
        (0..count)
            .map(|i| {
                let t = i as f64 / f64::from(SAMPLE_RATE);
                level * (2.0 * PI * frequency * t).sin() as f32
            })
            .collect()
    }

    fn peak_db(samples: &[f32]) -> f32 {
        decibels(peak(samples))
    }

    fn rms_db(samples: &[f32]) -> f32 {
        let power = samples.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>();
        decibels((power / samples.len() as f64).sqrt() as f32)
    }

    fn run(chain: &mut AudioChain, samples: &mut [f32]) {
        for block in samples.chunks_mut(480) {
            chain.process(block, &|_| None);
        }
    }

    #[test]
    fn gain_and_polarity_scale_every_sample() {
        let mut chain = chain(
            2,
            vec![
                AudioFilterKind::Gain(Gain { gain_db: -6.0 }),
                AudioFilterKind::InvertPolarity(Default::default()),
            ],
        );
        let mut samples = vec![0.5, -0.25];
        run(&mut chain, &mut samples);
        let factor = amplitude(-6.0);
        assert_eq!(samples, vec![-0.5 * factor, 0.25 * factor]);
    }

    #[test]
    fn a_compressor_reduces_a_steady_tone_by_its_ratio() {
        let mut chain = chain(
            1,
            vec![AudioFilterKind::Compressor(Compressor {
                ratio: 4.0,
                threshold_db: -18.0,
                ..Compressor::default()
            })],
        );
        let mut samples = sine(1000.0, amplitude(-6.0), 1.0);
        run(&mut chain, &mut samples);
        // 12 dB over the threshold comes out 3 dB over.
        let settled = peak_db(&samples[24_000..]);
        assert!((settled + 15.0).abs() < 0.05, "{settled}");
    }

    #[test]
    fn a_limiter_never_lets_a_sample_past_its_threshold() {
        let mut chain = chain(
            1,
            vec![AudioFilterKind::Limiter(LimiterSettings {
                threshold_db: -12.0,
                release_ms: 60.0,
            })],
        );
        let mut samples = sine(1000.0, 1.0, 0.5);
        samples.extend(sine(100.0, amplitude(-3.0), 0.5));
        run(&mut chain, &mut samples);
        assert!(peak(&samples) <= amplitude(-12.0) * 1.000_001);
        assert!((peak_db(&samples[30_000..]) + 12.0).abs() < 0.01);
    }

    #[test]
    fn a_gate_silences_sound_under_its_thresholds_and_passes_sound_over() {
        let mut chain = chain(1, vec![AudioFilterKind::NoiseGate(NoiseGate::default())]);
        // -20 dB passes, -40 dB is cut once the hold and release are over.
        let mut samples = sine(1000.0, amplitude(-20.0), 0.5);
        samples.extend(sine(1000.0, amplitude(-40.0), 1.0));
        run(&mut chain, &mut samples);
        assert!((peak_db(&samples[12_000..24_000]) + 20.0).abs() < 0.05);
        assert_eq!(peak(&samples[48_000..]), 0.0);
    }

    #[test]
    fn expanders_and_upward_compressors_move_quiet_sound_by_their_ratio() {
        let expander = AudioFilterKind::Expander(Expander {
            detection: Detection::Peak,
            ..Expander::default()
        });
        let upward = AudioFilterKind::UpwardCompressor(UpwardCompressor {
            detection: Detection::Peak,
            ..UpwardCompressor::default()
        });
        // 10 dB under -40 at 2:1 comes out 20 dB under; 20 dB under -20 at
        // 2:1 comes out 10 dB under.
        for (kind, input, output) in [(expander, -50.0, -60.0), (upward, -40.0, -30.0)] {
            let mut chain = chain(1, vec![kind]);
            let mut samples = sine(1000.0, amplitude(input), 1.0);
            run(&mut chain, &mut samples);
            let settled = peak_db(&samples[24_000..]);
            assert!((settled - output).abs() < 0.05, "{settled} not {output}");
        }
    }

    #[test]
    fn rms_detection_reads_a_tone_3_db_under_its_peak() {
        let mut rms = Rms::default();
        let samples = sine(1000.0, 0.5, 0.1);
        let level = samples.iter().map(|s| rms.next(&[*s])).last().unwrap();
        assert!((decibels(level) - decibels(0.5 / 2f32.sqrt())).abs() < 0.01);
    }

    #[test]
    fn the_equalizer_is_flat_at_0_db_and_turns_each_band() {
        let response = |gains: [f32; 3], frequency: f64| {
            let mut chain = chain(
                1,
                vec![AudioFilterKind::Equalizer(EqualizerSettings {
                    low_db: gains[0],
                    mid_db: gains[1],
                    high_db: gains[2],
                })],
            );
            let mut samples = sine(frequency, 0.5, 1.0);
            run(&mut chain, &mut samples);
            // RMS over whole cycles: an all-pass moves where samples fall
            // on the wave, which changes the sampled peaks.
            rms_db(&samples[24_000..]) - rms_db(&sine(frequency, 0.5, 0.5))
        };
        for frequency in [50.0, 250.0, 1000.0, 4000.0, 12_000.0] {
            let flat = response([0.0; 3], frequency);
            assert!(flat.abs() < 0.05, "{frequency} Hz: {flat} dB");
        }
        assert!((response([-20.0, 0.0, 0.0], 50.0) + 20.0).abs() < 0.5);
        assert!((response([0.0, 12.0, 0.0], 1000.0) - 12.0).abs() < 0.5);
        assert!((response([0.0, 0.0, -12.0], 15_000.0) + 12.0).abs() < 0.5);
    }

    #[test]
    fn changing_a_setting_keeps_state_and_switching_on_starts_afresh() {
        let gate = |enabled, open_threshold_db| AudioFilter {
            id: None,
            enabled,
            kind: AudioFilterKind::NoiseGate(NoiseGate {
                open_threshold_db,
                ..NoiseGate::default()
            }),
            unknown: Default::default(),
        };
        let mut chain = AudioChain::new(1);
        chain.update(&[gate(true, -26.0)]);
        let mut samples = sine(1000.0, 0.5, 0.1);
        run(&mut chain, &mut samples);
        let gain = |chain: &AudioChain| match &chain.links[0].processor {
            Processor::Gate(gate) => gate.gain,
            _ => unreachable!(),
        };
        assert_eq!(gain(&chain), 1.0);
        chain.update(&[gate(true, -20.0)]);
        assert_eq!(gain(&chain), 1.0);
        chain.update(&[gate(false, -20.0)]);
        chain.update(&[gate(true, -20.0)]);
        assert_eq!(gain(&chain), 0.0);
    }

    #[test]
    fn noise_suppression_keeps_the_same_delay_whatever_the_chunk_size() {
        let mut suppressor = Suppressor::new(2);
        let mut total = 0;
        for size in [100, 480, 7, 960, 333].iter().cycle().take(40) {
            let mut chunk = vec![0.0; size * 2];
            suppressor.process(&mut chunk, 2);
            total += size;
            // What is waiting to come out plus what is being gathered is
            // always one frame (RNNoise's own frame makes up the rest of
            // the latency), and output never runs short.
            for c in 0..2 {
                let waiting = suppressor.output[c].len() + suppressor.input[c].len();
                assert_eq!(
                    waiting,
                    Suppressor::LATENCY - DENOISE_FRAME,
                    "after {total}"
                );
                assert!(!suppressor.output[c].is_empty());
            }
        }
    }
}
