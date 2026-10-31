//! The live engine: a setup's sources, the program and the mix running all
//! the time, as in OBS, with recordings and other outputs (a stream, the
//! app's preview) attaching and detaching while it runs.
//!
//! Sources open in the background as the setup changes; layout, scene and
//! filter changes show at the next program frame. A recording keeps the
//! sources it records (and the canvas) as they were when it started, and
//! changes to them apply once it ends.

use parking_lot::{Condvar, Mutex};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::Local;
use isocast_protocol::api::{
    AudioLevel, ChannelLevel, EngineInfo, ScenesInfo, SourceRun, SourceStatus,
};
use isocast_protocol::manifest::SessionStatus;
use isocast_protocol::{
    Canvas, CanvasId, Codec, Monitoring, PROGRAM, Setup, SourceKind, Transition, TransitionKind,
};

use super::disk::{DiskMeter, DiskProbe};
use super::file::{Done, IsoDemand, Split, file_end, split_channel, write_files};
use super::frame_stream::{self, FrameFeed};
use super::iso_frames::IsoFrames;
use super::mix::{MIX_DELAY, Meter, Mixer, StereoMeter};
use super::monitor::{Closing, Monitor, MonitorSlot, Tap};
use super::program::{Compositor, Feed};
use super::scene_feeds::{self, SceneFeeds};
use super::scenes::{SceneInput, SceneThread};
use super::shared_frames::{self, SharedFeed};
use super::timings::CompositorTimings;
use super::{
    AUDIO_TAIL, COMMIT_TIMEOUT, CaptureReport, DiskReport, FIRST_FRAME_TIMEOUT, FilePlan,
    FolderLock, Formats, MIN_FREE_BYTES, MIN_FREE_SECONDS, MIN_TAKE, MIX_QUEUE_DEPTH, MONITOR_FPS,
    NoPicture, OnAir, Output, PROGRAM_WARM_TIMEOUT, Packet, QUEUE_DEPTH, RecordOptions, Report,
    SPACE_CHECK_INTERVAL, SPEED_MARGIN, SPLIT_LEAD, STALL_AFTER, START_DELAY, STATUS_INTERVAL,
    SessionControl, Shared, Slots, SourceFilters, SplitRefused, SplitTake, Take, WriterInputs,
    create_writers, expand_home, interrupted_note, interrupted_sessions, is_program, live_status,
    next_recording, plan_take, sidechain_sources, take_manifest, wait_for_first_picture,
    wait_for_frame,
};
use super::{AudioStopped, audio_stop_errors, file_audio_stops};
#[cfg(not(windows))]
use super::{write_manifest, write_manifest_durable};
use crate::audio_filter::AudioStage;
use crate::critical::Critical;
use crate::gpu::{Completion, Gpu, PendingFrame};
use crate::source::{self, Audio, AudioFeed, FrameSlot, Timeline, Video};
use crate::transition::stinger_id;
use crate::{FileWriter, Platform, SAMPLE_RATE, Time, space};
#[cfg(windows)]
use isocast_protocol::manifest::SessionManifest;

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
        /// Shared control state may be half-updated: recordings must be
        /// finalized by the emergency path.
        broken: bool,
    },
}

/// Called with what happens in the engine, from its threads.
pub type OnEngineEvent = Arc<dyn Fn(EngineEvent) + Send + Sync>;

/// Where packets from one producer (a source's audio, the program's frames,
/// the mix) go: outputs that come and go while it runs.
pub(crate) struct Routes<F> {
    list: Critical<Vec<Route<F>>>,
    next: AtomicU64,
}

struct Route<F> {
    key: u64,
    output: super::tap::Output<F>,
    track: usize,
    /// Packets from before this are left out (audio from a little before,
    /// so the first chunk can be trimmed to the start).
    from: Time,
    /// Audio filters this output's copy goes through.
    filter: Option<AudioStage>,
    /// Counts packets the output missed because it had fallen behind, for
    /// outputs that report them.
    missed: Option<Arc<AtomicU64>>,
    gap: Option<source::GapCallback>,
    /// A program-mix recording may receive its final audio early at Stop;
    /// live taps must keep their ordinary delivery schedule.
    recording_audio: bool,
}

impl<F> Default for Routes<F> {
    fn default() -> Self {
        Self {
            list: Critical::new(Vec::new()),
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
        self.add_counted(output.into(), track, from, filter, None, false)
    }

    /// As [`Routes::add`], counting in `missed` the packets the output
    /// misses because its queue is full.
    fn add_counted(
        &self,
        output: super::tap::Output<F>,
        track: usize,
        from: Time,
        filter: Option<AudioStage>,
        missed: Option<Arc<AtomicU64>>,
        recording_audio: bool,
    ) -> u64 {
        self.add_audio_route(output, track, from, filter, (missed, None), recording_audio)
    }

    fn add_audio_route(
        &self,
        output: super::tap::Output<F>,
        track: usize,
        from: Time,
        filter: Option<AudioStage>,
        (missed, gap): (Option<Arc<AtomicU64>>, Option<source::GapCallback>),
        recording_audio: bool,
    ) -> u64 {
        let key = self.next.fetch_add(1, Ordering::Relaxed);
        self.list.lock().push(Route {
            key,
            output,
            track,
            from,
            filter,
            missed,
            gap,
            recording_audio,
        });
        key
    }

    pub(super) fn add_file_audio(
        &self,
        output: Output<F>,
        track: usize,
        from: Time,
        filter: Option<AudioStage>,
        missed: Arc<AtomicU64>,
        plan: &FilePlan,
    ) -> u64
    where
        F: Send + 'static,
    {
        let tx = output.clone();
        let overflow = missed.clone();
        let gap: source::GapCallback = Box::new(move |until, duration| {
            // If even this marker cannot fit, retain its loss via the
            // writer's existing queue-overflow counter.
            if matches!(
                tx.try_send(Packet::AudioGap {
                    from: until - duration,
                    until
                }),
                Err(TrySendError::Full(_))
            ) {
                overflow.fetch_add(1, Ordering::Relaxed);
            }
        });
        // Regular queue loss is already detected by continuous tracks.
        // Sparse tracks count explicit losses, never silence itself.
        self.add_audio_route(
            output.into(),
            track,
            from,
            filter,
            (plan.audio_timing[track].sparse.then_some(missed), Some(gap)),
            plan.audio[track] == PROGRAM,
        )
    }

    /// Loss follows the same active audio routes as the samples, but does
    /// not depend on silence detection or space in the mixer's input queue.
    pub(super) fn source_gap(&self, until: Time, duration: Duration) {
        for route in self.list.read().iter() {
            if until > route.from
                && let Some(report) = &route.gap
            {
                report(until, duration.min(until.saturating_since(route.from)));
            }
        }
    }

    pub(crate) fn remove(&self, key: u64) {
        self.list.lock().retain(|route| route.key != key);
    }

    pub(super) fn add_mix_input(
        &self,
        output: Output<F>,
        track: usize,
        mixed: bool,
        id: &str,
        control: Arc<SessionControl>,
        program: Arc<Routes<F>>,
    ) -> u64
    where
        F: Send + 'static,
    {
        let gap = mixed.then(|| {
            let id = id.to_owned();
            Box::new(move |until, duration| {
                let settings = control.audio_settings(&id).unwrap_or_default();
                if !settings.muted && settings.in_program() {
                    program.source_gap(until, duration);
                }
            }) as source::GapCallback
        });
        self.add_audio_route(
            output.into(),
            track,
            Time::from_nanos(0),
            None,
            (None, gap),
            false,
        )
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.list.read().is_empty()
    }

    fn has_recording(&self) -> bool {
        self.list
            .read()
            .iter()
            .any(|route| route.output.is_recording())
    }

    /// Drop every output, so their receivers see the producer is gone.
    fn clear(&self) {
        self.list.lock().clear();
    }

    /// Send a frame to every output. One that has fallen behind misses it
    /// rather than holding up the others.
    #[cfg(test)]
    pub(crate) fn video(&self, frame: &F, pts: Time, canvas: Option<CanvasId>) {
        self.video_retained(frame, pts, canvas, |frame, _| Some(frame.clone()));
    }

    pub(crate) fn video_retained(
        &self,
        frame: &F,
        pts: Time,
        canvas: Option<CanvasId>,
        retain: fn(&F, crate::FrameUse) -> Option<F>,
    ) {
        for route in self.list.read().iter() {
            if pts >= route.from {
                let sent = route.output.video(frame, pts, retain);
                if matches!(&sent, Err(TrySendError::Full(_)))
                    && route.output.is_recording()
                    && let Some(canvas) = canvas
                {
                    crate::governor::protected_loss(canvas, 1);
                }
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
        for route in self.list.lock().iter_mut() {
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
    pub packets: super::tap::Packets<F>,
    /// Old program frames evicted because the reader fell behind.
    pub missed_frames: Arc<AtomicU64>,
    /// Mix chunks lost to a full tap, counted even while its encoder stalls.
    pub missed_audio_chunks: Arc<AtomicU64>,
    video: Arc<Routes<F>>,
    audio: Arc<Routes<F>>,
    keys: (Option<u64>, u64),
}

impl<F: Clone> Drop for ProgramTap<F> {
    fn drop(&mut self) {
        if let Some(key) = self.keys.0 {
            self.video.remove(key);
        }
        self.audio.remove(self.keys.1);
    }
}

/// A monitor-only mix. Its raw input routes and DSP state never belong to
/// the recording mixer, and its queues cannot backpressure those routes.
pub(crate) struct MonitorAudio<F: Clone> {
    pub packets: mpsc::Receiver<Packet<F>>,
    stop: Arc<AtomicBool>,
    routes: Arc<Mutex<Vec<MonitorRoute<F>>>>,
    thread: Option<JoinHandle<()>>,
}

struct MonitorRoute<F> {
    id: String,
    source: Arc<Routes<F>>,
    key: u64,
}

impl<F: Clone> Drop for MonitorAudio<F> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for route in self.routes.lock().drain(..) {
            route.source.remove(route.key);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A way to re-tap the program without keeping a route (and the compositor)
/// active. This keeps the inner state, not the Engine: dropping the Engine
/// still stops it, which a parked encoder can observe without a receiver.
#[derive(Clone)]
pub(crate) struct ProgramTapSource<P: Platform>(Arc<Inner<P>>, CanvasId);

impl<P: Platform> ProgramTapSource<P> {
    pub(crate) fn stopped(&self) -> bool {
        self.0.stopping.load(Ordering::SeqCst)
    }

    pub(crate) fn tap(
        &self,
        depth: usize,
        missed_audio_chunks: Arc<AtomicU64>,
    ) -> Result<ProgramTap<P::Frame>> {
        self.tap_inner(depth, missed_audio_chunks, true)
    }

    pub(crate) fn monitor_audio(&self, depth: usize) -> Result<MonitorAudio<P::Frame>> {
        let inner = &self.0;
        let state = inner.state.read();
        let program = state
            .mixer
            .as_ref()
            .context("program audio mixer is not running")?;
        let epoch = program.shared.epoch;
        let from = inner.platform.now().ticks_since(epoch, SAMPLE_RATE).max(0);
        let setup = state.setup.clone();
        let mut revision = state.revision;
        let inputs = program.inputs.clone();
        let program_shared = program.shared.clone();
        drop(state);

        // Independent definitions/reports as well as independent filter instances:
        // a monitor plugin failure must not change the recording chain's state.
        let filters = Arc::new(crate::audio_filter::LiveAudioFilters::default());
        filters.set(&setup);
        let settings = |id: &str| {
            setup
                .source(id)
                .map(|source| source.audio)
                .unwrap_or_default()
        };
        let mixed = inputs
            .iter()
            .filter(|(_, _, mixed)| *mixed)
            .map(|(id, channels, _)| (id.clone(), *channels, settings(id)))
            .collect();
        // Same bounded capture-burst reserve as the program mixer; this
        // optional copy never waits on it and never changes the record queue.
        let (input, received) = sync_channel(MIX_QUEUE_DEPTH);
        let (output, packets) = sync_channel(depth);
        let mut mixer = Mixer::new(mixed, setup.audio.pan_law, received, Arc::default());
        for (id, channels, _) in inputs.iter().filter(|(_, _, mixed)| !*mixed) {
            mixer.add_key(id.clone(), *channels, settings(id));
        }
        mixer.set_filters(filters.clone());
        let missed = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let mut tap = MonitorAudio {
            packets,
            stop: stop.clone(),
            routes: Arc::default(),
            thread: None,
        };
        // Filter creation happens outside engine state. Revalidate and bind
        // under the same read guard, so a source reopen cannot race the adds.
        let state = inner.state.read();
        if !state
            .mixer
            .as_ref()
            .is_some_and(|mixer| Arc::ptr_eq(&mixer.shared, &program_shared))
        {
            bail!("program audio changed while opening monitor; reoffer");
        }
        for (track, (id, ..)) in inputs.iter().enumerate() {
            let source = state
                .sources
                .get(id)
                .and_then(|source| source.audio.as_ref())
                .context("program audio source is unavailable")?
                .routes
                .clone();
            let key = source.add_counted(
                // A bounded raw-audio channel, not a file writer. This
                // variant's is_recording() is true, but only program VIDEO
                // routes use that test; recording_audio stays false here.
                super::tap::Output::Recording(input.clone()),
                track,
                Time::from_nanos(0),
                None,
                Some(missed.clone()),
                false,
            );
            tap.routes.lock().push(MonitorRoute {
                id: id.clone(),
                source,
                key,
            });
        }
        drop(state);
        drop(input);
        let routes = tap.routes.clone();
        let inner = inner.clone();
        tap.thread = Some(thread::Builder::new().name("monitor-mix".into()).spawn(move || {
            // Deliberately not platform.audio_thread(): this optional copy must
            // not compete at the recording mix's audio scheduling priority.
            let mut refreshed = std::time::Instant::now();
            let mut warned = false;
            let alive = || {
                if stop.load(Ordering::SeqCst) || inner.stopping.load(Ordering::SeqCst)
                    || program_shared.end() != Time::MAX {
                    return false;
                }
                if !warned && missed.load(Ordering::Relaxed) != 0 {
                    warned = true;
                    isocast_diagnostics::diagnostic!("warn"; "WHEP monitor input queue overflow; monitor audio may contain gaps");
                }
                if refreshed.elapsed() >= Duration::from_millis(100) {
                    refreshed = std::time::Instant::now();
                    // Never wait for a setup change while holding the monitor.
                    let Some(state) = inner.state.try_read() else { return true };
                    {
                        let routes = routes.lock();
                        if stop.load(Ordering::SeqCst) { return false; }
                        for route in routes.iter() {
                            let Some(audio) = state.sources.get(&route.id).and_then(|source| source.audio.as_ref()) else {
                                return false;
                            };
                            if !Arc::ptr_eq(&audio.routes, &route.source) { return false; }
                        }
                    }
                    let changed = (revision != state.revision).then(|| state.setup.clone());
                    revision = state.revision;
                    drop(state);
                    if let Some(setup) = changed { filters.set(&setup); }
                }
                true
            };
            if let Err(message) = isocast_diagnostics::contain("WHEP monitor mix", || {
                mixer.run_monitor(&inner.platform, epoch, from, &inner.control, output, alive);
            }) {
                isocast_diagnostics::diagnostic!("warn"; "WHEP monitor mix ended: {message}");
            }
        })?);
        Ok(tap)
    }

    fn tap_inner(
        &self,
        depth: usize,
        missed_audio_chunks: Arc<AtomicU64>,
        video: bool,
    ) -> Result<ProgramTap<P::Frame>> {
        let inner = &self.0;
        // Serialize acquiring the canvas with update's refusal check.
        let _state = inner.state.lock();
        let output = inner
            .output(self.1)
            .ok_or_else(|| anyhow::anyhow!("{}", canvas_off(self.1)))?;
        let from = inner.platform.now();
        let missed_frames = Arc::new(AtomicU64::new(0));
        let (tx, packets) = super::tap::channel(depth, missed_frames.clone());
        let keys = (
            video.then(|| {
                output.program_video.add_counted(
                    super::tap::Output::Live(tx.clone()),
                    0,
                    from,
                    None,
                    Some(missed_frames.clone()),
                    false,
                )
            }),
            inner.program_audio.add_counted(
                super::tap::Output::Live(tx),
                0,
                from,
                None,
                Some(missed_audio_chunks.clone()),
                false,
            ),
        );
        Ok(ProgramTap {
            packets,
            missed_frames,
            missed_audio_chunks,
            video: output.program_video.clone(),
            audio: inner.program_audio.clone(),
            keys,
        })
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
    state: Critical<State<P>>,
    /// The newest setup the reconciler hasn't opened sources for yet.
    wanted: Critical<Option<Arc<Setup>>>,
    /// Native opens cannot be cancelled while macOS presents a prompt.
    /// Keep one in flight per source (including removed/changed sources).
    opening: Mutex<HashSet<String>>,
    wake: Condvar,
    main: CanvasOutput<P>,
    /// The extra canvases' outputs (`[vertical]`, `[[canvases]]`, #84),
    /// each created when a setup first enables it. Their source slots and
    /// audio mix belong to the same engine; only composition and output
    /// are separate.
    extras: ExtraOutputs<P>,
    /// Scenes drawn for monitors on air or not (`scene:ID` targets:
    /// projectors and the multiview), off the program's thread.
    scene_feeds: Arc<SceneFeeds<P::Frame>>,
    /// A camera-only source is needed only while that output is running.
    camera_source: Mutex<Option<String>>,
    program_audio: Arc<Routes<P::Frame>>,
    program_meter: Arc<StereoMeter>,
    /// Monitors' H.264 streams, by what they show.
    frame_streams: Arc<frame_stream::Streams>,
    opus_stream: Mutex<Option<std::sync::Weak<super::opus_stream::Shared>>>,
    monitor: Arc<MonitorSlot>,
    /// The newest levels measured.
    levels: Mutex<Vec<AudioLevel>>,
    /// The loudest levels since a recording's status last read them.
    loudest: Critical<BTreeMap<String, (f64, f64)>>,
    recording: AtomicBool,
    /// Audio sources that stopped (and came back) while a recording runs,
    /// for its manifests and report; `None` while none runs.
    audio_stops: Mutex<Option<Vec<AudioStopped>>>,
    /// Counts changes to `audio_stops`, so the recording rewrites its
    /// manifest when one comes.
    audio_stop_changes: AtomicU64,
    /// Measured write speeds by folder, and when.
    speeds: Mutex<HashMap<PathBuf, (Time, f64)>>,
}

/// The most extra canvases an engine makes outputs for in its life (a
/// canvas turned off keeps its output, as ids stay put).
const MAX_ENGINE_CANVASES: usize = 2 * isocast_protocol::MAX_EXTRA_CANVASES;

/// The engine's extra canvases' outputs, by id. Insert-only, so a
/// reference to one lives as long as the engine: a canvas turned off keeps
/// its output, disabled, and gets it back when it is turned on again.
struct ExtraOutputs<P: Platform>([OnceLock<ExtraOutput<P>>; MAX_ENGINE_CANVASES]);

struct ExtraOutput<P: Platform> {
    id: CanvasId,
    output: CanvasOutput<P>,
    /// Its arrangements of the scenes drawn for monitors
    /// (`<id>-scene:ID`: scene projectors on this canvas, #96).
    scene_feeds: Arc<SceneFeeds<P::Frame>>,
}

impl<P: Platform> ExtraOutputs<P> {
    fn new() -> Self {
        Self(std::array::from_fn(|_| OnceLock::new()))
    }

    /// Canvas `id`'s output, enabled or not, if it was ever made.
    fn get(&self, id: CanvasId) -> Option<&ExtraOutput<P>> {
        self.0
            .iter()
            .map_while(OnceLock::get)
            .find(|extra| extra.id == id)
    }

    /// Canvas `id`'s output, made by `make` if it has none yet.
    fn get_or_init(
        &self,
        id: CanvasId,
        make: impl Fn() -> ExtraOutput<P>,
    ) -> Result<&ExtraOutput<P>, String> {
        for slot in &self.0 {
            let extra = slot.get_or_init(&make);
            if extra.id == id {
                return Ok(extra);
            }
        }
        Err(format!(
            "canvas {id:?}: this engine has made outputs for {MAX_ENGINE_CANVASES} canvases \
             already; restart the engine to use more"
        ))
    }

    /// Every extra canvas's output made so far.
    fn iter(&self) -> impl Iterator<Item = &ExtraOutput<P>> {
        self.0.iter().map_while(OnceLock::get)
    }
}

/// How messages name canvas `canvas`: "vertical canvas", "canvas \"x\"".
pub(crate) fn canvas_words(canvas: CanvasId) -> String {
    match canvas {
        CanvasId::Main => "main canvas".into(),
        CanvasId::Vertical => "vertical canvas".into(),
        other => format!("canvas {other:?}"),
    }
}

/// How messages name canvas `canvas`'s program: "program", "vertical
/// program", "canvas \"x\"'s program".
pub(crate) fn canvas_program_words(canvas: CanvasId) -> String {
    match canvas {
        CanvasId::Main => "program".into(),
        CanvasId::Vertical => "vertical program".into(),
        other => format!("canvas {other:?}'s program"),
    }
}

/// Why canvas `canvas` has no output: it isn't on.
pub(crate) fn canvas_off(canvas: CanvasId) -> String {
    match canvas {
        CanvasId::Vertical => "vertical: canvas is not enabled".to_string(),
        other => format!("canvas: canvas {other:?} is not enabled"),
    }
}

/// The worker name of canvas `canvas`'s compositor: `compositor`,
/// `vertical compositor`, `<id> compositor`.
pub fn compositor_worker(canvas: CanvasId) -> String {
    match canvas {
        CanvasId::Main => "compositor".into(),
        other => format!("{other} compositor"),
    }
}

/// The canvas whose compositor the worker `name` is (see
/// [`compositor_worker`]).
pub fn compositor_canvas(name: &str) -> Option<CanvasId> {
    if name == "compositor" {
        return Some(CanvasId::Main);
    }
    CanvasId::parse(name.strip_suffix(" compositor")?).ok()
}

/// One composition of the engine's captured sources. Each output size needs
/// its own bounded frame pool; captured source frames are shared, never copied
/// or captured twice. No vertical worker or pool exists in a main-only setup.
struct CanvasOutput<P: Platform> {
    control: Arc<SessionControl>,
    follow_main: AtomicBool,
    enabled: AtomicBool,
    /// The program's newest canvases.
    program: Arc<FrameSlot<P::Frame>>,
    /// The composited BGRA picture, before the encoder's YUV conversion.
    program_rgb: Arc<FrameSlot<P::Frame>>,
    /// Studio mode's preview scene, drawn while someone wants it.
    preview: Arc<FrameSlot<P::Frame>>,
    preview_scene: Mutex<Option<Arc<str>>>,
    /// The highest rate the preview was asked for since the program last
    /// looked (0: nobody).
    preview_wanted: AtomicU32,
    /// Recording preparation and virtual-camera outputs are not disposable monitors.
    protected_program_wanted: AtomicU32,
    camera_preview_wanted: AtomicU32,
    /// Resolves the setup's scenes into what the program draws whenever
    /// they change.
    scenes: SceneThread<P::Frame>,
    program_video: Arc<Routes<P::Frame>>,
    /// The setup revision each recent program (and preview) frame was
    /// drawn from, by tick, so monitors can tell clients which layout a
    /// frame shows.
    drawn: Arc<frame_stream::Drawn>,
    frames_drawn: AtomicU64,
    /// Shared with replay buffers, which count the program's misses while
    /// they run.
    frames_missed: Arc<AtomicU64>,
    /// Program frames drawn per second over the last second (`f64` bits).
    program_fps: AtomicU64,
    /// What the compositor's recent ticks cost.
    timings: Mutex<CompositorTimings>,
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
    /// The extra canvases' compositors, by canvas.
    extra_compositors: BTreeMap<CanvasId, CompositorRun>,
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
    /// The end of the newest audio sent to the outputs (on the session
    /// clock, sync offset included); `i64::MIN` before any.
    last_end: Arc<AtomicI64>,
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

impl<P: Platform> CanvasOutput<P> {
    fn new(
        canvas: CanvasId,
        platform: &P,
        timeline: &Arc<Timeline>,
        control: Arc<SessionControl>,
        on_event: &Option<OnEngineEvent>,
    ) -> Self {
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
                Arc::new(move |message, broken| {
                    if let Some(on_event) = &on_event {
                        on_event(EngineEvent::WorkerFailed {
                            name: if canvas.is_main() {
                                "scenes".to_string()
                            } else {
                                format!("{canvas} scenes")
                            },
                            message,
                            // This scene thread belongs only to its canvas;
                            // the compositor observes poison and closes it.
                            broken: broken && canvas.is_main(),
                        });
                    }
                })
            },
        );
        Self {
            control,
            follow_main: AtomicBool::new(false),
            enabled: AtomicBool::new(true),
            program: Arc::new(FrameSlot::with_capacity(timeline.clone(), 1)),
            program_rgb: Arc::new(FrameSlot::with_capacity(timeline.clone(), 1)),
            preview: Arc::new(FrameSlot::with_capacity(timeline.clone(), 1)),
            preview_scene: Mutex::new(None),
            preview_wanted: AtomicU32::new(0),
            protected_program_wanted: AtomicU32::new(0),
            camera_preview_wanted: AtomicU32::new(0),
            scenes,
            program_video: Arc::default(),
            drawn: Arc::default(),
            frames_drawn: AtomicU64::new(0),
            frames_missed: Arc::default(),
            program_fps: AtomicU64::new(0),
            timings: Mutex::default(),
            encoders: Default::default(),
        }
    }
}

impl<P: Platform> Inner<P> {
    /// Note for the running recording, if any, that audio source `id`
    /// stopped at `at` (the end of its last audio) because of `reason`.
    fn audio_stopped(&self, id: &str, at: Time, reason: &str) {
        if let Some(stops) = self.audio_stops.lock().as_mut() {
            stops.push(AudioStopped {
                source: id.to_string(),
                at,
                reason: reason.to_string(),
                resumed: None,
            });
            self.audio_stop_changes.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Audio source `id` delivered its first audio, at `at`: if the running
    /// recording noted it as stopped, it is back from there.
    fn audio_resumed(&self, id: &str, at: Time) {
        if let Some(stops) = self.audio_stops.lock().as_mut()
            && let Some(stop) = stops
                .iter_mut()
                .rev()
                .find(|stop| stop.source == id && stop.resumed.is_none())
        {
            stop.resumed = Some(at.max(stop.at));
            self.audio_stop_changes.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// The canvas a program or preview target shows (see
    /// [`CanvasId::from_target`]), if it is the main one or an extra
    /// canvas the engine has an output for: a source may be called
    /// `screen-preview`.
    fn canvas_target(&self, target: &str) -> Option<(CanvasId, bool)> {
        CanvasId::from_target(target)
            .filter(|(canvas, _)| canvas.is_main() || self.extras.get(*canvas).is_some())
    }

    /// The same for a scene target (see [`CanvasId::from_scene_target`]).
    fn scene_target<'a>(&self, target: &'a str) -> Option<(CanvasId, &'a str)> {
        CanvasId::from_scene_target(target)
            .filter(|(canvas, _)| canvas.is_main() || self.extras.get(*canvas).is_some())
    }

    /// Canvas `canvas`'s scene feeds (the main canvas's if it has none).
    fn feeds(&self, canvas: CanvasId) -> &Arc<SceneFeeds<P::Frame>> {
        match canvas {
            CanvasId::Main => &self.scene_feeds,
            other => self
                .extras
                .get(other)
                .map_or(&self.scene_feeds, |extra| &extra.scene_feeds),
        }
    }

    fn output(&self, canvas: CanvasId) -> Option<&CanvasOutput<P>> {
        match canvas {
            CanvasId::Main => Some(&self.main),
            other => self
                .extras
                .get(other)
                .map(|extra| &extra.output)
                .filter(|v| v.enabled.load(Ordering::SeqCst)),
        }
    }

    fn worker<T>(&self, name: &str, run: impl FnOnce() -> T) -> Result<T, String> {
        let result = isocast_diagnostics::contain(name, run);
        if let Err(message) = &result {
            self.worker_errors.lock().push(message.clone());
            // Only a panic that cut a change to shared control state short
            // (see `crate::critical`) needs the emergency stop; a panic
            // that merely looked at it leaves every recording running.
            let broken = self.state.is_broken()
                || self.wanted.is_broken()
                || self.loudest.is_broken()
                || self.main.program_video.list.is_broken()
                || self.program_audio.list.is_broken();
            if broken {
                self.emergency.store(true, Ordering::SeqCst);
            }
            if name == "compositor" {
                self.main.program.stop(message.clone());
                self.main
                    .program_fps
                    .store(0.0f64.to_bits(), Ordering::Relaxed);
            } else if let Some(canvas) = compositor_canvas(name)
                && let Some(extra) = self.extras.get(canvas)
            {
                let output = &extra.output;
                output.program.stop(message.clone());
                output
                    .program_fps
                    .store(0.0f64.to_bits(), Ordering::Relaxed);
                self.timeline
                    .set_on_air(canvas, std::iter::empty(), self.platform.now());
            }
            self.event(EngineEvent::WorkerFailed {
                name: name.into(),
                message: message.clone(),
                broken,
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
                self.main
                    .preview_wanted
                    .fetch_max(fps.max(1), Ordering::SeqCst);
            }
            other => {
                if let Some((canvas, preview)) = self.canvas_target(other)
                    && let Some(output) = self.output(canvas)
                {
                    if !preview {
                        output.control.want_program(fps);
                    } else {
                        output
                            .preview_wanted
                            .fetch_max(fps.max(1), Ordering::SeqCst);
                    }
                }
            }
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
        let inner = Arc::new(Inner {
            emergency: AtomicBool::new(false),
            worker_errors: Mutex::new(Vec::new()),
            platform: platform.clone(),
            main: CanvasOutput::new(
                CanvasId::Main,
                platform,
                &timeline,
                control.clone(),
                &on_event,
            ),
            extras: ExtraOutputs::new(),
            control,
            scene_feeds: Arc::new(SceneFeeds::new(timeline.clone())),
            timeline,
            epoch: AtomicI64::new(i64::MIN),
            on_event,
            stopping: AtomicBool::new(false),
            state: Critical::new(State {
                setup: setup.clone(),
                sources: BTreeMap::new(),
                stingers: HashMap::new(),
                mixer: None,
                compositor: None,
                extra_compositors: BTreeMap::new(),
                pins: Vec::new(),
                revision: 0,
            }),
            wanted: Critical::new(None),
            opening: Mutex::new(HashSet::new()),
            wake: Condvar::new(),
            camera_source: Mutex::new(None),
            program_audio: Arc::default(),
            program_meter: Arc::default(),
            frame_streams: Arc::default(),
            opus_stream: Mutex::default(),
            monitor: Arc::new(Mutex::new(monitor)),
            levels: Mutex::new(Vec::new()),
            loudest: Critical::new(BTreeMap::new()),
            recording: AtomicBool::new(false),
            audio_stops: Mutex::new(None),
            audio_stop_changes: AtomicU64::new(0),
            speeds: Mutex::new(HashMap::new()),
        });
        reconcile(&inner, setup);

        // Wait for every source's first frame (cameras take a moment to
        // warm up), so the program starts with all of them.
        let waiting_since = platform.now();
        loop {
            let ready = {
                let state = inner.state.read();
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
            let mut state = inner.state.lock();
            publish(&inner, &mut state);
        }
        // Measure the recording disk now, so the first recording starts at
        // once.
        let folder = expand_home(&inner.state.read().setup.recording.folder);
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
            let mut state = inner.state.lock();
            if let Some(issue) = setup_refusal(inner, &state.setup, &setup) {
                return Err(format!("{}: {}", issue.path, issue.message));
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
        *inner.wanted.lock() = Some(setup);
        inner.wake.notify_all();
        Ok(())
    }

    /// Why the canvas can't change to `canvas` now (a recording of the
    /// program or a stream is using the current one), if it can't.
    pub fn canvas_refusal(&self, canvas: &Canvas) -> Option<String> {
        let state = self.inner.state.read();
        canvas_refusal(&self.inner, &state.setup.canvas, canvas)
    }

    /// A plan and its apply use the same output-ownership checks, including
    /// a vertical canvas whose frame rate inherits a change to the main one.
    pub fn setup_refusal(&self, setup: &Setup) -> Option<isocast_protocol::Issue> {
        let state = self.inner.state.read();
        setup_refusal(&self.inner, &state.setup, setup)
    }

    /// Stop everything: the recording, if one runs, should be stopped
    /// first.
    pub fn stop(&self) {
        let inner = &self.inner;
        inner.stopping.store(true, Ordering::SeqCst);
        inner.wake.notify_all();
        frame_stream::stop(&inner.frame_streams);
        for thread in self.threads.lock().drain(..) {
            let _ = thread.join();
        }
        let (compositor, extra_compositors, mixer, sources, stingers) = {
            let mut state = inner.state.lock();
            (
                state.compositor.take(),
                std::mem::take(&mut state.extra_compositors),
                state.mixer.take(),
                std::mem::take(&mut state.sources),
                std::mem::take(&mut state.stingers),
            )
        };
        if let Some(compositor) = compositor {
            stop_compositor(compositor);
        }
        for compositor in extra_compositors.into_values() {
            stop_compositor(compositor);
        }
        inner.main.scenes.stop();
        for extra in inner.extras.iter() {
            extra.output.scenes.stop();
            extra.output.program_video.clear();
        }
        if let Some(mixer) = mixer {
            stop_mixer(inner, mixer, &BTreeMap::new());
        }
        drop(sources);
        drop(stingers);
        drop(Closing(inner.monitor.lock().clone()));
        // A tap holds these routes itself, so they outlive the engine:
        // dropping the outputs is what tells a stream the program ended.
        inner.main.program_video.clear();
        inner.program_audio.clear();
    }

    /// The control the engine follows: scene switches, markers, live audio
    /// and filter settings.
    pub fn control(&self) -> &Arc<SessionControl> {
        &self.inner.control
    }

    /// Why `canvas`'s program stopped (its compositor failed), while it
    /// stays stopped.
    pub fn canvas_stopped(&self, canvas: CanvasId) -> Option<String> {
        self.inner.output(canvas)?.program.stopped()
    }

    /// Start/resume must not announce working program output after its worker
    /// has failed. Stop remains available to finalize every recording.
    pub fn program_failure(&self) -> Option<String> {
        let main = self.inner.main.program.stopped().map(|reason| format!(
            "program output has failed: {reason}; stop the recording and restart the live engine"
        ));
        main.or_else(|| {
            let setup = self.inner.state.read().setup.clone();
            setup.extra_canvas_ids().into_iter().find_map(|canvas| {
                setup
                    .extra_canvas(canvas)?
                    .recording
                    .as_ref()
                    .filter(|recording| recording.enabled)?;
                self.inner.output(canvas)?.program.stopped().map(|reason| {
                    format!("{} output has failed: {reason}; stop the recording and restart the live engine", canvas_program_words(canvas))
                })
            })
        })
    }

    /// The platform it runs on.
    pub fn platform(&self) -> &P {
        &self.inner.platform
    }

    /// The setup the engine runs.
    pub fn setup(&self) -> Arc<Setup> {
        self.inner.state.read().setup.clone()
    }

    /// What the engine is doing: its sources, canvas and counters.
    pub fn info(&self) -> EngineInfo {
        let inner = &self.inner;
        let state = inner.state.read();
        let clocks = inner.timeline.clocks.info();
        let mut compositor = inner.main.timings.lock().info();
        if let Some(clock) = clocks.iter().find(|clock| clock.id == "program") {
            compositor.wake_lateness_us = clock.wake_lateness_us;
        }
        let mut held_by = Vec::new();
        if inner.recording.load(Ordering::SeqCst) {
            held_by.push("recording".to_string());
        }
        let canvases: Vec<isocast_protocol::api::CanvasEngineInfo> = state
            .setup
            .extra_canvas_ids()
            .into_iter()
            .filter_map(|id| {
                let vertical = state.setup.extra_canvas(id)?;
                let output = inner.output(id)?;
                let mut compositor = output.timings.lock().info();
                if let Some(clock) = clocks.iter().find(|clock| clock.id == id.program()) {
                    compositor.wake_lateness_us = clock.wake_lateness_us;
                }
                let (raster_us, content, content_mb) = output.scenes.content();
                Some(isocast_protocol::api::CanvasEngineInfo {
                    id,
                    canvas: vertical.canvas(&state.setup.canvas),
                    program_frames: output.frames_drawn.load(Ordering::Relaxed),
                    missed_frames: output.frames_missed.load(Ordering::Relaxed),
                    fps: f64::from_bits(output.program_fps.load(Ordering::Relaxed)),
                    stopped: output.program.stopped(),
                    compositor,
                    scenes: ScenesInfo {
                        resolve_us: output.scenes.resolve_us(),
                        raster_us,
                        content,
                        content_mb,
                    },
                })
            })
            .collect();
        EngineInfo {
            vertical: canvases
                .iter()
                .find(|c| c.id.is_vertical())
                .cloned()
                .map(|info| isocast_protocol::api::CanvasEngineInfo {
                    id: CanvasId::Main,
                    ..info
                }),
            canvases,
            clocks,
            worker_errors: self
                .inner
                .worker_errors
                .lock()
                .iter()
                .cloned()
                .chain(self.inner.main.scenes.failure())
                .chain(
                    self.inner
                        .extras
                        .iter()
                        .filter_map(|extra| extra.output.scenes.failure()),
                )
                .collect(),
            uptime_seconds: inner.epoch().map_or(0.0, |epoch| {
                inner.platform.now().saturating_since(epoch).as_secs_f64()
            }),
            canvas: state.setup.canvas.clone(),
            sources: statuses(&state),
            program_frames: inner.main.frames_drawn.load(Ordering::Relaxed),
            missed_frames: inner.main.frames_missed.load(Ordering::Relaxed),
            fps: f64::from_bits(inner.main.program_fps.load(Ordering::Relaxed)),
            outputs: held_by,
            compositor,
            scenes: {
                let (raster_us, content, content_mb) = inner.main.scenes.content();
                ScenesInfo {
                    resolve_us: inner.main.scenes.resolve_us(),
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
        self.resolved_canvas(CanvasId::Main)
    }

    pub fn resolved_canvas(&self, canvas: CanvasId) -> Option<isocast_protocol::api::Resolved> {
        let scenes = self.inner.output(canvas)?.scenes.latest()?;
        Some(isocast_protocol::api::Resolved {
            generation: scenes.generation,
            revision: scenes.revision,
            scenes: crate::compiled::resolved(&scenes.compiled),
        })
    }

    /// Each source's state, as [`Engine::info`] reports it.
    pub fn sources(&self) -> Vec<SourceStatus> {
        statuses(&self.inner.state.read())
    }

    /// The first source that couldn't open, and why.
    pub fn first_failure(&self) -> Option<(String, String)> {
        let state = self.inner.state.read();
        state.sources.iter().find_map(|(id, live)| {
            (live.run == SourceRun::Failed)
                .then(|| (id.clone(), live.error.clone().unwrap_or_default()))
        })
    }

    /// The newest levels of every audio source and the program mix.
    pub fn levels(&self) -> Vec<AudioLevel> {
        self.inner.levels.lock().clone()
    }

    /// Close source `id` and open it again, as it is in the setup: after
    /// something outside the setup changed what it opens (a screen or
    /// window chosen again in the desktop's sharing dialog). Refused while
    /// a recording uses it, which keeps it as it is. Nothing to do if the
    /// engine doesn't have it open.
    pub fn reopen(&self, id: &str) -> Result<(), String> {
        self.reopen_unpinned(id)
    }

    /// Whether a recording uses source `id`, keeping it as it is.
    pub fn recording_uses(&self, id: &str) -> bool {
        self.inner.state.lock().pins.iter().any(|pin| pin == id)
    }

    fn reopen_unpinned(&self, id: &str) -> Result<(), String> {
        let inner = &self.inner;
        let (old, setup) = {
            let mut state = inner.state.lock();
            if state.pins.contains(&id.to_string()) {
                return Err(format!(
                    "source {id:?} is being recorded, which keeps it as it is; choose again once \
                     the recording stops"
                ));
            }
            let old = state.sources.remove(id);
            if old.is_some() {
                publish(inner, &mut state);
            }
            (old, state.setup.clone())
        };
        if old.is_none() {
            return Ok(());
        }
        drop(old);
        *inner.wanted.lock() = Some(setup);
        inner.wake.notify_all();
        Ok(())
    }

    /// Stage `scene` as studio mode's preview (or none).
    pub fn set_preview(&self, scene: Option<String>) {
        let _ = self.set_canvas_preview(CanvasId::Main, scene);
    }

    pub fn set_canvas_preview(
        &self,
        canvas: CanvasId,
        scene: Option<String>,
    ) -> Result<(), String> {
        let output = self
            .inner
            .output(canvas)
            .ok_or_else(|| canvas_off(canvas))?;
        if let Some(id) = &scene
            && !self
                .inner
                .state
                .read()
                .setup
                .scenes
                .iter()
                .any(|s| s.id == *id)
        {
            return Err(format!("scene: no scene {id:?}"));
        }
        let scene: Option<Arc<str>> = scene.map(Into::into);
        let mut staged = output.preview_scene.lock();
        // Frames of another scene must not show as this one's.
        if *staged != scene {
            output.preview.clear();
        }
        *staged = scene.clone();
        drop(staged);
        if canvas == CanvasId::Main {
            let followers: Vec<CanvasId> = self
                .inner
                .extras
                .iter()
                .filter(|extra| {
                    extra.output.enabled.load(Ordering::SeqCst)
                        && extra.output.follow_main.load(Ordering::SeqCst)
                })
                .map(|extra| extra.id)
                .collect();
            // Every follower gets it, even if one refuses.
            let mut first_error = None;
            for follower in followers {
                if let Err(error) =
                    self.set_canvas_preview(follower, scene.as_ref().map(|scene| scene.to_string()))
                {
                    first_error.get_or_insert(error);
                }
            }
            if let Some(error) = first_error {
                return Err(error);
            }
        }
        Ok(())
    }

    pub fn switch_canvas_scene(
        &self,
        canvas: CanvasId,
        at: Time,
        scene: String,
        mut transition: Transition,
    ) -> Result<Option<(u32, f64)>, String> {
        self.inner.control.prepare(&mut transition)?;
        let state = self.inner.state.read();
        if !state.setup.scenes.iter().any(|s| s.id == scene) {
            return Err(format!(
                "scene: no scene {scene:?}; scenes: {}",
                state
                    .setup
                    .scenes
                    .iter()
                    .map(|scene| scene.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let output = self
            .inner
            .output(canvas)
            .ok_or_else(|| canvas_off(canvas))?;
        if !canvas.is_main() && output.follow_main.load(Ordering::SeqCst) {
            return Err(format!(
                "{}.follow_main: set false to switch the {} independently",
                state.setup.canvas_field(canvas),
                canvas_words(canvas)
            ));
        }
        let placed = output
            .control
            .switch_scene(at, scene, transition)
            .ok()
            .flatten();
        if !canvas.is_main() {
            let mut main = self.inner.control.state.lock();
            main.changed = true;
            return Ok(main.check_end(at).ok().and_then(|()| main.place(at)));
        }
        Ok(placed)
    }

    pub fn canvas_scene(&self, canvas: CanvasId) -> Option<String> {
        let output = self.inner.output(canvas)?;
        let control = if output.follow_main.load(Ordering::SeqCst) {
            &self.inner.control
        } else {
            &output.control
        };
        control
            .on_air(self.inner.platform.now())
            .map(|on_air| on_air.scene.to_string())
    }

    /// The newest frame of `target` — [`PROGRAM`], `"preview"` (studio
    /// mode's preview scene, drawn for a while after being asked for) or a
    /// source id — and its time. An error lists the valid targets.
    pub fn frame(&self, target: &str) -> Result<Option<(P::Frame, Time)>, String> {
        let slot = slot_of(&self.inner, target, MONITOR_FPS)?;
        // The program and the preview are drawn only while someone asks:
        // an idle one has no picture until its next frame.
        let frame = match target {
            target if self.inner.canvas_target(target).is_some() => wait_for_frame(&slot),
            scene if self.inner.scene_target(scene).is_some() => {
                let deadline = std::time::Instant::now() + super::FRESH_FRAME_WAIT;
                let mut from = Time::from_nanos(i64::MIN);
                loop {
                    let left = deadline.saturating_duration_since(std::time::Instant::now());
                    let Some((frame, at)) = slot.wait_from(from, left) else {
                        break None;
                    };
                    if scene_frame_size(&self.inner, target, None).is_none_or(|wanted| {
                        let size = self.inner.platform.frame_size(&frame);
                        size.0 >= wanted.0 && size.1 >= wanted.1
                    }) {
                        break Some((frame, at));
                    }
                    // A tile may already be in the slot when full size is
                    // requested. Never return it as the one-off picture.
                    if std::time::Instant::now() >= deadline {
                        break None;
                    }
                    from = Time::from_nanos(at.as_nanos().saturating_add(1));
                }
            }
            _ => slot.newest_timed(),
        };
        Ok(frame.and_then(|(frame, at)| {
            P::retain_frame(&frame, crate::FrameUse::Auxiliary).map(|frame| (frame, at))
        }))
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
        let mut from = after.map_or(Time::from_nanos(i64::MIN), |after| {
            Time::from_nanos(after.min(i64::MAX as u64 - 1) as i64 + 1)
        });
        let until = platform.now() + timeout;
        loop {
            // The slot is looked up again now and then: a source that
            // reopens gets a new one, and the preview must stay wanted.
            let slot = slot_of_sized(&self.inner, target, fps, (max_width, None))?;
            let left = until.saturating_since(platform.now());
            if let Some((frame, at)) = slot.wait_from(from, left.min(SLOT_RECHECK)) {
                if scene_frame_size(&self.inner, target, max_width).is_some_and(|wanted| {
                    let size = platform.frame_size(&frame);
                    size.0 < wanted.0 || size.1 < wanted.1
                }) {
                    if platform.now() >= until || self.inner.stopping.load(Ordering::SeqCst) {
                        return Ok(None);
                    }
                    from = Time::from_nanos(at.as_nanos().saturating_add(1));
                    continue;
                }
                let Some(frame) = P::retain_frame(&frame, crate::FrameUse::Auxiliary) else {
                    return Ok(None);
                };
                let (jpeg, width, height) = platform
                    .encode_jpeg(&frame, max_width, quality)
                    .map_err(|err| format!("{err:#}"))?;
                let stamp = target_canvas(&self.inner, target)
                    .and_then(|canvas| self.inner.output(canvas))
                    .and_then(|output| frame_stream::stamp_at(&output.drawn, at));
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
    /// with everyone watching the same at that width and profile, which runs while
    /// anyone does, at the highest rate any of them wants. An error lists
    /// the valid targets.
    pub fn frames(
        &self,
        target: &str,
        max_width: Option<u32>,
        fps: u32,
        profile: isocast_protocol::api::H264Profile,
    ) -> Result<FrameFeed, String> {
        let fps = fps.clamp(1, 60);
        slot_of_sized(&self.inner, target, fps, (max_width, None))?;
        let key = frame_stream::Key {
            target: target.to_string(),
            max_width,
            profile,
        };
        let inner = self.inner.clone();
        let target = target.to_string();
        frame_stream::watch(
            &self.inner.frame_streams,
            &self.inner.platform,
            key,
            fps,
            // Called with the stream's rate now (the highest any of its
            // viewers wants), which the program is drawn at least at.
            move |fps| stream_source(&inner, &target, fps, false, (max_width, None)),
        )
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
        slot_of_sized(&self.inner, target, fps, max_size)?;
        let filters = (filtered && target_canvas(&self.inner, target).is_none()).then(|| {
            let (inner, id) = (self.inner.clone(), target.to_string());
            Box::new(move || {
                let setup = inner.state.read().setup.clone();
                let filters = setup
                    .source(&id)
                    .map_or_else(Vec::new, |s| s.filters.clone());
                (filters, inner.epoch())
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
            move |fps, size| stream_source(&inner, &target, fps, true, size),
            filters,
        ))
    }

    /// The highest frame rate any client's monitor stream asks for right
    /// now (see [`Engine::frames`]), if one runs: the program needn't be
    /// drawn faster than this for monitors alone.
    pub fn monitor_fps(&self) -> Option<u32> {
        frame_stream::highest_rate(&self.inner.frame_streams)
    }

    /// Whether a High profile monitor stream ([`Engine::frames`]) is
    /// guaranteed to be Constrained High on this platform (see
    /// [`Platform::monitor_h264_constrained_high`]).
    pub fn monitor_h264_constrained_high(&self) -> bool {
        self.inner.platform.monitor_h264_constrained_high()
    }

    /// An independent camera output; no change to recording routes or clocks.
    pub fn virtual_camera(&self, on_change: super::OnCameraChange) -> Result<super::VirtualCamera> {
        use isocast_protocol::virtual_camera::{Target, VirtualCameraStatus};
        let setup = self.setup();
        let settings = setup.virtual_camera.clone();
        settings
            .check_start(&setup, P::CAMERA_IS_MONITOR)
            .map_err(anyhow::Error::msg)?;
        let (width, height) = settings.size(&setup);
        let fps = settings.fps(&setup);
        if settings.target == Target::Preview && self.inner.main.preview_scene.lock().is_none() {
            bail!(
                "virtual_camera.target: preview is empty; stage a scene with isocast preview ID, or choose program, scene or source"
            );
        }
        let extra = settings.target_canvas();
        if let Some(canvas) = extra {
            canvas_camera_slot(&self.inner, canvas, fps).map_err(anyhow::Error::msg)?;
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
            *inner.camera_source.lock() = settings.source.clone();
            let guard = CameraSourceGuard(inner.clone());
            *inner.wanted.lock() = Some(setup.clone());
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
            move |after, fps| {
                let _keep_source_open = &source_guard;
                if inner.stopping.load(Ordering::SeqCst) {
                    bail!("virtual_camera: the engine stopped");
                }
                if settings.target == Target::Scene && !P::CAMERA_IS_MONITOR {
                    return Ok(inner.main.scenes.latest().map(|scenes| {
                        super::virtual_camera::Input::Scene(scenes, settings.scene.clone().unwrap())
                    }));
                }
                if !P::CAMERA_IS_MONITOR
                    && settings.target != Target::Source
                    && let Some(output) = inner.output(extra.unwrap_or(CanvasId::Main))
                {
                    let wanted = if settings.target == Target::Preview {
                        &output.camera_preview_wanted
                    } else {
                        &output.protected_program_wanted
                    };
                    wanted.fetch_max(fps, Ordering::Relaxed);
                }
                let slot = match (settings.target, extra) {
                    // A contained failure of the extra canvas fails the
                    // camera, saying why, rather than dropping every frame
                    // quietly.
                    (_, Some(canvas)) => canvas_camera_slot(&inner, canvas, fps),
                    (Target::Program, None) => slot_of(&inner, PROGRAM, fps),
                    (Target::Scene, None) if P::CAMERA_IS_MONITOR => scene_slot(
                        &inner,
                        CanvasId::Main,
                        settings.scene.as_deref().unwrap(),
                        fps,
                        (Some(width), Some(height)),
                    ),
                    (Target::Preview, None) => slot_of(&inner, "preview", fps),
                    (Target::Source, None) => {
                        slot_of(&inner, settings.source.as_deref().unwrap(), fps)
                    }
                    (Target::Scene | Target::Vertical | Target::Canvas, None) => Err(
                        "virtual_camera.canvas: give an extra canvas's id for target = canvas"
                            .to_string(),
                    ),
                };
                let slot = match slot {
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
                        if P::CAMERA_IS_MONITOR {
                            // Allow compositor jitter, but don't borrow the
                            // next camera tick waiting for a fresh picture.
                            Duration::from_millis(250) / fps
                        } else {
                            Duration::from_millis(50)
                        },
                    )
                };
                Ok(frame.and_then(|(frame, time)| {
                    P::retain_frame(&frame, crate::FrameUse::Auxiliary)
                        .map(|frame| super::virtual_camera::Input::Frame(frame, time))
                }))
            },
            on_change,
        )
    }

    /// The program's frames and mix for an output like a stream, from the
    /// next frame on. Audio may queue `depth` chunks; video keeps only its
    /// latest eight pictures so a stalled stream cannot pin the whole pool.
    pub fn tap_program(&self, depth: usize) -> ProgramTap<P::Frame> {
        self.program_tap_source()
            .tap(depth, Arc::new(AtomicU64::new(0)))
            .expect("the main canvas always exists")
    }

    /// A separate early program monitor mix as shared 48 kHz stereo Opus.
    pub fn opus(&self) -> Result<super::OpusFeed> {
        super::opus_stream::watch(&self.inner.opus_stream, self.program_tap_source())
    }

    pub(crate) fn program_tap_source(&self) -> ProgramTapSource<P> {
        ProgramTapSource(self.inner.clone(), CanvasId::Main)
    }

    pub(crate) fn canvas_tap_source(&self, canvas: CanvasId) -> Result<ProgramTapSource<P>> {
        if self.inner.output(canvas).is_none() {
            bail!("{}", canvas_off(canvas));
        }
        Ok(ProgramTapSource(self.inner.clone(), canvas))
    }

    /// The program's shared encoders, for outputs that take it compressed.
    pub(crate) fn encoders(&self) -> &crate::stream::encoder::Encoders {
        &self.inner.main.encoders
    }

    /// The count of ticks `canvas`'s program missed, live (for replay
    /// buffers' status); None while that canvas is off.
    pub(crate) fn program_misses(&self, canvas: CanvasId) -> Option<Arc<AtomicU64>> {
        self.inner
            .output(canvas)
            .map(|output| output.frames_missed.clone())
    }

    pub(crate) fn canvas_encoders(
        &self,
        canvas: CanvasId,
    ) -> Result<&crate::stream::encoder::Encoders> {
        self.inner
            .output(canvas)
            .map(|output| &output.encoders)
            .ok_or_else(|| anyhow::anyhow!("{}", canvas_off(canvas)))
    }

    pub fn stream_canvas(
        &self,
        canvas: CanvasId,
        id: &str,
        settings: isocast_protocol::stream::StreamSettings,
        key: String,
        on_change: crate::stream::OnChange,
    ) -> Result<crate::stream::Stream> {
        crate::stream::start_canvas(self, canvas, id, settings, key, on_change)
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
        let _preview_priority = crate::governor::Recording::new(&inner.state.read().setup.governor);
        *inner.audio_stops.lock() = Some(Vec::new());
        let result = isocast_diagnostics::contain("recording", || record(self, options))
            .map_err(anyhow::Error::msg)
            .and_then(|r| r);
        // No finalizer remains after return, including a contained panic.
        // Stale pending metadata must not leave the next session busy forever.
        inner.control.metadata_pending.lock().clear();
        {
            let mut state = inner.state.lock();
            state.pins.clear();
            // Changes to the sources it recorded apply now.
            let setup = state.setup.clone();
            if !inner.state.is_broken() {
                *inner.wanted.lock() = Some(setup);
            }
        }
        inner.wake.notify_all();
        *inner.audio_stops.lock() = None;
        inner.recording.store(false, Ordering::SeqCst);
        result
    }
}

/// Returning a camera-only source to the ordinary source lifecycle happens
/// on every exit, including failed native startup or a disconnected camera.
struct CameraSourceGuard<P: Platform>(Arc<Inner<P>>);

impl<P: Platform> Drop for CameraSourceGuard<P> {
    fn drop(&mut self) {
        *self.0.camera_source.lock() = None;
        if !self.0.stopping.load(Ordering::SeqCst) {
            let setup = self.0.state.read().setup.clone();
            *self.0.wanted.lock() = Some(setup);
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
    // Other canvases' layouts use the very same opened sources and frame
    // slots.
    let extras: Vec<Setup> = setup
        .extra_canvas_ids()
        .into_iter()
        .filter_map(|canvas| setup.for_canvas(canvas))
        .collect();
    for extra in &extras {
        ids.extend(extra.scenes.iter().flat_map(|s| s.all_sources()));
    }
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
    // What sources' masks show runs while they do (#115).
    let mut masks = setup.mask_needs(&ids);
    for extra in &extras {
        masks.extend(extra.mask_needs(&ids));
    }
    ids.extend(masks);
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
    (canvas(current) != canvas(new) && !inner.main.program_video.is_empty()).then(|| {
        format!(
            "the canvas stays {}×{} at {} fps while recording, streaming or keeping a replay \
             buffer; change it once they stop",
            current.width, current.height, current.fps
        )
    })
}

fn setup_refusal<P: Platform>(
    inner: &Inner<P>,
    current: &Setup,
    new: &Setup,
) -> Option<isocast_protocol::Issue> {
    // Outputs are made once per canvas id for the engine's life: a setup
    // whose new canvases don't fit is refused here, not skipped later
    // (#84 review M6).
    let free = MAX_ENGINE_CANVASES - inner.extras.iter().count();
    let fresh: Vec<CanvasId> = new
        .extra_canvas_ids()
        .into_iter()
        .filter(|id| inner.extras.get(*id).is_none())
        .collect();
    if fresh.len() > free {
        return Some(isocast_protocol::Issue {
            path: new.canvas_field(fresh[free]),
            message: format!(
                "this live engine has made outputs for {} canvases already, the most it \
                 holds; restart the engine once nothing records or streams (`isocast \
                 restart`), then apply this again",
                MAX_ENGINE_CANVASES - free
            ),
        });
    }
    if let Some(message) = canvas_refusal(inner, &current.canvas, &new.canvas) {
        return Some(isocast_protocol::Issue {
            path: "canvas".into(),
            message,
        });
    }
    let mut ids = current.extra_canvas_ids();
    for id in new.extra_canvas_ids() {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    for id in ids {
        let size = |setup: &Setup| setup.extra_canvas(id).map(|v| v.canvas(&setup.canvas));
        let field = if current.has_canvas(id) {
            current.canvas_field(id)
        } else {
            new.canvas_field(id)
        };
        if size(current) != size(new)
            && (inner
                .extras
                .get(id)
                .is_some_and(|extra| !extra.output.program_video.is_empty())
                // Recording has snapshotted its formats before routes are
                // added (source opens, disk checks and encoder placement
                // may still run).
                || inner.recording.load(Ordering::SeqCst) && current.has_canvas(id))
        {
            return Some(isocast_protocol::Issue {
                path: field,
                message: "the canvas stays at its current size and fps while recording, streaming or keeping a replay buffer; change it once they stop".into(),
            });
        }
        if inner.recording.load(Ordering::SeqCst)
            && current.extra_canvas(id).map(|v| v.follow_main)
                != new.extra_canvas(id).map(|v| v.follow_main)
        {
            return Some(isocast_protocol::Issue {
                path: format!("{field}.follow_main"),
                message: "change scene linking once the recording stops".into(),
            });
        }
    }
    None
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
            // A screen's capture depth follows its ISO file: a new depth
            // opens it again (a recording keeps it as it is).
            if matches!(
                source.kind,
                SourceKind::Display(_) | SourceKind::Window(_) | SourceKind::Application(_)
            ) && setup.recording.iso.captures_ten_bit(source)
                && let Some(fields) = definition.as_object_mut()
            {
                fields.insert("capture_ten_bit".into(), true.into());
            }
            Some(definition)
        })
        .unwrap_or_default()
}

/// Open the sources the reconciler is asked for, one setup at a time.
fn reconciler<P: Platform>(inner: &Arc<Inner<P>>) {
    loop {
        let setup = {
            let mut wanted = inner.wanted.lock();
            loop {
                if inner.stopping.load(Ordering::SeqCst) {
                    return;
                }
                if let Some(setup) = wanted.take() {
                    break setup;
                }
                wanted.wait(&inner.wake);
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
    if let Some(id) = inner.camera_source.lock().clone()
        && !needed.contains(&id)
    {
        needed.push(id);
    }
    let mut opening = Vec::new();
    let mut closed = Vec::new();
    {
        let mut state = inner.state.lock();
        for id in &needed {
            if inner.opening.lock().contains(id) {
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
        inner.opening.lock().insert(id.clone());
        let permission = setup
            .source(&id)
            .and_then(|s| inner.platform.source_permission(&s.kind));
        let mut pending = Live::failed(definition.clone(), String::new());
        pending.run = if permission.is_some() {
            SourceRun::WaitingForPermission
        } else {
            SourceRun::Opening
        };
        pending.error = permission.as_ref().map(|p| format!("waiting for permission: {p}; {}", isocast_protocol::platform_text::select_linux("check macOS Privacy & Security or its permission prompt", "check Windows Settings › Privacy & security (Privacy on Windows 10); Camera and Microphone must allow desktop apps (isocastd.exe)",
            "check that the device exists and isn't held by another app, and that PipeWire and the desktop's screen-sharing portal are running")));
        let event = pending.status(&id);
        let old = {
            let mut state = inner.state.lock();
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
            let mut state = inner.state.lock();
            let current = state.setup.clone();
            let wanted = !inner.stopping.load(Ordering::SeqCst)
                && definition_of(&current, &id) == definition
                && (needed_sources(&current).contains(&id)
                    || inner.camera_source.lock().as_ref() == Some(&id));
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
            inner.opening.lock().remove(&id);
            if !wanted && !inner.stopping.load(Ordering::SeqCst) {
                *inner.wanted.lock() = Some(current);
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
    let loaded: Vec<String> = inner.state.lock().stingers.keys().cloned().collect();
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
        let mut state = inner.state.lock();
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
        let _ = feed.gap.set(Box::new({
            let (routes, id, control) = (
                Arc::downgrade(&routes),
                id.to_owned(),
                inner.control.clone(),
            );
            move |until, duration| {
                let Some(routes) = routes.upgrade() else {
                    return;
                };
                let offset = control
                    .audio_settings(&id)
                    .map_or(0, |s| i64::from(s.offset_ms) * 1_000_000);
                routes.source_gap(Time::from_nanos(until.as_nanos() + offset), duration);
            }
        }));
        let last_end = Arc::new(AtomicI64::new(i64::MIN));
        let thread = thread::spawn({
            let fault_inner = inner.clone();
            let failure_feed = feed.clone();
            let platform = inner.platform.clone();
            let control = inner.control.clone();
            let routes = routes.clone();
            let meter = meter.clone();
            let stop = stop.clone();
            let last_end = last_end.clone();
            let id = id.to_string();
            move || {
                let failure = fault_inner.worker(&format!("audio source {id}"), || {
                    platform.audio_thread();
                    let frame_samples = channels.max(1) as usize;
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
                                // A source a recording noted as stopped is
                                // back (the engine opened it again).
                                if last_end.load(Ordering::Relaxed) == i64::MIN {
                                    fault_inner.audio_resumed(&id, pts);
                                }
                                let frames = (samples.len() / frame_samples) as u64;
                                last_end.store(
                                    pts.add_ticks(frames, SAMPLE_RATE).as_nanos(),
                                    Ordering::Relaxed,
                                );
                            }
                            Err(RecvTimeoutError::Timeout) => {
                                if audio.feed.stopped.lock().is_some() {
                                    break;
                                }
                            }
                            Err(RecvTimeoutError::Disconnected) => break,
                        }
                    }
                });
                if let Err(message) = failure {
                    *failure_feed.stopped.lock() = Some(message);
                }
            }
        });
        Self {
            routes,
            meter,
            channels,
            last_end,
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
    inner.main.scenes.submit(SceneInput {
        setup: setup.clone(),
        revision: state.revision,
        slots: program_slots.clone(),
    });

    for canvas in setup.extra_canvas_ids() {
        let Some(canvas_setup) = setup.for_canvas(canvas) else {
            continue;
        };
        let made = inner.extras.get_or_init(canvas, || ExtraOutput {
            id: canvas,
            output: CanvasOutput::new(
                canvas,
                &inner.platform,
                &inner.timeline,
                Arc::new(SessionControl::with_data(inner.control.data().clone())),
                &inner.on_event,
            ),
            scene_feeds: Arc::new(SceneFeeds::new(inner.timeline.clone())),
        });
        let output = match made {
            Ok(extra) => &extra.output,
            Err(message) => {
                inner.event(EngineEvent::Warning(message));
                continue;
            }
        };
        let follow = setup
            .extra_canvas(canvas)
            .expect("extra canvas setup")
            .follow_main;
        let was_following = output.follow_main.swap(follow, Ordering::SeqCst);
        if output
            .control
            .on_air(inner.platform.now())
            .is_none_or(|on_air| !setup.scenes.iter().any(|scene| scene.id == *on_air.scene))
            || (was_following && !follow)
        {
            let scene = inner
                .control
                .on_air(inner.platform.now())
                .map(|on_air| on_air.scene.to_string())
                .or_else(|| setup.scenes.first().map(|s| s.id.clone()));
            if let Some(scene) = scene {
                let _ =
                    output
                        .control
                        .switch_scene(inner.platform.now(), scene, Transition::default());
            }
        }
        output.enabled.store(true, Ordering::SeqCst);
        if follow {
            let preview = inner.main.preview_scene.lock().clone();
            let mut staged = output.preview_scene.lock();
            if *staged != preview {
                output.preview.clear();
                *staged = preview;
            }
        }
        output.scenes.submit(SceneInput {
            setup: Arc::new(canvas_setup),
            revision: state.revision,
            slots: program_slots.clone(),
        });
    }

    let control = &inner.control;
    let mut frames = slots;
    frames.push((PROGRAM.to_string(), inner.main.program.clone()));
    for canvas in setup.extra_canvas_ids() {
        if let Some(output) = inner.output(canvas) {
            frames.push((canvas.program().to_string(), output.program.clone()));
        }
    }
    control.set_frames(frames);
    *control.browsers.lock() = state
        .sources
        .iter()
        .filter_map(|(id, live)| Some((id.clone(), live.opened.as_ref()?.browser()?)))
        .collect();
    *control.stingers.lock() = state
        .stingers
        .iter()
        .map(|(file, (_, length))| (file.clone(), *length))
        .collect();
    control.set_audio(&setup);
    control.set_filters(&setup);

    // Another monitoring device: sources heard switch to it.
    let monitor = inner.monitor.lock().clone();
    if monitor.device() != setup.audio.monitoring_device {
        *inner.monitor.lock() = Arc::new(Monitor::new(&setup.audio.monitoring_device));
        drop(Closing(monitor));
    }

    if inner.epoch().is_some() && !inner.stopping.load(Ordering::SeqCst) {
        ensure_mixer(inner, state);
        ensure_compositor(inner, state);
        ensure_extra_compositors(inner, state);
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
                platform.audio_thread();
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
    for (track, (id, _, mixed)) in mixer.inputs.iter().enumerate() {
        let Some(audio) = state.sources.get(id).and_then(|live| live.audio.as_ref()) else {
            continue;
        };
        let reader = Arc::as_ptr(&audio.routes) as usize;
        if mixer.routed.get(id).map(|(r, _)| *r) != Some(reader) {
            let key = audio.routes.add_mix_input(
                mixer.tx.clone(),
                track,
                *mixed,
                id,
                inner.control.clone(),
                inner.program_audio.clone(),
            );
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
        && (run.canvas == wanted || !inner.main.program_video.is_empty())
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
                run_program(&inner, CanvasId::Main, compositor, wanted.2, &stop)
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

/// Run (or stop) each extra canvas's compositor as the setup wants it.
fn ensure_extra_compositors<P: Platform>(inner: &Arc<Inner<P>>, state: &mut State<P>) {
    let mut canvases: Vec<CanvasId> = inner.extras.iter().map(|extra| extra.id).collect();
    for id in state.extra_compositors.keys() {
        if !canvases.contains(id) {
            canvases.push(*id);
        }
    }
    for canvas in canvases {
        ensure_extra_compositor(inner, state, canvas);
    }
}

fn ensure_extra_compositor<P: Platform>(inner: &Arc<Inner<P>>, state: &mut State<P>, id: CanvasId) {
    let wanted = state
        .setup
        .extra_canvas(id)
        .map(|v| v.canvas(&state.setup.canvas));
    if let (Some(run), Some(canvas)) = (state.extra_compositors.get(&id), &wanted)
        && run.canvas == (canvas.width, canvas.height, canvas.fps)
    {
        return;
    }
    if let Some(old) = state.extra_compositors.remove(&id) {
        stop_compositor(old);
    }
    let Some(extra) = inner.extras.get(id) else {
        return;
    };
    let output = &extra.output;
    let Some(canvas) = wanted else {
        output.enabled.store(false, Ordering::SeqCst);
        output.program.clear();
        output.program_rgb.clear();
        output.preview.clear();
        output
            .program_fps
            .store(0.0f64.to_bits(), Ordering::Relaxed);
        inner
            .timeline
            .set_on_air(id, std::iter::empty(), inner.platform.now());
        return;
    };
    // Broken canvas-local scene state would fail a new compositor at once:
    // only a new engine recovers from that, so it stays stopped.
    if output.scenes.broken() {
        let why = output.program.stopped();
        if !why
            .as_deref()
            .is_some_and(|w| w.contains("scene state is broken"))
        {
            let why = why.map(|w| format!(" ({w})")).unwrap_or_default();
            let whose = match id {
                CanvasId::Vertical => "the vertical canvas's".to_string(),
                other => format!("canvas {other:?}'s"),
            };
            output.program.replace_stop(format!(
                "{whose} scene state is broken{why}; only restarting the engine recovers it"
            ));
        }
        return;
    }
    let compositor = match Compositor::new(&inner.platform, (canvas.width, canvas.height)) {
        Ok(compositor) => compositor,
        Err(err) => {
            let message = format!(
                "could not set up the GPU for the {}: {err:#}",
                canvas_program_words(id)
            );
            output.program.stop(message.clone());
            inner.event(EngineEvent::Warning(message));
            return;
        }
    };
    // A new compositor feeds the program again (the canvas was turned off
    // and on, or resized, after a contained failure), so it no longer
    // reports the old one's failure (#96).
    output.program.resume();
    let stop = Arc::new(AtomicBool::new(false));
    let thread = thread::spawn({
        let inner = inner.clone();
        let stop = stop.clone();
        let fps = canvas.fps;
        move || {
            let _ = inner.worker(&compositor_worker(id), || {
                run_program(&inner, id, compositor, fps, &stop)
            });
        }
    });
    state.extra_compositors.insert(
        id,
        CompositorRun {
            stop,
            thread: Some(thread),
            canvas: (canvas.width, canvas.height, canvas.fps),
        },
    );
}

fn target_canvas<P: Platform>(inner: &Inner<P>, target: &str) -> Option<CanvasId> {
    inner.canvas_target(target).map(|(canvas, _)| canvas)
}

/// Minimum raster for a one-off scene picture. Live tiles may have left a
/// smaller frame in the shared slot before this request raised its demand.
fn scene_frame_size<P: Platform>(
    inner: &Inner<P>,
    target: &str,
    max_width: Option<u32>,
) -> Option<(u32, u32)> {
    let (canvas, _) = inner.scene_target(target)?;
    let scenes = inner.output(canvas)?.scenes.latest()?;
    Some(shared_frames::fitted(
        scenes.compiled.canvas,
        (max_width, None),
    ))
}

/// Where a monitor stream finds `target`'s frames, asked for at `fps` (the
/// program and the preview are drawn at least that often, on the canvas's
/// frame grid, and say which setup revision each frame shows).
fn stream_source<P: Platform>(
    inner: &Arc<Inner<P>>,
    target: &str,
    fps: u32,
    rgb: bool,
    max_size: (Option<u32>, Option<u32>),
) -> Result<frame_stream::Source<P::Frame>, String> {
    if inner.stopping.load(Ordering::SeqCst) {
        return Err("the live engine stopped".into());
    }
    let mut slot = slot_of_sized(inner, target, fps, max_size)?;
    let canvas = target_canvas(inner, target);
    if rgb
        && inner
            .canvas_target(target)
            .is_some_and(|(_, preview)| !preview)
        && let Some(output) = canvas.and_then(|canvas| inner.output(canvas))
    {
        slot = output.program_rgb.clone();
    }
    let grid = canvas.and_then(|canvas| {
        let state = inner.state.lock();
        let run = match canvas {
            CanvasId::Main => state.compositor.as_ref(),
            other => state.extra_compositors.get(&other),
        };
        inner.epoch().zip(run.map(|run| run.canvas.2))
    });
    let drawn = canvas.and_then(|canvas| inner.output(canvas).map(|output| output.drawn.clone()));
    Ok(frame_stream::Source { slot, grid, drawn })
}

/// The vertical program's frames for the virtual camera (target =
/// vertical), or why it has none: the canvas is off, or its program
/// stopped (a contained vertical failure: `engine.vertical.stopped` says
/// how it comes back).
/// The program slot of extra canvas `canvas` for the virtual camera
/// (target = vertical or canvas), or why it can't show it.
fn canvas_camera_slot<P: Platform>(
    inner: &Arc<Inner<P>>,
    canvas: CanvasId,
    fps: u32,
) -> Result<Arc<FrameSlot<P::Frame>>, String> {
    let slot = slot_of(inner, &canvas.program_target(), fps).map_err(|_| {
        if canvas.is_vertical() {
            "virtual_camera.target: vertical needs the vertical canvas; turn it on ([vertical]) \
             or give program, preview, scene or source"
                .to_string()
        } else {
            format!(
                "virtual_camera.canvas: the canvas {canvas:?} isn't on; add it ([[canvases]]) \
                 or give another target"
            )
        }
    })?;
    if let Some(why) = slot.stopped() {
        let recovery = if why.contains("scene state is broken")
            || why.contains("scene worker has broken state")
        {
            "restart the engine"
        } else {
            "after recording stops, turn the canvas off and on, or restart the engine"
        };
        return Err(format!(
            "virtual_camera: the {canvas} program stopped ({why}); {recovery}, then start \
             the camera again, or give program"
        ));
    }
    Ok(slot)
}

/// Where frames of `target` arrive: the program, `"preview"` (both kept
/// drawn at least `fps` times a second for a while after each ask) or a
/// video source. An error lists the valid targets.
fn slot_of<P: Platform>(
    inner: &Arc<Inner<P>>,
    target: &str,
    fps: u32,
) -> Result<Arc<FrameSlot<P::Frame>>, String> {
    slot_of_sized(inner, target, fps, (None, None))
}

fn slot_of_sized<P: Platform>(
    inner: &Arc<Inner<P>>,
    target: &str,
    fps: u32,
    max_size: (Option<u32>, Option<u32>),
) -> Result<Arc<FrameSlot<P::Frame>>, String> {
    if let Some(id) = target.strip_prefix(scene_feeds::PREFIX) {
        return scene_slot(inner, CanvasId::Main, id, fps, max_size);
    }
    if let Some((canvas, id)) = CanvasId::from_scene_target(target) {
        return scene_slot(inner, canvas, id, fps, max_size);
    }
    // The program and the preview are drawn while they are asked for.
    inner.want(target, fps);
    match target {
        PROGRAM => Ok(inner.main.program.clone()),
        "preview" => {
            if inner.main.preview_scene.lock().is_none() {
                return Err("nothing is staged in preview; stage a scene with `preview`".into());
            }
            Ok(inner.main.preview.clone())
        }
        target if inner.canvas_target(target).is_some() => {
            let (canvas, preview) = inner.canvas_target(target).expect("checked");
            let output = inner.output(canvas).ok_or_else(|| canvas_off(canvas))?;
            if preview {
                if output.preview_scene.lock().is_none() {
                    return Err(format!(
                        "nothing is staged in {canvas} preview; stage a scene with `preview --canvas {canvas}`"
                    ));
                }
                Ok(output.preview.clone())
            } else {
                Ok(output.program.clone())
            }
        }
        id => {
            let state = inner.state.read();
            // An audio source has no frames to wait for.
            match state.sources.get(id).and_then(|live| live.video.as_ref()) {
                Some(video) => Ok(video.slot.clone()),
                None => {
                    // A canvas target (`vertical-program`) of a canvas
                    // that's off.
                    if !state.sources.contains_key(id)
                        && let Some((canvas, _)) = CanvasId::from_target(id)
                    {
                        return Err(canvas_off(canvas));
                    }
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
                         \"program\", \"preview\", \"scene:ID\" or one of: {}",
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

/// Where frames of scene `id` arrive, drawn at least `fps` times a second
/// for a while after each ask by the scene feeds' thread (started here if
/// it isn't running). An error lists the scenes.
fn scene_slot<P: Platform>(
    inner: &Arc<Inner<P>>,
    canvas: CanvasId,
    id: &str,
    fps: u32,
    max_size: (Option<u32>, Option<u32>),
) -> Result<Arc<FrameSlot<P::Frame>>, String> {
    let prefix = canvas.scene_prefix();
    {
        let state = inner.state.lock();
        if !state.setup.scenes.iter().any(|scene| scene.id == id) {
            let ids: Vec<&str> = state.setup.scenes.iter().map(|s| s.id.as_str()).collect();
            return Err(format!(
                "target: {id:?} is not a scene; give \"{prefix}\" and one of: {}",
                if ids.is_empty() {
                    "(none)".to_string()
                } else {
                    ids.join(", ")
                }
            ));
        }
        if !state.setup.has_canvas(canvas) {
            return Err(canvas_off(canvas));
        }
    }
    let watched = match canvas {
        CanvasId::Main => &inner.scene_feeds,
        other => {
            &inner
                .extras
                .get(other)
                .ok_or_else(|| canvas_off(other))?
                .scene_feeds
        }
    };
    let (slot, start) = watched.want(id, fps, max_size)?;
    if start {
        let weak = Arc::downgrade(inner);
        let failed = watched.clone();
        let feeds = watched.clone();
        let platform = inner.platform.clone();
        let epoch = inner.epoch().unwrap_or_else(|| platform.now());
        let spawned = isocast_diagnostics::spawn(
            &if canvas.is_main() {
                "scene feeds".to_string()
            } else {
                format!("{canvas} scene feeds")
            },
            move || {
                let scenes = || {
                    weak.upgrade().and_then(|inner| match canvas {
                        CanvasId::Main => inner.main.scenes.latest(),
                        other => inner.output(other)?.scenes.latest(),
                    })
                };
                let stop = || {
                    weak.upgrade()
                        .is_none_or(|inner| inner.stopping.load(Ordering::SeqCst))
                };
                if let Err(error) = scene_feeds::run(&platform, &feeds, scenes, epoch, stop) {
                    let wait = feeds.failed(format!("{error:#}"));
                    isocast_diagnostics::diagnostic!("warn";
                        "scene feeds stopped: {error:#}; trying again in {} s", wait.as_secs()
                    );
                }
            },
            move |panic| {
                failed.failed(panic);
            },
        );
        if let Err(error) = spawned {
            watched.failed(format!("{error}"));
            return Err(format!("could not start drawing scenes: {error}"));
        }
    }
    Ok(slot)
}

/// Developer-mode `ISOCAST_COMPOSITOR_FAIL_AFTER` has failed the main
/// (`[0]`) or the vertical (`[1]`) canvas's compositor.
static FAULT_FIRED: [AtomicBool; 2] = [const { AtomicBool::new(false) }; 2];

/// The program's frame clock: at each tick, the scene on air drawn from the
/// frames nearest to it, for the program's outputs; and studio mode's
/// preview scene while someone wants it. With no outputs (no recording or
/// stream), the program is drawn only while clients' monitors ask for it,
/// at the rates they ask for (on the canvas's grid); what is on air is
/// still published at every tick.
fn run_program<P: Platform>(
    inner: &Arc<Inner<P>>,
    canvas: CanvasId,
    mut compositor: Compositor<P>,
    fps: u32,
    stop: &AtomicBool,
) {
    let Some(epoch) = inner.epoch() else {
        return;
    };
    let Some(output) = inner.output(canvas) else {
        return;
    };
    let platform = &inner.platform;
    platform.real_time_thread();
    compositor.set_epoch(epoch);
    // Isolated developer-mode fault exercise, on the live clock. The poison
    // variant unwinds with the canvas-local scene lock held, not shared state.
    let fault = |name| {
        isocast_diagnostics::developer()
            .then(|| std::env::var(name).ok())
            .flatten()
            .and_then(|value| {
                let (target, seconds) = value.split_once('=')?;
                let matches = target == canvas.as_str();
                matches
                    .then(|| seconds.parse::<f64>().ok())
                    .flatten()
                    .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
            })
    };
    let fail_after = fault("ISOCAST_COMPOSITOR_FAIL_AFTER");
    let poison_after = fault("ISOCAST_SCENES_POISON_AFTER");
    let frame = Duration::from_secs(1) / fps;
    // The version of the compiled scenes the compositor draws.
    let mut clock = inner.timeline.clocks.start(canvas.program(), frame);
    let mut version = 0;
    let mut warned = false;
    let mut retry_gpu = std::time::Instant::now();
    // The sources on air, kept between ticks.
    let mut on_air_sources: Vec<Arc<str>> = Vec::new();
    // The rates monitors show the program and the preview at.
    let mut program_rates = Rates::default();
    let mut preview_rates = Rates::default();
    let mut protected_program_rates = Rates::default();
    let mut camera_preview_rates = Rates::default();
    let first = platform.now().ticks_since(epoch, fps).max(0) as u64 + 1;
    // Where the frame rate is measured from: a time and the frames drawn
    // by then.
    let mut window = (platform.now(), output.frames_drawn.load(Ordering::Relaxed));
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
        // The live compositor survives between recordings. Reopening only
        // the shared cache would leave this owner on its lost device forever,
        // even though a new scene viewer could already obtain a healthy one.
        if compositor.device_lost() {
            output.frames_missed.fetch_add(1, Ordering::Relaxed);
            if !output.program_video.is_empty() {
                crate::governor::program_missed(canvas, 1);
            }
            if std::time::Instant::now() < retry_gpu {
                continue;
            }
            retry_gpu = std::time::Instant::now() + Duration::from_secs(1);
            match Compositor::new(platform, compositor.canvas()) {
                Ok(mut replacement) => {
                    replacement.set_epoch(epoch);
                    compositor = replacement;
                    version = 0;
                    warned = false;
                    inner.event(EngineEvent::Warning(format!(
                        "the {canvas:?} GPU device was lost; its compositor was recreated"
                    )));
                }
                Err(error) => {
                    if !std::mem::replace(&mut warned, true) {
                        inner.event(EngineEvent::Warning(format!(
                            "could not reopen the {canvas:?} GPU: {error:#}"
                        )));
                    }
                    continue;
                }
            }
        }
        if poison_after.is_some_and(|after| tick.saturating_since(epoch) >= after) {
            output.scenes.poison();
        }
        // Once per canvas per process, so a compositor started again
        // afterwards (the canvas turned off and on) keeps running.
        assert!(
            !(fail_after.is_some_and(|after| tick.saturating_since(epoch) >= after)
                && !FAULT_FIRED[usize::from(!canvas.is_main())].swap(true, Ordering::SeqCst)),
            "injected compositor failure"
        );
        assert!(
            !output.scenes.broken(),
            "the canvas scene worker has broken state"
        );
        // What this tick costs the thread, if it gives the program a frame.
        let cpu = platform.thread_cpu_time();
        let mut gave = None;
        let now = platform.now();
        let elapsed = now.saturating_since(window.0);
        if elapsed >= Duration::from_secs(1) {
            let drawn = output.frames_drawn.load(Ordering::Relaxed);
            let rate = drawn.saturating_sub(window.1) as f64 / elapsed.as_secs_f64();
            output.program_fps.store(rate.to_bits(), Ordering::Relaxed);
            window = (now, drawn);
        }
        // One atomic load, unless the scenes changed.
        if output.scenes.version() != version
            && let Some(scenes) = output.scenes.latest()
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
        let control = if output.follow_main.load(Ordering::SeqCst) {
            &inner.control
        } else {
            &output.control
        };
        let now = platform.now();
        program_rates.ask(output.control.take_program_wanted(), fps, now);
        preview_rates.ask(output.preview_wanted.swap(0, Ordering::SeqCst), fps, now);
        protected_program_rates.ask(
            output.protected_program_wanted.swap(0, Ordering::Relaxed),
            fps,
            now,
        );
        camera_preview_rates.ask(
            output.camera_preview_wanted.swap(0, Ordering::Relaxed),
            fps,
            now,
        );
        let outputs = !output.program_video.is_empty();
        compositor.protect_inputs(output.program_video.has_recording());
        let monitored = program_rates.any(now) && !crate::governor::previews_paused();
        let preview_wanted = (preview_rates.any(now) && !crate::governor::previews_paused())
            || camera_preview_rates.any(now);
        let obsolete = if platform.prefers_deadline_timer() {
            now >= epoch.add_ticks(n + 1, fps) + frame
        } else {
            now > tick + 3 * frame
        };
        if obsolete {
            if outputs || monitored {
                output.frames_missed.fetch_add(1, Ordering::Relaxed);
                if outputs {
                    crate::governor::program_missed(canvas, 1);
                }
            }
            continue;
        }
        let on_air = control.on_air(tick);
        // A frame on, so a stinger's video can start a frame early.
        let ahead = control.on_air(tick + frame);
        if compositor.visible(on_air.as_ref(), ahead.as_ref(), tick, &mut on_air_sources) {
            inner
                .timeline
                .set_on_air(canvas, on_air_sources.iter().map(|id| &**id), tick);
        }
        let monitor_due = program_rates.draws_preview(n, fps);
        if outputs || monitor_due || protected_program_rates.draws(n, fps) {
            let draw_started = std::time::Instant::now();
            match compositor.draw(
                Feed::Program,
                on_air.as_ref(),
                tick,
                tick + frame / 2,
                monitored,
            ) {
                Ok(Some((image, reused))) => {
                    gave = Some((reused, compositor.draws(), compositor.animated()));
                    let inner = inner.clone();
                    let scene = on_air
                        .as_ref()
                        .filter(|on_air| on_air.from.is_none())
                        .map(|on_air| on_air.scene.clone());
                    image.then(move |(image, rgb), gpu_wait| {
                        let Some(output) = inner.output(canvas) else {
                            return;
                        };
                        // Publish the stamp and pixels together, only after
                        // the GPU has finished with this tick's inputs.
                        frame_stream::drawn(&output.drawn, tick, stamp);
                        let feeds = inner.feeds(canvas);
                        if let Some(scene) = scene {
                            feeds.publish(
                                &scene,
                                version,
                                rgb.as_ref().unwrap_or(&image).clone(),
                                tick,
                                Some(Feed::Program),
                                P::retain_frame,
                            );
                        } else {
                            feeds.clear_reuse(Feed::Program);
                        }
                        if let Some(rgb) = rgb {
                            output.program_rgb.push(rgb, tick);
                        }
                        output.program.push(image.clone(), tick);
                        output.program_video.video_retained(
                            &image,
                            tick,
                            Some(canvas),
                            P::retain_frame,
                        );
                        output.frames_drawn.fetch_add(1, Ordering::Relaxed);
                        output
                            .timings
                            .lock()
                            .gpu(if reused { Duration::ZERO } else { gpu_wait });
                    });
                }
                Ok(None) => {
                    output.frames_missed.fetch_add(1, Ordering::Relaxed);
                    if outputs {
                        crate::governor::program_missed(canvas, 1);
                    }
                }
                Err(err) => {
                    output.frames_missed.fetch_add(1, Ordering::Relaxed);
                    if outputs {
                        crate::governor::program_missed(canvas, 1);
                    }
                    if !std::mem::replace(&mut warned, true) {
                        inner.event(EngineEvent::Warning(format!(
                            "the {canvas:?} program could not draw a frame: {err:#}"
                        )));
                    }
                }
            }
            output.timings.lock().draw_wall(draw_started.elapsed());
        } else if !monitored && !outputs && !protected_program_rates.any(now) {
            // Nothing wants the program: let its canvas go back to the pool.
            output.program.clear();
            output.program_rgb.clear();
            let feeds = inner.feeds(canvas);
            feeds.clear_reuse(Feed::Program);
            compositor.forget(Feed::Program);
        }
        let scene = preview_wanted
            .then(|| output.preview_scene.lock().clone())
            .flatten();
        match scene {
            Some(scene)
                if preview_rates.draws_preview(n, fps) || camera_preview_rates.draws(n, fps) =>
            {
                compositor.protect_inputs(false);
                let staged = OnAir { scene, from: None };
                if let Ok(Some((image, _))) =
                    compositor.draw(Feed::Preview, Some(&staged), tick, tick + frame / 2, true)
                {
                    let slot = output.preview.clone();
                    let inner = inner.clone();
                    image.then(move |(rgb, _), _| {
                        let feeds = inner.feeds(canvas);
                        feeds.publish(
                            &staged.scene,
                            version,
                            rgb.clone(),
                            tick,
                            Some(Feed::Preview),
                            P::retain_frame,
                        );
                        slot.push(rgb, tick);
                    });
                }
            }
            Some(_) => {}
            None => {
                output.preview.clear();
                let feeds = inner.feeds(canvas);
                feeds.clear_reuse(Feed::Preview);
                compositor.forget(Feed::Preview);
            }
        }
        if let (Some((reused, draws, animated)), Some(from), Some(to)) =
            (gave, cpu, platform.thread_cpu_time())
        {
            let spent = to.saturating_sub(from);
            output.timings.lock().tick(spent, reused, draws, animated);
        }
        // Pictures of another size than layout took them to be: the scenes
        // are laid out again, and adopted at a coming tick.
        let resized = compositor.resized();
        if resized.len() > 0 {
            output.scenes.resized(resized);
        }
    }
    if let Err(error) = compositor.drain() {
        inner.event(EngineEvent::Warning(format!(
            "the {canvas:?} program could not finish its GPU frames: {error:#}"
        )));
    }
}

fn program_errors<P: Platform>(
    inner: &Inner<P>,
    plans: &[Arc<FilePlan>],
    errors: &mut Vec<String>,
) {
    let canvases = std::iter::once(CanvasId::Main).chain(inner.extras.iter().map(|e| e.id));
    for canvas in canvases {
        if plans.iter().any(|plan| plan.source == canvas.program())
            && let Some(reason) = inner
                .output(canvas)
                .and_then(|output| output.program.stopped())
        {
            errors.push(format!(
                "{}: {reason}; program output failed",
                canvas.program()
            ));
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

    /// Reapply the live ceiling to cached monitor demand, not camera outputs.
    fn draws_preview(&self, n: u64, canvas_fps: u32) -> bool {
        self.0.iter().any(|(fps, _)| {
            let fps = crate::governor::monitor_fps(*fps);
            fps > 0 && n.is_multiple_of(u64::from((canvas_fps / fps).max(1)))
        })
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
            let mut state = inner.state.lock();
            let now = inner.platform.now();
            if now >= next_retry {
                next_retry = now + RETRY_INTERVAL;
                let failed = state.sources.iter().any(|(id, live)| {
                    matches!(
                        live.run,
                        SourceRun::Failed | SourceRun::WaitingForPermission
                    ) && !state.pins.contains(id)
                });
                let mut wanted = inner.wanted.lock();
                if failed && wanted.is_none() {
                    *wanted = Some(state.setup.clone());
                    inner.wake.notify_all();
                }
            }
            let setup = state.setup.clone();
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
                            .and_then(|audio| audio.feed.stopped.lock().clone())
                    })
                    .or_else(|| live.opened.as_ref().and_then(|o| o.failure()));
                if let Some(reason) = stopped {
                    if let Some(video) = &live.video {
                        video.slot.stop(reason.clone());
                    }
                    if let Some(audio) = &live.audio {
                        let last = audio.last_end.load(Ordering::Relaxed);
                        // Media and browser audio may be silent for a while
                        // before it stops: its last sound isn't the stop.
                        let sparse = super::file::AudioTiming::new(setup.source(id)).sparse;
                        let at = if last == i64::MIN || sparse {
                            now
                        } else {
                            Time::from_nanos(last)
                        };
                        inner.audio_stopped(id, at, &reason);
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
            let mut loudest = inner.loudest.lock();
            for level in &levels {
                let entry = loudest
                    .entry(level.source.clone())
                    .or_insert((f64::MIN, f64::MIN));
                entry.0 = entry.0.max(level.peak_db);
                entry.1 = entry.1.max(level.rms_db);
            }
        }
        *inner.levels.lock() = levels.clone();
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
    /// Its files' encoders take only Y'CbCr (H.264, HEVC on a platform
    /// whose encoders want Y'CbCr).
    ycbcr_only: bool,
    /// Every source's filters, if the files record this one filtered.
    filters: Option<SourceFilters>,
    /// When the engine's clock started: mask animations on `clock` run
    /// from it, as in the program (#115).
    epoch: Time,
    /// Where frames are filtered or converted, made when the first frame
    /// needs it.
    stage: Option<Box<IsoFrames<P>>>,
    outputs: Vec<IsoOutput<P::Frame>>,
}

/// Where an ISO file's frames go, and its share of the hardware encoders:
/// ticks it would refuse are left out before they are drawn (#29).
struct IsoOutput<F> {
    tx: Output<F>,
    /// From which frame time its file wants every picture (see
    /// `IsoDemand`): its file starts on a picture, and so does the next
    /// take's at a split.
    wanted: Arc<IsoDemand>,
    credit: super::pressure::IsoCredit,
    /// It takes the current tick's frame.
    admitted: bool,
}

/// The GPU the recording's ISO files filter and convert frames with, one
/// for all of them, opened when the first one needs it.
type IsoGpu = Mutex<Option<Gpu>>;

impl<P: Platform> IsoRunner<P> {
    /// Whether `image` must go through the stage before the files take it.
    fn needs_stage(&self, platform: &P, image: &P::Frame) -> bool {
        self.filters.is_some()
            || (self.ycbcr_only && !platform.frame_is_ycbcr(image))
            || (platform.encoder_frame_format() == crate::EncoderFrameFormat::Bgra
                && platform.frame_size(image) != self.size)
    }

    /// The stage, made if it isn't yet. Setting one up takes a while (a
    /// few frames' time), so it is made before the recording starts when
    /// the source's frames show it is needed.
    fn stage(&mut self, platform: &P, gpu: &IsoGpu) -> Result<&mut IsoFrames<P>> {
        if self.stage.is_none() {
            let gpu = {
                let mut gpu = gpu.lock();
                if gpu.as_ref().is_some_and(Gpu::is_lost) {
                    *gpu = None;
                }
                match &*gpu {
                    Some(gpu) => gpu.clone(),
                    None => gpu.insert(Gpu::for_platform(platform)?).clone(),
                }
            };
            let stage = IsoFrames::new(
                platform,
                gpu,
                &self.id,
                self.size,
                self.filters.clone(),
                self.epoch,
                isocast_protocol::ScaleFilter::Bilinear,
            )
            .with_context(|| format!("could not prepare {:?}'s frames for its file", self.id))?;
            self.stage = Some(Box::new(stage));
            isocast_diagnostics::diagnostic!("debug"; "{}: opened ISO GPU stage", self.id);
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
    ) -> Result<Option<PendingFrame<P::Frame>>> {
        if !self.needs_stage(platform, &image) {
            return Ok(Some(PendingFrame::ready(image)));
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

/// When a split asked for at `now` begins the next take: the next whole
/// second from the session's `epoch` (files start there, so it falls on
/// every file's frame grid) at least [`SPLIT_LEAD`] away, and at least a
/// second after the `current` take's start.
fn split_time(epoch: Time, current: Time, now: Time) -> Time {
    // The first whole second at or after `time`: whole ticks from `epoch`
    // are exactly where each file's frames fall (`Time::add_ticks`).
    let second_after = |time: Time| {
        let mut ticks = time.ticks_since(epoch, 1).max(0) as u64;
        if epoch.add_ticks(ticks, 1) < time {
            ticks += 1;
        }
        ticks
    };
    let ticks = second_after(now + SPLIT_LEAD).max(second_after(current + Duration::from_secs(1)));
    epoch.add_ticks(ticks, 1)
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
    let mut last_gpu: Option<Completion> = None;
    let governor_pin = crate::governor::iso_pin(&runner.id);
    let mut retry_gpu = std::time::Instant::now();
    for n in 0.. {
        let tick = epoch.add_ticks(n, fps);
        // Frames up to half a frame after the tick are nearest to it; wait
        // another half frame for them to arrive.
        runner.clock.sleep_until(platform, tick + frame);
        if tick >= shared.end() {
            break;
        }
        if runner
            .stage
            .as_ref()
            .is_some_and(|stage| stage.device_lost())
        {
            // The old device suppresses its hand-offs. Start a new chain:
            // linking new pictures behind its fence would suppress those
            // too. Missing ticks remain visible in file drop accounting.
            runner.stage = None;
            last_gpu = None;
            isocast_diagnostics::diagnostic!("warn";
                "{}: GPU device lost; recreating its ISO stage", runner.id);
        }
        // Its file ends here, after the frames it captured before it
        // stopped (one newer than the last tick's pick is still due); the
        // rest of the recording goes on.
        if let Some(reason) = runner.slot.stopped()
            && runner
                .slot
                .peek_until_timed(Time::MAX)
                .is_none_or(|(_, at)| at + frame / 2 <= tick)
        {
            bail!(
                "{}: the capture stopped, so its file ends early: {reason}",
                runner.id
            );
        }
        let now = platform.now();
        let obsolete = if platform.prefers_deadline_timer() {
            now >= epoch.add_ticks(n + 1, fps) + frame
        } else {
            now > tick + 3 * frame
        };
        if obsolete {
            continue;
        }
        let Some(image) = runner.slot.newest_until(tick + frame / 2) else {
            continue;
        };
        let Some(image) = P::retain_frame(&image, crate::FrameUse::Recording) else {
            continue;
        };
        // Encoder admission first, so a refused tick is never drawn.
        let mut admitted = false;
        for output in &mut runner.outputs {
            output.admitted = tick.as_nanos() >= output.wanted.from()
                || output
                    .credit
                    .admit(crate::governor::iso_ceiling(&governor_pin));
            admitted |= output.admitted;
        }
        let image = if admitted {
            if runner.stage.is_none() && runner.needs_stage(platform, &image) {
                let now = std::time::Instant::now();
                if now < retry_gpu {
                    // Missing pictures still count as drops against the
                    // file's frame grid. A failing replacement must not
                    // rebuild shaders and pools at the ISO frame rate.
                    continue;
                }
                retry_gpu = now + Duration::from_secs(1);
            }
            // None: its frames are all still being written.
            let prepared = runner.prepare(platform, gpu, image, tick);
            if prepared.is_err()
                && runner
                    .stage
                    .as_ref()
                    .is_some_and(|stage| stage.device_lost())
            {
                continue;
            }
            let Some(image) = prepared? else {
                continue;
            };
            Some(image)
        } else {
            None
        };
        let outputs: Vec<_> = runner
            .outputs
            .iter()
            .map(|output| (output.tx.clone(), output.admitted))
            .collect();
        let send = move |image: Option<P::Frame>| {
            for (output, admitted) in outputs {
                let packet = match &image {
                    Some(image) if admitted => {
                        match P::retain_frame(image, crate::FrameUse::Recording) {
                            Some(frame) => Packet::Video { frame, pts: tick },
                            None => Packet::Skipped { pts: tick },
                        }
                    }
                    _ => Packet::Skipped { pts: tick },
                };
                let _ = output.try_send(packet);
            }
        };
        match image {
            Some(mut image) => {
                // Unconverted pictures and skipped ticks must not overtake
                // an earlier conversion still in flight on this ISO clock.
                if image.done.is_none() {
                    image.done = last_gpu.clone();
                }
                last_gpu = image.done.clone();
                image.then(move |image, _| send(Some(image)));
            }
            None => match &last_gpu {
                Some(done) => {
                    if let Err(error) = done.ready() {
                        if runner
                            .stage
                            .as_ref()
                            .is_some_and(|stage| stage.device_lost())
                        {
                            continue;
                        }
                        return Err(error);
                    }
                    done.then(move |_| send(None));
                }
                None => send(None),
            },
        }
    }
    if let Some(stage) = &mut runner.stage {
        stage.drain()?;
    }
    if let Some(done) = last_gpu {
        done.wait()?;
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
    let setup = inner.state.read().setup.clone();
    let setup = &*setup;
    let mut warnings = Vec::new();
    let iso = &setup.recording.iso;
    let program = &setup.recording.program;
    // The extra canvases whose programs are recorded (`[vertical]`'s and
    // `[[canvases]]`'), each with its output and the control whose scene
    // timeline its files follow: main's while it follows main, else its
    // own.
    let mut extras_recorded: Vec<(CanvasId, &CanvasOutput<P>, &SessionControl)> = Vec::new();
    for canvas in setup.extra_canvas_ids() {
        let recorded = setup
            .for_canvas(canvas)
            .is_some_and(|s| s.recording.program.enabled);
        if !recorded {
            continue;
        }
        let output = inner
            .output(canvas)
            .ok_or_else(|| anyhow::anyhow!("{}", canvas_off(canvas)))?;
        let follows = setup
            .extra_canvas(canvas)
            .expect("extra canvas setup")
            .follow_main;
        extras_recorded.push((
            canvas,
            output,
            if follows {
                &**control
            } else {
                &*output.control
            },
        ));
    }
    let extras_recorded = extras_recorded;
    let vertical_enabled = !extras_recorded.is_empty();
    // Each control a recorded extra canvas follows once (main's aside):
    // they record their scene timelines.
    let mut extra_controls: Vec<&SessionControl> = Vec::new();
    for (_, _, extra) in &extras_recorded {
        if !std::ptr::eq(*extra, &**control)
            && !extra_controls.iter().any(|c| std::ptr::eq(*c, *extra))
        {
            extra_controls.push(extra);
        }
    }
    let extra_controls = extra_controls;
    // The manifest's scene timelines of the extra canvases.
    let canvas_controls: Vec<(CanvasId, &SessionControl)> = extras_recorded
        .iter()
        .map(|(canvas, _, control)| (*canvas, *control))
        .collect();
    let canvas_controls = &canvas_controls[..];
    let extra_output = |source: &str| {
        extras_recorded
            .iter()
            .find(|(canvas, _, _)| canvas.program() == source)
            .map(|(_, output, _)| *output)
    };
    if program.enabled
        && let Some(reason) = inner.main.program.stopped()
    {
        bail!(
            "program output has failed: {reason}; stop the recording and restart the live engine"
        );
    }
    for (canvas, output, _) in &extras_recorded {
        if let Some(reason) = output.program.stopped() {
            bail!(
                "{} output has failed: {reason}; restart the live engine",
                canvas_program_words(*canvas)
            );
        }
    }
    let iso_enabled = iso.enabled && !iso.sources.is_empty();
    if !iso_enabled && !program.enabled && !vertical_enabled {
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
    #[cfg(not(windows))]
    let _folder_lock = FolderLock::hold(&folder);
    #[cfg(windows)]
    let folder_lock = FolderLock::hold(&folder)?;
    // Every checkpoint uses the original Windows folder object even if the
    // path or a parent junction is renamed meanwhile. Unix stays unchanged.
    #[cfg(windows)]
    let write_manifest =
        |path: &Path, manifest: &SessionManifest| folder_lock.write(path, manifest, false);
    #[cfg(windows)]
    let write_manifest_durable =
        |path: &Path, manifest: &SessionManifest| folder_lock.write(path, manifest, true);
    #[cfg(windows)]
    let finish_manifest =
        |path: &Path, manifest: &SessionManifest| folder_lock.finish(path, manifest, control);
    #[cfg(not(windows))]
    let finish_manifest = write_manifest;
    #[cfg(windows)]
    let publish_final = |path: &Path, manifest: &SessionManifest| {
        folder_lock.save_final(path, manifest)?;
        folder_lock.write(path, manifest, false)
    };
    #[cfg(not(windows))]
    let publish_final = write_manifest;

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
    if program.enabled || vertical_enabled {
        required.extend(program.audio.iter().map(String::as_str));
    }
    let source_wait = required
        .iter()
        .filter_map(|id| setup.source(id))
        .map(|source| platform.source_open_timeout(&source.kind) + Duration::from_secs(5))
        .fold(SOURCE_WAIT, std::cmp::max);
    let waiting_since = platform.now();
    loop {
        let state = inner.state.read();
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
        if !pending || platform.now().saturating_since(waiting_since) >= source_wait {
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

    let _recording_activity = platform.recording_activity()?;

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
        cameras: BTreeMap::new(),
        recording: next_recording(),
    };
    let mut slots: BTreeMap<String, Arc<FrameSlot<P::Frame>>> = BTreeMap::new();
    let mut readers: BTreeMap<String, Arc<Routes<P::Frame>>> = BTreeMap::new();
    {
        let mut state = inner.state.lock();
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
            if let Some(camera) = live.opened.as_ref().and_then(|o| o.camera_metadata()) {
                formats.cameras.insert(id.clone(), camera);
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
        inner.speeds.lock().remove(&folder);
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
        match inner.main.encoders.warm(engine, &spec, &options.stop) {
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
    let _warm_vertical = crate::stream::warm_vertical(engine, setup, &options.stop, &mut warnings);
    // Every other canvas's replay may join later too (#68, #84), including
    // with its default settings and no stream. Place each before the
    // writers; a stream of the same format shares that session, not a
    // second encoder.
    let _warm_canvas_replays: Vec<_> = setup
        .extra_canvas_ids()
        .into_iter()
        .filter_map(|canvas| {
            let result = (|| {
                let spec = crate::replay::canvas_encoder_spec(
                    engine,
                    canvas,
                    &setup.recording.replay,
                    setup.stream_of(canvas),
                )?;
                engine
                    .canvas_encoders(canvas)?
                    .warm_canvas(engine, canvas, &spec, &options.stop)
            })();
            match result {
                Ok(subscription) => Some(subscription),
                Err(error) => {
                    warnings.push(format!(
                        "could not prepare the {} replay encoder before recording: {error:#}",
                        canvas_words(canvas)
                    ));
                    None
                }
            }
        })
        .collect();
    if options.stop.load(Ordering::SeqCst) {
        bail!("the recording was stopped before it started, so nothing was recorded");
    }
    // Each ISO source's frame clock, with the stage its frames go through
    // set up now if its frames need one: that takes a few frames' time.
    let iso_gpu = IsoGpu::default();
    let ycbcr_only = platform.encoder_frame_format() == crate::EncoderFrameFormat::Ycbcr
        && matches!(iso.encoder.codec, Codec::H264 | Codec::Hevc);
    // Encoders that take BGRA as drawn open for the pixel format a raw ISO
    // source's frames come in, so those sources' first frames decide their
    // stage before the writers open.
    let raw_inputs = platform.encoder_frame_format() == crate::EncoderFrameFormat::Bgra;
    let mut runners: BTreeMap<String, IsoRunner<P>> = BTreeMap::new();
    for plan in plans.iter().filter(|plan| !is_program(plan)) {
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
            epoch: inner.epoch().expect("the engine clock has started"),
            stage: None,
            outputs: Vec::new(),
        };
        let newest = match raw_inputs && runner.filters.is_none() {
            true => {
                match wait_for_first_picture(&runner.slot, FIRST_FRAME_TIMEOUT, &options.stop) {
                    Ok((frame, _)) => Some(frame),
                    Err(NoPicture::Stopped) => {
                        bail!(
                            "the recording was stopped before it started, so nothing was recorded"
                        )
                    }
                    Err(NoPicture::TimedOut) => bail!(
                        "recording.iso.sources: {id:?} has no input picture yet (waited {} s after \
                         it opened).\nCheck that it shows a picture in a preview, or set its \
                         fps (for example 30) in its source settings.",
                        FIRST_FRAME_TIMEOUT.as_secs()
                    ),
                }
            }
            false => runner.slot.newest(),
        };
        if runner.filters.is_some()
            || newest.is_some_and(|frame| runner.needs_stage(platform, &frame))
        {
            runner.stage(platform, &iso_gpu)?;
        }
        runners.insert(id.to_string(), runner);
    }
    // The sources whose files take their frames as captured (no stage):
    // their writers open for those frames' pixel format. Kept for splits.
    let writer_inputs: WriterInputs<P::Frame> = match raw_inputs {
        true => runners
            .iter()
            .filter(|(_, runner)| runner.stage.is_none())
            .map(|(id, runner)| (id.clone(), runner.slot.clone()))
            .collect(),
        false => WriterInputs::new(),
    };
    // Draw each recorded program before choosing the epoch, so its first
    // draws (a canvas let go while nothing watched it, cold GPU pipelines)
    // don't consume the take's first slots (#150). Software GPU drivers
    // must finish one; elsewhere warming is bounded and the recording
    // starts anyway, its program's skipped head counted as dropped.
    let required = platform.recording_needs_program_frame();
    let warm_started = std::time::Instant::now();
    let warm_until = warm_started + PROGRAM_WARM_TIMEOUT;
    let program_output = |plan: &FilePlan| {
        if plan.source == PROGRAM {
            &inner.main
        } else {
            extra_output(&plan.source).expect("recorded canvas output")
        }
    };
    // Every program starts drawing at once, so several idle canvases warm
    // together within the one bound rather than one after another.
    for plan in plans.iter().filter(|plan| is_program(plan)) {
        program_output(plan)
            .protected_program_wanted
            .fetch_max(plan.spec.video.fps, Ordering::Relaxed);
    }
    for plan in plans.iter().filter(|plan| is_program(plan)) {
        // A picture from the last two ticks shows a program already drawing
        // (a preview or monitor watching): no wait. Ticks publish a frame
        // after they're due, so an idle canvas still has to draw one.
        let since = platform.now() - Duration::from_secs(2) / plan.spec.video.fps;
        let deadline = if required {
            std::time::Instant::now() + FIRST_FRAME_TIMEOUT
        } else {
            warm_until
        };
        loop {
            if options.stop.load(Ordering::SeqCst) {
                bail!("the recording was stopped before it started, so nothing was recorded");
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                if !required {
                    isocast_diagnostics::diagnostic!("warn";
                        "recording: {} drew no picture within {} ms of warming; starting anyway",
                        plan.source,
                        PROGRAM_WARM_TIMEOUT.as_millis()
                    );
                    break;
                }
                bail!(
                    "{}: the program did not finish preparing a picture within {} seconds; no recording files were opened",
                    plan.source,
                    FIRST_FRAME_TIMEOUT.as_secs()
                );
            }
            // Required preparation must keep drawing even when disposable
            // previews are paused. The slot publishes after GPU completion.
            program_output(plan)
                .protected_program_wanted
                .fetch_max(plan.spec.video.fps, Ordering::Relaxed);
            let slot =
                slot_of(inner, &plan.source, plan.spec.video.fps).map_err(anyhow::Error::msg)?;
            if slot
                .wait_from(since, left.min(Duration::from_millis(100)))
                .is_some()
            {
                break;
            }
        }
    }
    if plans.iter().any(|plan| is_program(plan)) {
        isocast_diagnostics::diagnostic!("info";
            "recording: program warm-up took {} ms",
            warm_started.elapsed().as_millis()
        );
    }
    let (writers, admission_notes) = create_writers(platform, plans, &writer_inputs)?;

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
        let wanted = Arc::new(IsoDemand::default());
        if plan.source == PROGRAM {
            let key = inner.main.program_video.add(tx.clone(), 0, epoch, None);
            routes.0.push((inner.main.program_video.clone(), key));
        } else if let Some(output) = extra_output(&plan.source) {
            let video = &output.program_video;
            let key = video.add(tx.clone(), 0, epoch, None);
            routes.0.push((video.clone(), key));
        } else if let Some(runner) = runners.get_mut(plan.source.as_str()) {
            runner.outputs.push(IsoOutput {
                tx: tx.clone(),
                wanted: wanted.clone(),
                credit: Default::default(),
                admitted: false,
            });
        }
        for (track, id) in plan.audio.iter().enumerate() {
            let reader = if id == PROGRAM {
                Some(&inner.program_audio)
            } else {
                readers.get(id)
            };
            let Some(reader) = reader else {
                continue;
            };
            // ISO files that record sources' audio filtered get it through
            // a filter stage of their own, apart from the mix's.
            let filter = setup
                .source(id)
                .filter(|source| iso.filtered && source.audio_filters.iter().any(|f| f.enabled))
                .map(|_| {
                    AudioStage::new(
                        id,
                        formats.audio[id] as usize,
                        control.audio_filters.clone(),
                    )
                });
            // Sparse sources legitimately leave timestamp gaps, so count their
            // actual queue losses instead of treating every silent gap as loss.
            let key =
                reader.add_file_audio(tx.clone(), track, epoch, filter, missed_audio.clone(), plan);
            routes.0.push((reader.clone(), key));
        }
        receivers.push((rx, missed_audio, wanted));
    }
    // Frames captured before the recording don't count in its report.
    let mut counted: Vec<(String, Arc<FrameSlot<P::Frame>>)> = plans
        .iter()
        .filter(|p| !is_program(p))
        .map(|p| (p.source.clone(), slots[&p.source].clone()))
        .collect();
    if program.enabled {
        counted.push((PROGRAM.to_string(), inner.main.program.clone()));
    }
    for (canvas, output, _) in &extras_recorded {
        counted.push((canvas.program().to_string(), output.program.clone()));
    }
    let captured_before: Vec<u64> = counted
        .iter()
        .map(|(_, slot)| slot.captured.load(Ordering::Relaxed))
        .collect();

    // Before the start is announced, so a split asked for as soon as it
    // is goes to this recording.
    control.begin_recording(epoch, clock.clone());
    for extra in &extra_controls {
        extra.begin_recording(epoch, clock.clone());
    }
    if let Some(duration) = options.duration {
        control.end_recording(epoch + duration);
        for extra in &extra_controls {
            extra.end_recording(epoch + duration);
        }
    }
    // However the recording ends, a split asked for meanwhile is answered.
    let _finish = FinishRecording(control);
    let _extra_finish: Vec<FinishRecording> = extra_controls
        .iter()
        .map(|extra| FinishRecording(extra))
        .collect();
    if let Some(on_start) = options.on_start {
        let paths: Vec<PathBuf> = plans.iter().map(|p| p.spec.path.clone()).collect();
        on_start(&paths, &first.manifest_path, &admission_notes);
    }
    let started_at = Local::now() + epoch.saturating_since(platform.now());
    let mut takes = vec![Take::begin(first, epoch, started_at)];
    // Audio sources that stopped so far (see `Inner::audio_stops`).
    let stops = || inner.audio_stops.lock().clone().unwrap_or_default();
    let write = |take: &Take, status, errors: &[String]| {
        write_manifest(
            &take.manifest_path,
            &take_manifest(
                platform,
                setup,
                take,
                status,
                errors,
                &stops(),
                Some(control),
                canvas_controls,
                &formats.cameras,
            ),
        )
    };
    write(&takes[0], SessionStatus::Recording, &[])?;

    let (done_tx, done_rx) = mpsc::channel::<Done>();
    // Takes reported to `on_take` so far, and those before that cursor
    // passed over because their metadata failed (the end tries them again).
    let mut reported = 0;
    let mut withheld = Vec::new();
    let mut errors = thread::scope(|scope| {
        let shared = &shared;
        let mut split_senders = Vec::new();
        // Each file's demand for every picture, which a split sets before
        // its writer thread can receive it (see `IsoDemand`).
        let mut demands = Vec::new();
        let writer_threads: Vec<_> = writers
            .into_iter()
            .zip(receivers)
            .zip(takes[0].plans.clone())
            .enumerate()
            .map(|(index, ((writer, (packets, missed, wanted)), plan))| {
                let (split_tx, split_rx) = split_channel();
                split_senders.push(split_tx);
                demands.push(wanted.clone());
                let done = done_tx.clone();
                let clock = clock.clone();
                // Program writers have producers too. If either compositor
                // stops, its audio must not keep a dead video writer taking
                // split handoffs whose first frame can never arrive.
                let source = match plan.source.as_str() {
                    PROGRAM => Some(inner.main.program.clone()),
                    id if extra_output(id).is_some() => {
                        extra_output(id).map(|output| output.program.clone())
                    }
                    id => slots.get(id).cloned(),
                };
                scope.spawn(move || {
                    platform.real_time_thread();
                    write_files(
                        writer, plan, index, packets, split_rx, done, shared, clock, missed,
                        source, wanted,
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
        let mut handover = false;
        let mut audio_stops_seen = inner.audio_stop_changes.load(Ordering::SeqCst);
        // A pause's commit barrier under way: its generation, cut and deadline.
        let mut pending_commit: Option<(u64, Time, Time)> = None;
        let disk_probe = DiskProbe::start(platform, &folder);
        let mut disk_meter = DiskMeter::default();
        let mut losses = 0;
        let mut next_status = platform.now() + STATUS_INTERVAL;
        let mut output_activity = (1, 0);
        let mut next_space_check = platform.now();
        let mut disk_full = None;
        let stop_below = reserve.max((rate * MIN_FREE_SECONDS).max(MIN_FREE_BYTES) as u64);
        let coordinator = isocast_diagnostics::contain("recording coordinator", || {
            loop {
                let now = platform.now();
                if inner.emergency.load(Ordering::SeqCst)
                    || inner.state.is_broken()
                    || inner.main.scenes.broken()
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
                let activity = (
                    takes.len(),
                    current
                        .plans
                        .iter()
                        .filter(|plan| plan.output_active.load(Ordering::Acquire) == 1)
                        .count(),
                );
                if let Some(on_status) = options.on_status
                    && (now >= next_status || (activity.1 > 0 && activity != output_activity))
                {
                    next_status = now + STATUS_INTERVAL;
                    output_activity = activity;
                    // The loudest of each source since the last status.
                    let levels = std::mem::take(&mut *inner.loudest.lock())
                        .into_iter()
                        .map(|(source, (peak_db, rms_db))| AudioLevel {
                            source,
                            peak_db,
                            rms_db,
                            channels: None,
                        })
                        .collect();
                    let mut status = live_status(&current.plans, levels, shared, now, &clock);
                    status.monitor = inner.monitor.lock().status();
                    status.handover = control.handover(now);
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
                                &stops(),
                                Some(control),
                                canvas_controls,
                                &formats.cameras,
                            ),
                        )
                    } else {
                        write(current, SessionStatus::Recording, errors)
                    };
                    let mut state = control.state.lock();
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
                        .filter_map(|p| p.commit_error.lock().clone())
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
                            let cuts = clock.state.read();
                            committed
                                .iter()
                                .map(|p| p.recoverable(current.epoch, &cuts))
                                .try_fold(f64::INFINITY, |min, r| r.map(|r| min.min(r)))
                                .filter(|r| r.is_finite())
                        } else {
                            None
                        };
                        clock.set_recoverable(at, recoverable);
                        control.state.lock().pause_outcome =
                            Some((generation, recoverable, errors.clone()));
                        checkpoint(generation, &errors, true);
                        pending_commit = None;
                    }
                }
                if !all_started
                    && current
                        .plans
                        .iter()
                        .zip(&current.results)
                        .all(|(p, result)| {
                            p.start.load(Ordering::SeqCst) != i64::MIN || result.is_some()
                        })
                {
                    all_started = true;
                    // Best effort: the final manifest is written at the end.
                    let _ = write(current, SessionStatus::Recording, &[]);
                }
                // An audio source stopped (or came back): say so in the
                // manifest at once, so a crash keeps it.
                let changes = inner.audio_stop_changes.load(Ordering::SeqCst);
                if changes != audio_stops_seen {
                    audio_stops_seen = changes;
                    let _ = write(current, SessionStatus::Recording, &[]);
                }
                // A stop asked for comes before a split asked for at the same
                // time: the split is then refused, rather than announcing a take
                // the stop ends before it begins.
                if options.stop.load(Ordering::SeqCst) {
                    shared.stop_at(now.max(current.epoch + MIN_TAKE));
                }
                // A split: the next take's files start at a moment on every
                // file's frame grid (the next whole second on the recording's
                // clock), at least a second after the current take's start.
                if let Some(reply) = control.take_split() {
                    let number = takes.len() as u32 + 1;
                    let next = plan_take(setup, &formats, &folder, number)
                        .and_then(|plan| {
                            Ok((create_writers(platform, &plan.plans, &writer_inputs)?, plan))
                        })
                        .map_err(|err| SplitRefused {
                            ending: false,
                            busy: false,
                            message: format!("split refused; the current take continues: {err:#}"),
                        })
                        .and_then(|((writers, admission_notes), plan)| {
                            // Timed once the files exist, so none of them misses
                            // the start.
                            let now = platform.now();
                            let current = takes.last().expect("a take");
                            let at = split_time(shared.epoch, current.epoch, now);
                            if options.stop.load(Ordering::SeqCst) || at + MIN_TAKE > shared.end() {
                                writers.into_iter().for_each(FileWriter::cancel);
                                return Err(SplitRefused {
                                    ending: true,
                                    busy: false,
                                    message:
                                        "the recording ends before a new take could be recorded"
                                            .into(),
                                });
                            }
                            Ok((plan, writers, at, now, admission_notes))
                        });
                    match next {
                        Ok((plan, writers, at, now, admission_notes)) => {
                            let mut files = Vec::new();
                            let manifest = plan.manifest_path.clone();
                            let index = takes.len();
                            let mut gone = Vec::new();
                            let mut notes = admission_notes;
                            let current = takes.last().expect("a take");
                            for (i, (((split, demand), writer), file)) in split_senders
                                .iter()
                                .zip(&demands)
                                .zip(writers)
                                .zip(plan.plans.clone())
                                .enumerate()
                            {
                                let path = file.spec.path.clone();
                                // Before the writer thread can take the split:
                                // however far behind it is, admission hands
                                // over every picture from `at` on.
                                demand.split_sent(at);
                                let sent = split.send(Split {
                                    at,
                                    take: index,
                                    writer,
                                    plan: file,
                                });
                                // That file's writer thread has stopped (its
                                // writer failed), so this take can't have it.
                                if let Err(mpsc::SendError(split)) = sent {
                                    demand.split_settled(at);
                                    split.writer.cancel();
                                    gone.push(i);
                                } else {
                                    // Its source has given this take no frame
                                    // a second in: its file is only written if
                                    // one arrives (otherwise the manifest lists
                                    // it as delivering none).
                                    if current.plans[i].start.load(Ordering::SeqCst) == i64::MIN
                                        && current.results[i].is_none()
                                        && now >= current.epoch + STALL_AFTER
                                    {
                                        notes.push(format!(
                                            "{}: source {:?} has delivered no frames since the \
                                             current take began; this file is written only if \
                                             it delivers one",
                                            path.display(),
                                            current.plans[i].source
                                        ));
                                    }
                                    files.push(path);
                                }
                            }
                            if files.is_empty() {
                                control.state.lock().splitting = false;
                                let _ = reply.send(Err(SplitRefused {
                                    ending: true,
                                    busy: false,
                                    message:
                                        "the recording's writers stopped before it could split"
                                            .into(),
                                }));
                                continue;
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
                            handover = true;
                            let _ =
                                write(takes.last().expect("a take"), SessionStatus::Recording, &[]);
                            all_started = false;
                            let _ = reply.send(Ok(SplitTake {
                                take: number,
                                files,
                                manifest,
                                begins_at: started_at,
                                begins_in: at.saturating_since(now),
                                notes,
                            }));
                        }
                        Err(refused) => {
                            control.state.lock().splitting = false;
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
                    if !take.finalized
                        && take.metadata_error.is_none()
                        && take.results.iter().all(Option::is_some)
                    {
                        // A blocked publication costs this thread one attempt
                        // a second per take (rebuilding the manifest, then
                        // REPLACE_BACKOFF's ~310 ms of synchronous retries
                        // per inspection/rename), never a retry every tick.
                        #[cfg(windows)]
                        if take
                            .metadata_retry_at
                            .is_some_and(|at| std::time::Instant::now() < at)
                        {
                            continue;
                        }
                        let mut errors = take.errors();
                        program_errors(inner, &take.plans, &mut errors);
                        let stops = stops();
                        take.audio_errors = audio_stop_errors(setup, take, &stops, Some(control));
                        errors.extend(take.audio_errors.iter().cloned());
                        let manifest = take_manifest(
                            platform,
                            setup,
                            take,
                            take.status(&errors),
                            &errors,
                            &stops,
                            Some(control),
                            canvas_controls,
                            &formats.cameras,
                        );
                        match publish_final(&take.manifest_path, &manifest) {
                            Ok(()) => {
                                take.finalized = true;
                                #[cfg(windows)]
                                control.metadata_result(&take.manifest_path, None);
                            }
                            Err(error) => {
                                #[cfg(windows)]
                                if isocast_mux::file::metadata_blocked(&error) {
                                    control.metadata_result(&take.manifest_path, Some(&error));
                                    take.metadata_retry_at =
                                        Some(std::time::Instant::now() + Duration::from_secs(1));
                                } else {
                                    control.metadata_result(&take.manifest_path, None);
                                    take.metadata_error = Some(format!("{error:#}"));
                                }
                                #[cfg(not(windows))]
                                {
                                    take.metadata_error = Some(format!("{error:#}"));
                                }
                                if take.metadata_error.is_some() {
                                    isocast_diagnostics::diagnostic!("warn"; "could not finish take metadata: {error:#}");
                                }
                            }
                        }
                        take.manifest = Some(manifest);
                    }
                }
                // Keep at most one handover in flight. A scheduled take isn't
                // recording yet, and its predecessor still owns native encoder
                // sessions until finalization. Accepting more splits here used
                // to queue future takes faster than time could reach them.
                // A failed source or stalled finalizer must not disable
                // Split/Pause forever. After this take's minimum length its
                // start is in the past, so releasing cannot queue future
                // starts faster than the recording clock reaches them.
                if handover
                    && ((all_started && takes[..earlier].iter().all(|take| take.finalized))
                        || now >= takes.last().expect("a take").epoch + MIN_TAKE)
                {
                    control.state.lock().splitting = false;
                    handover = false;
                }
                // Finished takes, in order. One whose split comes after the
                // recording's end isn't: the end rewrites it.
                let ended_by = shared.end();
                report_finished(
                    |i| {
                        let take = &takes[i];
                        take.end
                            .is_some_and(|end| end <= ended_by)
                            .then(|| Publication::of(take))
                    },
                    earlier,
                    &mut reported,
                    &mut withheld,
                    |i| {
                        let take = &takes[i];
                        if let (Some(on_take), Some(manifest)) = (options.on_take, &take.manifest) {
                            on_take(i as u32 + 1, &take.manifest_path, manifest);
                        }
                    },
                );
                // Stop when asked (even just after a split's reply, every
                // announced file gets a moment; checked again here for one
                // asked for since the split above), or when every file has
                // stopped (for example because the disk is full).
                let observed_stop = options.stop.load(Ordering::SeqCst);
                if observed_stop {
                    let current = takes.last().expect("a take");
                    shared.stop_at(now.max(current.epoch + MIN_TAKE));
                }
                if writer_threads.iter().all(|t| t.is_finished()) {
                    shared.stop_at(now.max(shared.epoch));
                }
                // Markers and switches after the end are refused from now on
                // (the files take a moment longer to finish).
                let end = shared.end();
                if end != Time::MAX {
                    control.end_recording(end);
                    for extra in &extra_controls {
                        extra.end_recording(end);
                    }
                }
                if now >= end {
                    break;
                }
                #[cfg(windows)]
                control.wait_recording(&options.stop, observed_stop);
                #[cfg(not(windows))]
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
            || inner.state.is_broken()
            || inner.main.scenes.broken()
        {
            errors.push(
                "recording interrupted: broken shared state; writers finalized by emergency stop"
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
    let stops = stops();
    for (i, take) in takes.iter_mut().enumerate() {
        let mut take_errors = take.errors();
        if !take.finalized {
            program_errors(inner, &take.plans, &mut take_errors);
        }
        let audio_errors = audio_stop_errors(setup, take, &stops, Some(control));
        take_errors.extend(audio_errors.iter().cloned());
        file_errors.extend(take_errors.iter().cloned());
        if i == last {
            take_errors.extend(errors.iter().cloned());
        }
        // A take finalized before a stop in it was noticed is made again,
        // with its saved errors and the audio ones as they are now.
        let restated = take.finalized && audio_errors != take.audio_errors;
        if restated && let Some(saved) = &take.manifest {
            take_errors = saved
                .errors
                .iter()
                .filter(|e| !take.audio_errors.contains(e))
                .cloned()
                .chain(audio_errors.iter().cloned())
                .collect();
        }
        // A manifest that can't be written (the disk is full) doesn't lose
        // the report: the files it would list are finished all the same.
        // One that failed during the recording is tried once more here (the
        // coordinator never replays it): rewriting even after a failed
        // post-publication flush only replaces a complete file with one.
        let failed = take.metadata_error.take();
        if i == last || !take.finalized || restated {
            let published = take.finalized;
            let manifest = take_manifest(
                platform,
                setup,
                take,
                take.status(&take_errors),
                &take_errors,
                &stops,
                Some(control),
                canvas_controls,
                &formats.cameras,
            );
            match finish_manifest(&take.manifest_path, &manifest) {
                Ok(()) => {
                    take.finalized = true;
                    take.manifest = Some(manifest);
                }
                Err(err) => {
                    let err = format!("{err:#}");
                    file_errors.extend(failed.filter(|failed| *failed != err));
                    file_errors.push(err);
                    // A restatement that can't be written leaves the take's
                    // published manifest in place: it stays finished, and is
                    // reported with that manifest.
                    if !published {
                        take.manifest = Some(manifest);
                    }
                }
            }
        }
        manifests.push(take.manifest_path.clone());
        let reports: Vec<_> = take.results.iter_mut().map(|r| r.take()?.ok()).collect();
        let mapped = |at| control.recording_time(at);
        for (plan, report) in take.plans.iter().zip(reports) {
            if let Some(mut report) = report {
                report.audio_stops = file_audio_stops(setup, take, plan, &stops, mapped);
                files.push(report);
            }
        }
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
        for (i, take) in takes.iter().enumerate() {
            if (i >= reported || withheld.contains(&i))
                && take.finalized
                && let Some(manifest) = &take.manifest
            {
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

/// Where an ended take's final manifest is, for reporting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Publication {
    Published,
    /// Its write failed for good during the recording; the end tries again.
    Failed,
    /// Still being written (or waiting out a reader that holds it).
    Pending,
}

impl Publication {
    fn of(take: &Take) -> Self {
        if take.finalized {
            Self::Published
        } else if take.metadata_error.is_some() {
            Self::Failed
        } else {
            Self::Pending
        }
    }
}

/// Moves `reported` over the first `earlier` takes in order, calling
/// `report` for each published one. A failed one is passed over into
/// `withheld`, unreported, so later takes aren't held back by it; the cursor
/// waits at the first take still pending, or not yet ended (`None`).
fn report_finished(
    state: impl Fn(usize) -> Option<Publication>,
    earlier: usize,
    reported: &mut usize,
    withheld: &mut Vec<usize>,
    mut report: impl FnMut(usize),
) {
    while *reported < earlier {
        match state(*reported) {
            Some(Publication::Published) => report(*reported),
            Some(Publication::Failed) => withheld.push(*reported),
            Some(Publication::Pending) | None => return,
        }
        *reported += 1;
    }
}

/// The write speed of the disk holding `folder`, measured at most every
/// [`SPEED_TTL`].
fn write_speed<P: Platform>(inner: &Inner<P>, folder: &Path) -> Result<f64> {
    let now = inner.platform.now();
    if let Some((at, speed)) = inner.speeds.lock().get(folder)
        && now.saturating_since(*at) < SPEED_TTL
    {
        return Ok(*speed);
    }
    let speed = space::write_speed(folder)
        .with_context(|| format!("could not write a test file in {}", folder.display()))?;
    inner
        .speeds
        .lock()
        .insert(folder.to_path_buf(), (now, speed));
    Ok(speed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_monitor_input_does_not_change_record_packets_and_drop_joins() {
        let source: Arc<Routes<u32>> = Arc::default();
        let (record, recorded) = sync_channel(4);
        source.add(record, 0, Time::from_nanos(0), None);
        let (input, received) = sync_channel(1);
        let missed = Arc::new(AtomicU64::new(0));
        let key = source.add_counted(
            input.into(),
            0,
            Time::from_nanos(0),
            None,
            Some(missed.clone()),
            false,
        );
        let samples: Arc<[f32]> = vec![0.125; 960].into();
        for tick in 0..2 {
            source.audio(&samples, Time::from_nanos(tick * 10_000_000));
        }
        assert_eq!(missed.load(Ordering::Relaxed), 1);
        for tick in 0..2 {
            let Packet::Audio {
                samples: actual,
                pts,
                ..
            } = recorded.recv().unwrap()
            else {
                panic!("expected audio")
            };
            assert_eq!(pts, Time::from_nanos(tick * 10_000_000));
            assert!(Arc::ptr_eq(&actual, &samples));
        }
        let (output, packets) = sync_channel(1);
        let joined = Arc::new(AtomicBool::new(false));
        let finished = joined.clone();
        let thread = thread::spawn(move || {
            while let Ok(packet) = received.recv() {
                let _ = output.try_send(packet);
            }
            finished.store(true, Ordering::Release);
        });
        let monitor = MonitorAudio {
            packets,
            stop: Arc::new(AtomicBool::new(false)),
            routes: Arc::new(Mutex::new(vec![MonitorRoute {
                id: "tone".into(),
                source: source.clone(),
                key,
            }])),
            thread: Some(thread),
        };
        drop(monitor);
        assert!(joined.load(Ordering::Acquire));
        assert_eq!(
            source.list.read().len(),
            1,
            "only the record route survives"
        );
        source.audio(&samples, Time::from_nanos(20_000_000));
        assert!(recorded.recv().is_ok());
    }

    #[test]
    fn a_failed_take_manifest_does_not_hold_back_later_takes() {
        use Publication::*;
        let run = |states: &[Option<Publication>], reported: &mut usize, withheld: &mut Vec<_>| {
            let mut out = Vec::new();
            report_finished(
                |i| states[i],
                states.len(),
                reported,
                withheld,
                |i| out.push(i),
            );
            out
        };
        let (mut reported, mut withheld) = (0, Vec::new());
        // Take 1 failed for good: take 2 is still reported, take 3 waits.
        let states = [
            Some(Failed),
            Some(Published),
            Some(Pending),
            Some(Published),
        ];
        assert_eq!(run(&states, &mut reported, &mut withheld), [1]);
        assert_eq!((reported, withheld.as_slice()), (2, &[0][..]));
        // Ticks while take 3 stays pending report nothing more.
        assert!(run(&states, &mut reported, &mut withheld).is_empty());
        assert_eq!(withheld, [0]);
        // Once it's published, it and take 4 follow, each once.
        let states = [
            Some(Failed),
            Some(Published),
            Some(Published),
            Some(Published),
        ];
        assert_eq!(run(&states, &mut reported, &mut withheld), [2, 3]);
        assert_eq!((reported, withheld.as_slice()), (4, &[0][..]));
        // A take that hasn't ended stops the cursor whatever comes after it.
        let (mut reported, mut withheld) = (0, Vec::new());
        assert!(run(&[None, Some(Published)], &mut reported, &mut withheld).is_empty());
        assert_eq!((reported, withheld.len()), (0, 0));
    }

    #[test]
    fn splits_fall_on_whole_seconds_a_second_into_the_take() {
        // An epoch that isn't a whole number of frames from anything.
        let epoch = Time::from_nanos(123_456_789_017);
        let ms = |ms: u64| Duration::from_millis(ms);
        // A take that began on the epoch, 2.37 s ago: the next whole
        // second at least half a second away.
        let now = epoch + ms(2_370);
        let at = split_time(epoch, epoch, now);
        assert_eq!(at, epoch + Duration::from_secs(3));
        for fps in [60, 30, 24] {
            let frames = at.ticks_since(epoch, fps) as u64;
            assert_eq!(epoch.add_ticks(frames, fps), at, "{fps} fps");
        }
        // A take lasts at least a second: one that only just began.
        let current = epoch + Duration::from_secs(3);
        let again = split_time(epoch, current, epoch + ms(3_100));
        assert_eq!(again, current + Duration::from_secs(1));
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
        routes.add_counted(recording.into(), 0, at, None, None, true);
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
            tx.into(),
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
            routes.video(&n, Time::from_nanos(i64::from(n)), None);
        }
        assert_eq!(missed.load(Ordering::Relaxed), 3);
        assert_eq!(rx.try_iter().count(), 2);
        // Frames from before an output's start aren't missed.
        routes.video(&9, Time::from_nanos(-1), None);
        assert_eq!(missed.load(Ordering::Relaxed), 3);
        // Audio taps use the same non-blocking route and count each chunk.
        let samples: Arc<[f32]> = vec![0.0; 960].into();
        for _ in 0..3 {
            routes.audio(&samples, Time::from_nanos(0));
        }
        assert_eq!(missed.load(Ordering::Relaxed), 4);
        assert_eq!(rx.try_iter().count(), 2);
    }

    #[test]
    fn a_stalled_live_tap_releases_old_pictures_and_preserves_audio() {
        // A 32-frame pool with a healthy recording consumer and a stalled
        // stream. Keep weak references so this measures actual frame
        // lifetimes, not the queue's own accounting.
        let pool: Vec<_> = (0..32).map(|_| Arc::new(())).collect();
        let routes = Routes::default();
        let missed_video = Arc::new(AtomicU64::new(0));
        let missed_audio = Arc::new(AtomicU64::new(0));
        let (live, packets) = super::super::tap::channel(96, missed_video.clone());
        let video_key = routes.add_counted(
            super::super::tap::Output::Live(live.clone()),
            0,
            Time::from_nanos(0),
            None,
            Some(missed_video.clone()),
            false,
        );
        let audio_routes = Routes::default();
        let audio_key = audio_routes.add_counted(
            super::super::tap::Output::Live(live),
            0,
            Time::from_nanos(0),
            None,
            Some(missed_audio.clone()),
            false,
        );
        let (recording, recorded) = sync_channel(64);
        routes.add(recording, 0, Time::from_nanos(0), None);
        let samples: Arc<[f32]> = vec![0.25; 960].into();
        for n in 0..60 {
            let frame = pool
                .iter()
                .find(|frame| Arc::strong_count(frame) == 1)
                .expect("a stalled stream must leave frames for recording");
            let pts = Time::from_nanos(n);
            routes.video(frame, pts, None);
            audio_routes.audio(&samples, pts);
            assert!(matches!(recorded.try_recv(), Ok(Packet::Video { pts: at, .. }) if at == pts));
        }
        assert_eq!(pool.iter().filter(|f| Arc::strong_count(f) > 1).count(), 8);
        assert_eq!(missed_video.load(Ordering::Relaxed), 52);
        assert_eq!(missed_audio.load(Ordering::Relaxed), 0);
        routes.remove(video_key);
        audio_routes.remove(audio_key);
        let mut video = Vec::new();
        let mut audio = 0;
        loop {
            match packets.recv_timeout(Duration::ZERO) {
                Ok(Packet::Video { pts, .. }) => video.push(pts.as_nanos()),
                Ok(Packet::Audio { .. }) => audio += 1,
                Ok(Packet::Skipped { .. }) => panic!("live taps never receive ISO skipped ticks"),
                Ok(Packet::AudioGap { .. }) => panic!("live taps never receive file loss markers"),
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => panic!("every sender was removed"),
            }
        }
        assert_eq!(video, (52..60).collect::<Vec<_>>());
        assert_eq!(audio, 60);
        assert!(pool.iter().all(|f| Arc::strong_count(f) == 1));

        // Negative control: the old mixed 96-packet tap pins the whole
        // pool even though its recording consumer releases every frame.
        let (old, _receiver) = sync_channel(96);
        for frame in &pool {
            old.try_send(Packet::Video {
                frame: frame.clone(),
                pts: Time::from_nanos(0),
            })
            .unwrap();
        }
        assert!(!pool.iter().any(|f| Arc::strong_count(f) == 1));
    }

    #[test]
    fn only_screens_whose_capture_depth_changes_reopen() {
        let setup = |screen: &str, iso: &str| {
            Setup::from_toml(&format!(
                r##"
version = 1
name = "t"
canvas = {{ width = 1920, height = 1080, fps = 30 }}
sources = [
  {{ id = "screen", kind = "display", display = "main"{screen} }},
  {{ id = "other", kind = "display", display = "side" }},
  {{ id = "cam", kind = "camera", device = "Cam" }},
  {{ id = "beep", kind = "tone" }},
]
[[scenes]]
id = "main"
items = [{{ source = "screen", rect = {{ x = 0, y = 0, width = 1920, height = 1080 }} }}]
[recording]
folder = "."
program = {{ encoder = {{ codec = "hevc", quality = 70 }} }}
iso = {iso}
"##
            ))
            .unwrap()
        };
        let iso = |depth: &str| {
            format!(
                r#"{{ enabled = true, sources = ["screen", "cam"], encoder = {{ codec = "hevc"{depth} }} }}"#
            )
        };
        let ids = ["screen", "other", "cam", "beep"];
        let changed = |a: &Setup, b: &Setup| -> Vec<&'static str> {
            ids.into_iter()
                .filter(|id| definition_of(a, id) != definition_of(b, id))
                .collect()
        };
        let default = setup("", &iso(""));
        // The default (8) and a saved 8 open everything as before.
        let eight = setup("", &iso(", bit_depth = 8"));
        assert!(changed(&default, &eight).is_empty());
        // The screen's own 10 reopens only it; the other screen, the
        // camera (native as ever) and the tone stay open.
        let own_ten = setup(", encoder = { bit_depth = 10 }", &iso(""));
        assert_eq!(changed(&default, &own_ten), ["screen"]);
        // Its own 8 is as the default; so is its own 10 without its ISO
        // file, as the program draws 8 bits.
        let own_eight = setup(", encoder = { bit_depth = 8 }", &iso(""));
        assert!(changed(&default, &own_eight).is_empty());
        let off = r#"{ enabled = false, encoder = { codec = "hevc" } }"#;
        let program_only = setup(", encoder = { bit_depth = 10 }", off);
        assert!(changed(&setup("", off), &program_only).is_empty());
        // A set 10 reopens every screen, as it always did; the screen's own
        // 8 keeps only it as it was.
        let ten = setup("", &iso(", bit_depth = 10"));
        assert_eq!(changed(&default, &ten), ["screen", "other"]);
        let mut disabled = ten.clone();
        disabled.recording.iso.enabled = false;
        assert!(changed(&default, &disabled).is_empty());
        assert_eq!(changed(&ten, &disabled), ["screen", "other"]);
        let ten_own_eight = setup(", encoder = { bit_depth = 8 }", &iso(", bit_depth = 10"));
        assert_eq!(changed(&default, &ten_own_eight), ["other"]);
    }

    /// A platform that can't capture, composite or encode anything: enough
    /// for an engine to start and run its program's monitor workers.
    mod inert {
        use std::path::Path;
        use std::sync::{Arc, OnceLock};
        use std::time::Instant;

        use anyhow::{Result, bail};
        use isocast_protocol::api::Machine;
        use isocast_protocol::devices::{Devices, PermissionKind, PermissionsReport};
        use isocast_protocol::{Canvas, SourceKind};

        use crate::gpu::{FrameImporter, Gpu, GpuFrame};
        use crate::{
            AudioEncoder, AudioRender, AudioSink, BrowserControl, BrowserPage, CameraRequest,
            CaptureFormat, EncodedVideo, FileWriter, Finished, FramePool, LiveAudioSpec,
            LiveVideoSpec, MediaInfo, MediaPart, MediaReader, Platform, Probe, ScreenTarget, Time,
            VideoEncoder, VideoSink, Volume, WriterSpec,
        };

        #[derive(Clone)]
        pub(super) struct Inert;

        /// A resource it never makes.
        pub(super) enum Never {}

        impl FramePool for Never {
            type Frame = ();
            fn draw(&mut self, _draw: &mut dyn FnMut(&mut [u8], usize)) -> Result<Option<()>> {
                match *self {}
            }
            fn take(&mut self) -> Result<Option<()>> {
                match *self {}
            }
        }

        impl FileWriter for Never {
            type Frame = ();
            fn start(&mut self, _at: Time) -> Result<()> {
                match *self {}
            }
            fn append_video(&mut self, _frame: &(), _pts: Time) -> Result<bool> {
                match *self {}
            }
            fn append_audio(
                &mut self,
                _track: usize,
                _samples: &[f32],
                _pts: Time,
            ) -> Result<bool> {
                match *self {}
            }
            fn finish(self, _end: Time) -> Result<Finished> {
                match self {}
            }
            fn cancel(self) {
                match self {}
            }
        }

        impl MediaReader for Never {
            type Frame = ();
            fn next_video(&mut self) -> Result<Option<((), std::time::Duration)>> {
                match *self {}
            }
            fn next_audio(&mut self) -> Result<Option<(Vec<f32>, std::time::Duration)>> {
                match *self {}
            }
        }

        impl VideoEncoder for Never {
            type Frame = ();
            fn encode(&mut self, _frame: &(), _pts: Time, _keyframe: bool) -> Result<()> {
                match *self {}
            }
            fn receive(&mut self) -> Result<Option<EncodedVideo>> {
                match *self {}
            }
            fn receive_timeout(
                &mut self,
                _timeout: std::time::Duration,
            ) -> Result<Option<EncodedVideo>> {
                match *self {}
            }
            fn dropped(&self) -> u64 {
                match *self {}
            }
            fn set_rate(&mut self, _fps: u32, _bitrate_kbps: u32) -> Result<()> {
                match *self {}
            }
        }

        impl AudioEncoder for Never {
            fn encode(&mut self, _samples: &[f32]) -> Result<Vec<Vec<u8>>> {
                match *self {}
            }
            fn delay(&self) -> u32 {
                match *self {}
            }
        }

        impl FrameImporter for Never {
            type Frame = ();
            fn import(&mut self, _frame: &()) -> Result<GpuFrame> {
                match *self {}
            }
            fn target(&mut self, _frame: &()) -> Result<wgpu::Texture> {
                match *self {}
            }
            fn ycbcr_target(&mut self, _frame: &()) -> Result<(wgpu::Texture, wgpu::Texture)> {
                match *self {}
            }
        }

        impl Platform for Inert {
            type Frame = ();
            type FramePool = Never;
            type Writer = Never;
            type Capture = Never;
            type Importer = Never;
            type Media = Never;
            type Playback = Never;
            type VideoEncoder = Never;
            type AudioEncoder = Never;
            type SharedSurface = ();

            fn now(&self) -> Time {
                static ORIGIN: OnceLock<Instant> = OnceLock::new();
                let nanos = ORIGIN.get_or_init(Instant::now).elapsed().as_nanos();
                Time::from_nanos(i64::try_from(nanos).unwrap_or(i64::MAX))
            }
            fn devices(&self) -> Result<Devices> {
                bail!("inert")
            }
            fn permissions(&self) -> Result<PermissionsReport> {
                bail!("inert")
            }
            fn request_permissions(
                &self,
                _only: Option<PermissionKind>,
            ) -> Result<PermissionsReport> {
                bail!("inert")
            }
            fn open_permission_settings(&self, _permission: PermissionKind) -> Result<String> {
                bail!("inert")
            }
            fn media_info(&self, _path: &Path) -> Result<MediaInfo> {
                bail!("inert")
            }
            fn open_media(&self, _path: &Path, _part: MediaPart) -> Result<Never> {
                bail!("inert")
            }
            fn frame_importer(&self, _gpu: &Gpu) -> Result<Never> {
                bail!("inert")
            }
            fn machine(&self) -> Machine {
                Machine {
                    chip: "inert".into(),
                    memory_gb: 0.0,
                    cores: 1,
                    os: "inert".into(),
                    hardware_encoders: Vec::new(),
                }
            }
            fn probe(
                &self,
                _id: &str,
                _source: &SourceKind,
                _canvas: &Canvas,
            ) -> Result<Option<Probe>> {
                bail!("inert")
            }
            fn volume(&self, _path: &Path) -> Result<Volume> {
                bail!("inert")
            }
            fn frame_pool(&self, _width: u32, _height: u32) -> Result<Never> {
                bail!("inert")
            }
            fn ycbcr_frame_pool(&self, _width: u32, _height: u32) -> Result<Never> {
                bail!("inert")
            }
            fn frame_size(&self, _frame: &()) -> (u32, u32) {
                (0, 0)
            }
            fn frame_is_ycbcr(&self, _frame: &()) -> bool {
                false
            }
            fn shared_surface(
                &self,
                _gpu: &Gpu,
                _width: u32,
                _height: u32,
            ) -> Result<((), wgpu::Texture)> {
                bail!("inert")
            }
            fn encode_jpeg(
                &self,
                _frame: &(),
                _max_width: Option<u32>,
                _quality: f32,
            ) -> Result<(Vec<u8>, u32, u32)> {
                bail!("inert")
            }
            fn create_writer(&self, _spec: &WriterSpec) -> Result<Never> {
                bail!("inert")
            }
            fn capture_screen(
                &self,
                _target: ScreenTarget<'_>,
                _fps: u32,
                _ten_bit: bool,
                _cursor: bool,
                _sink: VideoSink<()>,
            ) -> Result<(Never, CaptureFormat)> {
                bail!("inert")
            }
            fn capture_camera(
                &self,
                _device: &str,
                _request: CameraRequest,
                _canvas: &Canvas,
                _sink: VideoSink<()>,
            ) -> Result<(Never, CaptureFormat)> {
                bail!("inert")
            }
            fn capture_microphone(
                &self,
                _device: &str,
                _channels: Option<&[u32]>,
                _sink: AudioSink,
            ) -> Result<(Never, u32)> {
                bail!("inert")
            }
            fn capture_system_audio(
                &self,
                _exclude: &[String],
                _sink: AudioSink,
                _exclude_own: bool,
            ) -> Result<(Never, u32)> {
                bail!("inert")
            }
            fn play_audio(&self, _device: &str, _render: AudioRender) -> Result<(Never, String)> {
                bail!("inert")
            }
            fn capture_browser(
                &self,
                _page: &BrowserPage<'_>,
                _video: VideoSink<()>,
                _audio: Option<AudioSink>,
            ) -> Result<(Never, CaptureFormat, Arc<dyn BrowserControl>)> {
                bail!("inert")
            }
            fn capture_application_audio(
                &self,
                _app: &str,
                _sink: AudioSink,
            ) -> Result<(Never, u32)> {
                bail!("inert")
            }
            fn create_video_encoder(&self, _spec: &LiveVideoSpec) -> Result<Never> {
                bail!("inert")
            }
            fn create_audio_encoder(&self, _spec: &LiveAudioSpec) -> Result<Never> {
                bail!("inert")
            }
            fn has_secret(&self, _service: &str, _account: &str) -> Result<bool> {
                bail!("inert")
            }
            fn secret(&self, _service: &str, _account: &str) -> Result<Option<String>> {
                bail!("inert")
            }
            fn set_secret(&self, _service: &str, _account: &str, _secret: &str) -> Result<()> {
                bail!("inert")
            }
            fn delete_secret(&self, _service: &str, _account: &str) -> Result<bool> {
                bail!("inert")
            }
        }
    }

    /// `stop` ends every monitor stream before it returns (its worker owns a
    /// native encoder that must be gone before the process exits), and a
    /// monitor asked for afterwards is refused rather than started on a
    /// stopped engine.
    #[test]
    fn stop_ends_monitor_streams_and_refuses_new_ones() {
        use isocast_protocol::api::{H264Profile, frames};

        let setup = Setup::from_toml(
            r##"
version = 1
name = "t"
[canvas]
width = 64
height = 64
fps = 30
[[scenes]]
id = "main"
items = []
[recording]
folder = "/nonexistent/isocast-engine-test"
[recording.program]
encoder = { codec = "hevc", quality = 70 }
"##,
        )
        .unwrap();
        let engine = Engine::start(&inert::Inert, &setup, None, None).unwrap();
        let feed = engine
            .frames(PROGRAM, None, 30, H264Profile::High)
            .expect("a running engine streams its program");

        engine.stop();
        // Already there: stop joined the worker that sent it.
        let end = feed
            .records
            .try_iter()
            .map(|record| frames::Record::read(&mut &record[..]).unwrap().unwrap())
            .find(|record| record.kind == frames::END)
            .expect("the monitor heard the stream end before stop returned");
        let why = String::from_utf8(end.payload).unwrap();
        assert!(why.contains("engine stopped"), "ended with {why:?}");

        match engine.frames(PROGRAM, None, 30, H264Profile::High) {
            Ok(_) => panic!("a monitor started on a stopped engine"),
            Err(why) => assert_eq!(why, "engine stopped"),
        }
    }
}
