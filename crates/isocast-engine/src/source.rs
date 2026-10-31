//! Opening sources. Each kind's opener starts what produces it (a device
//! capture, or a generator thread) delivering video frames into a
//! [`FrameSlot`] and audio into an [`AudioFeed`], stamped on the host
//! clock. Recordings, the program and snapshots all read from there.
//!
//! To add a kind: give it settings in `isocast_protocol::source`, then an
//! arm in [`open`] (and a [`Platform`] method if it needs the OS).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, sync_channel};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use isocast_protocol::platform_text as wording;
use isocast_protocol::{
    CameraSource, MediaSource, Setup, SlideshowSource, Source, SourceKind, TestPatternSource,
    TextSource, ToneSource,
};
use parking_lot::{Condvar, Mutex};

use crate::data::DataStore;
use crate::picture::{self, Picture};
use crate::record::expand_home;
use crate::synthetic::{TestPattern, Tone};
use crate::text;
use crate::{
    AudioSink, BrowserControl, BrowserPage, CameraMetadataReader, CameraRequest, CaptureFormat,
    FramePool, MediaInfo, MediaPart, MediaReader, PageLocation, Platform, SAMPLE_RATE,
    ScreenTarget, Time, VideoSink,
};

/// Audio is delivered in 10 ms chunks by generators, like a capture device.
pub(crate) const AUDIO_CHUNK: u64 = SAMPLE_RATE as u64 / 100;
/// Audio chunks a source may queue before its reader takes them.
const AUDIO_QUEUE: usize = 1000;

/// The session's timeline: when it starts (generators line their frames
/// and beeps up with it; frames before it don't count as captured), and
/// which sources the program has on air, since when.
#[derive(Default)]
pub(crate) struct Timeline {
    pub(crate) clocks: Arc<crate::pacing::Clocks>,
    start: OnceLock<Time>,
    /// Source ids on air in the program and when each came on air; `None`
    /// when there is no program, so every source counts as on air from the
    /// start.
    on_air: Mutex<Option<HashMap<String, (Time, u64)>>>,
    /// Which bit of an on-air entry each canvas has (#84), main first.
    canvas_bits: Mutex<Vec<isocast_protocol::CanvasId>>,
    /// The program has said what it shows at least once.
    published: AtomicBool,
}

impl Timeline {
    pub(crate) fn set_start(&self, at: Time) {
        let _ = self.start.set(at);
    }

    pub(crate) fn start(&self) -> Option<Time> {
        self.start.get().copied()
    }

    /// Wait for the timeline to start, unless `stop` is set first.
    fn wait(&self, stop: &AtomicBool) -> Option<Time> {
        loop {
            if stop.load(Ordering::SeqCst) {
                return None;
            }
            if let Some(at) = self.start() {
                return Some(at);
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Start tracking which sources the program has on air.
    pub(crate) fn track_on_air(&self) {
        self.on_air.lock().get_or_insert_with(HashMap::new);
    }

    /// A canvas shows `sources` from `at`. A source stays on air while
    /// either canvas shows it, so switching one cannot stop the other's media.
    pub(crate) fn set_on_air<'a>(
        &self,
        canvas: isocast_protocol::CanvasId,
        sources: impl IntoIterator<Item = &'a str>,
        at: Time,
    ) {
        let mut on_air = self.on_air.lock();
        let Some(on_air) = on_air.as_mut() else {
            return;
        };
        let now: HashSet<&str> = sources.into_iter().collect();
        let bit = {
            let mut bits = self.canvas_bits.lock();
            if bits.is_empty() {
                bits.extend([
                    isocast_protocol::CanvasId::Main,
                    isocast_protocol::CanvasId::Vertical,
                ]);
            }
            let index = match bits.iter().position(|c| *c == canvas) {
                Some(index) => index,
                None => {
                    bits.push(canvas);
                    bits.len() - 1
                }
            };
            // An engine has at most 17 canvases in its life; past 64 they
            // share the last bit, which only keeps a source on air longer.
            1u64 << index.min(63)
        };
        on_air.retain(|id, (_, shown)| {
            if !now.contains(id.as_str()) {
                *shown &= !bit;
            }
            *shown != 0
        });
        for id in now {
            on_air.entry(id.to_string()).or_insert((at, 0)).1 |= bit;
        }
        self.published.store(true, Ordering::SeqCst);
    }

    /// Whether what's on air is known yet: with a program, once it has
    /// drawn its first frame.
    fn on_air_known(&self) -> bool {
        self.on_air.lock().is_none() || self.published.load(Ordering::SeqCst)
    }

    /// Since when source `id` has been on air, if it is; with no program,
    /// since the start.
    fn on_air_since(&self, id: &str) -> Option<Time> {
        match self.on_air.lock().as_ref() {
            Some(on_air) => on_air.get(id).map(|(since, _)| *since),
            None => self.start(),
        }
    }
}

/// The latest frames from a source, for frame clocks to choose from.
pub(crate) struct FrameSlot<F> {
    frames: Mutex<VecDeque<(F, Time)>>,
    /// Media's decoded next pictures. Timed readers can select them without
    /// waiting for the producer's delivery wake; untimed readers cannot.
    ahead: Option<Mutex<VecDeque<(F, Time)>>>,
    /// The most frames kept.
    capacity: usize,
    /// Signalled when a frame arrives.
    arrived: Condvar,
    stopped: Mutex<Option<String>>,
    /// Frames received since the timeline started.
    pub(crate) captured: AtomicU64,
    /// When the last second's frames arrived, to measure the rate.
    arrivals: Mutex<VecDeque<Instant>>,
    timeline: Arc<Timeline>,
}

impl<F: Clone> FrameSlot<F> {
    /// The compositor's newest frame and the ISO clock's half-frame older
    /// selection, plus one arriving frame. Capture (notably SCK's eight
    /// surfaces) needs the rest back rather than a six-frame history.
    const CAPACITY: usize = 3;

    pub(crate) fn new(timeline: Arc<Timeline>) -> Self {
        Self::with_capacity(timeline, Self::CAPACITY)
    }

    /// A slot keeping at most `capacity` frames: for frames only ever read
    /// newest first, like the program's canvases.
    pub(crate) fn with_capacity(timeline: Arc<Timeline>, capacity: usize) -> Self {
        Self {
            frames: Mutex::new(VecDeque::with_capacity(capacity)),
            ahead: None,
            capacity: capacity.max(1),
            arrived: Condvar::new(),
            stopped: Mutex::new(None),
            captured: AtomicU64::new(0),
            arrivals: Mutex::new(VecDeque::new()),
            timeline,
        }
    }

    pub(crate) fn push(&self, frame: F, at: Time) {
        let now = Instant::now();
        let mut arrivals = self.arrivals.lock();
        while arrivals
            .front()
            .is_some_and(|t| now - *t > Duration::from_secs(1))
            || arrivals.len() >= 1000
        {
            arrivals.pop_front();
        }
        arrivals.push_back(now);
        drop(arrivals);
        if self.timeline.start().is_some_and(|start| at >= start) {
            self.captured.fetch_add(1, Ordering::Relaxed);
        }
        let mut frames = self.frames.lock();
        while frames.len() >= self.capacity {
            frames.pop_front();
        }
        frames.push_back((frame, at));
        drop(frames);
        self.arrived.notify_all();
    }

    /// The newest decoded media frame whose timestamp is in the reader's
    /// presentation window. Readers share the buffers without copying pixels.
    fn ahead_until(&self, limit: Time) -> Option<(F, Time)> {
        self.ahead
            .as_ref()?
            .lock()
            .iter()
            .rev()
            .find(|(_, at)| *at <= limit)
            .cloned()
    }

    /// The newest frame captured at or after `from`, and when, waiting up
    /// to `timeout` for one to arrive; leaves it for the frame clock.
    pub(crate) fn wait_from(&self, from: Time, timeout: Duration) -> Option<(F, Time)> {
        let mut frames = self.frames.lock();
        self.arrived.wait_while_for(
            &mut frames,
            |frames| frames.back().is_none_or(|(_, at)| *at < from),
            timeout,
        );
        frames.back().filter(|(_, at)| *at >= from).cloned()
    }

    /// The newest frame and when it was captured, waiting up to `timeout`
    /// for one if the slot is empty; leaves it for the frame clock.
    pub(crate) fn wait_any(&self, timeout: Duration) -> Option<(F, Time)> {
        let mut frames = self.frames.lock();
        self.arrived
            .wait_while_for(&mut frames, |frames| frames.is_empty(), timeout);
        frames.back().cloned()
    }

    /// The newest frame captured no later than `limit`. Other readers may
    /// still be drawing an earlier tick, so only the producer trims history.
    pub(crate) fn newest_until(&self, limit: Time) -> Option<F> {
        self.peek_until_timed(limit).map(|(frame, _)| frame)
    }

    /// The newest frame captured no later than `limit`, and when (which
    /// tells it from the frame before), leaving it (and older ones) for the
    /// frame clock.
    pub(crate) fn peek_until_timed(&self, limit: Time) -> Option<(F, Time)> {
        if let Some(frame) = self.ahead_until(limit) {
            return Some(frame);
        }
        let frames = self.frames.lock();
        let (frame, at) = frames.iter().rev().find(|(_, at)| *at <= limit)?;
        Some((frame.clone(), *at))
    }

    /// The newest frame captured from `from` to `until`, and when, leaving
    /// it for the frame clock.
    pub(crate) fn peek_between(&self, from: Time, until: Time) -> Option<(F, Time)> {
        if let Some(frame) = self.ahead_until(until).filter(|(_, at)| *at >= from) {
            return Some(frame);
        }
        let frames = self.frames.lock();
        let (frame, at) = frames
            .iter()
            .rev()
            .find(|(_, at)| *at >= from && *at <= until)?;
        Some((frame.clone(), *at))
    }

    /// Frames received per second over the last second; 0 if fewer than
    /// two came, or none for half a second.
    pub(crate) fn received_fps(&self) -> f64 {
        let now = Instant::now();
        let arrivals = self.arrivals.lock();
        let recent = arrivals
            .iter()
            .filter(|t| now - **t <= Duration::from_secs(1));
        let (first, last, count) = recent.fold((None, None, 0u32), |(first, _, n), t| {
            (first.or(Some(*t)), Some(*t), n + 1)
        });
        match (first, last) {
            (Some(first), Some(last)) if count >= 2 && now - last <= Duration::from_millis(500) => {
                f64::from(count - 1) / (last - first).as_secs_f64()
            }
            _ => 0.0,
        }
    }

    /// Forget every frame.
    pub(crate) fn clear(&self) {
        // Same lock order as media delivery: invalidation and promotion
        // of a pending picture must not leave either store selectable.
        let mut ahead = self.ahead.as_ref().map(|ahead| ahead.lock());
        let mut frames = self.frames.lock();
        frames.clear();
        if let Some(ahead) = ahead.as_mut() {
            ahead.clear();
        }
    }

    /// A stinger starts a new play only after every canvas let it go.
    /// Hold the on-air lock through invalidation so a concurrent canvas
    /// cannot activate the shared source between the check and the clear.
    pub(crate) fn clear_if_inactive(&self, id: &str) {
        let on_air = self.timeline.on_air.lock();
        if on_air
            .as_ref()
            .is_none_or(|sources| !sources.contains_key(id))
        {
            self.clear();
        }
    }

    /// The newest frame and when it was captured, leaving it for the frame
    /// clock.
    pub(crate) fn newest_timed(&self) -> Option<(F, Time)> {
        self.frames.lock().back().cloned()
    }

    /// The newest frame, leaving it for the frame clock.
    pub(crate) fn newest(&self) -> Option<F> {
        self.frames.lock().back().map(|(frame, _)| frame.clone())
    }

    pub(crate) fn has_frame(&self) -> bool {
        !self.frames.lock().is_empty()
    }

    pub(crate) fn stop(&self, reason: String) {
        self.stopped.lock().get_or_insert(reason);
    }

    pub(crate) fn stopped(&self) -> Option<String> {
        self.stopped.lock().clone()
    }

    /// Update a stopped output's reason without briefly making it healthy.
    pub(crate) fn replace_stop(&self, reason: String) {
        *self.stopped.lock() = Some(reason);
    }

    /// Forget why it stopped: something feeds it again (a new compositor
    /// for a canvas whose last one failed).
    pub(crate) fn resume(&self) {
        self.stopped.lock().take();
    }
}

/// State shared between an audio source and its reader.
#[derive(Default)]
pub(crate) struct AudioFeed {
    /// The source has delivered audio (or is ready to).
    pub(crate) started: AtomicBool,
    pub(crate) stopped: Mutex<Option<String>>,
    pub(crate) gap: OnceLock<GapCallback>,
}

pub(crate) type GapCallback = Box<dyn Fn(Time, Duration) + Send + Sync>;

/// Interleaved audio chunks and when their first sample was captured.
pub(crate) type AudioChunks = Receiver<(Arc<[f32]>, Time)>;

pub(crate) struct Video<F> {
    pub(crate) slot: Arc<FrameSlot<F>>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// The rate files recording it run at.
    pub(crate) fps: u32,
}

pub(crate) struct Audio {
    pub(crate) feed: Arc<AudioFeed>,
    pub(crate) chunks: AudioChunks,
    pub(crate) channels: u32,
}

/// An opened source: what it delivers. Dropping it stops it.
pub(crate) struct Opened<P: Platform> {
    pub(crate) video: Option<Video<P::Frame>>,
    pub(crate) audio: Option<Audio>,
    _running: Running<P>,
}

/// Keeps a source running until dropped.
enum Running<P: Platform> {
    Capture {
        _capture: P::Capture,
        /// A camera's metadata reader, from [`Platform::camera_metadata`].
        camera: Option<CameraMetadataReader>,
    },
    Generator {
        _generator: Generator,
    },
    /// A still picture: its frame stays in the slot.
    Still,
    /// Several generators, like a media file's video and audio.
    Generators {
        _generators: Vec<Generator>,
    },
    /// A web page, and what reloads it as it comes on and off air (which
    /// stops before the page closes).
    Browser {
        _watcher: Option<Generator>,
        control: Arc<dyn BrowserControl>,
        _capture: P::Capture,
    },
}

impl<P: Platform> Opened<P> {
    pub(crate) fn failure(&self) -> Option<String> {
        let get = |g: &Generator| g.failure.lock().clone();
        match &self._running {
            Running::Generator { _generator } => get(_generator),
            Running::Generators { _generators } => _generators.iter().find_map(get),
            Running::Browser { _watcher, .. } => _watcher.as_ref().and_then(get),
            _ => None,
        }
    }
    /// What a camera source's capture has observed, if its platform says.
    pub(crate) fn camera_metadata(&self) -> Option<CameraMetadataReader> {
        match &self._running {
            Running::Capture { camera, .. } => camera.clone(),
            _ => None,
        }
    }
    /// The page of a browser source.
    pub(crate) fn browser(&self) -> Option<Arc<dyn BrowserControl>> {
        match &self._running {
            Running::Browser { control, .. } => Some(control.clone()),
            _ => None,
        }
    }
}

/// A thread producing a synthetic source, stopped when dropped.
struct Generator {
    failure: Arc<Mutex<Option<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Generator {
    fn spawn(run: impl FnOnce(&AtomicBool) + Send + 'static) -> Self {
        let failure = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = thread::spawn({
            let stop = stop.clone();
            let failure = failure.clone();
            move || {
                if let Err(message) =
                    isocast_diagnostics::contain("source generator", || run(&stop))
                {
                    isocast_diagnostics::emit(
                        "error",
                        "source",
                        "source worker failed",
                        serde_json::json!({"error":message}),
                    );
                    *failure.lock() = Some(message);
                }
            }
        });
        Self {
            failure,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Generator {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// How long opening a source may take before it is given up on.
const OPEN_TIMEOUT: Duration = Duration::from_secs(8);
/// Given-up opens of one source that may still be stuck in macOS at once.
/// A native call can't be cancelled, so each holds its thread until macOS
/// returns (a driver that stops answering made Core Audio wait 11 minutes,
/// #82); a retry still starts a fresh open, since the device may answer
/// again, but no more than this many pile up.
const STUCK_PER_SOURCE: usize = 2;
/// Opens, stuck or not, of all sources at once.
const OPENS_AT_MOST: usize = 32;

/// What the user can do about a source that doesn't answer.
pub(crate) struct OpenHelp {
    /// What is opened, for messages: `microphone "Studio Audio"`.
    what: String,
    /// Waiting for this permission's prompt, if one may be up.
    permission: Option<String>,
    /// A device with a driver of its own (a camera or microphone).
    device: bool,
}

impl OpenHelp {
    fn context(&self) -> String {
        match (&self.permission, self.device) {
            (Some(p), _) => format!(
                "waiting for permission: {p}; {}",
                wording::select_linux(
                    "check the macOS permission prompt or Privacy & Security",
                    "check Windows Settings › Privacy & security (Privacy on Windows 10); Camera and Microphone must allow desktop apps (isocastd.exe)",
                    "check that the device exists and isn't held by another app, and that PipeWire and the desktop's screen-sharing portal are running"
                )
            ),
            (None, true) => format!(
                "its driver, or the app that provides it, isn't answering {}: \
                 quit and reopen that app or unplug and replug the device (or choose another \
                 device), then start again",
                wording::OS
            ),
            (None, false) => "check its file/device, then start again".into(),
        }
    }
}

/// Run a native open on its own thread for the platform's bounded timeout,
/// including time spent behind macOS prompts. `slot` names what is opened
/// (a source and its device): a timed-out open is given up on and the next
/// one starts afresh, while at most [`STUCK_PER_SOURCE`] given-up opens of
/// one slot may still be running.
fn bounded_open<T: Send + 'static>(
    slot: String,
    help: OpenHelp,
    timeout: Duration,
    run: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    /// The opens still running, by slot: each one's token and when it
    /// began; notified as each returns.
    type Running = (Mutex<HashMap<String, Vec<(u64, Instant)>>>, Condvar);
    static PENDING: OnceLock<Running> = OnceLock::new();
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let pending = PENDING.get_or_init(Default::default);
    let context = help.context();
    let what = &help.what;
    let token = NEXT.fetch_add(1, Ordering::Relaxed);
    let deadline = Instant::now() + timeout;
    {
        let mut slots = pending.0.lock();
        // With too many stuck, wait (within the same time) for one to
        // return: a device that answers again opens at once.
        loop {
            let running = slots.get(&slot).map_or(&[][..], Vec::as_slice);
            if running.len() < STUCK_PER_SOURCE {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                let oldest = running.iter().map(|(_, began)| began.elapsed()).max();
                bail!(
                    "{what} is not answering: {} earlier opens are still waiting on {os} \
                     (the oldest for {} s); {context}",
                    running.len(),
                    oldest.unwrap_or_default().as_secs(),
                    os = wording::OS
                );
            }
            pending.1.wait_for(&mut slots, deadline - now);
        }
        if slots.values().map(Vec::len).sum::<usize>() >= OPENS_AT_MOST {
            bail!(
                "too many sources are opening at once (at most {OPENS_AT_MOST}); check \
                 permissions and devices"
            );
        }
        slots
            .entry(slot.clone())
            .or_default()
            .push((token, Instant::now()));
    }
    // Frees the slot when the native call returns, however late, and says
    // how a given-up open ended (devices that wake up again, or fail).
    struct Pending {
        pending: &'static Running,
        slot: String,
        what: String,
        token: u64,
        abandoned: Arc<AtomicBool>,
        outcome: Option<String>,
    }
    impl Drop for Pending {
        fn drop(&mut self) {
            let mut slots = self.pending.0.lock();
            let mut began = None;
            if let Some(running) = slots.get_mut(&self.slot) {
                if let Some(at) = running.iter().position(|(t, _)| *t == self.token) {
                    began = Some(running.remove(at).1);
                }
                if running.is_empty() {
                    slots.remove(&self.slot);
                }
            }
            drop(slots);
            self.pending.1.notify_all();
            if self.abandoned.load(Ordering::SeqCst) {
                isocast_diagnostics::emit(
                    "warn",
                    "source",
                    "given-up source open returned",
                    serde_json::json!({
                        "source": self.what,
                        "seconds": began.map(|b| b.elapsed().as_secs_f64()),
                        "outcome": self.outcome.as_deref().unwrap_or("crashed"),
                    }),
                );
            }
        }
    }
    let abandoned = Arc::new(AtomicBool::new(false));
    let mut guard = Pending {
        pending,
        slot,
        what: what.clone(),
        token,
        abandoned: abandoned.clone(),
        outcome: None,
    };
    let (tx, rx) = sync_channel(1);
    thread::spawn(move || {
        let result = isocast_diagnostics::contain("native source open", run)
            .map_err(anyhow::Error::msg)
            .and_then(|r| r);
        guard.outcome = Some(match &result {
            Ok(_) => "opened, then closed".into(),
            Err(err) => format!("failed: {err:#}"),
        });
        // A given-up open's capture is dropped (closed) here, unused.
        let _ = tx.send(result);
        drop(guard);
    });
    match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => {
            abandoned.store(true, Ordering::SeqCst);
            // It may have finished just now: then use it after all.
            if let Ok(result) = rx.try_recv() {
                abandoned.store(false, Ordering::SeqCst);
                return result;
            }
            isocast_diagnostics::emit(
                "warn",
                "source",
                "source open given up",
                serde_json::json!({"source": what, "seconds": timeout.as_secs()}),
            );
            bail!(
                "{what} did not open within {} s; {context}",
                timeout.as_secs()
            )
        }
        Err(RecvTimeoutError::Disconnected) => bail!("opening {what} crashed"),
    }
}

/// Open the setup's source `id`, on the timeline that starts at `timeline`;
/// text showing a data key shows `data`'s value.
pub(crate) fn open<P: Platform>(
    platform: &P,
    setup: &Setup,
    id: &str,
    timeline: &Arc<Timeline>,
    data: &Arc<DataStore>,
) -> Result<Opened<P>> {
    let kind = setup.source(id).map(|s| &s.kind);
    let permission = kind.and_then(|kind| platform.source_permission(kind));
    // Opens are told apart by source and device, so choosing another
    // device opens at once even while the old one is stuck.
    let slot = format!(
        "source {id:?} {}",
        kind.and_then(|k| serde_json::to_string(k).ok())
            .unwrap_or_default()
    );
    let help = match kind {
        Some(SourceKind::Microphone(m)) => OpenHelp {
            what: format!("microphone {:?}", m.device),
            permission,
            device: true,
        },
        Some(SourceKind::Camera(c)) => OpenHelp {
            what: format!("camera {:?}", c.device),
            permission,
            device: true,
        },
        _ => OpenHelp {
            what: "the source".into(),
            permission,
            device: false,
        },
    };
    let (platform, setup, id, timeline, data) = (
        platform.clone(),
        setup.clone(),
        id.to_string(),
        timeline.clone(),
        data.clone(),
    );
    let (key, began) = (format!("source {id:?}"), std::time::Instant::now());
    let timeout = kind.map_or(OPEN_TIMEOUT, |kind| platform.source_open_timeout(kind));
    let opened = bounded_open(slot, help, timeout, move || {
        open_native(&platform, &setup, &id, &timeline, &data)
    });
    isocast_diagnostics::debug!(
        "capture: {key} {} after {:?}",
        if opened.is_ok() { "opened" } else { "failed" },
        began.elapsed()
    );
    opened
}

fn open_native<P: Platform>(
    platform: &P,
    setup: &Setup,
    id: &str,
    timeline: &Arc<Timeline>,
    data: &Arc<DataStore>,
) -> Result<Opened<P>> {
    let Some(source) = setup.source(id) else {
        let ids: Vec<&str> = setup.sources.iter().map(|s| s.id.as_str()).collect();
        bail!("no source {id:?}; sources: {}", ids.join(", "));
    };
    match &source.kind {
        SourceKind::Display(display) => screen(
            platform,
            setup,
            source,
            timeline,
            ScreenTarget::Display(&display.display),
            display.cursor,
        ),
        SourceKind::Window(window) => screen(
            platform,
            setup,
            source,
            timeline,
            ScreenTarget::Window {
                app: &window.app,
                title: window.title.as_deref(),
                source: &source.id,
            },
            window.cursor,
        ),
        SourceKind::Application(application) => screen(
            platform,
            setup,
            source,
            timeline,
            ScreenTarget::Application {
                app: &application.app,
                display: &application.display,
            },
            application.cursor,
        ),
        SourceKind::Camera(camera) => {
            let CameraSource {
                device,
                width,
                height,
                fps,
            } = camera;
            let slot = Arc::new(FrameSlot::new(timeline.clone()));
            let request = CameraRequest {
                size: width.zip(*height),
                fps: *fps,
            };
            let sink = slot_sink(&slot);
            let (capture, format) =
                platform.capture_camera(device, request, &setup.canvas, sink)?;
            let camera = platform.camera_metadata(&capture);
            Ok(captured_video(slot, capture, format, camera))
        }
        SourceKind::Microphone(microphone) => {
            let (feed, sink, chunks) = audio_feed(id);
            let (capture, channels) = platform.capture_microphone(
                &microphone.device,
                microphone.channels.as_deref(),
                sink,
            )?;
            Ok(captured_audio(feed, chunks, capture, channels))
        }
        SourceKind::SystemAudio(system) => {
            let (feed, sink, chunks) = audio_feed(id);
            // What monitoring plays would otherwise be recorded again.
            let (capture, channels) =
                platform.capture_system_audio(&system.exclude, sink, setup.monitors())?;
            Ok(captured_audio(feed, chunks, capture, channels))
        }
        SourceKind::ApplicationAudio(application) => {
            let (feed, sink, chunks) = audio_feed(id);
            let (capture, channels) = platform.capture_application_audio(&application.app, sink)?;
            Ok(captured_audio(feed, chunks, capture, channels))
        }
        SourceKind::TestPattern(pattern) => test_pattern(platform, id, pattern, timeline),
        SourceKind::Color(color) => {
            let [r, g, b, a] = color.color.rgba().context("not a colour")?;
            let byte = |v: f32| (v * 255.0).round() as u8;
            let (width, height) = (
                color.width.unwrap_or(setup.canvas.width),
                color.height.unwrap_or(setup.canvas.height),
            );
            let picture = Picture {
                width,
                height,
                rgba: [byte(r), byte(g), byte(b), byte(a)].repeat((width * height) as usize),
            };
            still(platform, timeline, &picture, setup.canvas.fps)
        }
        SourceKind::Image(image) => {
            let path = expand_home(&image.path);
            let mut frames = picture::load(&path, image.width.zip(image.height))?;
            if frames.len() == 1 {
                let (picture, _) = frames.remove(0);
                still(platform, timeline, &picture, setup.canvas.fps)
            } else {
                animation(platform, id, timeline, frames, setup.canvas.fps)
            }
        }
        SourceKind::Slideshow(slideshow) => {
            slideshow_generator(platform, id, slideshow, setup, timeline)
        }
        SourceKind::Text(settings) => match (&settings.file, &settings.data) {
            (Some(file), _) => text_file(
                platform,
                settings,
                expand_home(file),
                timeline,
                setup.canvas.fps,
            ),
            (None, Some(key)) => {
                text_data(platform, settings, key, data, timeline, setup.canvas.fps)
            }
            (None, None) => {
                let picture = text::render(settings, settings.text.as_deref().unwrap_or(""));
                still(platform, timeline, &picture, setup.canvas.fps)
            }
        },
        SourceKind::Tone(tone) => Ok(tone_generator(platform, id, tone, timeline)),
        SourceKind::Media(settings) => media(platform, id, settings, setup, timeline, true),
        SourceKind::Browser(browser) => {
            let file = browser.file.as_deref().map(expand_home);
            let location = match (&browser.url, &file) {
                (Some(url), _) => PageLocation::Url(url),
                (None, Some(file)) => PageLocation::File(file),
                (None, None) => bail!("give the page's url or file"),
            };
            let page = BrowserPage {
                location,
                width: browser.width,
                height: browser.height,
                fps: browser.fps,
                css: &browser.css,
            };
            let slot = Arc::new(FrameSlot::new(timeline.clone()));
            let (audio, audio_sink) = match browser.control_audio {
                true => {
                    let (feed, sink, chunks) = audio_feed(id);
                    // A page may stay silent; sessions needn't wait for it.
                    feed.started.store(true, Ordering::SeqCst);
                    let audio = Audio {
                        feed,
                        chunks,
                        channels: 2,
                    };
                    (Some(audio), Some(sink))
                }
                false => (None, None),
            };
            let (capture, format, control) =
                platform.capture_browser(&page, slot_sink(&slot), audio_sink)?;
            let watcher = (browser.shutdown_when_hidden || browser.refresh_when_shown).then(|| {
                let (id, timeline, control) = (id.to_string(), timeline.clone(), control.clone());
                let (shutdown, refresh) =
                    (browser.shutdown_when_hidden, browser.refresh_when_shown);
                Generator::spawn(move |stop| {
                    follow_air(&id, &timeline, &*control, shutdown, refresh, stop)
                })
            });
            Ok(Opened {
                video: Some(Video {
                    slot,
                    width: format.width,
                    height: format.height,
                    // Files recording it follow the canvas.
                    fps: setup.canvas.fps,
                }),
                audio,
                _running: Running::Browser {
                    _watcher: watcher,
                    control,
                    _capture: capture,
                },
            })
        }
    }
}

/// A screen capture: the same pixels a recording of the setup gets.
fn screen<P: Platform>(
    platform: &P,
    setup: &Setup,
    source: &Source,
    timeline: &Arc<Timeline>,
    target: ScreenTarget<'_>,
    cursor: bool,
) -> Result<Opened<P>> {
    let slot = Arc::new(FrameSlot::new(timeline.clone()));
    // Its depth follows its ISO file (`IsoRecording::captures_ten_bit`).
    let ten_bit = setup.recording.iso.captures_ten_bit(source);
    // Screens have no frame rate of their own; they follow the canvas.
    let sink = slot_sink(&slot);
    let (capture, format) =
        platform.capture_screen(target, setup.canvas.fps, ten_bit, cursor, sink)?;
    Ok(captured_video(slot, capture, format, None))
}

/// Unload a browser source's page while the program doesn't show it
/// (`shutdown`) and reload it when shown again (`refresh`), until `stop`.
fn follow_air(
    id: &str,
    timeline: &Timeline,
    control: &dyn BrowserControl,
    shutdown: bool,
    refresh: bool,
    stop: &AtomicBool,
) {
    if timeline.wait(stop).is_none() {
        return;
    }
    while !timeline.on_air_known() {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    // The page loaded when the source opened.
    let mut loaded = true;
    let mut last = None;
    let mut first = true;
    while !stop.load(Ordering::SeqCst) {
        let since = timeline.on_air_since(id);
        if first || since != last {
            match since {
                None if shutdown && loaded => {
                    control.set_loaded(false);
                    loaded = false;
                }
                None => {}
                Some(_) if shutdown && !loaded => {
                    control.set_loaded(true);
                    loaded = true;
                }
                Some(_) if refresh && !first => control.refresh(),
                Some(_) => {}
            }
            last = since;
            first = false;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn captured_video<P: Platform>(
    slot: Arc<FrameSlot<P::Frame>>,
    capture: P::Capture,
    format: CaptureFormat,
    camera: Option<CameraMetadataReader>,
) -> Opened<P> {
    Opened {
        video: Some(Video {
            slot,
            width: format.width,
            height: format.height,
            fps: format.fps,
        }),
        audio: None,
        _running: Running::Capture {
            _capture: capture,
            camera,
        },
    }
}

fn captured_audio<P: Platform>(
    feed: Arc<AudioFeed>,
    chunks: AudioChunks,
    capture: P::Capture,
    channels: u32,
) -> Opened<P> {
    Opened {
        video: None,
        audio: Some(Audio {
            feed,
            chunks,
            channels,
        }),
        _running: Running::Capture {
            _capture: capture,
            camera: None,
        },
    }
}

/// A sink that feeds a capture's frames into `slot`.
fn slot_sink<F: Clone + Send + 'static>(slot: &Arc<FrameSlot<F>>) -> VideoSink<F> {
    VideoSink {
        frame: Box::new({
            let slot = slot.clone();
            move |frame, at| slot.push(frame, at)
        }),
        stopped: Box::new({
            let slot = slot.clone();
            move |reason| slot.stop(reason)
        }),
    }
}

/// A feed for an audio capture, and the sink it delivers into.
fn audio_feed(id: &str) -> (Arc<AudioFeed>, AudioSink, AudioChunks) {
    let feed = Arc::new(AudioFeed::default());
    let (tx, rx) = sync_channel(AUDIO_QUEUE);
    let sink = AudioSink {
        report_gap: Box::new({
            let (id, feed) = (id.to_owned(), feed.clone());
            move |until, duration| {
                isocast_diagnostics::diagnostic!("warn";
                    "audio source {id:?} reported a {:.2} ms gap", duration.as_secs_f64() * 1000.0
                );
                if let Some(report) = feed.gap.get() {
                    report(until, duration);
                }
            }
        }),
        samples: Box::new({
            let feed = feed.clone();
            move |samples, at| {
                feed.started.store(true, Ordering::SeqCst);
                let _ = tx.try_send((samples.into(), at));
            }
        }),
        stopped: Box::new({
            let feed = feed.clone();
            move |reason| {
                feed.stopped.lock().get_or_insert(reason);
            }
        }),
    };
    (feed, sink, rx)
}

/// The test pattern: a first frame at once, then one frame per tick of the
/// timeline, so its flashes fall exactly on whole seconds.
fn test_pattern<P: Platform>(
    platform: &P,
    id: &str,
    settings: &TestPatternSource,
    timeline: &Arc<Timeline>,
) -> Result<Opened<P>> {
    let &TestPatternSource { width, height, fps } = settings;
    let pattern = TestPattern::new(id, width, height, fps);
    // Opt-in fault exercise for an isolated developer-mode engine: e.g.
    // ISOCAST_TEST_PATTERN_FAIL_AFTER=bars=5 stops that synthetic capture
    // five seconds into the live clock. Real capture sources are unaffected.
    let fail_after = std::env::var("ISOCAST_TEST_PATTERN_FAIL_AFTER")
        .ok()
        .and_then(|value| {
            let (source, seconds) = value.split_once('=')?;
            let seconds: f64 = seconds.parse().ok()?;
            (source == id && seconds.is_finite() && seconds >= 0.0)
                .then(|| Duration::try_from_secs_f64(seconds).ok())
                .flatten()
        });
    let mut clock = timeline
        .clocks
        .start(format!("source:{id}:video"), Duration::from_secs(1) / fps);
    let mut pool = platform.frame_pool(width, height)?;
    let slot = Arc::new(FrameSlot::new(timeline.clone()));
    if let Some(frame) =
        pool.draw(&mut |pixels, stride| pattern.draw(0, Duration::ZERO, pixels, stride))?
    {
        slot.push(frame, platform.now());
    }
    let generator = Generator::spawn({
        let (platform, slot, timeline) = (platform.clone(), slot.clone(), timeline.clone());
        move |stop| {
            // On time like a device's frames (#78): at the default QoS its
            // wakes ran up to 100 ms late, showing a pulse frames late.
            platform.real_time_thread();
            let Some(start) = timeline.wait(stop) else {
                return;
            };
            // A generator that falls behind drops frames, like a real device
            // would, instead of drifting late.
            let late = Duration::from_secs(2) / fps;
            for n in 0.. {
                let pts = start.add_ticks(n, fps);
                if !sleep_until(&platform, &mut clock, pts, stop) {
                    return;
                }
                if platform.now() > pts + late {
                    continue;
                }
                let since_start = pts.saturating_since(start);
                if fail_after.is_some_and(|after| since_start >= after) && crate::trace::on() {
                    slot.stop("test_pattern: injected capture failure (developer mode)".into());
                    return;
                }
                match pool.draw(&mut |pixels, stride| pattern.draw(n, since_start, pixels, stride))
                {
                    Ok(Some(frame)) => slot.push(frame, pts),
                    Ok(None) => {}
                    Err(err) => {
                        slot.stop(format!("{err:#}"));
                        return;
                    }
                }
            }
        }
    });
    Ok(Opened {
        video: Some(Video {
            slot,
            width,
            height,
            fps,
        }),
        audio: None,
        _running: Running::Generator {
            _generator: generator,
        },
    })
}

/// The tone: 10 ms chunks on the timeline, each delivered once all of it
/// has "happened", like a capture device.
fn tone_generator<P: Platform>(
    platform: &P,
    id: &str,
    settings: &ToneSource,
    timeline: &Arc<Timeline>,
) -> Opened<P> {
    let feed = Arc::new(AudioFeed::default());
    // Synthetic audio is ready at once.
    feed.started.store(true, Ordering::SeqCst);
    let (tx, chunks) = sync_channel::<(Arc<[f32]>, Time)>(AUDIO_QUEUE);
    let tone = Tone::new(settings.frequency);
    let mut clock = timeline
        .clocks
        .start(format!("source:{id}:audio"), Duration::from_millis(10));
    let generator = Generator::spawn({
        let (platform, timeline) = (platform.clone(), timeline.clone());
        move |stop| {
            let Some(start) = timeline.wait(stop) else {
                return;
            };
            let channels = Tone::CHANNELS as usize;
            for n in 0.. {
                let first = n * AUDIO_CHUNK;
                if !sleep_until(
                    &platform,
                    &mut clock,
                    start.add_ticks(first + AUDIO_CHUNK, SAMPLE_RATE),
                    stop,
                ) {
                    return;
                }
                let mut samples = vec![0.0; AUDIO_CHUNK as usize * channels];
                tone.fill(first, &mut samples);
                let pts = start.add_ticks(first, SAMPLE_RATE);
                let _ = tx.try_send((samples.into(), pts));
            }
        }
    });
    Opened {
        video: None,
        audio: Some(Audio {
            feed,
            chunks,
            channels: Tone::CHANNELS,
        }),
        _running: Running::Generator {
            _generator: generator,
        },
    }
}

/// A picture that never changes: one frame, kept in the slot and repeated
/// by the frame clock.
fn still<P: Platform>(
    platform: &P,
    timeline: &Arc<Timeline>,
    picture: &Picture,
    fps: u32,
) -> Result<Opened<P>> {
    let mut pool = platform.frame_pool(picture.width, picture.height)?;
    let frame = pool
        .draw(&mut |pixels, stride| picture.write_bgra(pixels, stride))?
        .context("could not draw the picture")?;
    let slot = Arc::new(FrameSlot::new(timeline.clone()));
    slot.push(frame, platform.now());
    Ok(Opened {
        video: Some(Video {
            slot,
            width: picture.width,
            height: picture.height,
            fps,
        }),
        audio: None,
        _running: Running::Still,
    })
}

/// An animated GIF, looping on the timeline.
fn animation<P: Platform>(
    platform: &P,
    id: &str,
    timeline: &Arc<Timeline>,
    frames: Vec<(Picture, Duration)>,
    fps: u32,
) -> Result<Opened<P>> {
    let (width, height) = (frames[0].0.width, frames[0].0.height);
    let mut clock = timeline
        .clocks
        .start(format!("source:{id}:video"), Duration::from_secs(1) / fps);
    let mut pool = platform.frame_pool(width, height)?;
    let slot = Arc::new(FrameSlot::new(timeline.clone()));
    if let Some(frame) = pool.draw(&mut |pixels, stride| frames[0].0.write_bgra(pixels, stride))? {
        slot.push(frame, platform.now());
    }
    let generator = Generator::spawn({
        let (platform, slot, timeline) = (platform.clone(), slot.clone(), timeline.clone());
        move |stop| {
            let Some(start) = timeline.wait(stop) else {
                return;
            };
            let mut at = start;
            for (picture, delay) in frames.iter().cycle() {
                if !sleep_until(&platform, &mut clock, at, stop) {
                    return;
                }
                if let Ok(Some(frame)) =
                    pool.draw(&mut |pixels, stride| picture.write_bgra(pixels, stride))
                {
                    slot.push(frame, at);
                }
                at = at + *delay;
            }
        }
    });
    Ok(Opened {
        video: Some(Video {
            slot,
            width,
            height,
            fps,
        }),
        audio: None,
        _running: Running::Generator {
            _generator: generator,
        },
    })
}

/// Frames drawn per second while a slide show fades between images.
const FADE_FPS: u32 = 30;

/// A slide show: each image for `slide_ms`, fading into the next over the
/// end of it; images are decoded as they come up.
fn slideshow_generator<P: Platform>(
    platform: &P,
    id: &str,
    settings: &SlideshowSource,
    setup: &Setup,
    timeline: &Arc<Timeline>,
) -> Result<Opened<P>> {
    let mut files: Vec<std::path::PathBuf> =
        settings.files.iter().map(|f| expand_home(f)).collect();
    if settings.shuffle {
        // A cheap shuffle seeded by the clock; the order needn't be secure.
        let mut seed = platform.now().as_nanos() as u64 | 1;
        for i in (1..files.len()).rev() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            files.swap(i, (seed % (i as u64 + 1)) as usize);
        }
    }
    let first = first_picture(&files[0])?;
    let (width, height) = settings
        .width
        .zip(settings.height)
        .unwrap_or((first.width, first.height));
    let first = first.fit(width, height);
    let mut pool = platform.frame_pool(width, height)?;
    let slot = Arc::new(FrameSlot::new(timeline.clone()));
    if let Some(frame) = pool.draw(&mut |pixels, stride| first.write_bgra(pixels, stride))? {
        slot.push(frame, platform.now());
    }
    let slide = Duration::from_millis(settings.slide_ms.into());
    let fade = if settings.transition.is_cut() {
        Duration::ZERO
    } else {
        Duration::from_millis(settings.transition.duration_ms.into())
    };
    let repeat = settings.repeat;
    let mut clock = timeline.clocks.start(
        format!("source:{id}:video"),
        Duration::from_secs(1) / FADE_FPS,
    );
    let generator = Generator::spawn({
        let (platform, slot, timeline) = (platform.clone(), slot.clone(), timeline.clone());
        move |stop| {
            let Some(start) = timeline.wait(stop) else {
                return;
            };
            // An image that can't be read shows as transparent.
            let load = |index: usize| {
                first_picture(&files[index % files.len()])
                    .map(|p| p.fit(width, height))
                    .unwrap_or_else(|_| Picture {
                        width,
                        height,
                        rgba: vec![0; (width * height * 4) as usize],
                    })
            };
            let mut show = |picture: &Picture, at: Time| {
                if let Ok(Some(frame)) =
                    pool.draw(&mut |pixels, stride| picture.write_bgra(pixels, stride))
                {
                    slot.push(frame, at);
                }
            };
            let mut current = first;
            for index in 0..u32::MAX {
                let begins = start + slide * index;
                if !sleep_until(&platform, &mut clock, begins, stop) {
                    return;
                }
                show(&current, begins);
                if index as usize + 1 >= files.len() && !repeat {
                    return;
                }
                let next = load(index as usize + 1);
                let fade_from = begins + slide - fade;
                let steps = (fade.as_secs_f64() * f64::from(FADE_FPS)).round() as u32;
                for step in 1..steps {
                    let at = fade_from + fade * step / steps;
                    if !sleep_until(&platform, &mut clock, at, stop) {
                        return;
                    }
                    show(&current.blend(&next, step as f32 / steps as f32), at);
                }
                current = next;
            }
        }
    });
    Ok(Opened {
        video: Some(Video {
            slot,
            width,
            height,
            fps: setup.canvas.fps,
        }),
        audio: None,
        _running: Running::Generator {
            _generator: generator,
        },
    })
}

/// How often a text file is checked for changes.
const TEXT_FILE_POLL: Duration = Duration::from_millis(500);

/// Text read from a file, drawn again whenever the file changes, at one
/// size: the settings' or the first text's.
fn text_file<P: Platform>(
    platform: &P,
    settings: &TextSource,
    path: std::path::PathBuf,
    timeline: &Arc<Timeline>,
    fps: u32,
) -> Result<Opened<P>> {
    let read = |path: &std::path::Path| {
        let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        std::fs::read_to_string(path).map(|text| (text, modified))
    };
    let (contents, mut modified) =
        read(&path).with_context(|| format!("could not read {}", path.display()))?;
    let first = text::render(settings, contents.trim_end());
    let mut settings = settings.clone();
    settings.width = Some(first.width);
    settings.height = Some(first.height);
    let mut pool = platform.frame_pool(first.width, first.height)?;
    let slot = Arc::new(FrameSlot::new(timeline.clone()));
    if let Some(frame) = pool.draw(&mut |pixels, stride| first.write_bgra(pixels, stride))? {
        slot.push(frame, platform.now());
    }
    let generator = Generator::spawn({
        let (platform, slot) = (platform.clone(), slot.clone());
        move |stop| {
            while !stop.load(Ordering::SeqCst) {
                thread::sleep(TEXT_FILE_POLL);
                let Ok((contents, now_modified)) = read(&path) else {
                    continue;
                };
                if now_modified == modified {
                    continue;
                }
                modified = now_modified;
                let picture = text::render(&settings, contents.trim_end());
                if let Ok(Some(frame)) =
                    pool.draw(&mut |pixels, stride| picture.write_bgra(pixels, stride))
                {
                    slot.push(frame, platform.now());
                }
            }
        }
    });
    Ok(Opened {
        video: Some(Video {
            slot,
            width: first.width,
            height: first.height,
            fps,
        }),
        audio: None,
        _running: Running::Generator {
            _generator: generator,
        },
    })
}

/// How long a text showing a data key waits for a change before looking
/// whether it should stop.
const TEXT_DATA_WAIT: Duration = Duration::from_millis(50);

/// Text showing data key `key`'s value, drawn again whenever the value
/// changes (at most once a canvas frame however often it changes), at the
/// settings' fixed size or its own: set `width` and `height` for text
/// that changes, so it keeps one size (its ISO file has the first's, and
/// is scaled to fit).
fn text_data<P: Platform>(
    platform: &P,
    settings: &TextSource,
    key: &str,
    data: &Arc<DataStore>,
    timeline: &Arc<Timeline>,
    fps: u32,
) -> Result<Opened<P>> {
    let first_text = data.text(key);
    let first = text::render(settings, &first_text);
    let (width, height) = (first.width, first.height);
    let mut pool = platform.frame_pool(width, height)?;
    let slot = Arc::new(FrameSlot::new(timeline.clone()));
    if let Some(frame) = pool.draw(&mut |pixels, stride| first.write_bgra(pixels, stride))? {
        slot.push(frame, platform.now());
    }
    // Set when the key changes.
    let changed = Arc::new((Mutex::new(false), Condvar::new()));
    let key: Arc<str> = key.into();
    let listening = data.listen({
        let (changed, key) = (changed.clone(), key.clone());
        Arc::new(move |keys: &[Arc<str>]| {
            if keys.contains(&key) {
                *changed.0.lock() = true;
                changed.1.notify_all();
            }
        })
    });
    let generator = Generator::spawn({
        let (platform, slot, data, settings) = (
            platform.clone(),
            slot.clone(),
            data.clone(),
            settings.clone(),
        );
        move |stop| {
            let _listening = listening;
            let interval = Duration::from_secs(1) / fps.max(1);
            let mut shown = first_text;
            let mut size = (width, height);
            let mut drawn: Option<Instant> = None;
            while !stop.load(Ordering::SeqCst) {
                {
                    let (flag, wake) = &*changed;
                    let mut flag = flag.lock();
                    while !*flag {
                        wake.wait_for(&mut flag, TEXT_DATA_WAIT);
                        if stop.load(Ordering::SeqCst) {
                            return;
                        }
                    }
                    *flag = false;
                }
                // At most once a canvas frame: later changes wait, and the
                // newest value is drawn.
                if let Some(since) = drawn.map(|at| at.elapsed())
                    && since < interval
                {
                    thread::sleep(interval - since);
                }
                let text = data.text(&key);
                if text == shown {
                    continue;
                }
                let picture = text::render(&settings, &text);
                if (picture.width, picture.height) != size {
                    match platform.frame_pool(picture.width, picture.height) {
                        Ok(new) => {
                            pool = new;
                            size = (picture.width, picture.height);
                        }
                        Err(_) => continue,
                    }
                }
                if let Ok(Some(frame)) =
                    pool.draw(&mut |pixels, stride| picture.write_bgra(pixels, stride))
                {
                    slot.push(frame, platform.now());
                }
                shown = text;
                drawn = Some(Instant::now());
            }
        }
    });
    Ok(Opened {
        video: Some(Video {
            slot,
            width,
            height,
            fps,
        }),
        audio: None,
        _running: Running::Generator {
            _generator: generator,
        },
    })
}

/// Video and audio files played on the timeline: each file starts where
/// the one before ended (at the playback speed), the playlist repeating if
/// it loops. Video and audio are read by separate generators on the same
/// schedule, so they stay in step. Without `with_audio`, only the video.
fn media<P: Platform>(
    platform: &P,
    id: &str,
    settings: &MediaSource,
    setup: &Setup,
    timeline: &Arc<Timeline>,
    with_audio: bool,
) -> Result<Opened<P>> {
    let files: Vec<std::path::PathBuf> = settings.files.iter().map(|f| expand_home(f)).collect();
    // Every file is checked now, so a missing one fails before recording.
    let infos: Vec<MediaInfo> = files
        .iter()
        .map(|file| platform.media_info(file))
        .collect::<Result<_>>()?;
    let speed = f64::from(settings.speed);
    let has_video = infos.iter().any(|info| info.video.is_some());
    // Sound alone is on air while a scene on air has an item for it, as in
    // OBS; with no item anywhere it can never come on air, so it plays all
    // along.
    let in_a_scene = setup
        .scenes
        .iter()
        .any(|scene| scene.all_sources().contains(&id));
    let playlist = Playlist {
        id: id.to_string(),
        files: files.into_iter().zip(infos.iter().copied()).collect(),
        speed,
        repeat: settings.repeat,
        restart: settings.restart_on_activate && (has_video || in_a_scene),
    };
    let mut generators = Vec::new();

    let video = match infos.iter().find_map(|info| info.video) {
        None => None,
        Some((width, height)) => {
            let mut clock = timeline.clocks.start(
                format!("source:{id}:video"),
                Duration::from_secs(1) / setup.canvas.fps,
            );
            // Canvases may draw up to three ticks late, independently of
            // the ISOs. Five delivered pictures cover that window at up to
            // 1.5 times the canvas cadence; with two decoded next pictures
            // this holds at most seven of macOS's base eight production
            // buffers. A decoder adds its own reorder depth to that budget.
            let mut slot = FrameSlot::with_capacity(timeline.clone(), 5);
            slot.ahead = Some(Mutex::new(VecDeque::with_capacity(2)));
            let slot = Arc::new(slot);
            // The first frame now, so the source is ready at once.
            let first = playlist
                .files
                .iter()
                .find(|(_, info)| info.video.is_some())
                .map(|(file, _)| file)
                .expect("found");
            if let Some((frame, _)) = platform.open_media(first, MediaPart::Video)?.next_video()? {
                slot.push(frame, platform.now());
            }
            generators.push(Generator::spawn({
                let (platform, slot, timeline, playlist) = (
                    platform.clone(),
                    slot.clone(),
                    timeline.clone(),
                    playlist.clone(),
                );
                move |stop| {
                    // Decode the following frame promptly too (#126).
                    platform.real_time_thread();
                    // Reported like a capture that stopped, so the engine
                    // says so and opens it again once the file is back.
                    let failed = |err: anyhow::Error| slot.stop(format!("{err:#}"));
                    playlist.run(
                        &platform,
                        &timeline,
                        MediaPart::Video,
                        stop,
                        &failed,
                        |reader, base, on| {
                            let ahead = slot.ahead.as_ref().expect("media slot");
                            let mut ended = false;
                            loop {
                                if stop.load(Ordering::SeqCst) || !on() {
                                    ahead.lock().clear();
                                    return Ok(false);
                                }
                                // Keep two pictures decoded before pacing
                                // delivery. A transient decoder stall must
                                // not hide the next presentation's picture.
                                if !ended && ahead.lock().len() < 2 {
                                    let Some((frame, at)) = reader.next_video()? else {
                                        // On a looping playlist, open the
                                        // next file while this one's last
                                        // picture is still queued. Reorder
                                        // priming must not start at its first
                                        // presentation deadline.
                                        if playlist.repeat {
                                            return Ok(true);
                                        }
                                        ended = true;
                                        continue;
                                    };
                                    if !on() {
                                        ahead.lock().clear();
                                        return Ok(false);
                                    }
                                    let pts = base + at.div_f64(speed);
                                    ahead.lock().push_back((frame, pts));
                                    continue;
                                }
                                let Some(pts) = ahead.lock().front().map(|(_, at)| *at) else {
                                    if ended {
                                        return Ok(true);
                                    }
                                    continue;
                                };
                                loop {
                                    if stop.load(Ordering::SeqCst) || !on() {
                                        ahead.lock().clear();
                                        return Ok(false);
                                    }
                                    let now = platform.now();
                                    if now >= pts {
                                        break;
                                    }
                                    // A restart cancels its pending frame
                                    // even in a low-frame-rate file.
                                    let until = if playlist.restart {
                                        pts.min(now + Duration::from_millis(5))
                                    } else {
                                        pts
                                    };
                                    if !sleep_until(&platform, &mut clock, until, stop) {
                                        ahead.lock().clear();
                                        return Ok(false);
                                    }
                                }
                                let mut pending = ahead.lock();
                                let Some((frame, pts)) = pending.pop_front() else {
                                    // A second canvas can clear the shared
                                    // slot without ending this activation.
                                    // Discard only this picture; on() above
                                    // decides whether the play has ended.
                                    continue;
                                };
                                slot.push(frame, pts);
                            }
                        },
                    );
                }
            }));
            Some(Video {
                slot,
                width,
                height,
                fps: setup.canvas.fps,
            })
        }
    };

    let audio = if with_audio && infos.iter().any(|info| info.audio) {
        let mut clock = timeline
            .clocks
            .start(format!("source:{id}:audio"), Duration::from_millis(10));
        let feed = Arc::new(AudioFeed::default());
        // Files are ready at once.
        feed.started.store(true, Ordering::SeqCst);
        let (tx, chunks) = sync_channel::<(Arc<[f32]>, Time)>(AUDIO_QUEUE);
        generators.push(Generator::spawn({
            let (platform, timeline, playlist, feed) = (
                platform.clone(),
                timeline.clone(),
                playlist.clone(),
                feed.clone(),
            );
            move |stop| {
                let failed = |err: anyhow::Error| {
                    feed.stopped.lock().get_or_insert(format!("{err:#}"));
                };
                playlist.run(
                    &platform,
                    &timeline,
                    MediaPart::Audio,
                    stop,
                    &failed,
                    |reader, base, on| {
                        while let Some((samples, at)) = reader.next_audio()? {
                            if !on() {
                                return Ok(false);
                            }
                            let samples = resample(&samples, 2, speed);
                            let pts = base + at.div_f64(speed);
                            let frames = (samples.len() / 2) as u64;
                            // Deliver each chunk once all of it has "happened".
                            if !sleep_until(
                                &platform,
                                &mut clock,
                                pts.add_ticks(frames, SAMPLE_RATE),
                                stop,
                            ) {
                                return Ok(false);
                            }
                            let _ = tx.try_send((samples.into(), pts));
                        }
                        Ok(true)
                    },
                );
            }
        }));
        Some(Audio {
            feed,
            chunks,
            channels: 2,
        })
    } else {
        None
    };

    Ok(Opened {
        video,
        audio,
        _running: Running::Generators {
            _generators: generators,
        },
    })
}

/// Open a stinger's video `file`, as source `id`: a media source that
/// plays from its start each time `id` comes on air (when a stinger
/// begins), video only. Returns it and how long the video lasts.
pub(crate) fn stinger<P: Platform>(
    platform: &P,
    setup: &Setup,
    id: &str,
    file: &str,
    timeline: &Arc<Timeline>,
) -> Result<(Opened<P>, Duration)> {
    let permission = platform.source_permission(&SourceKind::Media(MediaSource {
        files: vec![file.to_string()],
        repeat: false,
        speed: 1.0,
        restart_on_activate: true,
    }));
    let (platform, setup, id, file, timeline) = (
        platform.clone(),
        setup.clone(),
        id.to_string(),
        file.to_string(),
        timeline.clone(),
    );
    let help = OpenHelp {
        what: format!("the stinger video {file:?}"),
        permission,
        device: false,
    };
    bounded_open(
        format!("stinger {id:?} {file:?}"),
        help,
        OPEN_TIMEOUT,
        move || stinger_native(&platform, &setup, &id, &file, &timeline),
    )
}

fn stinger_native<P: Platform>(
    platform: &P,
    setup: &Setup,
    id: &str,
    file: &str,
    timeline: &Arc<Timeline>,
) -> Result<(Opened<P>, Duration)> {
    let info = platform.media_info(&expand_home(file))?;
    if info.video.is_none() {
        bail!("{file} has no video");
    }
    let settings = MediaSource {
        files: vec![file.to_string()],
        repeat: false,
        speed: 1.0,
        restart_on_activate: true,
    };
    let opened = media(platform, id, &settings, setup, timeline, false)?;
    Ok((opened, info.duration))
}

/// A media source's files and how they play.
#[derive(Clone)]
struct Playlist {
    /// The source's id, to know when it is on air.
    id: String,
    files: Vec<(std::path::PathBuf, MediaInfo)>,
    speed: f64,
    repeat: bool,
    /// Play from the start each time it comes on air, and only then.
    restart: bool,
}

impl Playlist {
    /// Play `part` on the timeline: from its start, or (restarting on
    /// activation) from each moment it comes on air until it goes off air.
    /// Restarting, the first file waits opened with its first sample read,
    /// so a play starts at once. A file that can't be read (it was deleted,
    /// say) ends that play and is passed to `failed`.
    fn run<P: Platform>(
        &self,
        platform: &P,
        timeline: &Timeline,
        part: MediaPart,
        stop: &AtomicBool,
        failed: &dyn Fn(anyhow::Error),
        mut read: impl FnMut(&mut Cued<P::Media>, Time, &dyn Fn() -> bool) -> Result<bool>,
    ) {
        let Some(start) = timeline.wait(stop) else {
            return;
        };
        if !self.restart {
            let played = self.play(platform, start, part, stop, None, |reader, base| {
                read(reader, base, &|| true)
            });
            if let Err(err) = played {
                failed(err);
            }
            return;
        }
        let mut played = None;
        let mut cued = None;
        while !stop.load(Ordering::SeqCst) {
            if cued.is_none() {
                cued = self.cue(platform, part);
            }
            // Both parts see the same moment, so they restart in step.
            let since = timeline.on_air_since(&self.id);
            if since.is_none() || since == played {
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            played = since;
            let since = since.expect("checked");
            let on = || timeline.on_air_since(&self.id) == Some(since);
            let played = self.play(platform, since, part, stop, cued.take(), |reader, base| {
                read(reader, base, &on)
            });
            // The next time it comes on air it tries again.
            if let Err(err) = played {
                failed(err);
            }
        }
    }

    /// The first file's `part` opened and its first sample read, if it
    /// has that part.
    fn cue<P: Platform>(&self, platform: &P, part: MediaPart) -> Option<Cued<P::Media>> {
        let (file, info) = self.files.first()?;
        let has = match part {
            MediaPart::Video => info.video.is_some(),
            MediaPart::Audio => info.audio,
        };
        if !has {
            return None;
        }
        let mut cued = Cued {
            reader: platform.open_media(file, part).ok()?,
            video: None,
            audio: None,
        };
        match part {
            MediaPart::Video => cued.video = cued.reader.next_video().ok()?,
            MediaPart::Audio => cued.audio = cued.reader.next_audio().ok()?,
        }
        Some(cued)
    }

    /// Open each file's `part` in turn on the timeline from `start` (the
    /// first file `cued` if given), and let `read` play it from its start
    /// time; `read` returns false to stop. Files without that part only
    /// take up their time. An error if a file can't be opened or read.
    fn play<P: Platform>(
        &self,
        platform: &P,
        start: Time,
        part: MediaPart,
        stop: &AtomicBool,
        mut cued: Option<Cued<P::Media>>,
        mut read: impl FnMut(&mut Cued<P::Media>, Time) -> Result<bool>,
    ) -> Result<()> {
        let mut base = start;
        loop {
            for (file, info) in &self.files {
                let has = match part {
                    MediaPart::Video => info.video.is_some(),
                    MediaPart::Audio => info.audio,
                };
                if has {
                    let mut reader = match cued.take() {
                        Some(cued) => cued,
                        None => Cued {
                            reader: platform.open_media(file, part)?,
                            video: None,
                            audio: None,
                        },
                    };
                    if !read(&mut reader, base)? {
                        return Ok(());
                    }
                }
                base = base + info.duration.div_f64(self.speed);
                if stop.load(Ordering::SeqCst) {
                    return Ok(());
                }
            }
            if !self.repeat {
                return Ok(());
            }
        }
    }
}

/// A media reader whose first sample may have been read ahead.
struct Cued<M: MediaReader> {
    reader: M,
    video: Option<(M::Frame, Duration)>,
    audio: Option<(Vec<f32>, Duration)>,
}

impl<M: MediaReader> Cued<M> {
    fn next_video(&mut self) -> Result<Option<(M::Frame, Duration)>> {
        match self.video.take() {
            Some(frame) => Ok(Some(frame)),
            None => self.reader.next_video(),
        }
    }

    fn next_audio(&mut self) -> Result<Option<(Vec<f32>, Duration)>> {
        match self.audio.take() {
            Some(samples) => Ok(Some(samples)),
            None => self.reader.next_audio(),
        }
    }
}

/// Interleaved `samples` played `speed` times as fast (and higher), by
/// linear interpolation.
fn resample(samples: &[f32], channels: usize, speed: f64) -> Vec<f32> {
    if speed == 1.0 {
        return samples.to_vec();
    }
    let frames = samples.len() / channels;
    let out_frames = (frames as f64 / speed).floor() as usize;
    let mut out = Vec::with_capacity(out_frames * channels);
    for i in 0..out_frames {
        let at = i as f64 * speed;
        let (index, t) = (at as usize, at.fract() as f32);
        let next = (index + 1).min(frames - 1);
        for c in 0..channels {
            let a = samples[index * channels + c];
            let b = samples[next * channels + c];
            out.push(a + (b - a) * t);
        }
    }
    out
}

/// The first frame of an image file.
fn first_picture(path: &std::path::Path) -> Result<Picture> {
    let mut frames = picture::load(path, None)?;
    Ok(frames.remove(0).0)
}

/// Sleep until `at` on the host clock, in steps so `stop` is noticed.
/// Returns false if stopped.
fn sleep_until<P: Platform>(
    platform: &P,
    clock: &mut crate::pacing::MediaClock,
    at: Time,
    stop: &AtomicBool,
) -> bool {
    loop {
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        let now = platform.now();
        if now >= at {
            return true;
        }
        clock.sleep_until(platform, at.min(now + Duration::from_millis(20)));
    }
}
