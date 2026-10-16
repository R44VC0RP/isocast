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

use std::collections::BTreeMap;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use isocast_protocol::api::MonitorStatus;
use isocast_protocol::{AudioSettings, Monitoring, PanLaw};
use parking_lot::Mutex;

use super::SessionControl;
use crate::{Platform, SAMPLE_RATE};

/// The margin a source starts with (5 ms), how much each gap adds, and the
/// most it can grow to (100 ms).
const FIRST_MARGIN: usize = SAMPLE_RATE as usize / 200;
const MARGIN_STEP: usize = SAMPLE_RATE as usize / 200;
const MAX_MARGIN: usize = SAMPLE_RATE as usize / 10;
/// How far past what it needs in hand a source's buffer may grow before
/// the oldest audio is dropped (50 ms).
const MAX_EXCESS: usize = SAMPLE_RATE as usize / 20;
/// A hard bound even if the output stops asking for audio (1.37 s at
/// 48 kHz). Ordinary latency is still set by the adaptive target below.
const CAPACITY: usize = 1 << 16;

/// What the session's monitoring device plays.
pub(super) struct Monitor {
    /// The output device, as the setup names it.
    device: String,
    state: Mutex<State>,
    /// The callback owns this, never the monitor or its playback handle.
    renderer: Arc<Renderer>,
}

struct Renderer {
    inputs: AtomicPtr<Inputs>,
    /// Even when idle, odd while rendering. A completed callback advances
    /// it so control threads can reclaim old input lists, even if another
    /// callback has already started. All pointer/epoch operations are
    /// SeqCst: publishing a new list and observing an idle reader must
    /// precede any later reader's load of that list.
    epoch: AtomicU64,
    request: AtomicUsize,
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
    inputs: BTreeMap<String, Arc<Buffer>>,
    /// These own every list the callback could still be borrowing. Only
    /// control threads allocate or destroy lists and their input Arcs.
    snapshot: Option<Arc<Inputs>>,
    retired: Vec<(u64, Arc<Inputs>)>,
    /// Whether opening the device has been tried.
    opened: bool,
    closed: bool,
    /// The device playing, and its name.
    playback: Option<Box<dyn Send>>,
    output: Option<String>,
    /// Why the device couldn't be opened.
    error: Option<String>,
    meter_power: [f64; 2],
    meter_frames: u64,
}

struct Inputs {
    buffers: Vec<Arc<Buffer>>,
}

/// The peak is exchanged by status; power is a cumulative total written
/// only by the callback. Renderer::levels takes all channels under the
/// render epoch and retains exchanged peaks on retries, so a concurrent
/// reset cannot lose a peak or split a callback across meter intervals.
#[derive(Default)]
struct Meter {
    peak: AtomicU32,
    power: AtomicU64,
}

impl Meter {
    fn add_all(&self, samples: impl Iterator<Item = f32>) {
        let mut peak = 0.0_f32;
        let mut power = 0.0;
        for sample in samples {
            // One invalid source sample must not poison cumulative power
            // (and therefore all subsequent meter intervals) forever.
            if !sample.is_finite() {
                continue;
            }
            peak = peak.max(sample.abs());
            power += f64::from(sample) * f64::from(sample);
        }
        // Nonnegative float bits have the same order as their values.
        self.peak.fetch_max(peak.to_bits(), Ordering::Relaxed);
        let total = f64::from_bits(self.power.load(Ordering::Relaxed)) + power;
        self.power.store(total.to_bits(), Ordering::SeqCst);
    }
}

/// One source's audio waiting to be played, in stereo after its gains.
struct Buffer {
    /// SPSC ring: pushes are serialized by Monitor::state, rendering by
    /// Renderer::epoch. Only the producer advances write; only the
    /// consumer advances read. Release/acquire keeps occupied slots from
    /// being overwritten until the consumer has finished reading them.
    frames: Box<[AtomicU64]>,
    read: AtomicUsize,
    write: AtomicUsize,
    /// Holding back until it has enough in hand.
    waiting: AtomicBool,
    /// The largest chunk the source has delivered, in frames.
    chunk: AtomicUsize,
    margin: AtomicUsize,
}

impl Buffer {
    fn new() -> Self {
        Self {
            frames: (0..CAPACITY).map(|_| AtomicU64::new(0)).collect(),
            read: AtomicUsize::new(0),
            write: AtomicUsize::new(0),
            waiting: AtomicBool::new(true),
            chunk: AtomicUsize::new(0),
            margin: AtomicUsize::new(FIRST_MARGIN),
        }
    }

    /// What it keeps in hand when the device asks for `request` frames at
    /// a time.
    fn target(&self, request: usize) -> usize {
        self.chunk
            .load(Ordering::Relaxed)
            .saturating_add(request)
            .saturating_add(self.margin.load(Ordering::Relaxed))
            .min(CAPACITY)
    }

    fn len(&self) -> usize {
        // Only called by a producer/status with the control lock held, so
        // write cannot change while read is sampled.
        let write = self.write.load(Ordering::Relaxed);
        write.wrapping_sub(self.read.load(Ordering::Acquire))
    }

    fn push(&self, samples: &[f32], channels: usize, gains: [f32; 2], mono: bool) {
        let chunk = samples.len() / channels;
        self.chunk.fetch_max(chunk, Ordering::Relaxed);
        let write = self.write.load(Ordering::Relaxed);
        let read = self.read.load(Ordering::Acquire);
        let available = CAPACITY - write.wrapping_sub(read);
        // If the device is stalled, never overwrite frames it might be
        // reading. Drop incoming overflow; render trims the queued excess
        // on its next call. A single oversized chunk keeps its tail.
        let samples = &samples[chunk.saturating_sub(CAPACITY) * channels..];
        let mut count = 0;
        for frame in samples.chunks_exact(channels).take(available) {
            let [left, right] = match frame {
                [mono] => [mono * gains[0], mono * gains[1]],
                [left, right, ..] if mono => {
                    let mono = (left + right) * 0.5;
                    [mono * gains[0], mono * gains[1]]
                }
                [left, right, ..] => [left * gains[0], right * gains[1]],
                [] => [0.0; 2],
            };
            let bits = u64::from(left.to_bits()) | (u64::from(right.to_bits()) << 32);
            self.frames[write.wrapping_add(count) % CAPACITY].store(bits, Ordering::Relaxed);
            count += 1;
        }
        self.write
            .store(write.wrapping_add(count), Ordering::Release);
    }

    /// Mix this input, returning whether it ran short this time.
    fn render(&self, out: &mut [f32], request: usize) -> bool {
        let mut read = self.read.load(Ordering::Relaxed);
        let write = self.write.load(Ordering::Acquire);
        let mut available = write.wrapping_sub(read);
        let target = self.target(request);
        let waiting = self.waiting.load(Ordering::Relaxed);
        if waiting && available < target {
            return false;
        }
        let limit = target
            .saturating_add(self.chunk.load(Ordering::Relaxed))
            .saturating_add(MAX_EXCESS);
        if waiting || available > limit {
            read = read.wrapping_add(available - target);
            available = target;
        }
        let count = out.len() / 2;
        let played = available.min(count);
        for frame in out.as_chunks_mut::<2>().0.iter_mut().take(played) {
            let bits = self.frames[read % CAPACITY].load(Ordering::Relaxed);
            frame[0] += f32::from_bits(bits as u32);
            frame[1] += f32::from_bits((bits >> 32) as u32);
            read = read.wrapping_add(1);
        }
        self.read.store(read, Ordering::Release);
        let underrun = played < count;
        self.waiting.store(underrun, Ordering::Relaxed);
        if underrun {
            let margin = self.margin.load(Ordering::Relaxed);
            self.margin
                .store((margin + MARGIN_STEP).min(MAX_MARGIN), Ordering::Relaxed);
        }
        underrun
    }
}

impl Monitor {
    /// A monitor playing through `device`, which opens once a source is
    /// heard.
    pub(super) fn new(device: &str) -> Self {
        Self {
            device: device.to_string(),
            state: Mutex::default(),
            renderer: Arc::new(Renderer {
                inputs: AtomicPtr::new(ptr::null_mut()),
                epoch: AtomicU64::new(0),
                request: AtomicUsize::new(0),
                meters: Default::default(),
                frames: AtomicU64::new(0),
                underruns: AtomicU64::new(0),
                silent: std::env::var_os("ISOCAST_MONITOR_SILENT")
                    .is_some_and(|value| !value.is_empty() && value != "0"),
            }),
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
            let mut state = self.state.lock();
            if state.closed || std::mem::replace(&mut state.opened, true) {
                return None;
            }
        }
        // A fixed strong reference to render data, not a Weak<Monitor>
        // upgraded/dropped on each callback. It neither owns playback nor
        // can perform the monitor's final destruction on the IO thread.
        let renderer = Arc::clone(&self.renderer);
        let render = Box::new(move |out: &mut [f32]| renderer.render(out));
        let opened = platform.play_audio(&self.device, render);
        let mut state = self.state.lock();
        if state.closed {
            // close can race the platform opening a device. Dispose this
            // late result here, outside the lock, rather than resurrect it.
            drop(state);
            drop(opened);
            return None;
        }
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
        let playback = {
            let mut state = self.state.lock();
            state.closed = true;
            state.playback.take()
        };
        drop(playback);
    }

    /// Publish an immutable view and retire its predecessor. The callback
    /// only borrows it while its epoch is odd; it never clones/drops Arcs.
    fn publish(&self, state: &mut State) {
        let snapshot = Arc::new(Inputs {
            buffers: state.inputs.values().cloned().collect(),
        });
        self.renderer
            .inputs
            .store(Arc::as_ptr(&snapshot).cast_mut(), Ordering::SeqCst);
        let epoch = self.renderer.epoch.load(Ordering::SeqCst);
        if let Some(old) = state.snapshot.replace(snapshot)
            && !epoch.is_multiple_of(2)
        {
            state.retired.push((epoch, old));
        }
        self.reclaim(state);
    }

    fn reclaim(&self, state: &mut State) {
        let epoch = self.renderer.epoch.load(Ordering::SeqCst);
        state
            .retired
            .retain(|(reader, _)| !epoch.is_multiple_of(2) && *reader == epoch);
    }

    /// Queue `samples` of source `id` (interleaved, `channels` per frame),
    /// scaled by left and right `gains`.
    fn push(&self, id: &str, samples: &[f32], channels: usize, gains: [f32; 2], mono: bool) {
        let channels = channels.max(1);
        let mut state = self.state.lock();
        if state.closed {
            return;
        }
        if !state.inputs.contains_key(id) {
            state.inputs.insert(id.to_string(), Arc::new(Buffer::new()));
            self.publish(&mut state);
        }
        state.inputs[id].push(samples, channels, gains, mono);
        self.reclaim(&mut state);
    }

    /// Stop playing source `id`.
    fn remove(&self, id: &str) {
        let mut state = self.state.lock();
        if state.inputs.remove(id).is_some() {
            self.publish(&mut state);
        }
    }

    /// Live status, once a source has been heard; resets the levels.
    pub(super) fn status(&self) -> Option<MonitorStatus> {
        let mut state = self.state.lock();
        if !state.opened {
            return None;
        }
        self.reclaim(&mut state);
        let [left, right] = self.renderer.levels(&mut state);
        let buffered = state.inputs.values().map(|b| b.len()).max();
        Some(MonitorStatus {
            device: self.device.clone(),
            output: state.output.clone(),
            error: state.error.clone(),
            sources: state.inputs.keys().cloned().collect(),
            peak_db: [left.0, right.0],
            rms_db: [left.1, right.1],
            frames: self.renderer.frames.load(Ordering::Relaxed),
            underruns: self.renderer.underruns.load(Ordering::Relaxed),
            buffered_ms: buffered.unwrap_or(0) as f64 * 1000.0 / f64::from(SAMPLE_RATE),
            silent: self.renderer.silent,
        })
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        // The platform stops/joins callbacks when playback is dropped.
        // Do this before State frees any of the lists Renderer borrows.
        self.close();
    }
}

/// A stack-only read guard: on unwind as well as return, end the borrow.
/// No owning pointer or heap-backed value is released by the callback.
struct Rendering<'a>(&'a AtomicU64);

impl Drop for Rendering<'_> {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl Renderer {
    /// Fill `out` (interleaved stereo) with the sources being heard. Called
    /// by the device.
    fn render(&self, out: &mut [f32]) {
        out.fill(0.0);
        let epoch = self.epoch.load(Ordering::SeqCst);
        if !epoch.is_multiple_of(2)
            || self
                .epoch
                .compare_exchange(
                    epoch,
                    epoch.wrapping_add(1),
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_err()
        {
            // AudioRender is FnMut (one consumer), but keep this safe even
            // if called concurrently. Never wait on another callback.
            return;
        }
        let _rendering = Rendering(&self.epoch);
        let count = out.len() / 2;
        let request = self.request.load(Ordering::Relaxed).max(count);
        self.request.store(request, Ordering::Relaxed);
        let mut underruns = 0;
        let inputs = self.inputs.load(Ordering::SeqCst);
        if !inputs.is_null() {
            // SAFETY: Monitor publishes initialized, immutable lists. Its
            // control thread retains this list until our odd epoch ends;
            // Monitor::drop stops playback before destroying any lists.
            // The epoch guard also serializes the rings' single consumer.
            let inputs = unsafe { &*inputs };
            for buffer in &inputs.buffers {
                underruns += u64::from(buffer.render(out, request));
            }
        }
        self.meters[0].add_all(out.iter().step_by(2).copied());
        self.meters[1].add_all(out.iter().skip(1).step_by(2).copied());
        self.frames.fetch_add(count as u64, Ordering::SeqCst);
        self.underruns.fetch_add(underruns, Ordering::Relaxed);
        if self.silent {
            out.fill(0.0);
        }
    }

    fn levels(&self, state: &mut State) -> [(f64, f64); 2] {
        let mut peak = [0.0_f32; 2];
        let (power, frames) = loop {
            let before = self.epoch.load(Ordering::SeqCst);
            if !before.is_multiple_of(2) {
                std::thread::yield_now();
                continue;
            }
            let power = self
                .meters
                .each_ref()
                .map(|m| f64::from_bits(m.power.load(Ordering::SeqCst)));
            let frames = self.frames.load(Ordering::SeqCst);
            // If a render races these exchanges, keep everything taken
            // and retry the whole snapshot. The callback uses fetch_max,
            // not load/store, so its peak cannot overwrite this reset.
            for (i, peak) in peak.iter_mut().enumerate() {
                *peak = peak.max(f32::from_bits(
                    self.meters[i].peak.swap(0, Ordering::SeqCst),
                ));
            }
            if before == self.epoch.load(Ordering::SeqCst) {
                break (power, frames);
            }
        };
        let count = frames.wrapping_sub(state.meter_frames);
        let levels = std::array::from_fn(|i| {
            let rms = if count == 0 {
                0.0
            } else {
                ((power[i] - state.meter_power[i]).max(0.0) / count as f64).sqrt()
            };
            let db = |level: f64| (20.0 * level.max(1e-6).log10()).max(-120.0);
            (db(f64::from(peak[i])), db(rms))
        });
        state.meter_power = power;
        state.meter_frames = frames;
        levels
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
        let monitor = self.monitor.lock().clone();
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
            monitor.push(&self.id, samples, self.channels, gains, self.settings.mono);
            self.playing = Some(monitor);
        } else if self.heard {
            monitor.remove(&self.id);
            self.playing = None;
        }
        self.heard = heard;
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        if let Some(monitor) = &self.playing {
            monitor.remove(&self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    // Thread-local, opt-in counters: other tests and producer/status
    // threads must not contaminate the real render path's measurements.
    thread_local! {
        static HEAP: Cell<Option<[usize; 3]>> = const { Cell::new(None) };
    }

    struct CountedHeap;

    fn heap_call(kind: usize) {
        let _ = HEAP.try_with(|heap| {
            if let Some(mut counts) = heap.get() {
                counts[kind] += 1;
                heap.set(Some(counts));
            }
        });
    }

    // SAFETY: all allocations and their original layouts/pointers are
    // passed unchanged to System; counters never allocate or touch them.
    unsafe impl GlobalAlloc for CountedHeap {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            heap_call(0);
            unsafe { System.alloc(layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            heap_call(1);
            unsafe { System.realloc(ptr, layout, size) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            heap_call(2);
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    #[global_allocator]
    static ALLOCATOR: CountedHeap = CountedHeap;

    #[test]
    fn sources_play_once_they_have_enough_and_wait_again_after_a_gap() {
        // Heard, even when the tests run with ISOCAST_MONITOR_SILENT set.
        let mut monitor = Monitor::new("default");
        Arc::get_mut(&mut monitor.renderer).unwrap().silent = false;
        let mut out = vec![0.0; 2 * 128];
        // The device asks for 128 frames at a time; the source delivers
        // 480-frame chunks (mono, at half gain on the right).
        monitor.renderer.render(&mut out);
        let target = 480 + 128 + FIRST_MARGIN;
        let chunk = vec![0.5; 480];
        monitor.push("m", &chunk, 1, [1.0, 0.5], false);
        monitor.renderer.render(&mut out);
        assert!(out.iter().all(|&s| s == 0.0), "not enough in hand yet");
        monitor.push("m", &chunk, 1, [1.0, 0.5], false);
        assert!(960 >= target);
        // This is the producer/status lock the old callback waited on.
        // Rendering and metering must finish even while it is held.
        let state = monitor.state.lock();
        HEAP.with(|heap| heap.set(Some([0; 3])));
        monitor.renderer.render(&mut out);
        let heap = HEAP.with(|heap| heap.take().unwrap());
        assert_eq!(
            heap, [0; 3],
            "render allocated, reallocated or freed memory"
        );
        assert!(out.chunks(2).all(|f| f == [0.5, 0.25]), "{:?}", &out[..4]);
        // It started with just its target in hand.
        let left = state.inputs["m"].len();
        assert_eq!(left, target - 128);
        drop(state);
        // Drain it: the last render runs short, counts a gap, and the
        // source waits for more, with a larger margin.
        for _ in 0..7 {
            monitor.renderer.render(&mut out);
        }
        assert_eq!(monitor.renderer.underruns.load(Ordering::Relaxed), 1);
        let state = monitor.state.lock();
        let buffer = &state.inputs["m"];
        assert!(buffer.waiting.load(Ordering::Relaxed) && buffer.len() == 0);
        assert_eq!(
            buffer.margin.load(Ordering::Relaxed),
            FIRST_MARGIN + MARGIN_STEP
        );
        drop(state);
        monitor.remove("m");
        // A stereo source explicitly downmixed: average before balancing,
        // so both the positive and negative channel contribute to playback.
        let stereo = [0.75, -0.25].repeat(480);
        monitor.push("stereo", &stereo, 2, [1.0, 0.5], true);
        monitor.push("stereo", &stereo, 2, [1.0, 0.5], true);
        // Start a fresh meter interval, then check atomic peak and RMS.
        monitor.renderer.levels(&mut monitor.state.lock());
        monitor.renderer.render(&mut out);
        assert!(
            out.as_chunks::<2>()
                .0
                .iter()
                .all(|frame| *frame == [0.25, 0.125])
        );
        let levels = monitor.renderer.levels(&mut monitor.state.lock());
        for (i, (peak, rms)) in levels.into_iter().enumerate() {
            let expected = 20.0 * [0.25_f64, 0.125][i].log10();
            assert!((peak - expected).abs() < 1e-6);
            assert!((rms - expected).abs() < 1e-6);
        }
        assert_eq!(
            monitor.renderer.levels(&mut monitor.state.lock()),
            [(-120.0, -120.0); 2]
        );

        // Removal cannot release an input while a render epoch borrows
        // its old list; the next control call, not the callback, frees it.
        let old = Arc::downgrade(&monitor.state.lock().inputs["stereo"]);
        monitor.renderer.epoch.fetch_add(1, Ordering::SeqCst);
        let reading = Rendering(&monitor.renderer.epoch);
        monitor.remove("stereo");
        assert!(old.upgrade().is_some());
        drop(reading);
        monitor.renderer.render(&mut out);
        assert!(out.iter().all(|&sample| sample == 0.0));
        assert!(
            old.upgrade().is_some(),
            "the callback must not reclaim inputs"
        );
        monitor.reclaim(&mut monitor.state.lock());
        assert!(old.upgrade().is_none());
    }

    #[test]
    fn audio_piling_up_is_dropped_down_to_what_is_needed() {
        // Heard, even when the tests run with ISOCAST_MONITOR_SILENT set.
        let mut monitor = Monitor::new("default");
        Arc::get_mut(&mut monitor.renderer).unwrap().silent = false;
        let mut out = vec![0.0; 2 * 512];
        monitor.renderer.render(&mut out);
        for _ in 0..1000 {
            monitor.push("m", &[0.1; 960], 2, [1.0, 1.0], false);
        }
        let state = monitor.state.lock();
        let buffer = &state.inputs["m"];
        assert_eq!(
            buffer.len(),
            CAPACITY,
            "the stalled device cannot grow the ring"
        );
        monitor.renderer.render(&mut out);
        assert_eq!(buffer.len(), buffer.target(512) - 512);
        assert!(out.iter().all(|&sample| sample == 0.1));
        drop(state);
        // Overflow must not wedge this source: once the consumer makes
        // room, newly delivered audio is admitted and heard again.
        for _ in 0..2 {
            monitor.push("m", &[0.2; 960], 2, [1.0, 1.0], false);
        }
        for _ in 0..3 {
            monitor.renderer.render(&mut out);
        }
        assert!(out.iter().all(|&sample| sample == 0.2));

        // Wrap both positions as well as the physical ring, with paired
        // channel values making torn frames visible. Push, render, status
        // and source churn run concurrently; none can lend an owning value
        // to the callback.
        monitor.remove("m");
        monitor.push("m", &[], 2, [1.0; 2], false);
        {
            let mut state = monitor.state.lock();
            state.opened = true;
            state.inputs["m"]
                .read
                .store(usize::MAX - 255, Ordering::Relaxed);
            state.inputs["m"]
                .write
                .store(usize::MAX - 255, Ordering::Relaxed);
        }
        let start = std::sync::Barrier::new(3);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let mut out = [0.0; 256];
                start.wait();
                HEAP.with(|heap| heap.set(Some([0; 3])));
                for _ in 0..4000 {
                    monitor.renderer.render(&mut out);
                    assert!(
                        out.as_chunks::<2>()
                            .0
                            .iter()
                            .all(|[left, right]| *left == -*right)
                    );
                    std::thread::yield_now();
                }
                let heap = HEAP.with(|heap| heap.take().unwrap());
                assert_eq!(heap, [0; 3], "4000 renders with concurrent source churn");
            });
            scope.spawn(|| {
                start.wait();
                for _ in 0..2000 {
                    let status = monitor.status().unwrap();
                    assert!(
                        status.buffered_ms <= CAPACITY as f64 * 1000.0 / f64::from(SAMPLE_RATE)
                    );
                    assert!(status.rms_db.iter().all(|db| db.is_finite()));
                    // Equal-and-opposite stereo has equal peaks and RMS;
                    // status must not reset one side in a later callback.
                    assert_eq!(status.peak_db[0], status.peak_db[1]);
                    assert_eq!(status.rms_db[0], status.rms_db[1]);
                    std::thread::yield_now();
                }
            });
            let mut samples = [0.25, -0.25].repeat(480);
            start.wait();
            for i in 0..2000 {
                let value = (i % 17) as f32 / 32.0;
                for frame in samples.as_chunks_mut::<2>().0 {
                    *frame = [value, -value];
                }
                monitor.push("m", &samples, 2, [1.0; 2], false);
                if i % 20 == 0 {
                    monitor.push("other", &samples, 2, [1.0; 2], false);
                    monitor.remove("other");
                }
                std::thread::yield_now();
            }
        });
        monitor.remove("m");
        monitor.reclaim(&mut monitor.state.lock());
        assert!(monitor.state.lock().retired.is_empty());
    }
}
