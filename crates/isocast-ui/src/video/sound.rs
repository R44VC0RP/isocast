//! Another computer's engine's program sound, played on this computer's
//! default output at the times the viewer's [`super::sync::Playout`] gives
//! it, so it lines up with the picture. Decoded Opus (48 kHz stereo
//! floats) waits in a timed queue. The output asks for samples with the
//! moment its first one reaches the speaker; the queue skips ahead when
//! it's late and plays silence when it's early (beyond [`TOLERANCE`]).
//!
//! Core Audio's default output unit on macOS (its render timestamp's host
//! time says when the buffer plays), WASAPI shared mode on Windows (the
//! samples already queued plus the stream's latency).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;

/// The rate and channels of the queue (Opus's, and the engine's mix).
pub const RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// How far a sample may play from its time before the queue resyncs.
const TOLERANCE: f64 = 0.030;
/// Beyond this from its times (and within [`TOLERANCE`]), the queue slips
/// a frame per buffer to follow the output's clock.
const DRIFT: f64 = 0.004;
/// The most sound queued (beyond it, the oldest goes).
const MAX_QUEUED: f64 = 1.0;

/// Samples to play from `start` (seconds on the viewer's playout clock).
#[derive(Debug, Default)]
struct Queue {
    start: f64,
    samples: VecDeque<f32>,
    /// Times the output was further than [`TOLERANCE`] from the queue
    /// (skipped or waited), and buffers played on time.
    resyncs: u64,
    on_time: u64,
    /// Frames slipped to follow the output's clock.
    slipped: u64,
    /// The output has played from this queue since it (re)started.
    aligned: bool,
    /// Times a push started the queue again (a gap or overlap beyond
    /// [`TOLERANCE`]), and by how much the last one was off, in ms.
    restarts: u64,
    last_restart: Option<f64>,
    /// The last buffer that ran short (for the log).
    last_short: Option<Short>,
    /// When sound was last queued.
    pushed: Option<Instant>,
    /// Buffers that ran out of sound part way (the output's own misses,
    /// asked for while the queue was busy, are counted in [`Sound`]).
    underruns: u64,
    /// The loudest sample queued since the last report.
    peak: f32,
}

/// A buffer that ran short: frames asked for and queued, and how late the
/// queue was against the buffer's time.
#[derive(Debug, Clone, Copy)]
struct Short {
    asked: usize,
    had: usize,
    late_ms: f64,
    aligned_now: bool,
    since_push_ms: f64,
}

impl std::fmt::Display for Short {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "asked {} frames, had {}, {:.1} ms late, {:.0} ms since sound came{}",
            self.asked,
            self.had,
            self.late_ms,
            self.since_push_ms,
            if self.aligned_now {
                ", just restarted"
            } else {
                ""
            }
        )
    }
}

impl Queue {
    /// A queue that never grows under the lock: room for [`MAX_QUEUED`]
    /// and a packet more.
    fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(
                ((MAX_QUEUED + 0.1) * f64::from(RATE)) as usize * CHANNELS,
            ),
            ..Self::default()
        }
    }

    fn end(&self) -> f64 {
        self.start + (self.samples.len() / CHANNELS) as f64 / f64::from(RATE)
    }

    /// Queue `samples` (interleaved stereo) to play from `at`.
    fn push(&mut self, at: f64, samples: &[f32]) {
        if self.samples.is_empty() || (at - self.end()).abs() > TOLERANCE {
            // A gap, an overlap or the first sound: start again from here.
            if !self.samples.is_empty() {
                self.restarts += 1;
                self.last_restart = Some((at - self.end()) * 1e3);
            }
            self.samples.clear();
            self.start = at;
            self.aligned = false;
        }
        self.samples.extend(samples);
        self.peak = samples.iter().fold(self.peak, |peak, s| peak.max(s.abs()));
        let max = (MAX_QUEUED * f64::from(RATE)) as usize * CHANNELS;
        if self.samples.len() > max {
            let drop = (self.samples.len() - max) / CHANNELS * CHANNELS;
            self.samples.drain(..drop);
            self.start += (drop / CHANNELS) as f64 / f64::from(RATE);
        }
    }

    /// Fill `out` (interleaved stereo) with what plays from `at`.
    fn render(&mut self, out: &mut [f32], at: f64) {
        out.fill(0.0);
        if self.samples.is_empty() {
            return;
        }
        let frame = 1.0 / f64::from(RATE);
        let mut written = 0;
        let late = at - self.start;
        let was_aligned = self.aligned;
        if !self.aligned {
            // The first buffer after a (re)start lines up exactly: what
            // drift correction would take minutes to recover.
            self.aligned = true;
            if late > 0.0 {
                let skip = ((late / frame) as usize * CHANNELS).min(self.samples.len());
                self.samples.drain(..skip);
                self.start += (skip / CHANNELS) as f64 * frame;
            } else {
                written = (((-late) / frame) as usize * CHANNELS).min(out.len());
            }
        } else if late > TOLERANCE {
            // Behind: skip what should already have played.
            self.resyncs += 1;
            let skip = ((late / frame) as usize * CHANNELS).min(self.samples.len());
            self.samples.drain(..skip);
            self.start += (skip / CHANNELS) as f64 * frame;
        } else if late < -TOLERANCE {
            // Ahead: silence until the first sample's time.
            self.resyncs += 1;
            written = (((-late) / frame) as usize * CHANNELS).min(out.len());
        } else {
            self.on_time += 1;
            // Drift between the engine's clock and this output's: slip one
            // frame a buffer (about 2 ms a second, inaudible) toward the
            // queue's times, so it never drifts out to a resync.
            if late > DRIFT && self.samples.len() > CHANNELS {
                self.samples.drain(..CHANNELS);
                self.start += frame;
                self.slipped += 1;
            } else if late < -DRIFT && out.len() >= CHANNELS {
                // Hold the first frame one frame longer.
                for (slot, sample) in out[..CHANNELS].iter_mut().zip(&self.samples) {
                    *slot = *sample;
                }
                written = CHANNELS;
                self.slipped += 1;
            }
        }
        let take = (out.len() - written).min(self.samples.len());
        if take < out.len() - written {
            // Ran out mid-buffer: the rest is silence.
            self.underruns += 1;
            self.last_short = Some(Short {
                asked: (out.len() - written) / CHANNELS,
                had: self.samples.len() / CHANNELS,
                late_ms: late * 1e3,
                aligned_now: written > 0 && !was_aligned,
                since_push_ms: 0.0,
            });
        }
        for (slot, sample) in out[written..written + take]
            .iter_mut()
            .zip(self.samples.drain(..take))
        {
            *slot = sample;
        }
        self.start += (take / CHANNELS) as f64 * frame;
    }
}

/// How long a read-ahead peak is held: longer than the gaps between the
/// output's biggest requests (WASAPI's ~40 ms ones came about every 10 s on
/// exon-desk), so the estimate never sags below them in between.
const PEAK_HOLD: Duration = Duration::from_secs(15);

/// The largest value seen over the last [`PEAK_HOLD`] (to twice that):
/// two buckets, the current one and the one before.
#[derive(Debug, Default)]
struct PeakHold {
    started: Option<Instant>,
    current: f64,
    previous: f64,
}

impl PeakHold {
    fn update(&mut self, now: Instant, value: f64) -> f64 {
        let started = *self.started.get_or_insert(now);
        if now.saturating_duration_since(started) >= PEAK_HOLD {
            self.previous = self.current;
            self.current = 0.0;
            self.started = Some(now);
        }
        self.current = self.current.max(value);
        self.current.max(self.previous)
    }
}

/// Playing sound: dropping it stops the output.
pub struct Sound {
    queue: Arc<Mutex<Queue>>,
    /// How far ahead the output asks (its latency plus what it buffers),
    /// in microseconds: the queue must hold at least that much.
    lead: Arc<std::sync::atomic::AtomicU64>,
    /// Buffers the output asked for while the queue was busy.
    misses: Arc<std::sync::atomic::AtomicU64>,
    _output: platform::Output,
}

impl Sound {
    /// Start the default output; `clock` is the playout clock's start.
    pub fn start(clock: Instant) -> Result<Self> {
        let queue = Arc::new(Mutex::new(Queue::new()));
        let lead: Arc<std::sync::atomic::AtomicU64> = Arc::default();
        let misses: Arc<std::sync::atomic::AtomicU64> = Arc::default();
        let render = {
            let (queue, lead, misses) = (queue.clone(), lead.clone(), misses.clone());
            let mut peak = PeakHold::default();
            move |out: &mut [f32], plays: Instant| {
                // The buffer's end, ahead of now, held at its peak over the
                // last few seconds (WASAPI asks 10 ms ahead most of the time
                // and 40 ms now and then: the queue must cover the 40).
                let now = Instant::now();
                let ahead = plays.saturating_duration_since(now).as_secs_f64()
                    + (out.len() / CHANNELS) as f64 / f64::from(RATE);
                let held = peak.update(now, ahead);
                lead.store((held * 1e6) as u64, std::sync::atomic::Ordering::Relaxed);
                let at = plays.saturating_duration_since(clock).as_secs_f64();
                // Never wait on the audio thread: silence if busy.
                match queue.try_lock() {
                    Ok(mut queue) => {
                        let short = queue.underruns;
                        queue.render(out, at);
                        // A short buffer: how long since sound last came
                        // (for the log: a stalled sender or a late output).
                        if queue.underruns != short
                            && let (Some(pushed), Some(last)) =
                                (queue.pushed, queue.last_short.as_mut())
                        {
                            last.since_push_ms =
                                now.saturating_duration_since(pushed).as_secs_f64() * 1e3;
                        }
                    }
                    Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                        poisoned.into_inner().render(out, at)
                    }
                    Err(std::sync::TryLockError::WouldBlock) => {
                        out.fill(0.0);
                        misses.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            }
        };
        Ok(Self {
            queue,
            lead,
            misses,
            _output: platform::Output::start(Box::new(render))?,
        })
    }

    /// Whether the output still plays (a Windows output stops when its
    /// device goes away; start a new `Sound` on the default one).
    pub fn alive(&self) -> bool {
        self._output.alive()
    }

    /// How far ahead of now the output asks for sound, in seconds: the
    /// playout delay must cover it, or every buffer runs short.
    pub fn lead(&self) -> f64 {
        self.lead.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1e6
    }

    /// How the output has kept to the queue's times (for the log).
    pub fn report(&self) -> String {
        let mut queue = self.lock();
        let peak = std::mem::take(&mut queue.peak);
        format!(
            "sound: {} buffers on time (within {:.0} ms), {} resynced, {} frames slipped for drift, {} ran short (last: {}), {} restarts (last {} ms off), {} missed while busy, {:.0} ms queued, peak {:.0} dBFS",
            queue.on_time,
            TOLERANCE * 1e3,
            queue.resyncs,
            queue.slipped,
            queue.underruns,
            queue
                .last_short
                .map_or_else(|| "none".to_string(), |short| short.to_string()),
            queue.restarts,
            queue
                .last_restart
                .map_or_else(|| "none".to_string(), |ms| format!("{ms:.1}")),
            self.misses.load(std::sync::atomic::Ordering::Relaxed),
            (queue.samples.len() / CHANNELS) as f64 / f64::from(RATE) * 1e3,
            20.0 * peak.max(1e-6).log10()
        )
    }

    /// Play `samples` (interleaved stereo at [`RATE`]) from `at` seconds on
    /// the playout clock.
    pub fn push(&self, at: f64, samples: &[f32]) {
        let mut queue = self.lock();
        queue.push(at, samples);
        queue.pushed = Some(Instant::now());
    }

    /// The queue, even after a panic elsewhere held it.
    fn lock(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// What an output calls for samples: the buffer and when its first sample
/// plays.
type Render = Box<dyn FnMut(&mut [f32], Instant) + Send>;

#[cfg(target_os = "macos")]
mod platform {
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::ptr::{self, NonNull};
    use std::slice;
    use std::time::{Duration, Instant};

    use anyhow::{Result, anyhow, bail};
    use objc2_audio_toolbox::{
        AURenderCallbackStruct, AudioComponentDescription, AudioComponentFindNext,
        AudioComponentInstanceDispose, AudioComponentInstanceNew, AudioOutputUnitStart,
        AudioOutputUnitStop, AudioUnit, AudioUnitInitialize, AudioUnitRenderActionFlags,
        AudioUnitSetProperty, AudioUnitUninitialize, kAudioUnitManufacturer_Apple,
        kAudioUnitProperty_SetRenderCallback, kAudioUnitProperty_StreamFormat,
        kAudioUnitScope_Input, kAudioUnitSubType_DefaultOutput, kAudioUnitType_Output,
    };
    use objc2_core_audio_types::{
        AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp, AudioTimeStampFlags,
        kAudioFormatFlagIsFloat, kAudioFormatFlagIsPacked, kAudioFormatLinearPCM,
    };

    use super::{RATE, Render};

    /// The default output unit, rendering through a boxed callback.
    pub struct Output {
        unit: AudioUnit,
        _render: Box<Render>,
    }

    // SAFETY: started on creation, stopped and disposed on drop; Core
    // Audio allows both from any thread.
    unsafe impl Send for Output {}

    impl Drop for Output {
        fn drop(&mut self) {
            unsafe {
                AudioOutputUnitStop(self.unit);
                AudioUnitUninitialize(self.unit);
                AudioComponentInstanceDispose(self.unit);
            }
        }
    }

    impl Output {
        /// The default output unit follows the system's output itself.
        pub fn alive(&self) -> bool {
            true
        }

        pub fn start(render: Render) -> Result<Self> {
            let description = AudioComponentDescription {
                componentType: kAudioUnitType_Output,
                componentSubType: kAudioUnitSubType_DefaultOutput,
                componentManufacturer: kAudioUnitManufacturer_Apple,
                componentFlags: 0,
                componentFlagsMask: 0,
            };
            let component =
                unsafe { AudioComponentFindNext(ptr::null_mut(), NonNull::from(&description)) };
            if component.is_null() {
                bail!("Core Audio has no output unit");
            }
            let mut unit: AudioUnit = ptr::null_mut();
            check(
                unsafe { AudioComponentInstanceNew(component, NonNull::from(&mut unit)) },
                "create an output unit",
            )?;
            let mut output = Self {
                unit,
                _render: Box::new(render),
            };
            let format = AudioStreamBasicDescription {
                mSampleRate: f64::from(RATE),
                mFormatID: kAudioFormatLinearPCM,
                mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
                mBytesPerPacket: 8,
                mFramesPerPacket: 1,
                mBytesPerFrame: 8,
                mChannelsPerFrame: 2,
                mBitsPerChannel: 32,
                mReserved: 0,
            };
            set(unit, kAudioUnitProperty_StreamFormat, &format)?;
            let callback = AURenderCallbackStruct {
                inputProc: Some(render_callback),
                inputProcRefCon: (&mut *output._render as *mut Render).cast(),
            };
            set(unit, kAudioUnitProperty_SetRenderCallback, &callback)?;
            check(
                unsafe { AudioUnitInitialize(unit) },
                "initialize the output unit",
            )?;
            check(
                unsafe { AudioOutputUnitStart(unit) },
                "start the output unit",
            )?;
            Ok(output)
        }
    }

    fn set<T>(unit: AudioUnit, property: u32, value: &T) -> Result<()> {
        check(
            unsafe {
                AudioUnitSetProperty(
                    unit,
                    property,
                    kAudioUnitScope_Input,
                    0,
                    (value as *const T).cast::<c_void>(),
                    size_of::<T>() as u32,
                )
            },
            &format!("set output unit property {property}"),
        )
    }

    fn check(status: i32, what: &str) -> Result<()> {
        match status {
            0 => Ok(()),
            status => Err(anyhow!("could not {what} (Core Audio error {status})")),
        }
    }

    #[repr(C)]
    struct Timebase {
        numer: u32,
        denom: u32,
    }

    unsafe extern "C" {
        fn mach_timebase_info(info: *mut Timebase) -> i32;
        fn mach_absolute_time() -> u64;
    }

    /// Host-clock ticks in nanoseconds.
    fn ticks_to_nanos(ticks: u64) -> i128 {
        static TIMEBASE: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();
        let (numer, denom) = *TIMEBASE.get_or_init(|| {
            let mut info = Timebase { numer: 0, denom: 0 };
            unsafe { mach_timebase_info(&mut info) };
            (info.numer.max(1), info.denom.max(1))
        });
        i128::from(ticks) * i128::from(numer) / i128::from(denom)
    }

    /// When the buffer of `time` plays, on this process's monotonic clock.
    fn plays(time: &AudioTimeStamp) -> Instant {
        let now = Instant::now();
        if !time.mFlags.contains(AudioTimeStampFlags::HostTimeValid) {
            return now;
        }
        let host_now = unsafe { mach_absolute_time() };
        let ahead = ticks_to_nanos(time.mHostTime) - ticks_to_nanos(host_now);
        match ahead >= 0 {
            true => now + Duration::from_nanos(ahead as u64),
            false => now
                .checked_sub(Duration::from_nanos((-ahead) as u64))
                .unwrap_or(now),
        }
    }

    unsafe extern "C-unwind" fn render_callback(
        refcon: NonNull<c_void>,
        _flags: NonNull<AudioUnitRenderActionFlags>,
        time: NonNull<AudioTimeStamp>,
        _bus: u32,
        _frames: u32,
        data: *mut AudioBufferList,
    ) -> i32 {
        // SAFETY: the refcon is the output's boxed render, which outlives
        // the unit, and only this callback uses it.
        let render = unsafe { refcon.cast::<Render>().as_mut() };
        let Some(list) = (unsafe { data.as_mut() }) else {
            return 0;
        };
        if list.mNumberBuffers == 0 || list.mBuffers[0].mData.is_null() {
            return 0;
        }
        let buffer = &mut list.mBuffers[0];
        let samples = buffer.mDataByteSize as usize / size_of::<f32>();
        let out = unsafe { slice::from_raw_parts_mut(buffer.mData.cast::<f32>(), samples) };
        let at = plays(unsafe { time.as_ref() });
        // A panic must not unwind into Core Audio; play silence instead.
        if catch_unwind(AssertUnwindSafe(|| render(out, at))).is_err() {
            out.fill(0.0);
        }
        0
    }
}

#[cfg(windows)]
mod platform {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, bail};
    use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows::Win32::Media::Audio::{
        AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
        AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioClient,
        IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX, eConsole,
        eRender,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
    };
    use windows::Win32::System::Threading::{
        AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW,
        WaitForSingleObject,
    };
    use windows::core::w;

    use super::{CHANNELS, RATE, Render};

    /// WAVE_FORMAT_IEEE_FLOAT.
    const FLOAT: u16 = 3;
    /// The shared-mode buffer asked for, in 100 ns units (40 ms).
    const BUFFER: i64 = 400_000;

    /// A WASAPI render thread on the default output.
    pub struct Output {
        stop: Arc<AtomicBool>,
        /// Cleared when the render thread ends on its own (the device was
        /// invalidated, unplugged or the default changed away).
        alive: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl Drop for Output {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    impl Output {
        pub fn alive(&self) -> bool {
            self.alive.load(Ordering::SeqCst)
        }

        pub fn start(render: Render) -> Result<Self> {
            let stop = Arc::new(AtomicBool::new(false));
            let alive = Arc::new(AtomicBool::new(true));
            let (ready_tx, ready) = std::sync::mpsc::channel();
            let thread = std::thread::Builder::new()
                .name("remote sound".into())
                .spawn({
                    let (stop, alive) = (stop.clone(), alive.clone());
                    move || {
                        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                        // Audio priority for the render thread (MMCSS).
                        let mut task = 0;
                        let mmcss = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task) };
                        let result = run(render, &stop, &ready_tx);
                        alive.store(false, Ordering::SeqCst);
                        if let Err(error) = result {
                            isocast_diagnostics::diagnostic!("warn"; "video: the sound output stopped: {error:#}");
                            let _ = ready_tx.send(Err(error));
                        }
                        if let Ok(handle) = mmcss {
                            let _ = unsafe { AvRevertMmThreadCharacteristics(handle) };
                        }
                        unsafe { CoUninitialize() };
                    }
                })
                .context("could not start the sound thread")?;
            match ready.recv() {
                Ok(Ok(())) => Ok(Self {
                    stop,
                    alive,
                    thread: Some(thread),
                }),
                Ok(Err(error)) => {
                    let _ = thread.join();
                    Err(error)
                }
                Err(_) => bail!("the sound thread ended"),
            }
        }
    }

    fn run(
        mut render: Render,
        stop: &AtomicBool,
        ready: &std::sync::mpsc::Sender<Result<()>>,
    ) -> Result<()> {
        unsafe {
            let devices: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let device = devices
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .context("no default sound output")?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let format = WAVEFORMATEX {
                wFormatTag: FLOAT,
                nChannels: CHANNELS as u16,
                nSamplesPerSec: RATE,
                nAvgBytesPerSec: RATE * 8,
                nBlockAlign: 8,
                wBitsPerSample: 32,
                cbSize: 0,
            };
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                    BUFFER,
                    0,
                    &format,
                    None,
                )
                .context("the sound output refused 48 kHz stereo")?;
            let guard = Event(CreateEventW(None, false, false, None)?);
            let event = guard.0;
            client.SetEventHandle(event)?;
            let size = client.GetBufferSize()?;
            let latency = Duration::from_nanos(client.GetStreamLatency()?.max(0) as u64 * 100);
            let renderer: IAudioRenderClient = client.GetService()?;
            client.Start()?;
            let _ = ready.send(Ok(()));
            let mut scratch = vec![0f32; size as usize * CHANNELS];
            // Why the loop ended on its own (AUDCLNT_E_DEVICE_INVALIDATED
            // and the like): the caller starts a new output.
            let mut lost = None;
            while !stop.load(Ordering::SeqCst) {
                if WaitForSingleObject(event, 200) != WAIT_OBJECT_0 {
                    continue;
                }
                let padding = match client.GetCurrentPadding() {
                    Ok(padding) => padding,
                    Err(error) => {
                        lost =
                            Some(anyhow::Error::from(error).context("the sound device went away"));
                        break;
                    }
                };
                let frames = size.saturating_sub(padding);
                if frames == 0 {
                    continue;
                }
                // What's queued plays first; then the stream's latency.
                let plays = Instant::now()
                    + latency
                    + Duration::from_secs_f64(f64::from(padding) / f64::from(RATE));
                let out = &mut scratch[..frames as usize * CHANNELS];
                render(out, plays);
                let buffer = match renderer.GetBuffer(frames) {
                    Ok(buffer) => buffer,
                    Err(error) => {
                        lost =
                            Some(anyhow::Error::from(error).context("the sound device went away"));
                        break;
                    }
                };
                std::ptr::copy_nonoverlapping(out.as_ptr(), buffer.cast::<f32>(), out.len());
                let _ = renderer.ReleaseBuffer(frames, 0);
            }
            let _ = client.Stop();
            if let Some(error) = lost {
                return Err(error);
            }
        }
        Ok(())
    }

    /// An event handle closed however `run` returns.
    struct Event(windows::Win32::Foundation::HANDLE);

    impl Drop for Event {
        fn drop(&mut self) {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    pub struct Output;

    impl Output {
        pub fn start(_render: super::Render) -> anyhow::Result<Self> {
            anyhow::bail!("no sound output on this OS")
        }

        pub fn alive(&self) -> bool {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(n: usize, value: f32) -> Vec<f32> {
        vec![value; n * CHANNELS]
    }

    #[test]
    fn plays_on_time_skips_when_late_and_waits_when_early() {
        let frame = 1.0 / f64::from(RATE);
        let mut queue = Queue::new();
        queue.push(1.0, &frames(480, 0.5));
        queue.push(1.0 + 480.0 * frame, &frames(480, 0.25));
        // On time: the first 10 ms.
        let mut out = frames(480, 9.0);
        queue.render(&mut out, 1.0);
        assert!(out.iter().all(|s| *s == 0.5));
        // Then 100 ms late: skipped past all of it, silence.
        let mut out = frames(480, 9.0);
        queue.render(&mut out, 1.12);
        assert!(out.iter().all(|s| *s == 0.0));
        // Early by 50 ms: 2400 frames of silence first.
        queue.push(2.0, &frames(480, 0.5));
        let mut out = frames(4800, 9.0);
        queue.render(&mut out, 1.95);
        assert!(out[..2399 * CHANNELS].iter().all(|s| *s == 0.0));
        assert!(
            out[2401 * CHANNELS..2879 * CHANNELS]
                .iter()
                .all(|s| *s == 0.5)
        );
    }

    /// A minute of 10 ms packets queued 40 ms ahead of an output that
    /// reads 10 ms buffers 20 ms ahead on a clock `ppm` off the engine's:
    /// slipping frames keeps it on time with no resync and no shortfall.
    fn drift(ppm: f64) -> Queue {
        let mut queue = Queue::new();
        let period = 0.010;
        let (mut packet, mut buffer) = (0u32, 0u32);
        // In time order: packets at the engine's rate, buffers at the
        // output's.
        loop {
            let push_at = f64::from(packet) * period;
            let render_at = f64::from(buffer) * period / (1.0 + ppm * 1e-6) + 0.005;
            if push_at.min(render_at) > 60.0 {
                break;
            }
            if push_at <= render_at {
                queue.push(push_at + 0.040, &frames(480, 0.5));
                packet += 1;
            } else {
                let mut out = frames(480, 0.0);
                queue.render(&mut out, render_at + 0.020);
                buffer += 1;
            }
        }
        queue
    }

    #[test]
    fn device_clock_drift_slips_frames_instead_of_resyncing() {
        for ppm in [-2000.0, -500.0, 500.0, 2000.0] {
            let queue = drift(ppm);
            assert_eq!(queue.resyncs, 0, "{ppm} ppm: slipped {}", queue.slipped);
            assert_eq!(queue.underruns, 0, "{ppm} ppm");
            assert!(queue.slipped > 0, "{ppm} ppm");
        }
    }

    /// The read-ahead estimate keeps a peak for at least [`PEAK_HOLD`] (a
    /// 40 ms WASAPI request every 10 s keeps it at 40 ms throughout), then
    /// lets it go within twice that.
    #[test]
    fn the_read_ahead_estimate_holds_its_peak() {
        let start = Instant::now();
        let at = |s: f64| start + Duration::from_secs_f64(s);
        let mut peak = PeakHold::default();
        let mut lowest_between = f64::MAX;
        for i in 0..6000 {
            let t = f64::from(i) * 0.010;
            let ahead = if i % 1000 == 999 { 0.050 } else { 0.020 };
            let held = peak.update(at(t), ahead);
            if t > 10.0 {
                lowest_between = lowest_between.min(held);
            }
        }
        assert_eq!(lowest_between, 0.050);
        // No more big requests: gone within two hold periods.
        let mut held = 0.0;
        for i in 0..4000 {
            held = peak.update(at(60.0 + f64::from(i) * 0.010), 0.020);
        }
        assert_eq!(held, 0.020);
    }

    #[test]
    fn a_gap_starts_the_queue_again() {
        let mut queue = Queue::default();
        queue.push(1.0, &frames(480, 0.5));
        queue.push(1.5, &frames(480, 0.25));
        assert_eq!(queue.start, 1.5);
        assert_eq!(queue.samples.len(), 480 * CHANNELS);
    }
}
