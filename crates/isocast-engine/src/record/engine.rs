//! The live engine: a setup's sources, the program and the mix running all
//! the time, as in OBS, with recordings and other outputs (a stream, the
//! app's preview) attaching and detaching while it runs.
//!
//! Sources open in the background as the setup changes; layout, scene and
//! filter changes show at the next program frame. A recording keeps the
//! sources it records (and the canvas) as they were when it started, and
//! changes to them apply once it ends.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::Local;
use isocast_protocol::api::{
    AudioLevel, ChannelLevel, EngineInfo, ScenesInfo, SourceRun, SourceStatus,
};
use isocast_protocol::manifest::SessionStatus;
use isocast_protocol::{Canvas, Codec, Monitoring, PROGRAM, Setup, SourceKind, TransitionKind};

use super::disk::{DiskMeter, DiskProbe};
use super::file::{Done, Split, file_end, write_files};
use super::frame_stream::{self, FrameFeed};
use super::iso_frames::IsoFrames;
use super::mix::{MIX_DELAY, Meter, Mixer, StereoMeter};
use super::monitor::{Closing, Monitor, MonitorSlot, Tap};
use super::program::{Compositor, Feed};
use super::scenes::{SceneInput, SceneThread};
use super::shared_frames::{self, SharedFeed};
use super::timings::CompositorTimings;
use super::{
    AUDIO_TAIL, COMMIT_TIMEOUT, CaptureReport, DiskReport, FIRST_FRAME_TIMEOUT, FilePlan,
    FolderLock, Formats, MIN_FREE_BYTES, MIN_FREE_SECONDS, MIN_TAKE, MIX_QUEUE_DEPTH, MONITOR_FPS,
    OnAir, Output, Packet, QUEUE_DEPTH, RecordOptions, Report, SPACE_CHECK_INTERVAL, SPEED_MARGIN,
    SPLIT_LEAD, SPLIT_SOON_LEAD, START_DELAY, STATUS_INTERVAL, SessionControl, Shared, Slots,
    SourceFilters, SplitRefused, SplitTake, SplitTiming, Take, create_writers, expand_home,
    interrupted_note, interrupted_sessions, live_status, plan_take, sidechain_sources,
    take_manifest, wait_for_frame, write_manifest, write_manifest_durable,
};
use crate::audio_filter::AudioStage;
use crate::gpu::Gpu;
use crate::source::{self, Audio, AudioFeed, FrameSlot, Timeline, Video};
use crate::transition::stinger_id;
use crate::{FileWriter, Platform, SAMPLE_RATE, Time, space};

/// How often audio levels are measured and reported, and failed sources
/// noticed.
const HOUSEKEEPING_INTERVAL: Duration = Duration::from_millis(50);
/// How long the studio-mode preview (and the program, when nothing records
/// or streams it) keeps being drawn after a client last asked for it.
const PREVIEW_HOLD: Duration = Duration::from_secs(3);
/// How often a wait for a monitor's frame looks its target up again.
const SLOT_RECHECK: Duration = Duration::from_millis(250);
/// How often sources that failed (a device unplugged, a permission not
/// yet granted) are tried again.
const RETRY_INTERVAL: Duration = Duration::from_secs(5);
/// How long a recording waits for sources it needs that are still opening.
const SOURCE_WAIT: Duration = Duration::from_secs(15);
/// How long a disk's measured write speed is trusted.
const SPEED_TTL: Duration = Duration::from_secs(600);
/// Audio this much before an output's start is still sent, so a file can
/// trim its first chunk to the start.
const AUDIO_LEAD: Duration = Duration::from_millis(100);

/// Something that happened in the engine, for clients.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    /// Every audio source's and the program mix's level, about 20 times a
    /// second.
    Levels(Vec<AudioLevel>),
    /// A source started opening, is running, or failed.
    Source(SourceStatus),
    /// Something needs attention.
    Warning(String),
    WorkerFailed {
        name: String,
        message: String,
        poisoned: bool,
    },
}

/// Called with what happens in the engine, from its threads.
pub type OnEngineEvent = Arc<dyn Fn(EngineEvent) + Send + Sync>;

/// Where packets from one producer (a source's audio, the program's frames,
/// the mix) go: outputs that come and go while it runs.
pub(crate) struct Routes<F> {
    list: Mutex<Vec<Route<F>>>,
    next: AtomicU64,
}

struct Route<F> {
    key: u64,
    output: Output<F>,
    track: usize,
    /// Packets from before this are left out (audio from a little before,
    /// so the first chunk can be trimmed to the start).
    from: Time,
    /// Audio filters this output's copy goes through.
    filter: Option<AudioStage>,
    /// Counts packets the output missed because it had fallen behind, for
    /// outputs that report them.
    missed: Option<Arc<AtomicU64>>,
    /// A program-mix recording may receive its final audio early at Stop;
    /// live taps must keep their ordinary delivery schedule.
    recording_audio: bool,
}

impl<F> Default for Routes<F> {
    fn default() -> Self {
        Self {
            list: Mutex::new(Vec::new()),
            next: AtomicU64::new(0),
        }
    }
}

impl<F: Clone> Routes<F> {
    /// Send packets from `from` on to `output` (as `track`, for audio),
    /// through `filter` if given. Returns a key to remove it with.
    pub(crate) fn add(
        &self,
        output: Output<F>,
        track: usize,
        from: Time,
        filter: Option<AudioStage>,
    ) -> u64 {
        self.add_counted(output, track, from, filter, None, false)
    }

    fn add_recording_audio(&self, output: Output<F>, track: usize, from: Time) -> u64 {
        self.add_counted(output, track, from, None, None, true)
    }

    /// As [`Routes::add`], counting in `missed` the packets the output
    /// misses because its queue is full.
    fn add_counted(
        &self,
        output: Output<F>,
        track: usize,
        from: Time,
        filter: Option<AudioStage>,
        missed: Option<Arc<AtomicU64>>,
        recording_audio: bool,
    ) -> u64 {
        let key = self.next.fetch_add(1, Ordering::Relaxed);
        self.list.lock().unwrap().push(Route {
            key,
            output,
            track,
            from,
            filter,
            missed,
            recording_audio,
        });
        key
    }

    pub(crate) fn remove(&self, key: u64) {
        self.list
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|route| route.key != key);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.list.lock().unwrap().is_empty()
    }

    /// Drop every output, so their receivers see the producer is gone.
    fn clear(&self) {
        self.list.lock().unwrap().clear();
    }

    /// Send a frame to every output. One that has fallen behind misses it
    /// rather than holding up the others.
    pub(crate) fn video(&self, frame: &F, pts: Time) {
        for route in self.list.lock().unwrap().iter() {
            if pts >= route.from {
                let sent = route.output.try_send(Packet::Video {
                    frame: frame.clone(),
                    pts,
                });
                if let (Err(TrySendError::Full(_)), Some(missed)) = (sent, &route.missed) {
                    missed.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// Send a chunk of audio to every output, filtered for those that
    /// filter it (placed back by the filters' delay, so it stays in sync).
    pub(crate) fn audio(&self, samples: &Arc<[f32]>, pts: Time) {
        self.audio_to(samples, pts, None);
    }

    /// Deliver a stopping recording's tail; report whether live taps need
    /// the same block later, at its original jitter-buffer deadline.
    pub(super) fn audio_recordings(&self, samples: &Arc<[f32]>, pts: Time) -> bool {
        self.audio_to(samples, pts, Some(true))
    }

    pub(super) fn audio_live(&self, samples: &Arc<[f32]>, pts: Time) {
        self.audio_to(samples, pts, Some(false));
    }

    fn audio_to(&self, samples: &Arc<[f32]>, pts: Time, recording: Option<bool>) -> bool {
        let mut live = false;
        for route in self.list.lock().unwrap().iter_mut() {
            live |= !route.recording_audio;
            if recording.is_some_and(|recording| recording != route.recording_audio) {
                continue;
            }
            if pts + AUDIO_LEAD < route.from {
                continue;
            }
            let packet = match &mut route.filter {
                None => Packet::Audio {
                    track: route.track,
                    samples: samples.clone(),
                    pts,
                },
                Some(stage) => {
                    let (filtered, latency) = stage.process(samples);
                    let delay = latency as i64 * 1_000_000_000 / i64::from(SAMPLE_RATE);
                    Packet::Audio {
                        track: route.track,
                        samples: filtered.into(),
                        pts: Time::from_nanos(pts.as_nanos() - delay),
                    }
                }
            };
            if let (Err(TrySendError::Full(_)), Some(missed)) =
                (route.output.try_send(packet), &route.missed)
            {
                missed.fetch_add(1, Ordering::Relaxed);
            }
        }
        live
    }
}

/// A live frame as a JPEG.
pub struct JpegFrame {
    /// Orders frames: the frame's time on the host clock, in nanoseconds.
    pub seq: u64,
    pub width: u32,
    pub height: u32,
    pub jpeg: Vec<u8>,
    /// For the program and the preview: the setup revision it was drawn
    /// from.
    pub revision: Option<u64>,
    /// And the layout generation it was drawn at.
    pub layout_generation: Option<u64>,
}

/// The program's video and audio for an output like a stream: a frame
/// per canvas tick and the program mix (stereo, track 0), from the next
/// frame on, until dropped.
pub struct ProgramTap<F: Clone> {
    pub packets: Receiver<Packet<F>>,
    /// Program frames that never reached `packets` because it was full
    /// (whoever reads it fell behind).
    pub missed_frames: Arc<AtomicU64>,
    /// Mix chunks lost to a full tap, counted even while its encoder stalls.
    pub missed_audio_chunks: Arc<AtomicU64>,
    video: Arc<Routes<F>>,
    audio: Arc<Routes<F>>,
    keys: (u64, u64),
}

impl<F: Clone> Drop for ProgramTap<F> {
    fn drop(&mut self) {
        self.video.remove(self.keys.0);
        self.audio.remove(self.keys.1);
    }
}

/// A way to re-tap the program without keeping a route (and the compositor)
/// active. This keeps the inner state, not the Engine: dropping the Engine
/// still stops it, which a parked encoder can observe without a receiver.
#[derive(Clone)]
pub(crate) struct ProgramTapSource<P: Platform>(Arc<Inner<P>>);

impl<P: Platform> ProgramTapSource<P> {
    pub(crate) fn stopped(&self) -> bool {
        self.0.stopping.load(Ordering::SeqCst)
    }

    pub(crate) fn tap(
        &self,
        depth: usize,
        missed_audio_chunks: Arc<AtomicU64>,
    ) -> ProgramTap<P::Frame> {
        let inner = &self.0;
        // Serialize acquiring the canvas with update's refusal check.
        let _state = inner.state.lock().unwrap();
        let (tx, packets) = sync_channel(depth);
        let from = inner.platform.now();
        let missed_frames = Arc::new(AtomicU64::new(0));
        let keys = (
            inner.program_video.add_counted(
                tx.clone(),
                0,
                from,
                None,
                Some(missed_frames.clone()),
                false,
            ),
            inner.program_audio.add_counted(
                tx,
                0,
                from,
                None,
                Some(missed_audio_chunks.clone()),
                false,
            ),
        );
        ProgramTap {
            packets,
            missed_frames,
            missed_audio_chunks,
            video: inner.program_video.clone(),
            audio: inner.program_audio.clone(),
            keys,
        }
    }
}

/// A running engine. Dropping it stops everything.
pub struct Engine<P: Platform> {
    inner: Arc<Inner<P>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

struct Inner<P: Platform> {
    emergency: AtomicBool,
    worker_errors: Mutex<Vec<String>>,
    platform: P,
    control: Arc<SessionControl>,
    timeline: Arc<Timeline>,
    /// The engine clock's zero: program frames are on its canvas grid and
    /// the mix counts samples from it. `i64::MIN` until it starts.
    epoch: AtomicI64,
    on_event: Option<OnEngineEvent>,
    stopping: AtomicBool,
    state: Mutex<State<P>>,
    /// The newest setup the reconciler hasn't opened sources for yet.
    wanted: Mutex<Option<Arc<Setup>>>,
    /// Native opens cannot be cancelled while macOS presents a prompt.
    /// Keep one in flight per source (including removed/changed sources).
    opening: Mutex<HashSet<String>>,
    wake: Condvar,
    /// The program's newest canvases.
    program: Arc<FrameSlot<P::Frame>>,
    /// Studio mode's preview scene, drawn while someone wants it.
    preview: Arc<FrameSlot<P::Frame>>,
    preview_scene: Mutex<Option<Arc<str>>>,
    /// A camera-only source is needed only while that output is running.
    camera_source: Mutex<Option<String>>,
    /// The highest rate the preview was asked for since the program last
    /// looked (0: nobody).
    preview_wanted: AtomicU32,
    /// Resolves the setup's scenes into what the program draws whenever
    /// they change.
    scenes: SceneThread<P::Frame>,
    program_video: Arc<Routes<P::Frame>>,
    program_audio: Arc<Routes<P::Frame>>,
    program_meter: Arc<StereoMeter>,
    /// Monitors' H.264 streams, by what they show.
    frame_streams: Arc<frame_stream::Streams>,
    /// The setup revision each recent program (and preview) frame was
    /// drawn from, by tick, so monitors can tell clients which layout a
    /// frame shows.
    drawn: Arc<frame_stream::Drawn>,
    monitor: Arc<MonitorSlot>,
    /// The newest levels measured.
    levels: Mutex<Vec<AudioLevel>>,
    /// The loudest levels since a recording's status last read them.
    loudest: Mutex<BTreeMap<String, (f64, f64)>>,
    recording: AtomicBool,
    frames_drawn: AtomicU64,
    frames_missed: AtomicU64,
    /// Program frames drawn per second over the last second (`f64` bits).
    program_fps: AtomicU64,
    /// What the compositor's recent ticks cost.
    timings: Mutex<CompositorTimings>,
    /// Measured write speeds by folder, and when.
    speeds: Mutex<HashMap<PathBuf, (Time, f64)>>,
    /// The program's shared H.264/AAC encoders (the stream's and the
    /// replay buffer's).
    encoders: crate::stream::encoder::Encoders,
}

struct State<P: Platform> {
    setup: Arc<Setup>,
    sources: BTreeMap<String, Live<P>>,
    /// Stinger videos by file, and how long each plays.
    stingers: HashMap<String, (source::Opened<P>, Duration)>,
    mixer: Option<MixerRun<P::Frame>>,
    compositor: Option<CompositorRun>,
    /// Sources a recording uses, which keep running as they are until it
    /// ends.
    pins: Vec<String>,
    /// The revision of `setup` (the daemon's count of applied changes).
    revision: u64,
}

/// One of the engine's sources.
struct Live<P: Platform> {
    /// What it was opened as, to tell when the setup changes it.
    definition: serde_json::Value,
    run: SourceRun,
    error: Option<String>,
    video: Option<Video<P::Frame>>,
    audio: Option<AudioRun<P::Frame>>,
    /// Keeps it running; dropped last.
    opened: Option<source::Opened<P>>,
}

/// An audio source's reader: meters it, plays it to the monitor, and
/// sends it to the outputs routed to it.
struct AudioRun<F> {
    routes: Arc<Routes<F>>,
    meter: Arc<Meter>,
    channels: u32,
    feed: Arc<AudioFeed>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl<F> Drop for AudioRun<F> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct MixerRun<F> {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    /// Its inputs: source, channels, and whether it is mixed (or only a
    /// sidechain); an input's track is its index.
    inputs: Vec<(String, u32, bool)>,
    tx: SyncSender<Packet<F>>,
    /// The audio reader each input is routed from, and the route's key.
    routed: HashMap<String, (usize, u64)>,
}

struct CompositorRun {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    canvas: (u32, u32, u32),
}

impl<P: Platform> Inner<P> {
    fn worker<T>(&self, name: &str, run: impl FnOnce() -> T) -> Result<T, String> {
        let result = isocast_diagnostics::contain(name, run);
        if let Err(message) = &result {
            self.worker_errors
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(message.clone());
            let poisoned = self.state.is_poisoned()
                || self.wanted.is_poisoned()
                || self.loudest.is_poisoned()
                || self.program_video.list.is_poisoned()
                || self.program_audio.list.is_poisoned();
            if poisoned {
                self.emergency.store(true, Ordering::SeqCst);
            }
            if name == "compositor" {
                self.program.stop(message.clone());
                self.program_fps.store(0.0f64.to_bits(), Ordering::Relaxed);
            }
            self.event(EngineEvent::WorkerFailed {
                name: name.into(),
                message: message.clone(),
                poisoned,
            });
        }
        result
    }
    /// Someone shows `target` (the program or studio mode's preview; others
    /// are captured anyway) at `fps`: it is drawn at least that often, on
    /// the canvas's frame grid, until [`PREVIEW_HOLD`] after the last ask.
    fn want(&self, target: &str, fps: u32) {
        match target {
            PROGRAM => self.control.want_program(fps),
            "preview" => {
                self.preview_wanted.fetch_max(fps.max(1), Ordering::SeqCst);
            }
            _ => {}
        }
    }

    fn epoch(&self) -> Option<Time> {
        match self.epoch.load(Ordering::SeqCst) {
            i64::MIN => None,
            nanos => Some(Time::from_nanos(nanos)),
        }
    }

    fn event(&self, event: EngineEvent) {
        if let Some(on_event) = &self.on_event {
            on_event(event);
        }
    }
}

impl<P: Platform> Engine<P> {
    /// Start `setup`'s sources, program and mix. Returns once the program
    /// runs; sources that can't open are reported (see [`Engine::info`])
    /// and retried when the setup next changes.
    pub fn start(
        platform: &P,
        setup: &Setup,
        control: Option<Arc<SessionControl>>,
        on_event: Option<OnEngineEvent>,
    ) -> Result<Self> {
        let issues = setup.validate();
        if !issues.is_empty() {
            let lines: Vec<String> = issues
                .iter()
                .map(|i| format!("{}: {}", i.path, i.message))
                .collect();
            bail!("the setup is not valid:\n  {}", lines.join("\n  "));
        }
        let control = control.unwrap_or_default();
        let timeline = Arc::new(Timeline::default());
        // The program always runs, so it decides what is on air.
        timeline.track_on_air();
        let setup = Arc::new(setup.clone());
        // Plugins' audio filters warn (a failure, a missing plugin) as the
        // engine does.
        if let Some(on_event) = &on_event {
            let on_event = on_event.clone();
            control
                .audio_filters
                .reports
                .set_warn(Arc::new(move |message| {
                    on_event(EngineEvent::Warning(message))
                }));
        }
        control.set_audio(&setup);
        control.set_filters(&setup);
        let monitor = Arc::new(Monitor::new(&setup.audio.monitoring_device));
        let scenes = SceneThread::start(
            {
                let platform = platform.clone();
                Arc::new(move |frame: &P::Frame| platform.frame_size(frame))
            },
            control.data().clone(),
            {
                let on_event = on_event.clone();
                Arc::new(move |message: String| {
                    if let Some(on_event) = &on_event {
                        on_event(EngineEvent::Warning(message));
                    }
                })
            },
            {
                let on_event = on_event.clone();
                Arc::new(move |message, poisoned| {
                    if let Some(on_event) = &on_event {
                        on_event(EngineEvent::WorkerFailed {
                            name: "scenes".into(),
                            message,
                            poisoned,
                        });
                    }
                })
            },
        );
        let inner = Arc::new(Inner {
            emergency: AtomicBool::new(false),
            worker_errors: Mutex::new(Vec::new()),
            platform: platform.clone(),
            control,
            // Only ever read newest first: no older canvases are kept.
            program: Arc::new(FrameSlot::with_capacity(timeline.clone(), 1)),
            preview: Arc::new(FrameSlot::with_capacity(timeline.clone(), 1)),
            timeline,
            epoch: AtomicI64::new(i64::MIN),
            on_event,
            stopping: AtomicBool::new(false),
            state: Mutex::new(State {
                setup: setup.clone(),
                sources: BTreeMap::new(),
                stingers: HashMap::new(),
                mixer: None,
                compositor: None,
                pins: Vec::new(),
                revision: 0,
            }),
            wanted: Mutex::new(None),
            opening: Mutex::new(HashSet::new()),
            wake: Condvar::new(),
            preview_scene: Mutex::new(None),
            camera_source: Mutex::new(None),
            preview_wanted: AtomicU32::new(0),
            scenes,
            program_video: Arc::default(),
            program_audio: Arc::default(),
            program_meter: Arc::default(),
            frame_streams: Arc::default(),
            drawn: Arc::default(),
            monitor: Arc::new(Mutex::new(monitor)),
            levels: Mutex::new(Vec::new()),
            loudest: Mutex::new(BTreeMap::new()),
            recording: AtomicBool::new(false),
            frames_drawn: AtomicU64::new(0),
            frames_missed: AtomicU64::new(0),
            program_fps: AtomicU64::new(0),
            timings: Mutex::default(),
            speeds: Mutex::new(HashMap::new()),
            encoders: Default::default(),
        });
        reconcile(&inner, setup);

        // Wait for every source's first frame (cameras take a moment to
        // warm up), so the program starts with all of them.
        let waiting_since = platform.now();
        loop {
            let ready = {
                let state = inner.state.lock().unwrap();
                state.sources.values().all(|live| {
                    live.video.as_ref().is_none_or(|v| v.slot.has_frame())
                        && live
                            .audio
                            .as_ref()
                            .is_none_or(|a| a.feed.started.load(Ordering::SeqCst))
                })
            };
            if ready || platform.now().saturating_since(waiting_since) >= FIRST_FRAME_TIMEOUT {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let epoch = platform.now() + START_DELAY;
        inner.timeline.set_start(epoch);
        inner.epoch.store(epoch.as_nanos(), Ordering::SeqCst);
        {
            let mut state = inner.state.lock().unwrap();
            publish(&inner, &mut state);
        }
        // Measure the recording disk now, so the first recording starts at
        // once.
        let folder = expand_home(&inner.state.lock().unwrap().setup.recording.folder);
        if folder.is_dir() {
            let inner = inner.clone();
            let _ = isocast_diagnostics::spawn(
                "initial disk probe",
                move || {
                    let _ = write_speed(&inner, &folder);
                },
                |_| {},
            );
        }
        let threads = vec![
            thread::spawn({
                let inner = inner.clone();
                move || {
                    let _ = inner.worker("source reconciler", || reconciler(&inner));
                }
            }),
            thread::spawn({
                let inner = inner.clone();
                move || {
                    let _ = inner.worker("source housekeeping", || housekeeping(&inner));
                }
            }),
        ];
        Ok(Self {
            inner,
            threads: Mutex::new(threads),
        })
    }

    /// Run `setup` (the daemon's `revision` of it) from now on: the program
    /// shows its scenes from the next frame, and that frame is stamped with
    /// `revision`; new and changed sources open in the background. A
    /// recording or stream keeps the canvas it started with, so changing
    /// the canvas size or frame rate then is refused.
    pub fn update(&self, setup: &Setup, revision: u64) -> Result<(), String> {
        let inner = &self.inner;
        let setup = Arc::new(setup.clone());
        {
            let mut state = inner.state.lock().unwrap();
            if let Some(message) = canvas_refusal(inner, &state.setup.canvas, &setup.canvas) {
                return Err(format!("canvas: {message}"));
            }
            // Stinger videos it adds load in the background; a switch to
            // one waits for it.
            let new: Vec<String> = stinger_files(&setup)
                .into_iter()
                .filter(|file| !state.stingers.contains_key(file))
                .collect();
            inner.control.loading_stingers(&new);
            state.setup = setup.clone();
            state.revision = revision;
            publish(inner, &mut state);
        }
        *inner.wanted.lock().unwrap() = Some(setup);
        inner.wake.notify_all();
        Ok(())
    }

    /// Why the canvas can't change to `canvas` now (a recording of the
    /// program or a stream is using the current one), if it can't.
    pub fn canvas_refusal(&self, canvas: &Canvas) -> Option<String> {
        let state = self.inner.state.lock().unwrap();
        canvas_refusal(&self.inner, &state.setup.canvas, canvas)
    }

    /// Stop everything: the recording, if one runs, should be stopped
    /// first.
    pub fn stop(&self) {
        let inner = &self.inner;
        inner.stopping.store(true, Ordering::SeqCst);
        inner.wake.notify_all();
        for thread in self.threads.lock().unwrap().drain(..) {
            let _ = thread.join();
        }
        let (compositor, mixer, sources, stingers) = {
            let mut state = inner.state.lock().unwrap();
            (
                state.compositor.take(),
                state.mixer.take(),
                std::mem::take(&mut state.sources),
                std::mem::take(&mut state.stingers),
            )
        };
        if let Some(compositor) = compositor {
            stop_compositor(compositor);
        }
        inner.scenes.stop();
        if let Some(mixer) = mixer {
            stop_mixer(inner, mixer, &BTreeMap::new());
        }
        drop(sources);
        drop(stingers);
        drop(Closing(inner.monitor.lock().unwrap().clone()));
        // A tap holds these routes itself, so they outlive the engine:
        // dropping the outputs is what tells a stream the program ended.
        inner.program_video.clear();
        inner.program_audio.clear();
    }

    /// The control the engine follows: scene switches, markers, live audio
    /// and filter settings.
    pub fn control(&self) -> &Arc<SessionControl> {
        &self.inner.control
    }

    /// Start/resume must not announce working program output after its worker
    /// has failed. Stop remains available to finalize every recording.
    pub fn program_failure(&self) -> Option<String> {
        self.inner.program.stopped().map(|reason| format!(
            "program output has failed: {reason}; stop the recording and restart the live engine"
        ))
    }

    /// The platform it runs on.
    pub fn platform(&self) -> &P {
        &self.inner.platform
    }

    /// The setup the engine runs.
    pub fn setup(&self) -> Arc<Setup> {
        self.inner.state.lock().unwrap().setup.clone()
    }

    /// What the engine is doing: its sources, canvas and counters.
    pub fn info(&self) -> EngineInfo {
        let inner = &self.inner;
        let state = inner.state.lock().unwrap();
        let clocks = inner.timeline.clocks.info();
        let mut compositor = inner.timings.lock().unwrap().info();
        if let Some(clock) = clocks.iter().find(|clock| clock.id == "program") {
            compositor.wake_lateness_us = clock.wake_lateness_us;
        }
        let mut held_by = Vec::new();
        if inner.recording.load(Ordering::SeqCst) {
            held_by.push("recording".to_string());
        }
        EngineInfo {
            clocks,
            worker_errors: self
                .inner
                .worker_errors
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .iter()
                .cloned()
                .chain(self.inner.scenes.failure())
                .collect(),
            uptime_seconds: inner.epoch().map_or(0.0, |epoch| {
                inner.platform.now().saturating_since(epoch).as_secs_f64()
            }),
            canvas: state.setup.canvas.clone(),
            sources: statuses(&state),
            program_frames: inner.frames_drawn.load(Ordering::Relaxed),
            missed_frames: inner.frames_missed.load(Ordering::Relaxed),
            fps: f64::from_bits(inner.program_fps.load(Ordering::Relaxed)),
            outputs: held_by,
            compositor,
            scenes: {
                let (raster_us, content, content_mb) = inner.scenes.content();
                ScenesInfo {
                    resolve_us: inner.scenes.resolve_us(),
                    raster_us,
                    content,
                    content_mb,
                }
            },
            audio_filters: inner.control.audio_filters.reports.info(),
        }
    }

    /// Where every item of the live setup's scenes is drawn, as laid out
    /// with the sources' picture sizes the engine has, and the layout's
    /// generation (program and preview frames carry the generation they
    /// were drawn at). `None` before the scenes are first resolved.
    pub fn resolved(&self) -> Option<isocast_protocol::api::Resolved> {
        let scenes = self.inner.scenes.latest()?;
        Some(isocast_protocol::api::Resolved {
            generation: scenes.generation,
            revision: scenes.revision,
            scenes: crate::compiled::resolved(&scenes.compiled),
        })
    }

    /// Each source's state, as [`Engine::info`] reports it.
    pub fn sources(&self) -> Vec<SourceStatus> {
        statuses(&self.inner.state.lock().unwrap())
    }

    /// The first source that couldn't open, and why.
    pub fn first_failure(&self) -> Option<(String, String)> {
        let state = self.inner.state.lock().unwrap();
        state.sources.iter().find_map(|(id, live)| {
            (live.run == SourceRun::Failed)
                .then(|| (id.clone(), live.error.clone().unwrap_or_default()))
        })
    }

    /// The newest levels of every audio source and the program mix.
    pub fn levels(&self) -> Vec<AudioLevel> {
        self.inner.levels.lock().unwrap().clone()
    }

    /// Stage `scene` as studio mode's preview (or none).
    pub fn set_preview(&self, scene: Option<String>) {
        let scene: Option<Arc<str>> = scene.map(Into::into);
        let mut staged = self.inner.preview_scene.lock().unwrap();
        // Frames of another scene must not show as this one's.
        if *staged != scene {
            self.inner.preview.clear();
        }
        *staged = scene;
    }

    /// The newest frame of `target` — [`PROGRAM`], `"preview"` (studio
    /// mode's preview scene, drawn for a while after being asked for) or a
    /// source id — and its time. An error lists the valid targets.
    pub fn frame(&self, target: &str) -> Result<Option<(P::Frame, Time)>, String> {
        let slot = slot_of(&self.inner, target, MONITOR_FPS)?;
        // The program and the preview are drawn only while someone asks:
        // an idle one has no picture until its next frame.
        Ok(match target {
            PROGRAM | "preview" => wait_for_frame(&slot),
            _ => slot.newest_timed(),
        })
    }

    /// The newest frame of `target` (see [`Engine::frame`]) newer than
    /// `after` (an earlier frame's `seq`), as a JPEG of `quality` no wider
    /// than `max_width`, waiting up to `timeout` for one. `None` if none
    /// came in time.
    pub fn frame_jpeg(
        &self,
        target: &str,
        after: Option<u64>,
        timeout: Duration,
        max_width: Option<u32>,
        quality: f32,
        fps: Option<u32>,
    ) -> Result<Option<JpegFrame>, String> {
        let fps = fps.unwrap_or(MONITOR_FPS);
        let platform = &self.inner.platform;
        let from = after.map_or(Time::from_nanos(i64::MIN), |after| {
            Time::from_nanos(after.min(i64::MAX as u64 - 1) as i64 + 1)
        });
        let until = platform.now() + timeout;
        loop {
            // The slot is looked up again now and then: a source that
            // reopens gets a new one, and the preview must stay wanted.
            let slot = slot_of(&self.inner, target, fps)?;
            let left = until.saturating_since(platform.now());
            if let Some((frame, at)) = slot.wait_from(from, left.min(SLOT_RECHECK)) {
                let (jpeg, width, height) = platform
                    .encode_jpeg(&frame, max_width, quality)
                    .map_err(|err| format!("{err:#}"))?;
                let stamp = matches!(target, PROGRAM | "preview")
                    .then(|| frame_stream::stamp_at(&self.inner.drawn, at))
                    .flatten();
                return Ok(Some(JpegFrame {
                    seq: at.as_nanos().max(0) as u64,
                    width,
                    height,
                    jpeg,
                    revision: stamp.map(|stamp| stamp.revision),
                    layout_generation: stamp.map(|stamp| stamp.generation),
                }));
            }
            if platform.now() >= until || self.inner.stopping.load(Ordering::SeqCst) {
                return Ok(None);
            }
        }
    }

    /// `target`'s picture (see [`Engine::frame`]) as low-latency H.264 at
    /// most `max_width` wide and `fps` frames a second (changed later with
    /// [`FrameFeed::rate`]), for a monitor: the records of a stream shared
    /// with everyone watching the same at that width, which runs while
    /// anyone does, at the highest rate any of them wants. An error lists
    /// the valid targets.
    pub fn frames(
        &self,
        target: &str,
        max_width: Option<u32>,
        fps: u32,
    ) -> Result<FrameFeed, String> {
        let fps = fps.clamp(1, 60);
        slot_of(&self.inner, target, fps)?;
        let key = frame_stream::Key {
            target: target.to_string(),
            max_width,
        };
        let inner = self.inner.clone();
        let target = target.to_string();
        Ok(frame_stream::watch(
            &self.inner.frame_streams,
            &self.inner.platform,
            key,
            fps,
            // Called with the stream's rate now (the highest any of its
            // viewers wants), which the program is drawn at least at.
            move |fps| stream_source(&inner, &target, fps),
        ))
    }

    /// `target`'s picture (see [`Engine::frame`]) for a monitor in a
    /// native client on this machine: drawn by the GPU, fitted in
    /// `max_size`, into a ring of `slots` surfaces shared with the client,
    /// at most `fps` frames a second (changed later with
    /// [`SharedControl`](super::SharedControl)), while the feed lives;
    /// a source through its filters if `filtered`. An error lists the
    /// valid targets.
    pub fn shared_frames(
        &self,
        target: &str,
        max_size: (Option<u32>, Option<u32>),
        fps: u32,
        slots: u32,
        filtered: bool,
    ) -> Result<SharedFeed<P::SharedSurface>, String> {
        let fps = fps.clamp(1, 60);
        slot_of(&self.inner, target, fps)?;
        let filters = (filtered && !matches!(target, PROGRAM | "preview")).then(|| {
            let (inner, id) = (self.inner.clone(), target.to_string());
            Box::new(move || {
                let setup = inner.state.lock().unwrap().setup.clone();
                setup
                    .source(&id)
                    .map_or_else(Vec::new, |s| s.filters.clone())
            }) as shared_frames::LiveFilters
        });
        let inner = self.inner.clone();
        let request = shared_frames::Request {
            target: target.to_string(),
            max_size,
            fps,
            slots,
        };
        let target = target.to_string();
        Ok(shared_frames::watch(
            &self.inner.platform,
            request,
            move |fps| stream_source(&inner, &target, fps),
            filters,
        ))
    }

    /// The highest frame rate any client's monitor stream asks for right
    /// now (see [`Engine::frames`]), if one runs: the program needn't be
    /// drawn faster than this for monitors alone.
    pub fn monitor_fps(&self) -> Option<u32> {
        frame_stream::highest_rate(&self.inner.frame_streams)
    }

    /// An independent camera output; no change to recording routes or clocks.
    pub fn virtual_camera(&self, on_change: super::OnCameraChange) -> Result<super::VirtualCamera> {
        use isocast_protocol::virtual_camera::{Target, VirtualCameraStatus};
        let setup = self.setup();
        let settings = setup.virtual_camera.clone();
        settings.check_start(&setup).map_err(anyhow::Error::msg)?;
        let (width, height) = settings.size(&setup);
        let fps = setup.canvas.fps;
        if settings.target == Target::Preview && self.inner.preview_scene.lock().unwrap().is_none()
        {
            bail!(
                "virtual_camera.target: preview is empty; stage a scene with isocast preview ID, or choose program, scene or source"
            );
        }
        let status = VirtualCameraStatus {
            settings: settings.clone(),
            width,
            height,
            fps,
            ..Default::default()
        };
        let inner = self.inner.clone();
        let source_guard = if settings.target == Target::Source {
            *inner.camera_source.lock().unwrap() = settings.source.clone();
            let guard = CameraSourceGuard(inner.clone());
            *inner.wanted.lock().unwrap() = Some(setup.clone());
            inner.wake.notify_all();
            Some(guard)
        } else {
            None
        };
        let began = std::time::Instant::now();
        super::virtual_camera::start(
            &self.inner.platform,
            self.inner.timeline.clocks.clone(),
            status,
            move |after| {
                let _keep_source_open = &source_guard;
                if inner.stopping.load(Ordering::SeqCst) {
                    bail!("virtual_camera: the engine stopped");
                }
                if settings.target == Target::Scene {
                    return Ok(inner.scenes.latest().map(|scenes| {
                        super::virtual_camera::Input::Scene(scenes, settings.scene.clone().unwrap())
                    }));
                }
                let target = match settings.target {
                    Target::Program => PROGRAM,
                    Target::Preview => "preview",
                    Target::Source => settings.source.as_deref().unwrap(),
                    Target::Scene => unreachable!(),
                };
                let slot = match slot_of(&inner, target, fps) {
                    Ok(slot) => slot,
                    Err(_)
                        if settings.target == Target::Source
                            && began.elapsed() < FIRST_FRAME_TIMEOUT =>
                    {
                        return Ok(None);
                    }
                    Err(message) => return Err(anyhow::Error::msg(message)),
                };
                let frame = if settings.target == Target::Source {
                    // Still/slow sources repeat on the output clock, as cameras do.
                    slot.newest_timed()
                        .map(|(frame, _)| (frame, inner.platform.now()))
                } else {
                    slot.wait_from(
                        Time::from_nanos(after.as_nanos() + 1),
                        Duration::from_millis(50),
                    )
                };
                Ok(frame.map(|(frame, time)| super::virtual_camera::Input::Frame(frame, time)))
            },
            on_change,
        )
    }

    /// The program's frames and mix for an output like a stream, from the
    /// next frame on, queueing up to `depth` packets.
    pub fn tap_program(&self, depth: usize) -> ProgramTap<P::Frame> {
        self.program_tap_source()
            .tap(depth, Arc::new(AtomicU64::new(0)))
    }

    pub(crate) fn program_tap_source(&self) -> ProgramTapSource<P> {
        ProgramTapSource(self.inner.clone())
    }

    /// The program's shared encoders, for outputs that take it compressed.
    pub(crate) fn encoders(&self) -> &crate::stream::encoder::Encoders {
        &self.inner.encoders
    }

    /// Whether a recording is running.
    pub fn is_recording(&self) -> bool {
        self.inner.recording.load(Ordering::SeqCst)
    }

    /// Record the setup the engine runs, blocking until the recording ends
    /// (when `options.stop` is set, after `options.duration`, when the disk
    /// is nearly full, or when every file has failed).
    pub fn record(&self, options: &RecordOptions) -> Result<Report> {
        let inner = &self.inner;
        if inner.recording.swap(true, Ordering::SeqCst) {
            bail!("already recording; stop the current recording first");
        }
        let result = isocast_diagnostics::contain("recording", || record(self, options))
            .map_err(anyhow::Error::msg)
            .and_then(|r| r);
        {
            let mut state = inner.state.lock().unwrap_or_else(|p| p.into_inner());
            state.pins.clear();
            // Changes to the sources it recorded apply now.
            let setup = state.setup.clone();
            if !inner.state.is_poisoned() {
                *inner.wanted.lock().unwrap_or_else(|p| p.into_inner()) = Some(setup);
            }
        }
        inner.wake.notify_all();
        inner.recording.store(false, Ordering::SeqCst);
        result
    }
}

/// Returning a camera-only source to the ordinary source lifecycle happens
/// on every exit, including failed native startup or a disconnected camera.
struct CameraSourceGuard<P: Platform>(Arc<Inner<P>>);

impl<P: Platform> Drop for CameraSourceGuard<P> {
    fn drop(&mut self) {
        *self.0.camera_source.lock().unwrap() = None;
        if !self.0.stopping.load(Ordering::SeqCst) {
            let setup = self.0.state.lock().unwrap().setup.clone();
            *self.0.wanted.lock().unwrap() = Some(setup);
            self.0.wake.notify_all();
        }
    }
}

impl<P: Platform> Drop for Engine<P> {
    fn drop(&mut self) {
        if !self.inner.stopping.load(Ordering::SeqCst) {
            self.stop();
        }
    }
}

fn statuses<P: Platform>(state: &State<P>) -> Vec<SourceStatus> {
    state
        .sources
        .iter()
        .map(|(id, live)| SourceStatus {
            source: id.clone(),
            state: live.run,
            error: live.error.clone(),
            width: live.video.as_ref().map(|v| v.width),
            height: live.video.as_ref().map(|v| v.height),
            fps: live.video.as_ref().map(|v| v.fps),
            received_fps: live.video.as_ref().map(|v| v.slot.received_fps()),
            channels: live.audio.as_ref().map(|a| a.channels),
        })
        .collect()
}

/// The sources a setup runs: every scene's (so switching never waits for
/// one), the program mix's and its sidechains, the ISO sources, and those
/// being monitored.
fn needed_sources(setup: &Setup) -> Vec<String> {
    let mut ids: Vec<&str> = Vec::new();
    ids.extend(setup.scenes.iter().flat_map(|s| s.all_sources()));
    ids.extend(setup.recording.program.audio.iter().map(String::as_str));
    ids.extend(sidechain_sources(setup));
    let iso = &setup.recording.iso;
    if iso.enabled {
        ids.extend(
            iso.sources
                .iter()
                .chain(iso.listed_audio())
                .map(String::as_str),
        );
    }
    ids.extend(
        setup
            .sources
            .iter()
            .filter(|s| s.kind.has_audio() && s.audio.monitor != Monitoring::Off)
            .map(|s| s.id.as_str()),
    );
    let mut unique: Vec<String> = Vec::new();
    for id in ids {
        if setup.source(id).is_some() && !unique.iter().any(|u| u == id) {
            unique.push(id.to_string());
        }
    }
    unique
}

/// Why the canvas can't change from `current` to `new`: the program's
/// frames are being recorded or streamed at the current size and rate.
fn canvas_refusal<P: Platform>(inner: &Inner<P>, current: &Canvas, new: &Canvas) -> Option<String> {
    let canvas = |c: &Canvas| (c.width, c.height, c.fps);
    (canvas(current) != canvas(new) && !inner.program_video.is_empty()).then(|| {
        format!(
            "the canvas stays {}×{} at {} fps while recording, streaming or keeping a replay \
             buffer; change it once they stop",
            current.width, current.height, current.fps
        )
    })
}

/// The stinger videos `setup`'s transitions play.
fn stinger_files(setup: &Setup) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    for (_, transition) in setup.transitions() {
        if let TransitionKind::Stinger(stinger) = &transition.kind
            && !files.contains(&stinger.file)
        {
            files.push(stinger.file.clone());
        }
    }
    files
}

/// What source `id` is opened as: a change means opening it again.
fn definition_of(setup: &Setup, id: &str) -> serde_json::Value {
    setup
        .source(id)
        .and_then(|source| {
            let mut definition = serde_json::to_value(&source.kind).ok()?;
            // A picture's rate follows the canvas (a test pattern has its
            // own): a new canvas rate opens it again, so its ISO file
            // records at that rate.
            if source.kind.has_video()
                && !matches!(source.kind, SourceKind::TestPattern(_))
                && let Some(fields) = definition.as_object_mut()
            {
                fields.insert("canvas_fps".into(), setup.canvas.fps.into());
            }
            Some(definition)
        })
        .unwrap_or_default()
}

/// Open the sources the reconciler is asked for, one setup at a time.
fn reconciler<P: Platform>(inner: &Arc<Inner<P>>) {
    loop {
        let setup = {
            let mut wanted = inner.wanted.lock().unwrap();
            loop {
                if inner.stopping.load(Ordering::SeqCst) {
                    return;
                }
                if let Some(setup) = wanted.take() {
                    break setup;
                }
                wanted = inner.wake.wait(wanted).unwrap();
            }
        };
        reconcile(inner, setup);
    }
}

/// Bring the engine's sources in line with `setup`: open new and changed
/// ones (all at once, outside the lock), close those no longer used
/// (unless a recording uses them), and retry those that failed.
fn reconcile<P: Platform>(inner: &Arc<Inner<P>>, setup: Arc<Setup>) {
    let mut needed = needed_sources(&setup);
    if let Some(id) = inner.camera_source.lock().unwrap().clone()
        && !needed.contains(&id)
    {
        needed.push(id);
    }
    let mut opening = Vec::new();
    let mut closed = Vec::new();
    {
        let mut state = inner.state.lock().unwrap();
        for id in &needed {
            if inner.opening.lock().unwrap().contains(id) {
                continue;
            }
            let definition = definition_of(&setup, id);
            match state.sources.get(id) {
                Some(live) if state.pins.contains(id) => {
                    if live.definition != definition {
                        inner.event(EngineEvent::Warning(format!(
                            "{id}: the recording keeps the source as it was; the change applies \
                             once it stops"
                        )));
                    }
                }
                Some(live) if live.definition == definition && live.run == SourceRun::Running => {}
                Some(live) if live.definition == definition => {
                    opening.push((id.clone(), definition));
                }
                _ => opening.push((id.clone(), definition)),
            }
        }
        let gone: Vec<String> = state
            .sources
            .keys()
            .filter(|id| !needed.contains(id) && !state.pins.contains(id))
            .cloned()
            .collect();
        for id in gone {
            closed.extend(state.sources.remove(&id));
        }
        if !closed.is_empty() {
            publish(inner, &mut state);
        }
    }
    drop(closed);
    for (id, definition) in opening {
        inner.opening.lock().unwrap().insert(id.clone());
        let permission = setup
            .source(&id)
            .and_then(|s| inner.platform.source_permission(&s.kind));
        let mut pending = Live::failed(definition.clone(), String::new());
        pending.run = if permission.is_some() {
            SourceRun::WaitingForPermission
        } else {
            SourceRun::Opening
        };
        pending.error = permission.as_ref().map(|p| format!("waiting for permission: {p}; check macOS Privacy & Security or its permission prompt"));
        let event = pending.status(&id);
        let old = {
            let mut state = inner.state.lock().unwrap();
            let old = state.sources.insert(id.clone(), pending);
            publish(inner, &mut state);
            old
        };
        drop(old);
        inner.event(event);
        let inner = inner.clone();
        let setup = setup.clone();
        // Publish each result independently: one hidden permission prompt
        // must not withhold the other sources, edits, or engine shutdown.
        thread::spawn(move || {
            let result = inner
                .worker("source open", || {
                    source::open(
                        &inner.platform,
                        &setup,
                        &id,
                        &inner.timeline,
                        inner.control.data(),
                    )
                })
                .map_err(anyhow::Error::msg)
                .and_then(|r| r);
            let mut state = inner.state.lock().unwrap();
            let current = state.setup.clone();
            let wanted = !inner.stopping.load(Ordering::SeqCst)
                && definition_of(&current, &id) == definition
                && (needed_sources(&current).contains(&id)
                    || inner.camera_source.lock().unwrap().as_ref() == Some(&id));
            let mut replaced = None;
            let event = if wanted {
                let live = match result {
                    Ok(opened) => Live::running(&inner, &current, &id, definition, opened),
                    Err(err) => {
                        let mut live = Live::failed(definition, format!("source {id:?}: {err:#}"));
                        let message = format!("{err:#}").to_lowercase();
                        let device_permission = setup.source(&id).is_some_and(|s| {
                            matches!(
                                s.kind,
                                SourceKind::Camera(_)
                                    | SourceKind::Microphone(_)
                                    | SourceKind::Display(_)
                                    | SourceKind::Window(_)
                                    | SourceKind::Application(_)
                                    | SourceKind::SystemAudio(_)
                                    | SourceKind::ApplicationAudio(_)
                            )
                        });
                        if let Some(permission) = permission
                            && (device_permission
                                || message.contains("permission")
                                || message.contains("not permitted")
                                || message.contains("not authorized")
                                || message.contains("access denied")
                                || err.chain().any(|e| {
                                    e.downcast_ref::<std::io::Error>().is_some_and(|e| {
                                        e.kind() == std::io::ErrorKind::PermissionDenied
                                    })
                                }))
                        {
                            live.run = SourceRun::WaitingForPermission;
                            live.error = Some(format!(
                                "waiting for permission: {permission}; source {id:?}: {err:#}"
                            ));
                        }
                        live
                    }
                };
                let event = live.status(&id);
                replaced = state.sources.insert(id.clone(), live);
                publish(&inner, &mut state);
                Some(event)
            } else {
                // Drop an obsolete native capture outside the state lock.
                if let Ok(opened) = result {
                    replaced = Some(Live::closed(opened));
                }
                state.sources.remove(&id);
                None
            };
            inner.opening.lock().unwrap().remove(&id);
            if !wanted && !inner.stopping.load(Ordering::SeqCst) {
                *inner.wanted.lock().unwrap() = Some(current);
                inner.wake.notify_all();
            }
            drop(state);
            drop(replaced);
            if let Some(event) = event {
                inner.event(event);
            }
        });
    }

    // Stinger videos the setup's transitions play.
    let files = stinger_files(&setup);
    let loaded: Vec<String> = inner
        .state
        .lock()
        .unwrap()
        .stingers
        .keys()
        .cloned()
        .collect();
    let missing: Vec<&String> = files.iter().filter(|f| !loaded.contains(f)).collect();

    let platform = &inner.platform;
    let stingers = thread::scope(|scope| {
        let stingers: Vec<_> = missing
            .iter()
            .map(|file| {
                let setup = &setup;
                scope.spawn(move || {
                    let id = stinger_id(file);
                    source::stinger(platform, setup, &id, file, &inner.timeline)
                })
            })
            .collect();
        stingers.into_iter().map(joined).collect::<Vec<_>>()
    });

    let mut replaced = Vec::new();
    let mut events = Vec::new();
    {
        let mut state = inner.state.lock().unwrap();
        let current = state.setup.clone();
        let attempted: Vec<String> = missing.iter().map(|file| (*file).clone()).collect();
        let mut failed = HashMap::new();
        for (file, result) in missing.into_iter().zip(stingers) {
            match result {
                Ok(stinger) => {
                    state.stingers.insert(file.clone(), stinger);
                }
                Err(err) => {
                    events.push(EngineEvent::Warning(format!(
                        "a stinger transition's video {file:?} could not be loaded: {err:#}"
                    )));
                    failed.insert(file.clone(), format!("{err:#}"));
                }
            }
        }
        let unused: Vec<String> = state
            .stingers
            .keys()
            .filter(|file| !files.contains(file))
            .cloned()
            .collect();
        for file in unused {
            if let Some((opened, _)) = state.stingers.remove(&file) {
                replaced.push(Live::<P>::closed(opened));
            }
        }
        publish(inner, &mut state);
        inner
            .control
            .loaded_stingers(&attempted, failed, &stinger_files(&current));
    }
    drop(replaced);
    for event in events {
        inner.event(event);
    }
}

/// What a thread opening something returned, or why it crashed.
fn joined<T>(handle: thread::ScopedJoinHandle<'_, Result<T>>) -> Result<T> {
    handle
        .join()
        .unwrap_or_else(|_| Err(anyhow::anyhow!("opening it crashed")))
}

impl<P: Platform> Live<P> {
    /// A source that opened: its audio gets a reader of its own.
    fn running(
        inner: &Arc<Inner<P>>,
        setup: &Setup,
        id: &str,
        definition: serde_json::Value,
        mut opened: source::Opened<P>,
    ) -> Self {
        let video = opened.video.take();
        let audio = opened.audio.take().map(|audio| {
            let settings = setup.source(id).map(|s| s.audio).unwrap_or_default();
            let tap = Tap::new(
                inner.monitor.clone(),
                id,
                audio.channels,
                settings,
                setup.audio.pan_law,
            );
            AudioRun::spawn(inner, id, audio, tap)
        });
        Self {
            definition,
            run: SourceRun::Running,
            error: None,
            video,
            audio,
            opened: Some(opened),
        }
    }

    fn failed(definition: serde_json::Value, error: String) -> Self {
        Self {
            definition,
            run: SourceRun::Failed,
            error: Some(error),
            video: None,
            audio: None,
            opened: None,
        }
    }

    /// Something opened that isn't wanted after all, to close.
    fn closed(opened: source::Opened<P>) -> Self {
        Self {
            definition: serde_json::Value::Null,
            run: SourceRun::Failed,
            error: None,
            video: None,
            audio: None,
            opened: Some(opened),
        }
    }

    fn status(&self, id: &str) -> EngineEvent {
        EngineEvent::Source(SourceStatus {
            source: id.to_string(),
            state: self.run,
            error: self.error.clone(),
            width: self.video.as_ref().map(|v| v.width),
            height: self.video.as_ref().map(|v| v.height),
            fps: self.video.as_ref().map(|v| v.fps),
            received_fps: None,
            channels: self.audio.as_ref().map(|a| a.channels),
        })
    }
}

impl<F: Clone + Send + 'static> AudioRun<F> {
    fn spawn<P: Platform<Frame = F>>(
        inner: &Arc<Inner<P>>,
        id: &str,
        audio: Audio,
        mut tap: Tap,
    ) -> Self {
        let routes: Arc<Routes<F>> = Arc::default();
        let meter: Arc<Meter> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let channels = audio.channels;
        let feed = audio.feed.clone();
        let thread = thread::spawn({
            let fault_inner = inner.clone();
            let failure_feed = feed.clone();
            let platform = inner.platform.clone();
            let control = inner.control.clone();
            let routes = routes.clone();
            let meter = meter.clone();
            let stop = stop.clone();
            let id = id.to_string();
            move || {
                let failure = fault_inner.worker(&format!("audio source {id}"), || {
                    platform.real_time_thread();
                    while !stop.load(Ordering::SeqCst) {
                        match audio.chunks.recv_timeout(Duration::from_millis(10)) {
                            Ok((samples, pts)) => {
                                // The source's sync offset places it later (or
                                // earlier) on the session clock.
                                let offset = control
                                    .audio_settings(&id)
                                    .map_or(0, |s| i64::from(s.offset_ms) * 1_000_000);
                                let pts = Time::from_nanos(pts.as_nanos() + offset);
                                meter.add(&samples);
                                tap.play(&platform, Some(&control), &samples);
                                routes.audio(&samples, pts);
                            }
                            Err(RecvTimeoutError::Timeout) => {
                                if audio.feed.stopped.lock().unwrap().is_some() {
                                    break;
                                }
                            }
                            Err(RecvTimeoutError::Disconnected) => break,
                        }
                    }
                });
                if let Err(message) = failure {
                    *failure_feed
                        .stopped
                        .lock()
                        .unwrap_or_else(|p| p.into_inner()) = Some(message);
                }
            }
        });
        Self {
            routes,
            meter,
            channels,
            feed,
            stop,
            thread: Some(thread),
        }
    }
}

/// Hand the setup's scenes and the running sources to the program, the
/// mix, the monitor and the control; start the program and mix once the
/// engine clock has started.
fn publish<P: Platform>(inner: &Arc<Inner<P>>, state: &mut State<P>) {
    let setup = state.setup.clone();
    let slots: Slots<P::Frame> = state
        .sources
        .iter()
        .filter_map(|(id, live)| Some((id.clone(), live.video.as_ref()?.slot.clone())))
        .collect();
    let mut program_slots = slots.clone();
    for (file, (opened, _)) in &state.stingers {
        if let Some(video) = &opened.video {
            program_slots.push((stinger_id(file), video.slot.clone()));
        }
    }
    inner.scenes.submit(SceneInput {
        setup: setup.clone(),
        revision: state.revision,
        slots: program_slots,
    });

    let control = &inner.control;
    let mut frames = slots;
    frames.push((PROGRAM.to_string(), inner.program.clone()));
    control.set_frames(frames);
    *control.browsers.lock().unwrap() = state
        .sources
        .iter()
        .filter_map(|(id, live)| Some((id.clone(), live.opened.as_ref()?.browser()?)))
        .collect();
    *control.stingers.lock().unwrap() = state
        .stingers
        .iter()
        .map(|(file, (_, length))| (file.clone(), *length))
        .collect();
    control.set_audio(&setup);
    control.set_filters(&setup);

    // Another monitoring device: sources heard switch to it.
    let monitor = inner.monitor.lock().unwrap().clone();
    if monitor.device() != setup.audio.monitoring_device {
        *inner.monitor.lock().unwrap() = Arc::new(Monitor::new(&setup.audio.monitoring_device));
        drop(Closing(monitor));
    }

    if inner.epoch().is_some() && !inner.stopping.load(Ordering::SeqCst) {
        ensure_mixer(inner, state);
        ensure_compositor(inner, state);
    }
}

/// Run the program mix with the setup's inputs, starting it again only if
/// they changed, and route each input's reader to it.
fn ensure_mixer<P: Platform>(inner: &Arc<Inner<P>>, state: &mut State<P>) {
    let setup = state.setup.clone();
    let channels = |id: &str| {
        state
            .sources
            .get(id)
            .and_then(|live| live.audio.as_ref())
            .map(|audio| audio.channels)
    };
    let mut wanted: Vec<(String, u32, bool)> = Vec::new();
    for id in &setup.recording.program.audio {
        if let Some(channels) = channels(id) {
            wanted.push((id.clone(), channels, true));
        }
    }
    for id in sidechain_sources(&setup) {
        if let Some(channels) = channels(id) {
            wanted.push((id.to_string(), channels, false));
        }
    }
    if state
        .mixer
        .as_ref()
        .is_none_or(|mixer| mixer.inputs != wanted)
    {
        let epoch = inner.epoch().expect("the engine clock has started");
        // The new mix takes over exactly where the old one stops.
        let now = inner.platform.now();
        let from = now.ticks_since(epoch, SAMPLE_RATE).max(0);
        if let Some(old) = state.mixer.take() {
            old.shared
                .stop_at(epoch.add_ticks(from as u64, SAMPLE_RATE));
            stop_mixer(inner, old, &state.sources);
        }
        let (tx, rx) = sync_channel(MIX_QUEUE_DEPTH);
        let settings = |id: &str| setup.source(id).map(|s| s.audio).unwrap_or_default();
        let mixed = wanted
            .iter()
            .filter(|(_, _, mixed)| *mixed)
            .map(|(id, channels, _)| (id.clone(), *channels, settings(id)))
            .collect();
        let mut mixer = Mixer::new(mixed, setup.audio.pan_law, rx, inner.program_meter.clone());
        for (id, channels, _) in wanted.iter().filter(|(_, _, mixed)| !*mixed) {
            mixer.add_key(id.clone(), *channels, settings(id));
        }
        mixer.set_filters(inner.control.audio_filters.clone());
        let shared = Arc::new(Shared {
            epoch,
            end: AtomicI64::new(i64::MAX),
        });
        let thread = thread::spawn({
            let fault_inner = inner.clone();
            let platform = inner.platform.clone();
            let control = inner.control.clone();
            let outputs = inner.program_audio.clone();
            let shared = shared.clone();
            move || {
                platform.real_time_thread();
                let _ = fault_inner.worker("mixer", || {
                    mixer.run(&platform, &shared, from, Some(&control), &outputs)
                });
            }
        });
        state.mixer = Some(MixerRun {
            shared,
            thread: Some(thread),
            inputs: wanted,
            tx,
            routed: HashMap::new(),
        });
    }
    let Some(mixer) = &mut state.mixer else {
        return;
    };
    for (track, (id, ..)) in mixer.inputs.iter().enumerate() {
        let Some(audio) = state.sources.get(id).and_then(|live| live.audio.as_ref()) else {
            continue;
        };
        let reader = Arc::as_ptr(&audio.routes) as usize;
        if mixer.routed.get(id).map(|(r, _)| *r) != Some(reader) {
            let key = audio
                .routes
                .add(mixer.tx.clone(), track, Time::from_nanos(0), None);
            mixer.routed.insert(id.clone(), (reader, key));
        }
    }
}

/// Unroute a mix from its inputs and wait for it to finish its last block.
fn stop_mixer<P: Platform>(
    inner: &Inner<P>,
    mut mixer: MixerRun<P::Frame>,
    sources: &BTreeMap<String, Live<P>>,
) {
    for (id, (reader, key)) in &mixer.routed {
        if let Some(audio) = sources.get(id).and_then(|live| live.audio.as_ref())
            && Arc::as_ptr(&audio.routes) as usize == *reader
        {
            audio.routes.remove(*key);
        }
    }
    mixer.shared.stop_at(inner.platform.now());
    if let Some(thread) = mixer.thread.take() {
        let _ = thread.join();
    }
}

/// Run the program at the setup's canvas size and rate, starting it again
/// only if they changed (and nothing is taking its frames).
fn ensure_compositor<P: Platform>(inner: &Arc<Inner<P>>, state: &mut State<P>) {
    let canvas = &state.setup.canvas;
    let wanted = (canvas.width, canvas.height, canvas.fps);
    if let Some(run) = &state.compositor
        && (run.canvas == wanted || !inner.program_video.is_empty())
    {
        return;
    }
    if let Some(old) = state.compositor.take() {
        stop_compositor(old);
    }
    let compositor = match Compositor::new(&inner.platform, (wanted.0, wanted.1)) {
        Ok(compositor) => compositor,
        Err(err) => {
            inner.event(EngineEvent::Warning(format!(
                "could not set up the GPU for the program: {err:#}"
            )));
            return;
        }
    };
    let stop = Arc::new(AtomicBool::new(false));
    let thread = thread::spawn({
        let inner = inner.clone();
        let stop = stop.clone();
        move || {
            let _ = inner.worker("compositor", || {
                run_program(&inner, compositor, wanted.2, &stop)
            });
        }
    });
    state.compositor = Some(CompositorRun {
        stop,
        thread: Some(thread),
        canvas: wanted,
    });
}

fn stop_compositor(mut run: CompositorRun) {
    run.stop.store(true, Ordering::SeqCst);
    if let Some(thread) = run.thread.take() {
        let _ = thread.join();
    }
}

/// Where a monitor stream finds `target`'s frames, asked for at `fps` (the
/// program and the preview are drawn at least that often, on the canvas's
/// frame grid, and say which setup revision each frame shows).
fn stream_source<P: Platform>(
    inner: &Inner<P>,
    target: &str,
    fps: u32,
) -> Result<frame_stream::Source<P::Frame>, String> {
    if inner.stopping.load(Ordering::SeqCst) {
        return Err("the live engine stopped".into());
    }
    let slot = slot_of(inner, target, fps)?;
    let canvas = matches!(target, PROGRAM | "preview");
    let grid = match canvas {
        true => inner.epoch().zip(
            inner
                .state
                .lock()
                .unwrap()
                .compositor
                .as_ref()
                .map(|run| run.canvas.2),
        ),
        false => None,
    };
    let drawn = canvas.then(|| inner.drawn.clone());
    Ok(frame_stream::Source { slot, grid, drawn })
}

/// Where frames of `target` arrive: the program, `"preview"` (both kept
/// drawn at least `fps` times a second for a while after each ask) or a
/// video source. An error lists the valid targets.
fn slot_of<P: Platform>(
    inner: &Inner<P>,
    target: &str,
    fps: u32,
) -> Result<Arc<FrameSlot<P::Frame>>, String> {
    // The program and the preview are drawn while they are asked for.
    inner.want(target, fps);
    match target {
        PROGRAM => Ok(inner.program.clone()),
        "preview" => {
            if inner.preview_scene.lock().unwrap().is_none() {
                return Err("nothing is staged in preview; stage a scene with `preview`".into());
            }
            Ok(inner.preview.clone())
        }
        id => {
            let state = inner.state.lock().unwrap();
            // An audio source has no frames to wait for.
            match state.sources.get(id).and_then(|live| live.video.as_ref()) {
                Some(video) => Ok(video.slot.clone()),
                None => {
                    let ids: Vec<&str> = state
                        .sources
                        .iter()
                        .filter(|(_, live)| live.video.is_some())
                        .map(|(id, _)| id.as_str())
                        .collect();
                    let why = match state.sources.get(id) {
                        Some(Live {
                            error: Some(error), ..
                        }) => format!(" (it failed: {error})"),
                        Some(_) => " (it has no picture)".into(),
                        None => String::new(),
                    };
                    Err(format!(
                        "target: {id:?} is not a running video source{why}; give \
                         \"program\", \"preview\" or one of: {}",
                        if ids.is_empty() {
                            "(none)".to_string()
                        } else {
                            ids.join(", ")
                        }
                    ))
                }
            }
        }
    }
}

/// The program's frame clock: at each tick, the scene on air drawn from the
/// frames nearest to it, for the program's outputs; and studio mode's
/// preview scene while someone wants it. With no outputs (no recording or
/// stream), the program is drawn only while clients' monitors ask for it,
/// at the rates they ask for (on the canvas's grid); what is on air is
/// still published at every tick.
fn run_program<P: Platform>(
    inner: &Arc<Inner<P>>,
    mut compositor: Compositor<P>,
    fps: u32,
    stop: &AtomicBool,
) {
    let Some(epoch) = inner.epoch() else {
        return;
    };
    let platform = &inner.platform;
    platform.real_time_thread();
    compositor.set_epoch(epoch);
    let frame = Duration::from_secs(1) / fps;
    // The version of the compiled scenes the compositor draws.
    let mut clock = inner.timeline.clocks.start("program".into(), frame);
    let mut version = 0;
    let mut warned = false;
    // The sources on air, kept between ticks.
    let mut on_air_sources: Vec<Arc<str>> = Vec::new();
    // The rates monitors show the program and the preview at.
    let mut program_rates = Rates::default();
    let mut preview_rates = Rates::default();
    let first = platform.now().ticks_since(epoch, fps).max(0) as u64 + 1;
    // Where the frame rate is measured from: a time and the frames drawn
    // by then.
    let mut window = (platform.now(), inner.frames_drawn.load(Ordering::Relaxed));
    // What the compositor's inputs come from: the setup revision and the
    // layout generation.
    let mut stamp = frame_stream::Stamp {
        revision: 0,
        generation: 0,
    };
    for n in first.. {
        let tick = epoch.add_ticks(n, fps);
        clock.sleep_until(platform, tick + frame);
        if stop.load(Ordering::SeqCst) || inner.stopping.load(Ordering::SeqCst) {
            break;
        }
        // What this tick costs the thread, if it gives the program a frame.
        let cpu = platform.thread_cpu_time();
        let mut gave = None;
        let now = platform.now();
        let elapsed = now.saturating_since(window.0);
        if elapsed >= Duration::from_secs(1) {
            let drawn = inner.frames_drawn.load(Ordering::Relaxed);
            let rate = drawn.saturating_sub(window.1) as f64 / elapsed.as_secs_f64();
            inner.program_fps.store(rate.to_bits(), Ordering::Relaxed);
            window = (now, drawn);
        }
        // One atomic load, unless the scenes changed.
        if inner.scenes.version() != version
            && let Some(scenes) = inner.scenes.latest()
            && scenes.version != version
        {
            let newest = scenes.version;
            let of = frame_stream::Stamp {
                revision: scenes.revision,
                generation: scenes.generation,
            };
            if compositor.adopt(scenes, tick) {
                version = newest;
                stamp = of;
            }
        }
        let control = &inner.control;
        let now = platform.now();
        program_rates.ask(control.take_program_wanted(), fps, now);
        preview_rates.ask(inner.preview_wanted.swap(0, Ordering::SeqCst), fps, now);
        let outputs = !inner.program_video.is_empty();
        let monitored = program_rates.any(now);
        let preview_wanted = preview_rates.any(now);
        if now > tick + 3 * frame {
            if outputs || monitored {
                inner.frames_missed.fetch_add(1, Ordering::Relaxed);
            }
            continue;
        }
        let on_air = control.on_air(tick);
        // A frame on, so a stinger's video can start a frame early.
        let ahead = control.on_air(tick + frame);
        if compositor.visible(on_air.as_ref(), ahead.as_ref(), tick, &mut on_air_sources) {
            inner
                .timeline
                .set_on_air(on_air_sources.iter().map(|id| &**id), tick);
        }
        if outputs || program_rates.draws(n, fps) {
            let draw_started = std::time::Instant::now();
            match compositor.draw(Feed::Program, on_air.as_ref(), tick, tick + frame / 2) {
                Ok(Some((image, reused))) => {
                    gave = Some((reused, compositor.draws(), compositor.animated()));
                    // Before the frame, so whoever takes it finds its revision.
                    frame_stream::drawn(&inner.drawn, tick, stamp);
                    inner.program.push(image.clone(), tick);
                    inner.program_video.video(&image, tick);
                    inner.frames_drawn.fetch_add(1, Ordering::Relaxed);
                }
                Ok(None) => {
                    inner.frames_missed.fetch_add(1, Ordering::Relaxed);
                }
                Err(err) => {
                    inner.frames_missed.fetch_add(1, Ordering::Relaxed);
                    if !std::mem::replace(&mut warned, true) {
                        inner.event(EngineEvent::Warning(format!(
                            "the program could not draw a frame: {err:#}"
                        )));
                    }
                }
            }
            inner
                .timings
                .lock()
                .unwrap()
                .draw(draw_started.elapsed(), compositor.gpu_wait);
        } else if !monitored && !outputs {
            // Nothing wants the program: let its canvas go back to the pool.
            inner.program.clear();
            compositor.forget(Feed::Program);
        }
        let scene = preview_wanted
            .then(|| inner.preview_scene.lock().unwrap().clone())
            .flatten();
        match scene {
            Some(scene) if preview_rates.draws(n, fps) => {
                let staged = OnAir { scene, from: None };
                if let Ok(Some((image, _))) =
                    compositor.draw(Feed::Preview, Some(&staged), tick, tick + frame / 2)
                {
                    inner.preview.push(image, tick);
                }
            }
            Some(_) => {}
            None => {
                inner.preview.clear();
                compositor.forget(Feed::Preview);
            }
        }
        if let (Some((reused, draws, animated)), Some(from), Some(to)) =
            (gave, cpu, platform.thread_cpu_time())
        {
            let spent = to.saturating_sub(from);
            inner
                .timings
                .lock()
                .unwrap()
                .tick(spent, reused, draws, animated);
        }
        // Pictures of another size than layout took them to be: the scenes
        // are laid out again, and adopted at a coming tick.
        let resized = compositor.resized();
        if resized.len() > 0 {
            inner.scenes.resized(resized);
        }
    }
}

/// The rates clients' monitors show a picture at, each until
/// [`PREVIEW_HOLD`] after it was last asked for.
#[derive(Default)]
struct Rates(Vec<(u32, Time)>);

impl Rates {
    /// Someone asked for `fps` (nobody if 0) at `now`, on a canvas running
    /// at `canvas_fps`; rates no longer asked for are forgotten.
    fn ask(&mut self, fps: u32, canvas_fps: u32, now: Time) {
        self.0.retain(|(_, until)| now < *until);
        if fps == 0 {
            return;
        }
        let fps = fps.min(canvas_fps);
        let until = now + PREVIEW_HOLD;
        match self.0.iter_mut().find(|(rate, _)| *rate == fps) {
            Some(entry) => entry.1 = until,
            None => self.0.push((fps, until)),
        }
    }

    /// Whether anyone still wants the picture.
    fn any(&self, now: Time) -> bool {
        self.0.iter().any(|(_, until)| now < *until)
    }

    /// Whether tick `n` of a canvas at `canvas_fps` is drawn for them: it
    /// lies on some rate's grid (every `canvas_fps / rate`th tick, so a rate
    /// that doesn't divide the canvas's gets a little more).
    fn draws(&self, n: u64, canvas_fps: u32) -> bool {
        self.0
            .iter()
            .any(|(fps, _)| n.is_multiple_of(u64::from((canvas_fps / fps).max(1))))
    }
}

/// Measure levels for clients, notice sources that stopped, and try
/// failed ones again now and then.
fn housekeeping<P: Platform>(inner: &Arc<Inner<P>>) {
    let mut next_retry = inner.platform.now() + RETRY_INTERVAL;
    while !inner.stopping.load(Ordering::SeqCst) {
        thread::sleep(HOUSEKEEPING_INTERVAL);
        let mut levels = Vec::new();
        let mut events = Vec::new();
        {
            let mut state = inner.state.lock().unwrap();
            let now = inner.platform.now();
            if now >= next_retry {
                next_retry = now + RETRY_INTERVAL;
                let failed = state.sources.iter().any(|(id, live)| {
                    matches!(
                        live.run,
                        SourceRun::Failed | SourceRun::WaitingForPermission
                    ) && !state.pins.contains(id)
                });
                let mut wanted = inner.wanted.lock().unwrap();
                if failed && wanted.is_none() {
                    *wanted = Some(state.setup.clone());
                    inner.wake.notify_all();
                }
            }
            for (id, live) in state.sources.iter_mut() {
                if let Some(audio) = &live.audio {
                    let (peak_db, rms_db) = audio.meter.take();
                    levels.push(AudioLevel {
                        source: id.clone(),
                        peak_db,
                        rms_db,
                        channels: None,
                    });
                }
                if live.run != SourceRun::Running {
                    continue;
                }
                let stopped = live
                    .video
                    .as_ref()
                    .and_then(|video| video.slot.stopped())
                    .or_else(|| {
                        live.audio
                            .as_ref()
                            .and_then(|audio| audio.feed.stopped.lock().unwrap().clone())
                    })
                    .or_else(|| live.opened.as_ref().and_then(|o| o.failure()));
                if let Some(reason) = stopped {
                    if let Some(video) = &live.video {
                        video.slot.stop(reason.clone());
                    }
                    live.run = SourceRun::Failed;
                    live.error = Some(format!("the capture stopped: {reason}"));
                    events.push(live.status(id));
                }
            }
        }
        // The program's, each channel too (the control bar's two bars).
        let ((peak_db, rms_db), channels) = inner.program_meter.take();
        levels.push(AudioLevel {
            source: PROGRAM.to_string(),
            peak_db,
            rms_db,
            channels: Some(channels.map(|(peak_db, rms_db)| ChannelLevel { peak_db, rms_db })),
        });
        {
            let mut loudest = inner.loudest.lock().unwrap();
            for level in &levels {
                let entry = loudest
                    .entry(level.source.clone())
                    .or_insert((f64::MIN, f64::MIN));
                entry.0 = entry.0.max(level.peak_db);
                entry.1 = entry.1.max(level.rms_db);
            }
        }
        *inner.levels.lock().unwrap() = levels.clone();
        for event in events {
            inner.event(event);
        }
        inner.event(EngineEvent::Levels(levels));
    }
}

/// A recording's frame clock for one ISO source: one frame per tick of the
/// recording's clock, the one captured nearest to it (repeating the last
/// one while the picture is still), filtered if the files record it
/// filtered, and converted to Y'CbCr if their encoders would otherwise
/// convert it themselves.
struct IsoRunner<P: Platform> {
    clock: crate::pacing::MediaClock,
    id: String,
    slot: Arc<FrameSlot<P::Frame>>,
    /// The files' frame size.
    size: (u32, u32),
    fps: u32,
    /// Its files' encoders take only Y'CbCr (H.264, HEVC).
    ycbcr_only: bool,
    /// Every source's filters, if the files record this one filtered.
    filters: Option<SourceFilters>,
    /// Where frames are filtered or converted, made when the first frame
    /// needs it.
    stage: Option<Box<IsoFrames<P>>>,
    outputs: Vec<Output<P::Frame>>,
}

/// The GPU the recording's ISO files filter and convert frames with, one
/// for all of them, opened when the first one needs it.
type IsoGpu = Mutex<Option<Gpu>>;

impl<P: Platform> IsoRunner<P> {
    /// Whether `image` must go through the stage before the files take it.
    fn needs_stage(&self, platform: &P, image: &P::Frame) -> bool {
        self.filters.is_some() || (self.ycbcr_only && !platform.frame_is_ycbcr(image))
    }

    /// The stage, made if it isn't yet. Setting one up takes a while (a
    /// few frames' time), so it is made before the recording starts when
    /// the source's frames show it is needed.
    fn stage(&mut self, platform: &P, gpu: &IsoGpu) -> Result<&mut IsoFrames<P>> {
        if self.stage.is_none() {
            let gpu = {
                let mut gpu = gpu.lock().unwrap();
                match &*gpu {
                    Some(gpu) => gpu.clone(),
                    None => gpu.insert(Gpu::new()?).clone(),
                }
            };
            let stage = IsoFrames::new(platform, gpu, &self.id, self.size, self.filters.clone())
                .with_context(|| {
                    format!("could not prepare {:?}'s frames for its file", self.id)
                })?;
            self.stage = Some(Box::new(stage));
        }
        Ok(self.stage.as_mut().expect("made above"))
    }

    /// `image`, as the files take it.
    fn prepare(
        &mut self,
        platform: &P,
        gpu: &IsoGpu,
        image: P::Frame,
        tick: Time,
    ) -> Result<Option<P::Frame>> {
        if !self.needs_stage(platform, &image) {
            return Ok(Some(image));
        }
        self.stage(platform, gpu)?.draw(&image, tick)
    }
}

/// Refuses splits once a recording has ended, however it ends (see
/// [`SessionControl::finish_recording`]).
struct FinishRecording<'a>(&'a SessionControl);

impl Drop for FinishRecording<'_> {
    fn drop(&mut self) {
        self.0.finish_recording();
    }
}

/// When a split asked for at `now` begins the next take: on the frame grid
/// of every file (files start at the session's `epoch`; their frame rates
/// are `rates`), at least a second after the `current` take's start, and
/// per `timing` either the next whole second at least [`SPLIT_LEAD`] away,
/// or the next moment at least [`SPLIT_SOON_LEAD`] away that every file's
/// frame rate and the audio's sample rate share and that is a whole number
/// of nanoseconds from the epoch (1/10 s for 60 and 30 fps files). Frame
/// times are nanoseconds rounded down, so only such moments fall exactly on
/// a frame of every file (and of the program, whose frames count from the
/// live engine's clock): at any other, a file's length would come out a
/// frame long.
fn split_time(epoch: Time, current: Time, rates: &[u32], timing: SplitTiming, now: Time) -> Time {
    let (rate, lead) = match timing {
        SplitTiming::WholeSecond => (1, SPLIT_LEAD),
        SplitTiming::Soonest => {
            let rate = rates
                .iter()
                .map(|fps| (*fps).max(1))
                .fold(gcd(1_000_000_000, SAMPLE_RATE), gcd);
            (rate, SPLIT_SOON_LEAD)
        }
    };
    // The first tick of `rate` at or after `time`: whole ticks from `epoch`
    // are exactly where each file's frames fall (`Time::add_ticks`).
    let tick_after = |time: Time| {
        let mut ticks = time.ticks_since(epoch, rate).max(0) as u64;
        if epoch.add_ticks(ticks, rate) < time {
            ticks += 1;
        }
        ticks
    };
    let ticks = tick_after(now + lead).max(tick_after(current + Duration::from_secs(1)));
    epoch.add_ticks(ticks, rate)
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd(b, a % b) }
}

fn run_iso<P: Platform>(
    platform: &P,
    mut runner: IsoRunner<P>,
    gpu: &IsoGpu,
    shared: &Shared,
) -> Result<()> {
    platform.real_time_thread();
    let epoch = shared.epoch;
    let fps = runner.fps;
    let frame = Duration::from_secs(1) / fps;
    for n in 0.. {
        let tick = epoch.add_ticks(n, fps);
        // Frames up to half a frame after the tick are nearest to it; wait
        // another half frame for them to arrive.
        runner.clock.sleep_until(platform, tick + frame);
        if tick >= shared.end() {
            break;
        }
        // Its file ends here; the rest of the recording goes on.
        if let Some(reason) = runner.slot.stopped() {
            bail!(
                "{}: the capture stopped, so its file ends early: {reason}",
                runner.id
            );
        }
        if platform.now() > tick + 3 * frame {
            continue;
        }
        let Some(image) = runner.slot.newest_until(tick + frame / 2) else {
            continue;
        };
        // None: its frames are all still being written.
        let Some(image) = runner.prepare(platform, gpu, image, tick)? else {
            continue;
        };
        for output in &runner.outputs {
            let _ = output.try_send(Packet::Video {
                frame: image.clone(),
                pts: tick,
            });
        }
    }
    Ok(())
}

/// Removes a recording's routes when it ends, however it ends.
struct Unroute<F: Clone>(Vec<(Arc<Routes<F>>, u64)>);

impl<F: Clone> Drop for Unroute<F> {
    fn drop(&mut self) {
        for (routes, key) in self.0.drain(..) {
            routes.remove(key);
        }
    }
}

/// Record the engine's setup until the recording ends.
/// 16601 -> "16,601", for the per-file drop warnings.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// One decimal, but never "0.0%" for a real drop or "100.0%" while some
/// frames survived.
fn drop_percent(dropped: u64, expected: u64) -> String {
    let percent = 100.0 * dropped as f64 / expected.max(1) as f64;
    if percent < 0.1 {
        "<0.1%".into()
    } else if percent > 99.9 && dropped < expected {
        ">99.9%".into()
    } else {
        format!("{percent:.1}%")
    }
}

fn record<P: Platform>(engine: &Engine<P>, options: &RecordOptions) -> Result<Report> {
    let inner = &engine.inner;
    let platform = &inner.platform;
    let control = &inner.control;
    let setup = inner.state.lock().unwrap().setup.clone();
    let setup = &*setup;
    let mut warnings = Vec::new();
    let iso = &setup.recording.iso;
    let program = &setup.recording.program;
    if program.enabled
        && let Some(reason) = inner.program.stopped()
    {
        bail!(
            "program output has failed: {reason}; stop the recording and restart the live engine"
        );
    }
    let iso_enabled = iso.enabled && !iso.sources.is_empty();
    if !iso_enabled && !program.enabled {
        bail!(
            "nothing to record: set recording.program.enabled = true, or set \
             recording.iso.enabled = true and list video sources in recording.iso.sources"
        );
    }
    let folder = match &options.folder {
        Some(folder) => folder.clone(),
        None => expand_home(&setup.recording.folder),
    };
    std::fs::create_dir_all(&folder)
        .with_context(|| format!("could not create the folder {}", folder.display()))?;
    // Recordings here that never finished (killed or crashed) are marked
    // as such and reported; then this one holds the folder's lock until it
    // ends, so it isn't taken for one of them.
    if let Some(note) = interrupted_note(&interrupted_sessions(&folder, true), true) {
        warnings.push(note);
    }
    let _folder_lock = FolderLock::hold(&folder);

    // The sources the files need: each must be running (a missing device or
    // permission fails here, before any file exists).
    let mut required: Vec<&str> = Vec::new();
    if iso_enabled {
        required.extend(
            iso.sources
                .iter()
                .chain(iso.listed_audio())
                .map(String::as_str),
        );
    }
    if program.enabled {
        required.extend(program.audio.iter().map(String::as_str));
    }
    let waiting_since = platform.now();
    loop {
        let state = inner.state.lock().unwrap();
        // A source the setup just changed (another device, say) is still
        // the old one until it reopens: wait for it, or the recording would
        // keep the old one to the end.
        let reopening = |id: &str, live: &Live<P>| live.definition != definition_of(setup, id);
        let pending = required.iter().any(|id| {
            state.sources.get(*id).is_none_or(|live| {
                matches!(
                    live.run,
                    SourceRun::Opening | SourceRun::WaitingForPermission
                ) || reopening(id, live)
            })
        });
        if !pending || platform.now().saturating_since(waiting_since) >= SOURCE_WAIT {
            for id in &required {
                // Which list makes the recording need it, for the way out.
                let has = |list: &[String]| list.iter().any(|s| s == id);
                let list = match iso.audio_field(id) {
                    _ if iso_enabled && has(&iso.sources) => "recording.iso.sources".into(),
                    Some(field) if iso_enabled => field,
                    _ => "recording.program.audio".to_string(),
                };
                let why = match state.sources.get(*id) {
                    Some(live) if reopening(id, live) => {
                        format!("could not start source {id:?}: its new settings are still opening")
                    }
                    Some(live) if live.run == SourceRun::Running => continue,
                    Some(live) => live
                        .error
                        .clone()
                        .unwrap_or_else(|| format!("could not start source {id:?}")),
                    None => format!("could not start source {id:?}: it is still opening"),
                };
                bail!(
                    "{why}\nThe recording needs {id:?} ({list} lists it): connect it or fix its \
                     device, or take it out of {list}"
                );
            }
            break;
        }
        drop(state);
        thread::sleep(Duration::from_millis(20));
    }

    // Sources whose ISO files are filtered.
    let filtered: Vec<&str> = match iso.filtered {
        true => setup
            .sources
            .iter()
            .filter(|source| source.filters.iter().any(|f| f.enabled))
            .map(|source| source.id.as_str())
            .collect(),
        false => Vec::new(),
    };
    // What each recorded source delivers, and the readers to route from.
    let mut formats = Formats {
        video: BTreeMap::new(),
        audio: BTreeMap::new(),
    };
    let mut slots: BTreeMap<String, Arc<FrameSlot<P::Frame>>> = BTreeMap::new();
    let mut readers: BTreeMap<String, Arc<Routes<P::Frame>>> = BTreeMap::new();
    {
        let mut state = inner.state.lock().unwrap();
        for (id, live) in &state.sources {
            if let Some(video) = &live.video {
                let mut size = (video.width, video.height);
                if filtered.contains(&id.as_str()) {
                    let filters = &setup.source(id).expect("validated").filters;
                    size = crate::filter::output_size(filters, size);
                }
                formats
                    .video
                    .insert(id.clone(), (size.0, size.1, video.fps));
                slots.insert(id.clone(), video.slot.clone());
            }
            if let Some(audio) = &live.audio {
                formats.audio.insert(id.clone(), audio.channels);
                readers.insert(id.clone(), audio.routes.clone());
            }
            if live.run == SourceRun::Failed
                && setup
                    .scenes
                    .iter()
                    .any(|s| s.all_sources().contains(&id.as_str()))
            {
                warnings.push(format!(
                    "{id} isn't running, so the program shows nothing for it: {}",
                    live.error.clone().unwrap_or_default()
                ));
            }
        }
        // A listed audio source that turned out to have no sound (a media
        // file without an audio track) is left out of the files, never
        // failing the take (#75).
        if iso_enabled {
            let mut warned = HashSet::new();
            for id in iso.listed_audio() {
                if !formats.audio.contains_key(id) && warned.insert(id) {
                    let field = iso
                        .audio_field(id)
                        .unwrap_or_else(|| "recording.iso.audio".into());
                    warnings.push(format!(
                        "{id} has no sound (its file has no audio track), so the ISO files \
                         record without it; take it out of {field}"
                    ));
                }
            }
        }
        // Keep the recorded sources as they are until the recording ends.
        if iso_enabled {
            state
                .pins
                .extend(iso.sources.iter().chain(iso.listed_audio()).cloned());
        }
    }
    let first = plan_take(setup, &formats, &folder, 1)?;
    let plans = &first.plans;

    // Check the disk has room before creating anything.
    let rate: f64 = plans.iter().map(|p| space::bytes_per_second(&p.spec)).sum();
    let volume = platform
        .volume(&folder)
        .with_context(|| format!("could not check the disk holding {}", folder.display()))?;
    let free = volume.free_bytes;
    let planned = options
        .duration
        .map_or(space::MIN_OPEN_ENDED_SECONDS, |d| d.as_secs_f64());
    let reserve = (setup.recording.keep_free_gb * 1e9) as u64;
    let needed = rate * planned + reserve as f64;
    if (free as f64) < needed {
        let kept = format!(
            "{} kept free (recording.keep_free_gb = {})",
            space::human(reserve as f64),
            setup.recording.keep_free_gb
        );
        if free <= reserve {
            bail!(
                "not enough free space in {}: {} free, less than the {kept}; free some \
                 space, record somewhere else with --folder, or lower recording.keep_free_gb \
                 (in GB, 0 or more)",
                folder.display(),
                space::human(free as f64),
            );
        }
        let length = match planned < 120.0 {
            true => format!("{planned:.0} s"),
            false => format!("{:.0} minutes", planned / 60.0),
        };
        bail!(
            "not enough free space in {}: {} free, but {length} may need up to {} (at up to \
             {}/minute) plus the {kept}; free some space, record somewhere else with \
             --folder, or record for less time with --seconds",
            folder.display(),
            space::human(free as f64),
            space::human(rate * planned),
            space::human(rate * 60.0),
        );
    }
    if options.duration.is_none() {
        let minutes = (free.saturating_sub(reserve)) as f64 / rate / 60.0;
        if minutes < 60.0 {
            warnings.push(format!(
                "{} free in {} may only last about {minutes:.0} minutes at the highest \
                 likely data rate; the recording stops cleanly if the disk gets full",
                space::human(free as f64),
                folder.display()
            ));
        }
    }

    // Check the disk is fast enough for every file at once.
    if volume.network {
        warnings.push(format!(
            "{} is on a network share ({}); a slow or dropped connection can drop frames \
             or lose the end of the files, so record to a local disk if you can",
            folder.display(),
            volume.file_system
        ));
    }
    let speed = write_speed(inner, &folder)?;
    if speed < rate {
        // One slow test (the disk was busy) mustn't refuse the next start.
        inner.speeds.lock().unwrap().remove(&folder);
        bail!(
            "{} is too slow for this recording: it took {}/s in a test, but the files may \
             need up to {}/s at their highest likely data rate; if the disk was busy, try \
             again, or record to a faster disk with --folder",
            folder.display(),
            space::human(speed),
            space::human(rate)
        );
    }
    if speed < rate * SPEED_MARGIN {
        warnings.push(format!(
            "{} took {}/s in a test, under {SPEED_MARGIN}× the {}/s these files may need; \
             watch for dropped frames",
            folder.display(),
            space::human(speed),
            space::human(rate)
        ));
    }

    // Stopped while getting ready (sources opening, the disk test): no
    // files yet, so nothing to finish.
    if options.stop.load(Ordering::SeqCst) {
        bail!("the recording was stopped before it started, so nothing was recorded");
    }
    // VideoToolbox places sessions on their first picture. Reserve the shared
    // stream/replay encoder before starting the recording writers, even when
    // the live output will only join later (#68). No output configured: no cost.
    let _warm = if setup.stream.is_some() || !setup.recording.replay.is_default() {
        let spec =
            crate::replay::encoder_spec(engine, &setup.recording.replay, setup.stream.as_ref());
        match inner.encoders.warm(engine, &spec, &options.stop) {
            Ok(subscription) => Some(subscription),
            Err(error) => {
                // A missing optional H.264 encoder must not prevent an otherwise
                // valid recording (for example ProRes on a hosted Mac).
                warnings.push(format!(
                    "could not prepare the live encoder before recording: {error:#}"
                ));
                None
            }
        }
    } else {
        None
    };
    // Stream destinations with their own bitrate or size have encoders of
    // their own (#73): place those too, so one starting mid-recording
    // joins a placed session rather than stalling the writers.
    let _warm_destinations =
        crate::stream::warm_destinations(engine, setup, &options.stop, &mut warnings);
    if options.stop.load(Ordering::SeqCst) {
        bail!("the recording was stopped before it started, so nothing was recorded");
    }
    // Each ISO source's frame clock, with the stage its frames go through
    // set up now if its frames need one: that takes a few frames' time.
    let iso_gpu = IsoGpu::default();
    let ycbcr_only = matches!(iso.encoder.codec, Codec::H264 | Codec::Hevc);
    let mut runners: BTreeMap<String, IsoRunner<P>> = BTreeMap::new();
    for plan in plans.iter().filter(|plan| plan.source != PROGRAM) {
        let id = plan.source.as_str();
        if runners.contains_key(id) {
            continue;
        }
        let (width, height, fps) = formats.video[id];
        let mut runner = IsoRunner {
            clock: inner
                .timeline
                .clocks
                .start(format!("iso:{id}"), Duration::from_secs(1) / fps),
            id: id.to_string(),
            slot: slots[id].clone(),
            size: (width, height),
            fps,
            ycbcr_only,
            filters: filtered.contains(&id).then(|| control.filters.clone()),
            stage: None,
            outputs: Vec::new(),
        };
        let newest = runner.slot.newest();
        if runner.filters.is_some()
            || newest.is_some_and(|frame| runner.needs_stage(platform, &frame))
        {
            runner.stage(platform, &iso_gpu)?;
        }
        runners.insert(id.to_string(), runner);
    }
    let writers = create_writers(platform, plans)?;

    // The recording starts on a program frame, at least a moment from now
    // so every file is ready for it.
    let engine_epoch = inner.epoch().expect("the engine clock has started");
    let fps = setup.canvas.fps;
    let after = platform.now() + START_DELAY;
    let epoch = engine_epoch.add_ticks(
        (after.ticks_since(engine_epoch, fps).max(0) + 1) as u64,
        fps,
    );
    let shared = Shared {
        epoch,
        end: AtomicI64::new(i64::MAX),
    };
    let clock = Arc::new(super::pause::RecordingClock::new(
        epoch,
        plans.iter().map(|plan| plan.spec.video.fps),
    ));
    if let Some(duration) = options.duration {
        shared.stop_at(shared.epoch + duration);
    }

    // Route the sources to the files.
    let mut receivers = Vec::new();
    let mut routes = Unroute(Vec::new());
    for plan in plans {
        let (tx, rx) = sync_channel(QUEUE_DEPTH);
        let missed_audio = Arc::new(AtomicU64::new(0));
        if plan.source == PROGRAM {
            let key = inner.program_video.add(tx.clone(), 0, epoch, None);
            routes.0.push((inner.program_video.clone(), key));
        } else if let Some(runner) = runners.get_mut(plan.source.as_str()) {
            runner.outputs.push(tx.clone());
        }
        for (track, id) in plan.audio.iter().enumerate() {
            if id == PROGRAM {
                let key = inner
                    .program_audio
                    .add_recording_audio(tx.clone(), track, epoch);
                routes.0.push((inner.program_audio.clone(), key));
                continue;
            }
            let Some(reader) = readers.get(id) else {
                continue;
            };
            // ISO files that record sources' audio filtered get it through
            // a filter stage of their own, apart from the mix's.
            let source = setup.source(id).expect("validated");
            let filter =
                (iso.filtered && source.audio_filters.iter().any(|f| f.enabled)).then(|| {
                    AudioStage::new(
                        id,
                        formats.audio[id] as usize,
                        control.audio_filters.clone(),
                    )
                });
            // Sparse sources legitimately leave timestamp gaps, so count their
            // actual queue losses instead of treating every silent gap as loss.
            let key = reader.add_counted(
                tx.clone(),
                track,
                epoch,
                filter,
                plan.audio_timing[track]
                    .sparse
                    .then(|| missed_audio.clone()),
                false,
            );
            routes.0.push((reader.clone(), key));
        }
        receivers.push((rx, missed_audio));
    }
    // Frames captured before the recording don't count in its report.
    let mut counted: Vec<(String, Arc<FrameSlot<P::Frame>>)> = plans
        .iter()
        .filter(|p| p.source != PROGRAM)
        .map(|p| (p.source.clone(), slots[&p.source].clone()))
        .collect();
    if program.enabled {
        counted.push((PROGRAM.to_string(), inner.program.clone()));
    }
    let captured_before: Vec<u64> = counted
        .iter()
        .map(|(_, slot)| slot.captured.load(Ordering::Relaxed))
        .collect();

    // Before the start is announced, so a split asked for as soon as it
    // is goes to this recording.
    control.begin_recording(epoch, clock.clone());
    if let Some(duration) = options.duration {
        control.end_recording(epoch + duration);
    }
    // However the recording ends, a split asked for meanwhile is answered.
    let _finish = FinishRecording(control);
    if let Some(on_start) = options.on_start {
        let paths: Vec<PathBuf> = plans.iter().map(|p| p.spec.path.clone()).collect();
        on_start(&paths, &first.manifest_path);
    }
    let started_at = Local::now() + epoch.saturating_since(platform.now());
    let mut takes = vec![Take::begin(first, epoch, started_at)];
    let write = |take: &Take, status, errors: &[String]| {
        write_manifest(
            &take.manifest_path,
            &take_manifest(platform, setup, take, status, errors, Some(control)),
        )
    };
    write(&takes[0], SessionStatus::Recording, &[])?;

    let (done_tx, done_rx) = mpsc::channel::<Done>();
    // Takes reported to `on_take` so far.
    let mut reported = 0;
    let mut errors = thread::scope(|scope| {
        let shared = &shared;
        let mut split_senders = Vec::new();
        let writer_threads: Vec<_> = writers
            .into_iter()
            .zip(receivers)
            .zip(takes[0].plans.clone())
            .enumerate()
            .map(|(index, ((writer, (packets, missed_audio)), plan))| {
                let (split_tx, split_rx) = mpsc::channel();
                split_senders.push(split_tx);
                let done = done_tx.clone();
                let clock = clock.clone();
                scope.spawn(move || {
                    platform.real_time_thread();
                    write_files(
                        writer,
                        plan,
                        index,
                        packets,
                        split_rx,
                        done,
                        shared,
                        clock,
                        missed_audio,
                    )
                })
            })
            .collect();
        drop(done_tx);
        let iso_gpu = &iso_gpu;
        let iso_threads: Vec<_> = runners
            .into_values()
            .map(|runner| scope.spawn(move || run_iso(platform, runner, iso_gpu, shared)))
            .collect();

        let mut all_started = false;
        // A pause's commit barrier under way: its generation, cut and deadline.
        let mut pending_commit: Option<(u64, Time, Time)> = None;
        let disk_probe = DiskProbe::start(platform, &folder);
        let mut disk_meter = DiskMeter::default();
        let mut losses = 0;
        let mut next_status = platform.now() + STATUS_INTERVAL;
        let mut next_space_check = platform.now();
        let mut disk_full = None;
        let stop_below = reserve.max((rate * MIN_FREE_SECONDS).max(MIN_FREE_BYTES) as u64);
        let coordinator = isocast_diagnostics::contain("recording coordinator", || {
            loop {
                let now = platform.now();
                if inner.emergency.load(Ordering::SeqCst)
                    || inner.state.is_poisoned()
                    || inner.scenes.poisoned()
                {
                    shared.stop_at(now.max(shared.epoch));
                    break;
                }
                // Stop cleanly, finishing every file, before the disk is full.
                if now >= next_space_check {
                    next_space_check = now + SPACE_CHECK_INTERVAL;
                    if let Ok(free) = platform.volume(&folder).map(|v| v.free_bytes)
                        && free < stop_below
                        && disk_full.is_none()
                    {
                        disk_full = Some(free);
                        shared.stop_at(now.max(shared.epoch));
                    }
                }
                let current = takes.last().expect("a take");
                if let Some(on_status) = options.on_status
                    && now >= next_status
                {
                    next_status = now + STATUS_INTERVAL;
                    // The loudest of each source since the last status.
                    let levels = std::mem::take(&mut *inner.loudest.lock().unwrap())
                        .into_iter()
                        .map(|(source, (peak_db, rms_db))| AudioLevel {
                            source,
                            peak_db,
                            rms_db,
                            channels: None,
                        })
                        .collect();
                    let mut status = live_status(&current.plans, levels, shared, now, &clock);
                    status.monitor = inner.monitor.lock().unwrap().status();
                    // While frames or audio are being lost, see whether the
                    // disk is why.
                    let lost = status
                        .files
                        .iter()
                        .map(|f| f.dropped_frames + f.audio_dropouts)
                        .sum();
                    if lost > losses {
                        disk_probe.want();
                    }
                    losses = lost;
                    status.disk = Some(disk_meter.sample(&current.plans, &disk_probe, now));
                    on_status(&status);
                }
                // Markers and scene switches go into the manifest right away.
                // A pause of crash-safe files first has every file commit what
                // it holds up to the cut, so its checkpoint states a fact:
                // the saved timeline up to which every file is on disk.
                let checkpoint = |generation: u64, errors: &[String], durable: bool| {
                    let result = if durable {
                        write_manifest_durable(
                            &current.manifest_path,
                            &take_manifest(
                                platform,
                                setup,
                                current,
                                SessionStatus::Recording,
                                errors,
                                Some(control),
                            ),
                        )
                    } else {
                        write(current, SessionStatus::Recording, errors)
                    };
                    let mut state = control.state.lock().unwrap();
                    state.manifest_error = result.err().map(|e| format!("{e:#}"));
                    if state.manifest_error.is_none() {
                        state.saved_pause_generation = generation;
                    }
                };
                if let Some(generation) = control.take_changed() {
                    let barrier = clock.open_pause().filter(|_| {
                        pending_commit.is_none()
                            && current
                                .plans
                                .iter()
                                .any(|p| p.crash_safe.load(Ordering::Acquire))
                    });
                    match barrier {
                        Some(at) => {
                            for plan in &current.plans {
                                if plan.crash_safe.load(Ordering::Acquire) {
                                    plan.commit_at.store(at.as_nanos(), Ordering::Relaxed);
                                    plan.commit_requested.store(generation, Ordering::Release);
                                }
                            }
                            pending_commit = Some((generation, at, now + COMMIT_TIMEOUT));
                        }
                        None => checkpoint(generation, &[], false),
                    }
                }
                if let Some((generation, at, deadline)) = pending_commit {
                    let committed: Vec<&Arc<FilePlan>> = current
                        .plans
                        .iter()
                        .filter(|p| p.crash_safe.load(Ordering::Acquire))
                        .collect();
                    let all = committed
                        .iter()
                        .all(|p| p.commit_done.load(Ordering::Acquire) >= generation);
                    // A file whose commit failed answers at once, with its
                    // error; one that never answers is reported at the
                    // deadline. Neither leaves this pause with a number.
                    let mut errors: Vec<String> = committed
                        .iter()
                        .filter(|p| p.commit_failed.load(Ordering::Acquire) >= generation)
                        .filter_map(|p| p.commit_error.lock().unwrap().clone())
                        .collect();
                    let failed = !errors.is_empty();
                    if all || failed || now >= deadline || shared.end() != Time::MAX {
                        if !all && !failed {
                            for plan in committed
                                .iter()
                                .filter(|p| p.commit_done.load(Ordering::Acquire) < generation)
                            {
                                errors.push(format!(
                                    "{}: did not confirm its commit within {} s of the pause; \
                                     nothing durable is promised for it at this pause",
                                    plan.spec.path.display(),
                                    COMMIT_TIMEOUT.as_secs()
                                ));
                            }
                        }
                        let recoverable = if all && !failed {
                            let cuts = clock.state.read().unwrap();
                            committed
                                .iter()
                                .map(|p| p.recoverable(current.epoch, &cuts))
                                .try_fold(f64::INFINITY, |min, r| r.map(|r| min.min(r)))
                                .filter(|r| r.is_finite())
                        } else {
                            None
                        };
                        clock.set_recoverable(at, recoverable);
                        control.state.lock().unwrap().pause_outcome =
                            Some((generation, recoverable, errors.clone()));
                        checkpoint(generation, &errors, true);
                        pending_commit = None;
                    }
                }
                if !all_started
                    && current
                        .plans
                        .iter()
                        .all(|p| p.start.load(Ordering::SeqCst) != i64::MIN)
                {
                    all_started = true;
                    // Best effort: the final manifest is written at the end.
                    let _ = write(current, SessionStatus::Recording, &[]);
                }
                // A stop asked for comes before a split asked for at the same
                // time: the split is then refused, rather than announcing a take
                // the stop ends before it begins.
                if options.stop.load(Ordering::SeqCst) {
                    shared.stop_at(now.max(shared.epoch + MIN_TAKE));
                }
                // A split: the next take's files start at a moment on every
                // file's frame grid (the next whole second on the recording's
                // clock, or the soonest such moment: see `SplitTiming`), at least
                // a second after the current take's start.
                if let Some((timing, reply)) = control.take_split() {
                    let number = takes.len() as u32 + 1;
                    let next = plan_take(setup, &formats, &folder, number)
                        .and_then(|plan| Ok((create_writers(platform, &plan.plans)?, plan)))
                        .map_err(|err| SplitRefused {
                            ending: false,
                            message: format!("{err:#}"),
                        })
                        .and_then(|(writers, plan)| {
                            // Timed once the files exist, so none of them misses
                            // the start.
                            let now = platform.now();
                            let current = takes.last().expect("a take");
                            let rates: Vec<u32> = current
                                .plans
                                .iter()
                                .chain(&plan.plans)
                                .map(|plan| plan.spec.video.fps)
                                .collect();
                            let at = split_time(shared.epoch, current.epoch, &rates, timing, now);
                            if at >= shared.end() {
                                writers.into_iter().for_each(FileWriter::cancel);
                                return Err(SplitRefused {
                                    ending: true,
                                    message: "the recording ends before a new take could start"
                                        .into(),
                                });
                            }
                            Ok((plan, writers, at, now))
                        });
                    match next {
                        Ok((plan, writers, at, now)) => {
                            let files: Vec<PathBuf> =
                                plan.plans.iter().map(|p| p.spec.path.clone()).collect();
                            let manifest = plan.manifest_path.clone();
                            let index = takes.len();
                            let mut gone = Vec::new();
                            for (i, ((split, writer), file)) in split_senders
                                .iter()
                                .zip(writers)
                                .zip(plan.plans.clone())
                                .enumerate()
                            {
                                let sent = split.send(Split {
                                    at,
                                    take: index,
                                    writer,
                                    plan: file,
                                });
                                // That file's writer thread has stopped (its
                                // writer failed), so this take can't have it.
                                if let Err(mpsc::SendError(split)) = sent {
                                    split.writer.cancel();
                                    gone.push(i);
                                }
                            }
                            let started_at = Local::now() + at.saturating_since(now);
                            takes.last_mut().expect("a take").end = Some(at);
                            let mut take = Take::begin(plan, at, started_at);
                            for i in gone {
                                take.results[i] = Some(Err(anyhow::anyhow!(
                                    "{}: not recorded, because this source's file stopped in the \
                                     previous take",
                                    take.plans[i].spec.path.display()
                                )));
                            }
                            takes.push(take);
                            control.begin_take(at);
                            let _ =
                                write(takes.last().expect("a take"), SessionStatus::Recording, &[]);
                            all_started = false;
                            let _ = reply.send(Ok(SplitTake {
                                take: number,
                                files,
                                manifest,
                                begins_at: started_at,
                                begins_in: at.saturating_since(now),
                            }));
                        }
                        Err(refused) => {
                            control.state.lock().unwrap().splitting = false;
                            let _ = reply.send(Err(refused));
                        }
                    }
                }
                // Finished files of earlier takes; a take is done when all are.
                for done in done_rx.try_iter() {
                    takes[done.take].results[done.index] = Some(done.result);
                }
                let earlier = takes.len() - 1;
                for take in &mut takes[..earlier] {
                    if !take.finalized && take.results.iter().all(Option::is_some) {
                        take.finalized = true;
                        let mut errors = take.errors();
                        if take.plans.iter().any(|p| p.source == PROGRAM)
                            && let Some(reason) = inner.program.stopped()
                        {
                            errors.push(format!("program: {reason}; program output failed"));
                        }
                        let manifest = take_manifest(
                            platform,
                            setup,
                            take,
                            take.status(&errors),
                            &errors,
                            Some(control),
                        );
                        let _ = write_manifest(&take.manifest_path, &manifest);
                        take.manifest = Some(manifest);
                    }
                }
                // Finished takes, in order. One whose split comes after the
                // recording's end isn't: the end rewrites it.
                while reported < earlier
                    && takes[reported].finalized
                    && takes[reported].end.is_some_and(|end| end <= shared.end())
                {
                    let take = &takes[reported];
                    if let (Some(on_take), Some(manifest)) = (options.on_take, &take.manifest) {
                        on_take(reported as u32 + 1, &take.manifest_path, manifest);
                    }
                    reported += 1;
                }
                // Stop when asked (a stop right after the start still gives
                // every announced file a moment; checked again here for one
                // asked for since the split above), or when every file has
                // stopped (for example because the disk is full).
                if options.stop.load(Ordering::SeqCst) {
                    shared.stop_at(now.max(shared.epoch + MIN_TAKE));
                }
                if writer_threads.iter().all(|t| t.is_finished()) {
                    shared.stop_at(now.max(shared.epoch));
                }
                // Markers and switches after the end are refused from now on
                // (the files take a moment longer to finish).
                let end = shared.end();
                if end != Time::MAX {
                    control.end_recording(end);
                }
                if now >= end {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        if coordinator.is_err() {
            shared.stop_at(platform.now().max(shared.epoch));
        }
        // No take begins from here on: a split asked for is refused at once.
        control.finish_recording();

        // Only Stop drains the live mix early, and only through the last
        // frame boundary a file needs. Include the active take when a split
        // is pending (the newest take may never have received a frame).
        if let Some(until) = takes
            .iter()
            .flat_map(|take| &take.plans)
            .filter(|plan| plan.audio.iter().any(|source| source == PROGRAM))
            .filter_map(|plan| {
                let start = plan.start.load(Ordering::SeqCst);
                (start != i64::MIN)
                    .then(|| file_end(Time::from_nanos(start), shared.end(), plan.spec.video.fps))
            })
            .max()
        {
            control
                .mix_drain_until
                .store(until.as_nanos(), Ordering::Release);
        }
        // Files finish themselves once their last frame and audio arrive.
        // Keep the full capture/mixer allowance for missing or late inputs,
        // but don't impose it on files that have already drained and closed.
        let deadline = shared.end() + MIX_DELAY.max(AUDIO_TAIL) + Duration::from_millis(100);
        while writer_threads.iter().any(|t| !t.is_finished()) && platform.now() < deadline {
            thread::sleep(Duration::from_millis(2));
        }
        drop(std::mem::replace(&mut routes, Unroute(Vec::new())));
        let mut errors = coordinator.err().into_iter().collect::<Vec<_>>();
        if inner.emergency.load(Ordering::SeqCst)
            || inner.state.is_poisoned()
            || inner.scenes.poisoned()
        {
            errors.push(
                "recording interrupted: poisoned shared state; writers finalized by emergency stop"
                    .into(),
            );
        }
        for thread in iso_threads {
            match thread.join() {
                Ok(Ok(())) => {}
                Ok(Err(err)) => errors.push(format!("{err:#}")),
                Err(_) => errors.push("ISO clock panicked; remaining writers finalized".into()),
            }
        }
        for thread in writer_threads {
            if thread.join().is_err() {
                errors.push("writer panicked while finalizing; inspect recovered fragments".into());
            }
        }
        control.mix_drain_until.store(0, Ordering::Release);
        if let Some(free) = disk_full {
            errors.push(format!(
                "stopped early because only {} was left free in {}; the files were \
                 finished and are playable",
                space::human(free as f64),
                folder.display()
            ));
        }
        errors
    });

    for done in done_rx.try_iter() {
        takes[done.take].results[done.index] = Some(done.result);
    }
    let end = shared.end();
    // A take the recording ended before it began left no files; the take
    // before it ran to the end.
    while takes.len() > 1 && takes.last().expect("a take").epoch >= end {
        let take = takes.pop().expect("a take");
        let _ = std::fs::remove_file(&take.manifest_path);
    }
    takes.last_mut().expect("a take").end = Some(end);
    let last = takes.len() - 1;
    let mut files = Vec::new();
    let mut manifests = Vec::new();
    let mut file_errors = Vec::new();
    for (i, take) in takes.iter_mut().enumerate() {
        let mut take_errors = take.errors();
        if !take.finalized
            && take.plans.iter().any(|p| p.source == PROGRAM)
            && let Some(reason) = inner.program.stopped()
        {
            take_errors.push(format!("program: {reason}; program output failed"));
        }
        file_errors.extend(take_errors.iter().cloned());
        if i == last {
            take_errors.extend(errors.iter().cloned());
        }
        // A manifest that can't be written (the disk is full) doesn't lose
        // the report: the files it would list are finished all the same.
        if i == last || !take.finalized {
            let manifest = take_manifest(
                platform,
                setup,
                take,
                take.status(&take_errors),
                &take_errors,
                Some(control),
            );
            if let Err(err) = write_manifest(&take.manifest_path, &manifest) {
                file_errors.push(format!("{err:#}"));
            }
            take.manifest = Some(manifest);
        }
        manifests.push(take.manifest_path.clone());
        files.extend(take.results.iter_mut().filter_map(|r| r.take()?.ok()));
    }
    errors.extend(file_errors);
    for file in &files {
        if file.dropped_frames > 0 {
            // Tail repeats are stored frames, but do not replace a missing
            // original picture in the expected-frame denominator.
            let expected = file
                .frames
                .saturating_sub(file.repeated_frames)
                .saturating_add(file.dropped_frames);
            warnings.push(format!(
                "{} take {}: {} {} dropped ({}); {}",
                file.source,
                file.take,
                thousands(file.dropped_frames),
                if file.dropped_frames == 1 {
                    "frame"
                } else {
                    "frames"
                },
                drop_percent(file.dropped_frames, expected),
                file.path.display()
            ));
        }
    }
    if let Some(on_take) = options.on_take {
        for (i, take) in takes.iter().enumerate().skip(reported) {
            if let Some(manifest) = &take.manifest {
                on_take(i as u32 + 1, &take.manifest_path, manifest);
            }
        }
    }

    Ok(Report {
        folder,
        disk: DiskReport {
            file_system: volume.file_system,
            network: volume.network,
            free_bytes: free,
            write_bytes_per_second: speed,
            estimated_bytes_per_second: rate,
        },
        manifest: manifests.last().cloned().unwrap_or_default(),
        takes: manifests,
        duration_seconds: clock
            .state
            .read()
            .unwrap()
            .map(shared.end())
            .saturating_since(shared.epoch)
            .as_secs_f64(),
        files,
        captures: counted
            .into_iter()
            .zip(captured_before)
            .map(|((source, slot), before)| CaptureReport {
                source,
                frames_captured: slot.captured.load(Ordering::Relaxed) - before,
            })
            .collect(),
        warnings,
        errors,
    })
}

/// The write speed of the disk holding `folder`, measured at most every
/// [`SPEED_TTL`].
fn write_speed<P: Platform>(inner: &Inner<P>, folder: &Path) -> Result<f64> {
    let now = inner.platform.now();
    if let Some((at, speed)) = inner.speeds.lock().unwrap().get(folder)
        && now.saturating_since(*at) < SPEED_TTL
    {
        return Ok(*speed);
    }
    let speed = space::write_speed(folder)
        .with_context(|| format!("could not write a test file in {}", folder.display()))?;
    inner
        .speeds
        .lock()
        .unwrap()
        .insert(folder.to_path_buf(), (now, speed));
    Ok(speed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soonest_splits_fall_exactly_on_every_files_frames() {
        // An epoch that isn't a whole number of frames from anything.
        let epoch = Time::from_nanos(123_456_789_017);
        let ms = |ms: u64| Duration::from_millis(ms);
        let rates = [60, 30, 60];
        // A take that began on the epoch, 2.37 s ago.
        let now = epoch + ms(2_370);
        let at = split_time(epoch, epoch, &rates, SplitTiming::Soonest, now);
        // The next tenth of a second (every 60 and 30 fps frame grid,
        // in whole nanoseconds) at least a quarter second away.
        assert_eq!(at, epoch + ms(2_700));
        for fps in [60, 30] {
            let frames = at.ticks_since(epoch, fps) as u64;
            assert_eq!(epoch.add_ticks(frames, fps), at, "{fps} fps");
            // A take from there to a whole second is whole frames long.
            let end = epoch + Duration::from_secs(4);
            let length = super::super::file::file_end(at, end, fps);
            assert_eq!(length, end, "{fps} fps");
        }
        // A take lasts at least a second: one that only just began.
        let current = epoch + ms(2_700);
        let again = split_time(
            epoch,
            current,
            &rates,
            SplitTiming::Soonest,
            epoch + ms(2_800),
        );
        assert_eq!(again, current + Duration::from_secs(1));
        // `split` keeps whole seconds at least half a second away.
        let whole = split_time(epoch, epoch, &rates, SplitTiming::WholeSecond, now);
        assert_eq!(whole, epoch + Duration::from_secs(3));
        // 24 fps: every 1/8 s.
        let film = split_time(epoch, epoch, &[24], SplitTiming::Soonest, now);
        assert_eq!(film, epoch + ms(2_625));
    }

    #[test]
    fn monitors_get_every_frame_of_the_grids_they_ask_for() {
        let at = |ms: i64| Time::from_nanos(ms * 1_000_000);
        let drawn = |rates: &Rates| (0..12).filter(|&n| rates.draws(n, 60)).collect::<Vec<_>>();
        let mut rates = Rates::default();
        rates.ask(0, 60, at(0));
        assert!(!rates.any(at(0)));
        assert_eq!(drawn(&rates), Vec::<u64>::new());
        rates.ask(30, 60, at(0));
        assert_eq!(drawn(&rates), [0, 2, 4, 6, 8, 10]);
        // A 20 fps monitor beside it gets its own grid too.
        rates.ask(20, 60, at(10));
        assert_eq!(drawn(&rates), [0, 2, 3, 4, 6, 8, 9, 10]);
        // A rate that doesn't divide the canvas's gets a little more; one
        // above it gets every frame.
        let mut odd = Rates::default();
        odd.ask(24, 60, at(0));
        assert_eq!(drawn(&odd), [0, 2, 4, 6, 8, 10]);
        odd.ask(120, 60, at(0));
        assert_eq!(drawn(&odd).len(), 12);
        // Each rate lasts until PREVIEW_HOLD after it was last asked for.
        let hold = PREVIEW_HOLD.as_millis() as i64;
        rates.ask(30, 60, at(1000));
        rates.ask(0, 60, at(hold + 20));
        assert_eq!(drawn(&rates), [0, 2, 4, 6, 8, 10]);
        rates.ask(0, 60, at(hold + 1000));
        assert!(!rates.any(at(hold + 1000)));
    }

    #[test]
    fn a_recording_drain_is_not_delivered_to_live_taps_or_duplicated() {
        let routes: Routes<u32> = Routes::default();
        let at = Time::from_nanos(0);
        let (recording, recorded) = sync_channel(2);
        let (stream, streamed) = sync_channel(2);
        routes.add_recording_audio(recording, 0, at);
        let live = routes.add(stream, 0, at, None);
        let samples: Arc<[f32]> = vec![0.25; 960].into();

        assert!(routes.audio_recordings(&samples, at));
        assert!(matches!(recorded.try_recv(), Ok(Packet::Audio { .. })));
        assert!(streamed.try_recv().is_err());
        routes.audio_live(&samples, at);
        assert!(matches!(streamed.try_recv(), Ok(Packet::Audio { .. })));
        assert!(recorded.try_recv().is_err());

        routes.audio(&samples, at);
        assert!(recorded.try_recv().is_ok());
        assert!(streamed.try_recv().is_ok());
        routes.remove(live);
        assert!(!routes.audio_recordings(&samples, at));
    }

    #[test]
    fn frames_an_output_misses_are_counted() {
        let routes: Routes<u32> = Routes::default();
        let (tx, rx) = sync_channel(2);
        let missed = Arc::new(AtomicU64::new(0));
        routes.add_counted(
            tx,
            0,
            Time::from_nanos(0),
            None,
            Some(missed.clone()),
            false,
        );
        // Uncounted outputs miss frames quietly, as before.
        let (quiet, _quiet_rx) = sync_channel(1);
        routes.add(quiet, 0, Time::from_nanos(0), None);
        for n in 0..5 {
            routes.video(&n, Time::from_nanos(i64::from(n)));
        }
        assert_eq!(missed.load(Ordering::Relaxed), 3);
        assert_eq!(rx.try_iter().count(), 2);
        // Frames from before an output's start aren't missed.
        routes.video(&9, Time::from_nanos(-1));
        assert_eq!(missed.load(Ordering::Relaxed), 3);
        // Audio taps use the same non-blocking route and count each chunk.
        let samples: Arc<[f32]> = vec![0.0; 960].into();
        for _ in 0..3 {
            routes.audio(&samples, Time::from_nanos(0));
        }
        assert_eq!(missed.load(Ordering::Relaxed), 4);
        assert_eq!(rx.try_iter().count(), 2);
    }
}
