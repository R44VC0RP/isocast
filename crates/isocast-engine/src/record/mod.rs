//! Recording sessions: start the sources a setup needs, write one file per
//! ISO source on the shared clock, and stop cleanly.

use parking_lot::Mutex;
use std::any::Any;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, SecondsFormat};
use isocast_protocol::manifest::{
    AudioStop, MANIFEST_VERSION, ManifestAudio, ManifestEncoder, ManifestFile, Marker, SceneChange,
    SessionManifest, SessionStatus,
};
use isocast_protocol::{
    AudioSettings, CanvasId, Container, Filter, PROGRAM, PanLaw, Setup, Transition, TransitionKind,
};

pub use isocast_protocol::api::{
    AudioLevel, CaptureReport, DiskReport, FileReport, LiveFile, LiveStatus, Report,
};

use crate::audio_filter::LiveAudioFilters;

mod disk;
mod engine;
mod file;
mod frame_stream;
mod iso_frames;
mod mix;
mod monitor;
mod opus_stream;
mod pause;
pub mod pressure;
mod program;
mod scene_feeds;
mod scenes;
mod shared_frames;
mod tap;
mod timings;
mod virtual_camera;
pub use virtual_camera::{CameraSink, OnCameraChange, VirtualCamera};

pub use engine::{Engine, EngineEvent, JpegFrame, OnEngineEvent, ProgramTap};
pub(crate) use engine::{ProgramTapSource, canvas_off};
pub use engine::{compositor_canvas, compositor_worker};
pub use frame_stream::{FeedRate, FrameFeed};
pub use opus_stream::{OpusFeed, OpusPacket};
pub use shared_frames::{DEFAULT_SLOTS, SharedControl, SharedEvent, SharedFeed};

use crate::source::{AUDIO_CHUNK, FrameSlot};
use crate::{
    AudioTrackSpec, BrowserControl, CameraMetadataReader, FileWriter, Platform, SAMPLE_RATE, Time,
    VideoTrackSpec, WriterRequest, WriterSpec,
};

/// How far ahead of "now" the session clock starts, so every source and
/// writer is running when the first frame is due.
const START_DELAY: Duration = Duration::from_millis(200);
/// How long to wait for every capture's first frame before the session
/// clock starts. Cameras can take a moment to warm up; waiting lets every
/// file start together.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a recording waits for each program to draw a picture before
/// choosing its epoch, on platforms that don't require one (#150). Past it
/// the recording starts anyway, and the program's late head counts as
/// dropped.
const PROGRAM_WARM_TIMEOUT: Duration = Duration::from_secs(1);
/// The shortest a recording stopped just after it started gets, so every
/// file it announced holds something.
const MIN_TAKE: Duration = Duration::from_secs(1);
/// How much faster than the estimated data rate a disk should test, so it
/// keeps up when its speed dips.
const SPEED_MARGIN: f64 = 2.0;
/// How far ahead a split is scheduled, so the next take's files exist in
/// time.
const SPLIT_LEAD: Duration = Duration::from_millis(500);
/// How long a switch waits for a stinger video the setup only just gained
/// to load.
const STINGER_WAIT: Duration = Duration::from_secs(2);
/// How long, in capture time, a file a split ended stays open for its last
/// audio, which can arrive after the next take's first frame.
const SPLIT_AUDIO_WAIT: Duration = Duration::from_millis(500);
/// How long, in capture time, a file a split ended stays open even once its
/// audio is complete, so that its encoder sessions are not torn down while
/// the next take's sessions start (they start one after another over the
/// first ~100 ms of the take). VideoToolbox places a new hardware session
/// by what the encode engines are doing at that moment; a session torn
/// down just then left the new ones badly placed, every ISO of the take
/// then dropping about half its pictures (one split in three on an M4 Max;
/// GitHub #41).
const SPLIT_FINISH_DELAY: Duration = Duration::from_millis(200);
/// How often live status is reported.
const STATUS_INTERVAL: Duration = Duration::from_secs(1);
/// How overdue a frame must be before live status counts it as dropped
/// even though no newer frame has arrived.
const STALL_AFTER: Duration = Duration::from_secs(1);
/// How often free space is checked while recording.
const SPACE_CHECK_INTERVAL: Duration = Duration::from_secs(1);
/// A recording stops cleanly when free space falls below
/// `recording.keep_free_gb`, or below this many seconds of its files at
/// their highest likely data rate (and [`MIN_FREE_BYTES`]) if that is more,
/// so that a reserve of 0 still leaves room to finish every file.
const MIN_FREE_SECONDS: f64 = 10.0;
const MIN_FREE_BYTES: f64 = 64e6;
/// Packets a file writer may fall behind before its sources drop for it.
const QUEUE_DEPTH: usize = 64;
/// Audio chunks the program mixer may hold before its sources drop for it.
const MIX_QUEUE_DEPTH: usize = 512;
/// Audio timestamps within this many samples (2 ms) of where the track
/// already is are treated as continuous; beyond it, gaps are filled with
/// silence and overlaps are trimmed.
const AUDIO_JITTER: i64 = SAMPLE_RATE as i64 / 500;
/// How long after the session ends audio captures keep delivering, so the
/// last chunks before the end still arrive.
const AUDIO_TAIL: Duration = Duration::from_millis(150);
/// Audio held for a file that has not received its first frame yet.
const MAX_PENDING_AUDIO: usize = 500;
/// How long after a pause cut a crash-safe file waits for the last inputs
/// before the cut before committing anyway (a stalled track must not hold
/// the pause reply).
const COMMIT_WAIT: Duration = Duration::from_millis(500);
/// How long the pause reply waits for every file's commit, at most.
const COMMIT_TIMEOUT: Duration = Duration::from_secs(3);

pub struct RecordOptions<'a> {
    /// Write files here instead of `recording.folder`.
    pub folder: Option<PathBuf>,
    /// Stop after this long. Without it, record until `stop` is set.
    pub duration: Option<Duration>,
    /// Set to stop recording (for example on Ctrl-C).
    pub stop: Arc<AtomicBool>,
    /// Called once every file is created and the sources are starting,
    /// with the paths being written.
    pub on_start: Option<OnStart<'a>>,
    /// Called about once a second while recording, with live counters.
    pub on_status: Option<OnStatus<'a>>,
    /// Called as each take finishes (a split's take once its files are
    /// finished, the last at the end), with its final manifest once written.
    /// Normally in take order; a failed publication retried at Stop can arrive
    /// after a later take. Use the supplied take number, not arrival order.
    pub on_take: Option<OnTake<'a>>,
    /// Markers, scene switches and live settings from another thread.
    pub control: Option<Arc<SessionControl>>,
}

/// Markers and scene switches for a running session, from another thread.
/// Times are on the host clock; the manifest places them on the session
/// timeline and is rewritten as they come in. Also gives other threads the
/// session's newest frames.
#[derive(Default)]
pub struct SessionControl {
    state: Mutex<ControlState>,
    /// Final manifests waiting on external readers/permissions. Kept separate
    /// from live controls: retrying publication must never hold their lock.
    metadata_pending: Mutex<BTreeMap<PathBuf, String>>,
    #[cfg(windows)]
    metadata_shutdown: AtomicBool,
    #[cfg(windows)]
    recording_wake: parking_lot::Condvar,
    /// While a recording stops, complete mix blocks through its last frame
    /// boundary may bypass the live jitter delay. Zero keeps normal pacing.
    mix_drain_until: AtomicI64,
    frames: Mutex<Frames>,
    /// Each audio source's mix settings, which can change while recording.
    audio: Mutex<BTreeMap<String, AudioSettings>>,
    /// How balance pans sources, which can change while recording.
    pan_law: Mutex<PanLaw>,
    /// The browser sources' pages, by source id.
    browsers: Mutex<BTreeMap<String, Arc<dyn BrowserControl>>>,
    /// Each source's filters, which can change while recording.
    filters: SourceFilters,
    /// Each source's audio filters, which can change while recording.
    audio_filters: Arc<LiveAudioFilters>,
    /// How long each stinger video the session loaded plays, by file.
    stingers: Mutex<HashMap<String, Duration>>,
    /// Stinger videos the setup gained that are still being loaded, by
    /// file: a switch to one waits a moment for it.
    loading_stingers: Mutex<Vec<String>>,
    /// Why stinger videos of the setup couldn't be loaded, by file.
    failed_stingers: Mutex<HashMap<String, String>>,
    /// The highest rate anyone asked to see the program at since the
    /// program last looked (0: nobody): it is drawn at least that often
    /// while asked, and at every canvas frame while outputs take them.
    program_wanted: AtomicU32,
    /// The data store text shows keys of (`data = "key"`): the daemon's,
    /// or one of its own.
    data: Arc<crate::data::DataStore>,
}

/// The rate a client's monitor is taken to show the program or studio
/// mode's preview at when it doesn't say (JPEG monitors fetch at most 30 a
/// second).
pub(crate) const MONITOR_FPS: u32 = 30;

/// How long a request for the program's picture waits for the program to
/// draw one when it was idle (it starts at its next frame).
const FRESH_FRAME_WAIT: Duration = Duration::from_millis(300);

/// The newest frame of `slot` and its time, waiting up to
/// [`FRESH_FRAME_WAIT`] for one if it has none.
pub(crate) fn wait_for_frame<F: Clone>(slot: &FrameSlot<F>) -> Option<(F, Time)> {
    let deadline = std::time::Instant::now() + FRESH_FRAME_WAIT;
    loop {
        if let Some(newest) = slot.newest_timed() {
            return Some(newest);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(2));
    }
}

/// The filters of the sources that have any, by source id, shared with the
/// program so changes show at once.
pub(crate) type SourceFilters = Arc<Mutex<HashMap<String, Arc<[Filter]>>>>;

/// `setup`'s sources' filters.
fn source_filters(setup: &Setup) -> HashMap<String, Arc<[Filter]>> {
    setup
        .sources
        .iter()
        .filter(|source| !source.filters.is_empty())
        .map(|source| (source.id.clone(), source.filters.as_slice().into()))
        .collect()
}

/// The session's capture slots, by source id. Their frame type depends on
/// the platform, so they are kept type-erased.
#[derive(Default)]
struct Frames {
    ids: Vec<String>,
    slots: Option<Box<dyn Any + Send + Sync>>,
}

#[derive(Default)]
struct ControlState {
    /// The session timeline's zero, once known.
    epoch: Option<Time>,
    /// When each take starts; the first at `epoch`.
    takes: Vec<Time>,
    markers: Vec<(Time, String)>,
    /// Scene switches: when, to which scene, how.
    scenes: Vec<(Time, Arc<str>, Transition)>,
    /// When the recording ends, once known (a time limit, a stop, a full
    /// disk): markers and switches after it aren't on its timeline.
    end: Option<Time>,
    /// Something was added since the manifest was last written.
    changed: bool,
    /// A request to split into a new take, answered with its files.
    split: Option<mpsc::Sender<SplitReply>>,
    /// The recording has ended (or never ran): splits are refused at once.
    finished: bool,
    clock: Option<Arc<pause::RecordingClock>>,
    splitting: bool,
    pause_generation: u64,
    saved_pause_generation: u64,
    manifest_error: Option<String>,
    /// What the commit barrier of the latest pause established, for its
    /// reply: the generation, the recoverable seconds (if every file
    /// confirmed) and the files' commit errors.
    pause_outcome: Option<(u64, Option<f64>, Vec<String>)>,
}

/// The take a split began.
#[derive(Debug, Clone)]
pub struct SplitTake {
    /// Its number in the session, from 1.
    pub take: u32,
    pub files: Vec<PathBuf>,
    /// Its session manifest.
    pub manifest: PathBuf,
    /// When its files begin (local time), and how long after the split was
    /// answered: frames and audio before then go to the take before.
    pub begins_at: DateTime<Local>,
    pub begins_in: Duration,
    /// Things to know about its files, like one whose source has
    /// delivered no frames (it is written only once one arrives).
    pub notes: Vec<String>,
}

/// Why a split didn't happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitRefused {
    /// The recording is ending (stopping, at its time limit, or the disk
    /// filling) or has ended, so no new take can begin in it.
    pub ending: bool,
    /// Another split still owns the handover; retry once its files are ready.
    pub busy: bool,
    pub message: String,
}

/// What a split is answered with.
pub type SplitReply = Result<SplitTake, SplitRefused>;

impl ControlState {
    /// The take `at` falls in (1 for the first) and its time on that
    /// take's timeline; before the first take, 0 in take 1.
    fn place(&self, at: Time) -> Option<(u32, f64)> {
        let first = *self.takes.first()?;
        let i = self
            .takes
            .iter()
            .rposition(|start| *start <= at)
            .unwrap_or(0);
        let from = self.takes[i].max(first);
        let seconds = match &self.clock {
            Some(clock) => {
                let cuts = clock.state.read();
                cuts.map(at).saturating_since(cuts.map(from)).as_secs_f64()
            }
            None => at.saturating_since(from).as_secs_f64(),
        };
        Some((i as u32 + 1, seconds))
    }

    /// An error with where the recording ended (take and time) if it ended
    /// before `at`.
    fn check_end(&self, at: Time) -> Result<(), (u32, f64)> {
        match self.end {
            Some(end) if at >= end => Err(self.place(end).unwrap_or((1, 0.0))),
            _ => Ok(()),
        }
    }
}

impl SessionControl {
    /// Publication failures still being retried by the recording worker.
    /// Media is already finished; the session must not report completion yet.
    pub fn metadata_pending(&self) -> Vec<String> {
        self.metadata_pending.lock().values().cloned().collect()
    }

    /// A stopped session waiting for metadata, not an earlier split take.
    /// Clients must still offer Stop while the next take is recording.
    pub fn metadata_finalizing(&self) -> bool {
        self.state.lock().finished && !self.metadata_pending.lock().is_empty()
    }

    /// Stop waiting for external readers after saving a durable final sidecar.
    /// This is engine shutdown, not an ordinary Stop request.
    #[cfg(windows)]
    pub fn shutdown_metadata(&self) {
        self.metadata_shutdown.store(true, Ordering::SeqCst);
    }

    #[cfg(windows)]
    fn metadata_result(&self, path: &Path, error: Option<&anyhow::Error>) {
        if let Some(error) = error {
            let message = format!(
                "{}: final metadata publication is pending: {error:#}; retrying in the background; \
                 close applications holding the manifest open or restore folder write permission; \
                 keep Isocast running to allow publication retries",
                path.display()
            );
            let changed = {
                let mut pending = self.metadata_pending.lock();
                if pending.get(path) == Some(&message) {
                    false
                } else {
                    pending.insert(path.to_owned(), message.clone());
                    true
                }
            };
            if changed {
                isocast_diagnostics::diagnostic!("warn"; "{message}");
            }
        } else {
            let removed = self.metadata_pending.lock().remove(path).is_some();
            if removed {
                isocast_diagnostics::diagnostic!("info"; "metadata publication no longer pending: {}", path.display());
            }
        }
    }

    /// Wake the coordinator after its caller sets the recording's stop flag.
    /// The same lock guards entering the wait, so the notification cannot be
    /// lost between checking the atomic flag and sleeping.
    #[cfg(windows)]
    pub fn wake_recording(&self) {
        let _state = self.state.lock();
        self.recording_wake.notify_all();
    }

    #[cfg(windows)]
    fn wait_recording(&self, stop: &AtomicBool, observed_stop: bool) {
        let mut state = self.state.lock();
        let interval = Duration::from_millis(10);
        if observed_stop {
            // An early Stop still respects the minimum take duration. Don't
            // spin while waiting for that already-scheduled future boundary.
            self.recording_wake.wait_for(&mut state, interval);
        } else {
            self.recording_wake.wait_while_for(
                &mut state,
                |_| !stop.load(Ordering::SeqCst),
                interval,
            );
        }
    }

    /// Schedule a shared recording cut. Hotkeys: `recording.pause`
    /// calls this with true, `recording.resume` with false; the
    /// `recording.pause_toggle` action chooses the opposite of `pause_status`.
    /// The daemon uses this same path for the Pause/Resume methods.
    pub fn set_paused(&self, paused: bool, now: Time, epoch: Time) -> Result<(Time, u64), String> {
        let mut state = self.state.lock();
        if state.epoch != Some(epoch) {
            return Err(
                "recording: the recording changed before pause/resume; retry on the current take"
                    .into(),
            );
        }
        if state.finished || state.end.is_some_and(|end| now >= end) {
            return Err("recording: stopping or idle; valid action: start".into());
        }
        if Self::busy(&state, now) {
            return Err(
                "recording: a take is starting or the previous take is finishing; retry \
                 pause/resume after the handover"
                    .into(),
            );
        }
        let clock = state
            .clock
            .as_ref()
            .ok_or("recording: still starting; wait for its first take")?;
        let at = clock.change(paused, now, state.end)?;
        state.changed = true;
        state.pause_generation += 1;
        Ok((at, state.pause_generation))
    }

    /// Identify the recording while a caller releases its own state lock.
    pub fn recording_epoch(&self) -> Option<Time> {
        self.state.lock().epoch
    }

    /// A pause reply waits for its manifest checkpoint as well as its cut.
    pub fn pause_saved(&self, generation: u64) -> Result<bool, String> {
        let state = self.state.lock();
        if let Some(error) = &state.manifest_error {
            return Err(format!(
                "recording.pauses: the cut was applied, but its manifest could not be saved: {error}"
            ));
        }
        Ok(state.saved_pause_generation >= generation)
    }

    /// A split's handover is under way (see `LiveStatus::handover`).
    pub fn handover(&self, now: Time) -> bool {
        Self::busy(&self.state.lock(), now)
    }

    fn busy(state: &ControlState, now: Time) -> bool {
        state.splitting || (state.takes.len() > 1 && state.takes.last().is_some_and(|at| *at > now))
    }

    pub fn pause_status(&self, now: Time) -> Option<isocast_protocol::api::RecordingPause> {
        let state = self.state.lock();
        let clock = state.clock.as_ref()?;
        Some(
            clock
                .state
                .read()
                .status(now.min(state.end.unwrap_or(Time::MAX))),
        )
    }

    /// The commit barrier's outcome for pause `generation`: the seconds
    /// every file is on disk up to, and the files' commit errors.
    pub fn pause_outcome(&self, generation: u64) -> Option<(Option<f64>, Vec<String>)> {
        let state = self.state.lock();
        state
            .pause_outcome
            .as_ref()
            .filter(|(g, ..)| *g == generation)
            .map(|(_, recoverable, errors)| (*recoverable, errors.clone()))
    }

    fn recording_time(&self, at: Time) -> Time {
        let state = self.state.lock();
        state
            .clock
            .as_ref()
            .map_or(at, |clock| clock.state.read().map(at))
    }

    /// A control whose engine shows `data`'s values in bound text.
    pub fn with_data(data: Arc<crate::data::DataStore>) -> Self {
        Self {
            data,
            ..Self::default()
        }
    }

    /// The data store bound text reads.
    pub fn data(&self) -> &Arc<crate::data::DataStore> {
        &self.data
    }

    /// Add a marker at `at`. Returns the take it falls in (1 for the first)
    /// and its time on that take's timeline, or `None` before the session
    /// clock has started (it then counts as 0 in take 1). A marker after
    /// the recording's end isn't added: the error is where it ended.
    pub fn marker(&self, at: Time, label: String) -> Result<Option<(u32, f64)>, (u32, f64)> {
        let mut state = self.state.lock();
        state.check_end(at)?;
        state.markers.push((at, label));
        state.changed = true;
        Ok(state.place(at))
    }

    /// Record that `scene` went on air at `at` with `transition`; see
    /// [`SessionControl::marker`]. A switch after the recording's end
    /// still changes the program, but isn't on the recording's timeline.
    pub fn switch_scene(
        &self,
        at: Time,
        scene: String,
        transition: Transition,
    ) -> Result<Option<(u32, f64)>, (u32, f64)> {
        let mut state = self.state.lock();
        state.scenes.push((at, scene.into(), transition));
        state.changed = true;
        state.check_end(at)?;
        Ok(state.place(at))
    }

    /// The recording ends at `at`, or sooner if it was already going to.
    fn end_recording(&self, at: Time) {
        let mut state = self.state.lock();
        state.end = Some(state.end.map_or(at, |end| end.min(at)));
    }

    /// Make `transition` ready to run in this session: a stinger lasts as
    /// long as its video, which the session must have loaded (from the
    /// setup's transitions). One the setup only just gained is waited for,
    /// up to [`STINGER_WAIT`].
    pub fn prepare(&self, transition: &mut Transition) -> Result<(), String> {
        let TransitionKind::Stinger(stinger) = &transition.kind else {
            return Ok(());
        };
        let waiting_since = std::time::Instant::now();
        loop {
            let stingers = self.stingers.lock();
            if let Some(length) = stingers.get(&stinger.file) {
                transition.duration_ms = length.as_millis().clamp(1, u128::from(u32::MAX)) as u32;
                return Ok(());
            }
            let loading = self.loading_stingers.lock().contains(&stinger.file);
            if loading && waiting_since.elapsed() < STINGER_WAIT {
                drop(stingers);
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            if loading {
                return Err(format!(
                    "transition.file: {:?} is still loading; try again in a moment",
                    stinger.file
                ));
            }
            if let Some(reason) = self.failed_stingers.lock().get(&stinger.file) {
                return Err(format!(
                    "transition.file: the stinger video {:?} couldn't be loaded ({reason}); fix \
                     the file, or switch with another transition",
                    stinger.file
                ));
            }
            let loaded: Vec<&str> = stingers.keys().map(String::as_str).collect();
            return Err(format!(
                "transition.file: {:?} isn't loaded; a stinger's video must be in the live \
                 setup's transitions (transition or scenes[].transition), which loads it; \
                 loaded: {}",
                stinger.file,
                if loaded.is_empty() {
                    "none".to_string()
                } else {
                    loaded.join(", ")
                }
            ));
        }
    }

    /// Note the stinger videos `files` as being loaded (see
    /// [`SessionControl::prepare`]).
    pub(crate) fn loading_stingers(&self, files: &[String]) {
        let mut loading = self.loading_stingers.lock();
        for file in files {
            if !loading.contains(file) {
                loading.push(file.clone());
            }
        }
    }

    /// The stinger videos `files` have loaded, or failed to (`failed`,
    /// with why); those not in `wanted` (the setup's) aren't loading any
    /// more either.
    pub(crate) fn loaded_stingers(
        &self,
        files: &[String],
        failed: HashMap<String, String>,
        wanted: &[String],
    ) {
        self.loading_stingers
            .lock()
            .retain(|file| !files.contains(file) && wanted.contains(file));
        let mut failures = self.failed_stingers.lock();
        failures.retain(|file, _| !files.contains(file) && wanted.contains(file));
        failures.extend(failed);
    }

    /// Ask the session to split into a new take (at the next whole second
    /// of the session clock at least half a second away: whole seconds lie
    /// on every file's frame grid), without stopping any source. The receiver gets the new take, or why it
    /// couldn't split; it is always answered, at once once the recording
    /// has ended.
    pub fn split(&self) -> mpsc::Receiver<SplitReply> {
        let (tx, rx) = mpsc::channel();
        let mut state = self.state.lock();
        if state.finished {
            let _ = tx.send(Err(SplitRefused {
                ending: true,
                busy: false,
                message: "the recording has stopped".into(),
            }));
        } else if state
            .clock
            .as_ref()
            .is_some_and(|clock| clock.state.read().requested_pause())
        {
            let _ = tx.send(Err(SplitRefused {
                ending: false,
                busy: false,
                message: "recording.paused: resume before split".into(),
            }));
        } else if state.splitting {
            let _ = tx.send(Err(SplitRefused {
                ending: false,
                busy: true,
                message: "recording.split: the previous split is still starting its files or \
                          finishing the old take; retry once it has finished; recording continues"
                    .into(),
            }));
        } else {
            state.splitting = true;
            state.split = Some(tx);
        }
        rx
    }

    fn take_split(&self) -> Option<mpsc::Sender<SplitReply>> {
        self.state.lock().split.take()
    }

    /// The recording has ended: a split still waiting is refused, and so is
    /// any asked for from now on (until the next recording begins).
    fn finish_recording(&self) {
        let mut state = self.state.lock();
        state.finished = true;
        if let Some(reply) = state.split.take() {
            let _ = reply.send(Err(SplitRefused {
                ending: true,
                busy: false,
                message: "the recording stopped before it could split".into(),
            }));
        }
    }

    /// Take the gain, mute, balance and audio filters of `setup`'s audio
    /// sources, and its pan law, for the program mix (and filtered ISO
    /// audio) from now on. Sync offsets apply from the next recording.
    pub fn set_audio(&self, setup: &Setup) {
        let mut audio = self.audio.lock();
        for source in setup.sources.iter().filter(|s| s.kind.has_audio()) {
            audio.insert(source.id.clone(), source.audio);
        }
        *self.pan_law.lock() = setup.audio.pan_law;
        self.audio_filters.set(setup);
    }

    /// Take `setup`'s sources' filters, for the program from its next frame.
    pub fn set_filters(&self, setup: &Setup) {
        *self.filters.lock() = source_filters(setup);
    }

    /// Reload browser source `id`'s page, bypassing the cache. Errors list
    /// the session's browser sources.
    pub fn refresh_browser(&self, id: &str) -> Result<(), String> {
        let browsers = self.browsers.lock();
        match browsers.get(id) {
            Some(page) => {
                page.refresh();
                Ok(())
            }
            None if browsers.is_empty() => Err("the recording has no browser sources".to_string()),
            None => Err(format!(
                "{id:?} is not a browser source of the recording; browser sources: {}",
                browsers.keys().cloned().collect::<Vec<_>>().join(", ")
            )),
        }
    }

    fn audio_settings(&self, id: &str) -> Option<AudioSettings> {
        self.audio.lock().get(id).copied()
    }

    fn pan_law(&self) -> PanLaw {
        *self.pan_law.lock()
    }

    /// Ids of the video sources the session captures from devices.
    pub fn captured(&self) -> Vec<String> {
        self.frames.lock().ids.clone()
    }

    /// The newest frame the session captured from source `id`, without
    /// taking it from the recording. `F` is the platform's frame type. The
    /// program draws only while something wants its picture, so asking for
    /// it ([`PROGRAM`]) when it has none waits a moment for a fresh one.
    pub fn newest_frame<F: Clone + Send + 'static>(&self, id: &str) -> Option<F> {
        let slot = {
            let frames = self.frames.lock();
            let slots = frames.slots.as_ref()?.downcast_ref::<Slots<F>>()?;
            let (_, slot) = slots.iter().find(|(source, _)| source == id)?;
            slot.clone()
        };
        if id != PROGRAM {
            return slot.newest();
        }
        self.want_program(MONITOR_FPS);
        wait_for_frame(&slot).map(|(frame, _)| frame)
    }

    /// Someone shows the program at `fps`: it is drawn at least that often
    /// (on the canvas's frame grid) for a while.
    pub(crate) fn want_program(&self, fps: u32) {
        self.program_wanted.fetch_max(fps.max(1), Ordering::SeqCst);
    }

    /// The highest rate the program was asked for since this was last
    /// called; 0 if nobody asked.
    pub(crate) fn take_program_wanted(&self) -> u32 {
        self.program_wanted.swap(0, Ordering::SeqCst)
    }

    fn set_frames<F: Clone + Send + 'static>(&self, slots: Slots<F>) {
        let mut frames = self.frames.lock();
        frames.ids = slots.iter().map(|(id, _)| id.clone()).collect();
        frames.slots = Some(Box::new(slots));
    }

    /// A recording starts at `epoch`: its first take, with no markers yet.
    fn begin_recording(&self, epoch: Time, clock: Arc<pause::RecordingClock>) {
        let mut state = self.state.lock();
        state.epoch = Some(epoch);
        state.takes = vec![epoch];
        state.markers.clear();
        state.end = None;
        state.split = None;
        state.finished = false;
        state.clock = Some(clock);
        state.splitting = false;
        state.pause_generation = 0;
        state.saved_pause_generation = 0;
        state.manifest_error = None;
        state.pause_outcome = None;
    }

    /// What the program shows at `at`: the scene of the last switch at or
    /// before it (or the first scene put on air), and the transition from
    /// the scene before still under way, if any.
    fn on_air(&self, at: Time) -> Option<OnAir> {
        let state = self.state.lock();
        let index = state
            .scenes
            .iter()
            .rposition(|(switched, ..)| *switched <= at)
            .unwrap_or(0);
        let (switched, scene, transition) = state.scenes.get(index)?;
        let mut on_air = OnAir {
            scene: scene.clone(),
            from: None,
        };
        if index > 0 && !transition.is_cut() && at >= *switched && transition.duration_ms > 0 {
            let duration = Duration::from_millis(transition.duration_ms.into());
            let progress = at.saturating_since(*switched).as_secs_f64() / duration.as_secs_f64();
            if progress < 1.0 {
                let (_, previous, _) = &state.scenes[index - 1];
                on_air.from = Some(Changing {
                    scene: previous.clone(),
                    transition: transition.clone(),
                    progress: progress as f32,
                    began: *switched,
                });
            }
        }
        Some(on_air)
    }

    fn begin_take(&self, at: Time) {
        let mut state = self.state.lock();
        state.takes.push(at);
    }

    fn take_changed(&self) -> Option<u64> {
        let mut state = self.state.lock();
        std::mem::take(&mut state.changed).then_some(state.pause_generation)
    }

    /// Markers and scene switches of the take running from `from` until
    /// `until`, on its timeline. The scene on air when it starts is at 0.
    fn timeline(&self, from: Time, until: Time) -> (Vec<Marker>, Vec<SceneChange>) {
        let state = self.state.lock();
        let cuts = state.clock.as_ref().map(|clock| clock.state.read());
        let seconds = |at: Time| match &cuts {
            Some(cuts) => cuts.map(at).saturating_since(cuts.map(from)).as_secs_f64(),
            None => at.saturating_since(from).as_secs_f64(),
        };
        let markers = state
            .markers
            .iter()
            .filter(|(at, _)| *at < until && (*at >= from || from == state.epoch.unwrap_or(from)))
            .map(|(at, label)| Marker {
                time_seconds: seconds(*at),
                label: label.clone(),
            })
            .collect();
        let on_air = state.scenes.iter().rposition(|(at, ..)| *at <= from);
        let scenes = state
            .scenes
            .iter()
            .enumerate()
            .filter(|(i, (at, ..))| Some(*i) == on_air || (*at > from && *at < until))
            .map(|(_, (at, scene, transition))| SceneChange {
                time_seconds: seconds(*at),
                scene: scene.to_string(),
                transition: (!transition.is_cut()).then(|| transition.clone()),
            })
            .collect();
        (markers, scenes)
    }
}

/// What the program shows at one moment.
pub(super) struct OnAir {
    pub scene: Arc<str>,
    /// The transition to it still under way, if any.
    pub from: Option<Changing>,
}

/// A transition under way.
pub(super) struct Changing {
    /// The scene it changes from.
    pub scene: Arc<str>,
    pub transition: Transition,
    /// How far it has got, 0 to 1.
    pub progress: f32,
    /// When it began (the switch).
    pub began: Time,
}

/// Called with live counters while recording.
pub type OnStatus<'a> = &'a (dyn Fn(&LiveStatus) + Sync);

/// Called with the paths of the files a session is writing, its first
/// take's manifest, and notes about how its files were opened (like the
/// platform's encoder admission; empty where there is nothing to say).
pub type OnStart<'a> = &'a (dyn Fn(&[PathBuf], &Path, &[String]) + Sync);

/// Called with a finished take's number (from 1), its manifest's path and
/// the manifest as written.
pub type OnTake<'a> = &'a (dyn Fn(u32, &Path, &SessionManifest) + Sync);

struct FilePlan {
    /// The recording this file belongs to (see `next_recording`).
    recording: u64,
    /// The take this file belongs to, from 1.
    take: u32,
    /// The take's timeline zero, once it is known.
    epoch: AtomicI64,
    source: String,
    /// The source's kind, for the manifest.
    kind: &'static str,
    /// When the file's timeline starts (its first frame), published by its
    /// writer; `i64::MIN` until then.
    start: AtomicI64,
    /// Live counters, published by the file's writer: frames written, and
    /// the time of the newest frame the writer has handled (written or not).
    frames: AtomicU64,
    newest: AtomicI64,
    audio_dropouts: AtomicU64,
    /// Native backend selection: -1 unknown, 0 software, 1 hardware.
    hardware_accelerated: AtomicI64,
    /// First encoded packet written: -1 unknown, 0 waiting, 1 active.
    output_active: AtomicI64,
    audio: Vec<String>,
    /// Source timing policy for each audio track (the program is continuous).
    audio_timing: Vec<file::AudioTiming>,
    spec: WriterSpec,
    /// Crash-safe writers only (see `FileWriter::crash_safe`): how much of
    /// the file, from its start, is on disk (nanoseconds; -1 unknown), and
    /// the commit barrier a pause asks for: the generation requested and
    /// the cut it must cover, then the generation the writer answered.
    crash_safe: AtomicBool,
    committed: AtomicI64,
    commit_requested: AtomicU64,
    commit_at: AtomicI64,
    commit_done: AtomicU64,
    /// The generation whose commit failed, and why: the file is failed
    /// from then on, and that pause promises nothing for it.
    commit_failed: AtomicU64,
    commit_error: Mutex<Option<String>>,
    /// A finalized movie can be playable even when its source failed. Keep
    /// its actual duration and tail repeats instead of inferring them later.
    failed_report: Mutex<Option<FileReport>>,
    /// How the platform admitted its encoder, once it has been created.
    encoder: Mutex<Option<ManifestEncoder>>,
}

impl FilePlan {
    /// Seconds of the take's saved timeline up to which this file is on
    /// disk, if its writer says so.
    fn recoverable(&self, epoch: Time, cuts: &pause::Cuts) -> Option<f64> {
        let committed = self.committed.load(Ordering::Relaxed);
        let start = self.start.load(Ordering::SeqCst);
        if committed < 0 || start == i64::MIN {
            return None;
        }
        let offset = cuts
            .map(Time::from_nanos(start))
            .saturating_since(cuts.map(epoch));
        Some(offset.as_secs_f64() + committed as f64 / 1e9)
    }
}

/// Where a source sends packets for one file or output.
type Output<F> = SyncSender<Packet<F>>;

/// A frame or a chunk of audio on the session clock, on its way to a file
/// or another output.
pub enum Packet<F> {
    Video {
        frame: F,
        pts: Time,
    },
    Audio {
        track: usize,
        samples: Arc<[f32]>,
        pts: Time,
    },
    /// An ISO file's frame for `pts` that encoder admission left out
    /// before it was drawn (see [`pressure::IsoCredit`]): the file counts
    /// the tick as a refused frame.
    Skipped {
        pts: Time,
    },
}

/// What the session's opened sources deliver: video size and rate, and
/// audio channels, by source id.
struct Formats {
    video: BTreeMap<String, (u32, u32, u32)>,
    audio: BTreeMap<String, u32>,
    /// Metadata readers of the cameras open when the session started, read
    /// only when a manifest is written. A device opened later (a changed
    /// source) isn't one of them, so its counters never mix in.
    cameras: BTreeMap<String, CameraMetadataReader>,
    /// This recording's identity, shared by all its takes.
    recording: u64,
}

/// A process-unique identity for one recording (all its takes): a split's
/// files are told apart from another recording's by it, never by names.
fn next_recording() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Frame slots by source id.
type Slots<F> = Vec<(String, Arc<FrameSlot<F>>)>;

/// State shared by every thread of a session.
struct Shared {
    epoch: Time,
    /// When the session ends; `Time::MAX` until known.
    end: AtomicI64,
}

impl Shared {
    fn end(&self) -> Time {
        Time::from_nanos(self.end.load(Ordering::SeqCst))
    }

    fn stop_at(&self, at: Time) {
        self.end.fetch_min(at.as_nanos(), Ordering::SeqCst);
    }
}

/// Record a session described by `setup`, blocking until it ends: its
/// sources start, the recording runs, and they stop. Every source must
/// start, or nothing is recorded.
pub fn record<P: Platform>(platform: &P, setup: &Setup, options: &RecordOptions) -> Result<Report> {
    let iso = &setup.recording.iso;
    if !(iso.enabled && !iso.sources.is_empty())
        && !setup.recording.program.enabled
        && !setup.extra_canvas_ids().into_iter().any(|canvas| {
            setup
                .for_canvas(canvas)
                .is_some_and(|s| s.recording.program.enabled)
        })
    {
        bail!(
            "nothing to record: set recording.program.enabled = true, or set \
             recording.iso.enabled = true and list video sources in recording.iso.sources"
        );
    }
    let engine = Engine::start(platform, setup, options.control.clone(), None)?;
    if let Some((_, error)) = engine.first_failure() {
        engine.stop();
        bail!("{error}");
    }
    let report = engine.record(options);
    engine.stop();
    report
}

/// One take: a file per ISO source and a manifest, on a timeline starting
/// at `epoch`.
struct Take {
    epoch: Time,
    started_at: DateTime<Local>,
    manifest_path: PathBuf,
    plans: Vec<Arc<FilePlan>>,
    /// Each file's outcome, once it is finished.
    results: Vec<Option<Result<FileReport>>>,
    /// When the take ends: a split, or the end of the session.
    end: Option<Time>,
    /// Its final manifest has been written.
    finalized: bool,
    // A non-contention failure must be reported, not replayed every tick (or
    // replayed after a successful publication whose final flush failed).
    metadata_error: Option<String>,
    #[cfg(windows)]
    metadata_retry_at: Option<std::time::Instant>,
    /// The final manifest, once made (for [`OnTake`]).
    manifest: Option<SessionManifest>,
    /// The audio-stop errors its final manifest was made with.
    audio_errors: Vec<String>,
}

/// A take's files and manifest name, before it begins.
struct TakePlan {
    plans: Vec<Arc<FilePlan>>,
    manifest_path: PathBuf,
}

impl Take {
    fn begin(plan: TakePlan, epoch: Time, started_at: DateTime<Local>) -> Self {
        for file in &plan.plans {
            file.epoch.store(epoch.as_nanos(), Ordering::SeqCst);
        }
        Self {
            epoch,
            started_at,
            manifest_path: plan.manifest_path,
            results: plan.plans.iter().map(|_| None).collect(),
            plans: plan.plans,
            end: None,
            finalized: false,
            metadata_error: None,
            #[cfg(windows)]
            metadata_retry_at: None,
            manifest: None,
            audio_errors: Vec::new(),
        }
    }

    fn errors(&self) -> Vec<String> {
        self.results
            .iter()
            .flatten()
            .filter_map(|r| r.as_ref().err().map(|err| format!("{err:#}")))
            .collect()
    }

    fn status(&self, errors: &[String]) -> SessionStatus {
        if errors.iter().any(|e| {
            e.starts_with("recording interrupted:")
                || e.starts_with("recording coordinator panicked:")
        }) {
            SessionStatus::Interrupted
        } else if errors.is_empty() {
            SessionStatus::Complete
        } else {
            SessionStatus::Failed
        }
    }
}

/// The files of take `number`, named for the current time.
fn plan_take(setup: &Setup, formats: &Formats, folder: &Path, number: u32) -> Result<TakePlan> {
    let iso = &setup.recording.iso;
    let program = &setup.recording.program;
    // The program has one track: the stereo mix of its audio sources.
    let program_audio: Vec<String> = if program.audio.is_empty() {
        Vec::new()
    } else {
        vec![PROGRAM.to_string()]
    };
    let audio_tracks = |ids: &[String]| -> Vec<AudioTrackSpec> {
        ids.iter()
            .map(|id| match id.as_str() {
                PROGRAM => AudioTrackSpec { channels: 2 },
                id => AudioTrackSpec {
                    channels: formats.audio[id],
                },
            })
            .collect()
    };
    let extension = |container| match container {
        Container::Mov => "mov",
        Container::Mp4 => "mp4",
    };
    let now = Local::now();
    // Each file: its source, kind, size and rate, encoder and audio.
    let mut files = Vec::new();
    if iso.enabled {
        for id in &iso.sources {
            let (width, height, fps) = formats.video[id];
            files.push((
                file_name(&iso.filename, id, &now),
                id.clone(),
                setup.source(id).expect("validated").kind.label(),
                (width, height, fps),
                iso.encoder_for(setup.source(id).expect("validated")),
                {
                    let mut audio = iso.audio_for(
                        setup.source(id).expect("validated"),
                        formats.audio.contains_key(id),
                    );
                    // A listed source with no audio track (a media file
                    // without sound) gets no track; the start warns (#75).
                    audio.retain(|id| formats.audio.contains_key(id));
                    audio
                },
            ));
        }
    }
    if program.enabled {
        // Drawn at the canvas size; the encoder scales it to the file's.
        let (width, height) = program.size(&setup.canvas);
        files.push((
            file_name(&program.filename, PROGRAM, &now),
            PROGRAM.to_string(),
            PROGRAM,
            (width, height, setup.canvas.fps),
            program.encoder.clone(),
            program_audio.clone(),
        ));
    }
    // Every recorded extra canvas's program (`[vertical]`, `[[canvases]]`).
    let extras: Vec<(CanvasId, Setup)> = setup
        .extra_canvas_ids()
        .into_iter()
        .filter_map(|canvas| Some((canvas, setup.for_canvas(canvas)?)))
        .collect();
    for (canvas, extra) in &extras {
        let program = &extra.recording.program;
        if program.enabled {
            let (width, height) = program.size(&extra.canvas);
            files.push((
                file_name(&program.filename, &canvas.program(), &now),
                canvas.program().to_string(),
                PROGRAM,
                (width, height, extra.canvas.fps),
                program.encoder.clone(),
                program_audio.clone(),
            ));
        }
    }
    let names: Vec<(String, &str)> = files
        .iter()
        .map(|(stem, _, _, _, encoder, _)| (stem.clone(), extension(encoder.container)))
        .collect();
    let (paths, manifest_path) =
        unique_paths(folder, &names, &file_name("{date} {time}", "", &now));
    let plans = files
        .into_iter()
        .zip(paths)
        .map(
            |((_, source, kind, (width, height, fps), encoder, audio), path)| {
                Arc::new(FilePlan {
                    recording: formats.recording,
                    take: number,
                    epoch: AtomicI64::new(i64::MIN),
                    source,
                    kind,
                    start: AtomicI64::new(i64::MIN),
                    frames: AtomicU64::new(0),
                    newest: AtomicI64::new(i64::MIN),
                    audio_dropouts: AtomicU64::new(0),
                    hardware_accelerated: AtomicI64::new(-1),
                    output_active: AtomicI64::new(-1),
                    crash_safe: AtomicBool::new(false),
                    committed: AtomicI64::new(-1),
                    commit_requested: AtomicU64::new(0),
                    commit_at: AtomicI64::new(i64::MIN),
                    commit_done: AtomicU64::new(0),
                    commit_failed: AtomicU64::new(0),
                    commit_error: Mutex::new(None),
                    failed_report: Mutex::new(None),
                    encoder: Mutex::new(None),
                    audio: audio.clone(),
                    audio_timing: audio
                        .iter()
                        .map(|id| file::AudioTiming::new(setup.source(id)))
                        .collect(),
                    spec: WriterSpec {
                        path,
                        container: encoder.container,
                        video: VideoTrackSpec {
                            width,
                            height,
                            fps,
                            encoder,
                        },
                        audio: audio_tracks(&audio),
                        created: now.into(),
                    },
                })
            },
        )
        .collect();
    Ok(TakePlan {
        plans,
        manifest_path,
    })
}

/// The sources the program mix's audio filters duck by that it doesn't
/// play itself.
fn sidechain_sources(setup: &Setup) -> Vec<&str> {
    let mix = &setup.recording.program.audio;
    let mut out = Vec::new();
    for id in mix {
        let filters = setup.source(id).map_or(&[][..], |s| &s.audio_filters[..]);
        for key in filters.iter().filter_map(|filter| filter.sidechain()) {
            if !mix.iter().any(|m| m == key) && !out.contains(&key) {
                out.push(key);
            }
        }
    }
    out
}

/// The slots of the ISO sources whose files take their frames as captured,
/// by source id, for writers that must see one before they open (see
/// [`Platform::create_writer_for_frame`]).
type WriterInputs<F> = BTreeMap<String, Arc<FrameSlot<F>>>;

/// Waits for an input's first frame, when the platform asks for it.
type FirstFrame<'a, F> = Box<dyn Fn() -> Result<F> + 'a>;

/// A writer for each file, or none if any can't be created, and notes on
/// how the platform admitted them. A file whose source is in `inputs` waits
/// briefly for that source's first frame.
fn create_writers<P: Platform>(
    platform: &P,
    plans: &[Arc<FilePlan>],
    inputs: &WriterInputs<P::Frame>,
) -> Result<(Vec<P::Writer>, Vec<String>)> {
    // Each input waits for its source's first frame only when the platform
    // asks for it, so the default keeps the per-file order.
    let waits: Vec<Option<FirstFrame<'_, P::Frame>>> = plans
        .iter()
        .map(|plan| {
            inputs.get(&plan.source).map(|slot| {
                let source = &plan.source;
                Box::new(move || {
                    wait_for_frame(slot)
                        .with_context(|| format!("{source:?} sent no frame to record"))
                        .map(|(frame, _)| frame)
                }) as FirstFrame<'_, P::Frame>
            })
        })
        .collect();
    let requests: Vec<WriterRequest<'_, P::Frame>> = plans
        .iter()
        .zip(&waits)
        .map(|(plan, wait)| WriterRequest {
            spec: &plan.spec,
            recording: plan.recording,
            take: plan.take,
            output: format!(
                "{}/{}",
                if is_program(plan) { "program" } else { "iso" },
                plan.source
            ),
            iso: !is_program(plan),
            input: wait.as_deref(),
        })
        .collect();
    let (mut writers, admission) = platform.create_writer_batch(&requests)?;
    if writers.len() != plans.len() {
        let opened = writers.len();
        writers.into_iter().for_each(FileWriter::cancel);
        bail!(
            "the platform opened {opened} writers for {} files; none were kept",
            plans.len()
        );
    }
    let mut notes = Vec::new();
    if let Some(admission) = &admission {
        let summary = admission_note(admission);
        isocast_diagnostics::emit(
            "info",
            "recording",
            &summary,
            serde_json::to_value(admission).unwrap_or_default(),
        );
        notes.push(summary);
        for plan in plans {
            let output = admission
                .outputs
                .iter()
                .find(|output| output.take == plan.take && output.path == plan.spec.path);
            *plan.encoder.lock() = output.map(|output| ManifestEncoder {
                admission: admission.mode.to_string(),
                source: admission.source.map(str::to_string),
                calibrated: admission.calibrated,
                key: admission.key.clone(),
                planned_preset: output.planned_preset,
                applied_preset: output.applied_preset,
                backend: output.backend.clone(),
                estimated_load: admission.estimated_load,
                reason: admission.reason.clone(),
            });
        }
    }
    for (plan, writer) in plans.iter().zip(&mut writers) {
        plan.hardware_accelerated.store(
            writer.hardware_accelerated().map_or(-1, i64::from),
            Ordering::Relaxed,
        );
        if let Some(canvas) = program_canvas(plan) {
            // Preserve the id for per-canvas pins; native admission
            // still puts every non-main canvas in the secondary class.
            writer.prioritize_video(canvas);
        }
    }
    Ok((writers, notes))
}

/// One line saying how a take's encoders were admitted: the mode, the
/// figures' source and key, the load, each file's planned and applied
/// preset, and why measured figures were not applied.
fn admission_note(admission: &crate::EncoderAdmission) -> String {
    let mut note = format!("encoder admission: {}", admission.mode);
    if let Some(source) = admission.source {
        note.push_str(&format!(
            " with {source} figures{}",
            if admission.calibrated {
                " (measured)"
            } else {
                " (estimates, not measurements)"
            }
        ));
    }
    if let Some(key) = &admission.key {
        note.push_str(&format!(", adapter {key}"));
    }
    if let Some(load) = admission.estimated_load {
        note.push_str(&format!(", estimated load {:.0}%", load * 100.0));
    }
    if admission.accounted_sessions > 0 || admission.handed_over_sessions > 0 {
        note.push_str(&format!(
            " ({} open session(s) charged at their fixed presets, {} handed over at the split)",
            admission.accounted_sessions, admission.handed_over_sessions
        ));
    }
    let files: Vec<String> = admission
        .outputs
        .iter()
        .map(|output| {
            let preset = |preset: Option<u32>| preset.map_or("-".to_string(), |p| format!("P{p}"));
            format!(
                "{} planned {} applied {} ({})",
                output.output,
                preset(output.planned_preset),
                preset(output.applied_preset),
                output.backend
            )
        })
        .collect();
    if !files.is_empty() {
        note.push_str(&format!(": {}", files.join(", ")));
    }
    if let Some(reason) = &admission.reason {
        note.push_str(&format!("; {reason}"));
    }
    note
}

/// Whether `plan` writes a canvas's program (not an ISO source).
fn is_program(plan: &FilePlan) -> bool {
    plan.kind == PROGRAM
}

/// Frame slots between a take's `epoch` and its program file's first
/// picture at `start`: ticks the program skipped (drawn too late, or never
/// drawn while it warmed up), so dropped (#150). An ISO file whose source
/// had no picture yet keeps its start offset instead: nothing was lost.
///
/// Only whole slots count: an extra canvas at its own rate ticks on the
/// engine's grid while the take's epoch is on the main canvas's, so its
/// first picture can come half a slot after the epoch without any lost.
fn head_dropped(plan: &FilePlan, epoch: Time, start: Time) -> u64 {
    if !is_program(plan) || start <= epoch {
        return 0;
    }
    let fps = plan.spec.video.fps;
    let mut slots = start.ticks_since(epoch, fps).max(0) as u64;
    // Truncated tick arithmetic: a slot reached from the epoch may lie 1 ns
    // past a picture on it (see `file::file_end`).
    if slots > 0
        && epoch.add_ticks(slots, fps).as_nanos() > start.as_nanos() + file::TICK_ROUNDING_NS
    {
        slots -= 1;
    }
    slots
}

/// The canvas whose program `plan` writes, if it writes one.
fn program_canvas(plan: &FilePlan) -> Option<CanvasId> {
    if !is_program(plan) {
        return None;
    }
    match plan.source.as_str() {
        PROGRAM => Some(CanvasId::Main),
        source => CanvasId::parse(source.strip_suffix("-program")?).ok(),
    }
}

/// Live counters for every file. A frame counts as dropped once a newer
/// one has reached the file, or once it is [`STALL_AFTER`] overdue (so a
/// file that stops receiving frames shows up too).
fn live_status(
    plans: &[Arc<FilePlan>],
    levels: Vec<AudioLevel>,
    shared: &Shared,
    now: Time,
    clock: &pause::RecordingClock,
) -> LiveStatus {
    let cuts = clock.state.read();
    let overdue = cuts.map((now - STALL_AFTER).min(shared.end()));
    let status = cuts.status(now.min(shared.end()));
    LiveStatus {
        levels,
        monitor: None,
        elapsed_seconds: status.elapsed_seconds,
        paused: status.paused,
        paused_seconds: status.paused_seconds,
        handover: false,
        disk: None,
        files: plans
            .iter()
            .map(|plan| {
                let start = plan.start.load(Ordering::SeqCst);
                // The newest frame first: the count is at least up to it.
                let newest = Time::from_nanos(plan.newest.load(Ordering::Acquire));
                let frames = plan.frames.load(Ordering::Relaxed);
                let epoch = plan.epoch.load(Ordering::SeqCst);
                let fps = plan.spec.video.fps;
                let due = if start == i64::MIN {
                    // A program owes its frames from the take's start even
                    // before its first one arrives (#150).
                    if is_program(plan) && epoch != i64::MIN {
                        overdue
                            .ticks_since(cuts.map(Time::from_nanos(epoch)), fps)
                            .max(0) as u64
                    } else {
                        0
                    }
                } else {
                    let start = Time::from_nanos(start);
                    let seen = newest.ticks_since(start, fps) + 1;
                    let head = if epoch == i64::MIN {
                        0
                    } else {
                        head_dropped(plan, cuts.map(Time::from_nanos(epoch)), start)
                    };
                    seen.max(overdue.ticks_since(start, fps)).max(0) as u64 + head
                };
                let committed = plan.committed.load(Ordering::Relaxed);
                LiveFile {
                    source: plan.source.clone(),
                    codec: Some(plan.spec.video.encoder.codec),
                    hardware_accelerated: match plan.hardware_accelerated.load(Ordering::Relaxed) {
                        0 => Some(false),
                        1 => Some(true),
                        _ => None,
                    },
                    output_active: match plan.output_active.load(Ordering::Acquire) {
                        0 => Some(false),
                        1 => Some(true),
                        _ => None,
                    },
                    frames,
                    dropped_frames: due.saturating_sub(frames),
                    audio_dropouts: plan.audio_dropouts.load(Ordering::Relaxed),
                    committed_seconds: (committed >= 0).then(|| committed as f64 / 1e9),
                }
            })
            .collect(),
    }
}

/// A take's manifest as it stands: files that haven't finished have no
/// duration or counts yet.
#[allow(clippy::too_many_arguments)]
fn take_manifest<P: Platform>(
    platform: &P,
    setup: &Setup,
    take: &Take,
    status: SessionStatus,
    errors: &[String],
    stops: &[AudioStopped],
    control: Option<&SessionControl>,
    canvas_controls: &[(CanvasId, &SessionControl)],
    cameras: &BTreeMap<String, CameraMetadataReader>,
) -> SessionManifest {
    let finished = status != SessionStatus::Recording;
    let mapped = |at| control.map_or(at, |c| c.recording_time(at));
    let plans = &take.plans;
    // A file that failed has no report; its counters say what it holds and
    // what it lost up to the take's end.
    let failed = |plan: &FilePlan| {
        if let Some(mut report) = plan.failed_report.lock().clone() {
            // Keep the finalized media duration, but account for the frames
            // missing through this take's actual end, including its tail.
            let start = plan.start.load(Ordering::SeqCst);
            if let Some(end) = take.end
                && start != i64::MIN
            {
                let start = Time::from_nanos(start);
                let fps = plan.spec.video.fps;
                let end = file::file_end(start, mapped(end).max(start), fps);
                let expected = end.ticks_since(start, fps).max(0) as u64
                    + head_dropped(plan, mapped(take.epoch), start);
                report.dropped_frames = report.dropped_frames.max(
                    expected.saturating_sub(report.frames.saturating_sub(report.repeated_frames)),
                );
            }
            return Some(report);
        }
        let start = plan.start.load(Ordering::SeqCst);
        let end = take.end?;
        // A failed writer may have closed long before the take ended. Read
        // the finalized/durable media instead of inventing the missing tail.
        let durable = platform
            .media_info(&plan.spec.path)
            .ok()
            .map(|i| i.duration);
        (start != i64::MIN).then(|| {
            let start = Time::from_nanos(start);
            let fps = plan.spec.video.fps;
            // Unreadable media: through its newest handled frame, never the
            // take's end.
            let newest = plan.newest.load(Ordering::SeqCst);
            let duration = durable.unwrap_or_else(|| {
                if newest == i64::MIN {
                    Duration::ZERO
                } else {
                    Time::from_nanos(newest)
                        .add_ticks(1, fps)
                        .saturating_since(start)
                }
            });
            let end = file::file_end(start, mapped(end).max(start), fps);
            let frames = plan.frames.load(Ordering::SeqCst);
            FileReport {
                path: plan.spec.path.clone(),
                take: plan.take,
                source: plan.source.clone(),
                audio: plan.audio.clone(),
                start_offset_seconds: start.saturating_since(mapped(take.epoch)).as_secs_f64(),
                duration_seconds: duration.as_secs_f64(),
                frames,
                repeated_frames: 0,
                dropped_frames: (end.ticks_since(start, fps).max(0) as u64
                    + head_dropped(plan, mapped(take.epoch), start))
                .saturating_sub(frames),
                audio_dropouts: plan.audio_dropouts.load(Ordering::Relaxed),
                audio_stops: Vec::new(),
            }
        })
    };
    let results: Vec<Option<Cow<FileReport>>> = take
        .results
        .iter()
        .zip(plans)
        .map(|(r, plan)| match r {
            Some(Ok(report)) => Some(Cow::Borrowed(report)),
            Some(Err(_)) => failed(plan).map(Cow::Owned),
            None => None,
        })
        .collect();
    let (markers, scenes) = control.map_or_else(Default::default, |control| {
        control.timeline(take.epoch, take.end.unwrap_or(Time::MAX))
    });
    SessionManifest {
        version: MANIFEST_VERSION,
        setup: setup.name.clone(),
        status,
        started_at: take
            .started_at
            .to_rfc3339_opts(SecondsFormat::Millis, false),
        host_time_ns: take.epoch.as_nanos(),
        duration_seconds: finished
            .then(|| {
                results
                    .iter()
                    .flatten()
                    .map(|r| r.start_offset_seconds + r.duration_seconds)
                    .reduce(f64::max)
            })
            .flatten(),
        markers,
        scenes,
        vertical_scenes: canvas_controls
            .iter()
            .find(|(canvas, _)| canvas.is_vertical())
            .map_or_else(Vec::new, |(_, control)| {
                control
                    .timeline(take.epoch, take.end.unwrap_or(Time::MAX))
                    .1
            }),
        canvas_scenes: canvas_controls
            .iter()
            .filter(|(canvas, _)| !canvas.is_main() && !canvas.is_vertical())
            .map(|(canvas, control)| {
                (
                    canvas.to_string(),
                    control
                        .timeline(take.epoch, take.end.unwrap_or(Time::MAX))
                        .1,
                )
            })
            .collect(),
        pauses: control.map_or_else(Vec::new, |c| {
            let state = c.state.lock();
            state.clock.as_ref().map_or_else(Vec::new, |clock| {
                clock
                    .state
                    .read()
                    .manifest(take.epoch, take.end.unwrap_or(Time::MAX))
            })
        }),
        files: plans
            .iter()
            .zip(results)
            .enumerate()
            .filter(|(index, (plan, _))| {
                // A failed handoff has no file. Keep its error in the take,
                // not a phantom path in the file list returned to clients.
                plan.start.load(Ordering::SeqCst) != i64::MIN
                    || !matches!(&take.results[*index], Some(Err(_)))
            })
            .map(|(_, (plan, result))| {
                let start = plan.start.load(Ordering::SeqCst);
                ManifestFile {
                    path: plan
                        .spec
                        .path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    source: plan.source.clone(),
                    kind: plan.kind.to_string(),
                    width: plan.spec.video.width,
                    height: plan.spec.video.height,
                    fps: plan.spec.video.fps,
                    audio: plan
                        .audio
                        .iter()
                        .zip(&plan.spec.audio)
                        .map(|(source, track)| ManifestAudio {
                            source: source.clone(),
                            channels: track.channels,
                        })
                        .collect(),
                    start_offset_seconds: (start != i64::MIN).then(|| {
                        Time::from_nanos(start)
                            .saturating_since(mapped(take.epoch))
                            .as_secs_f64()
                    }),
                    duration_seconds: result.as_ref().map(|r| r.duration_seconds),
                    frames: result.as_ref().map(|r| r.frames),
                    repeated_frames: result.as_ref().map(|r| r.repeated_frames),
                    dropped_frames: result.as_ref().map(|r| r.dropped_frames),
                    audio_dropouts: result.as_ref().map(|r| r.audio_dropouts),
                    audio_stops: file_audio_stops(setup, take, plan, stops, mapped),
                    encoder: plan.encoder.lock().clone(),
                }
            })
            .collect(),
        errors: errors.to_vec(),
        // Frozen once the take's final manifest is made, so restating it
        // later never adds what the cameras saw after the take ended.
        cameras: match &take.manifest {
            Some(manifest) => manifest.cameras.clone(),
            None => cameras
                .iter()
                .map(|(id, read)| (id.clone(), read()))
                .collect(),
        },
    }
}

/// An audio source that stopped while recording (a microphone unplugged, a
/// picked input gone, an app's audio tap ended), and when the engine had it
/// back, if it opened it again.
#[derive(Debug, Clone)]
pub(crate) struct AudioStopped {
    pub source: String,
    /// The end of the last audio it delivered, on the session clock (where
    /// the files' tracks lose it).
    pub at: Time,
    pub reason: String,
    /// The start of the first audio it delivered once reopened.
    pub resumed: Option<Time>,
}

/// How an audio stop's reason starts in a take that began after it.
const STOPPED_BEFORE: &str = "it stopped before this take began: ";

/// The stops of `stops` that `plan`'s file (in `take`) carries: one per
/// track holding the stopped source's audio, its own or the program mix
/// it was mixed into, timed on the take's timeline. `mapped` takes a time
/// on the session clock to the saved recording clock (pauses cut out).
fn file_audio_stops(
    setup: &Setup,
    take: &Take,
    plan: &FilePlan,
    stops: &[AudioStopped],
    mapped: impl Fn(Time) -> Time,
) -> Vec<AudioStop> {
    let end = take.end.unwrap_or(Time::MAX);
    let mix = &setup.recording.program.audio;
    let seconds = |at: Time| {
        mapped(at)
            .saturating_since(mapped(take.epoch))
            .as_secs_f64()
    };
    let mut out = Vec::new();
    for stop in stops.iter().filter(|stop| stop.at < end) {
        for (track, id) in plan.audio.iter().enumerate() {
            let program = id == PROGRAM && mix.contains(&stop.source);
            if !program && *id != stop.source {
                continue;
            }
            // Only the program mix takes a reopened source back: the ISO
            // files' routes are fixed for the recording.
            let resumed = stop.resumed.filter(|_| program);
            if resumed.is_some_and(|at| at <= take.epoch) {
                continue;
            }
            let before = stop.at <= take.epoch;
            out.push(AudioStop {
                source: stop.source.clone(),
                track: track as u32,
                time_seconds: if before { 0.0 } else { seconds(stop.at) },
                resumed_seconds: resumed.filter(|at| *at < end).map(seconds),
                reason: if before {
                    format!("{STOPPED_BEFORE}{}", stop.reason)
                } else {
                    stop.reason.clone()
                },
            });
        }
    }
    out
}

/// A take's errors for the audio sources that stopped in it (or before it
/// and stayed stopped), naming the files that lost their audio.
fn audio_stop_errors(
    setup: &Setup,
    take: &Take,
    stops: &[AudioStopped],
    control: Option<&SessionControl>,
) -> Vec<String> {
    let mapped = |at| control.map_or(at, |c| c.recording_time(at));
    // One line per stop, with every file it is missing from.
    let mut lines: Vec<(AudioStop, Vec<String>)> = Vec::new();
    for plan in &take.plans {
        let name = plan
            .spec
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        for stop in file_audio_stops(setup, take, plan, stops, mapped) {
            let same = |(line, _): &&mut (AudioStop, Vec<String>)| {
                (&line.source, line.time_seconds, line.resumed_seconds)
                    == (&stop.source, stop.time_seconds, stop.resumed_seconds)
            };
            match lines.iter_mut().find(same) {
                Some((_, files)) => {
                    if !files.contains(&name) {
                        files.push(name.clone());
                    }
                }
                None => lines.push((stop, vec![name.clone()])),
            }
        }
    }
    lines
        .into_iter()
        .map(|(stop, files)| {
            let AudioStop {
                source,
                time_seconds: time,
                resumed_seconds: resumed,
                reason,
                ..
            } = stop;
            let until = match resumed {
                Some(r) => format!(" until {r:.3} s"),
                None => String::new(),
            };
            let files_are = format!(
                "{} {}",
                files.join(", "),
                if files.len() == 1 { "is" } else { "are" }
            );
            match reason.strip_prefix(STOPPED_BEFORE) {
                Some(reason) => format!(
                    "{source}: its audio stopped before this take began, so {files_are} \
                     missing it from the start{until}: {reason}"
                ),
                None => format!(
                    "{source}: its audio stopped at {time:.3} s, so {files_are} missing it \
                     from there{until}: {reason}"
                ),
            }
        })
        .collect()
}

/// Write the manifest so that a reader (or a crash) never sees half of it.
/// The `.partial` is made new (`O_EXCL | O_NOFOLLOW`), never written
/// through whatever was at its name (a link a plugin put there, say).
#[cfg(not(windows))]
fn write_manifest(path: &Path, manifest: &SessionManifest) -> Result<()> {
    write_manifest_with(path, manifest, false)
}

/// Write the manifest and flush it (and its folder entry) through to the
/// disk: for the checkpoint a pause reply vouches for.
#[cfg(not(windows))]
fn write_manifest_durable(path: &Path, manifest: &SessionManifest) -> Result<()> {
    write_manifest_with(path, manifest, true)
}

#[cfg(not(windows))]
fn write_manifest_with(path: &Path, manifest: &SessionManifest, durable: bool) -> Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let partial = path.with_extension("json.partial");
    let json = serde_json::to_string_pretty(manifest)?;
    // Remove a leftover entry, never follow it into a plugin's target.
    if partial.symlink_metadata().is_ok() {
        let _ = std::fs::remove_file(&partial);
    }
    let write = || -> std::io::Result<()> {
        #[cfg(unix)]
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&partial)?;
        // create_new is atomic and refuses existing files, including links.
        // On Windows std also sets FILE_FLAG_OPEN_REPARSE_POINT. Keep the
        // Unix open flags and all durability/error handling unchanged.
        #[cfg(not(unix))]
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)?;
        file.write_all((json + "\n").as_bytes())?;
        if durable {
            file.sync_all()?;
        }
        drop(file);
        std::fs::rename(&partial, path)?;
        if durable && let Some(folder) = path.parent() {
            std::fs::File::open(folder)?.sync_all()?;
        }
        Ok(())
    };
    write()
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&partial);
        })
        .with_context(|| format!("could not write {}", path.display()))
}

/// The file recordings in a folder lock (shared) while they write there, so
/// a manifest a killed or crashed recording left saying "recording" can be
/// told from one still being written.
#[cfg(not(windows))]
const FOLDER_LOCK: &str = ".isocast.lock";

/// What an interrupted session's manifest says about it.
#[cfg(not(any(windows, target_os = "linux")))]
const INTERRUPTED: &str = "the recording ended without finishing: the process recording it \
                           (isocastd or `isocast record`) was stopped or crashed; only completed \
                           fragments may be playable, and a file with no completed fragment may \
                           not open. Recoverable durations and frame counts are unknown; inspect \
                           every file's video and audio tracks";
/// The same, for files a crash-safe writer left and recovery finished.
#[cfg(not(any(windows, target_os = "linux")))]
const INTERRUPTED_RECOVERED: &str = "the recording ended without finishing: the process recording it (isocastd or `isocast \
     record`) was stopped or crashed. Its files were finished from the fragments that had \
     reached the disk; the durations here are what was recovered, frame counts are unknown";

/// A recording's hold on its folder's lock, released when dropped.
#[cfg(not(windows))]
pub(crate) struct FolderLock {
    _file: std::fs::File,
}

#[cfg(not(windows))]
impl FolderLock {
    /// Hold `folder`'s lock alongside other recordings there; `None` if the
    /// folder can't be locked (some network shares).
    pub(crate) fn hold(folder: &Path) -> Option<Self> {
        let file = lock_file(folder)?;
        file.lock_shared().ok()?;
        Some(Self { _file: file })
    }
}

#[cfg(windows)]
pub(crate) struct FolderLock {
    directory: isocast_mux::file::Directory,
}

#[cfg(windows)]
impl FolderLock {
    pub(crate) fn hold(folder: &Path) -> Result<Self> {
        Ok(Self {
            directory: isocast_mux::file::Directory::for_recording(folder).with_context(|| {
                format!(
                    "could not acquire recording ownership of {}",
                    folder.display()
                )
            })?,
        })
    }

    pub(crate) fn write(
        &self,
        path: &Path,
        manifest: &SessionManifest,
        durable: bool,
    ) -> Result<()> {
        isocast_mux::recovery::write_manifest_in(
            &self.directory,
            path.file_name().context("manifest has no file name")?,
            manifest,
            durable,
        )
    }

    /// Already on the recording's background finalizer, after its writers have
    /// joined. Retain this pinned folder lease and the exact final counts until
    /// publication succeeds. Shutdown/the deadline leaves a durable sidecar
    /// when possible; if that is blocked too, try the primary before returning.
    pub(crate) fn finish(
        &self,
        path: &Path,
        manifest: &SessionManifest,
        control: &SessionControl,
    ) -> Result<()> {
        self.finish_until(
            path,
            manifest,
            control,
            std::time::Instant::now() + Duration::from_secs(300),
        )
    }

    pub(crate) fn save_final(&self, path: &Path, manifest: &SessionManifest) -> Result<()> {
        isocast_mux::recovery::save_final_in(
            &self.directory,
            path.file_name().context("manifest has no file name")?,
            manifest,
        )
    }

    fn finish_until(
        &self,
        path: &Path,
        manifest: &SessionManifest,
        control: &SessionControl,
        deadline: std::time::Instant,
    ) -> Result<()> {
        let mut final_saved = false;
        let mut delay = Duration::from_millis(500);
        loop {
            let result = (|| {
                if !final_saved {
                    self.save_final(path, manifest)?;
                    final_saved = true;
                }
                self.write(path, manifest, true)
            })();
            match result {
                Ok(()) => {
                    control.metadata_result(path, None);
                    return Ok(());
                }
                Err(error) if isocast_mux::file::metadata_blocked(&error) => {
                    control.metadata_result(path, Some(&error));
                    if control.metadata_shutdown.load(Ordering::SeqCst)
                        || std::time::Instant::now() >= deadline
                    {
                        control.metadata_result(path, None);
                        if !final_saved {
                            // A held sidecar must not prevent a last attempt at
                            // the primary when shutdown/deadline stops retries.
                            return self.write(path, manifest, true).with_context(|| format!(
                                "could not save final metadata sidecar for {}: {error:#}; primary publication also failed",
                                path.display()
                            ));
                        }
                        return Err(error).with_context(|| format!(
                            "final metadata saved durably in {}.final; publication stopped at shutdown or the five-minute retry limit; recovery adopts it if the primary is still recording",
                            path.display()
                        ));
                    }
                    thread::sleep(
                        delay.min(deadline.saturating_duration_since(std::time::Instant::now())),
                    );
                    delay = (delay * 2).min(Duration::from_secs(5));
                }
                Err(error) => {
                    control.metadata_result(path, None);
                    return Err(error);
                }
            }
        }
    }
}

#[cfg(not(windows))]
fn lock_file(folder: &Path) -> Option<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(folder.join(FOLDER_LOCK))
        .ok()
}

/// Session manifests in `folder` that still say "recording" although
/// nothing records there: their recording was killed or crashed. Empty
/// while a recording writes in the folder (it holds the lock), or if the
/// folder can't be read or locked. With `mark`, each is rewritten as interrupted,
/// saying so.
#[cfg(any(windows, target_os = "linux"))]
pub fn interrupted_sessions(folder: &Path, mark: bool) -> Vec<PathBuf> {
    isocast_mux::recovery::interrupted_sessions(folder, mark).unwrap_or_default()
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn interrupted_sessions(folder: &Path, mark: bool) -> Vec<PathBuf> {
    let Some(lock) = lock_file(folder) else {
        return Vec::new();
    };
    // Exclusive while scanning, so no recording starts writing meanwhile.
    if lock.try_lock().is_err() {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(|entry| Some(entry.ok()?.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".session.json") && !name.starts_with('.'))
        })
        .filter_map(|path| {
            let json = std::fs::read_to_string(&path).ok()?;
            let mut manifest: SessionManifest = serde_json::from_str(&json).ok()?;
            if manifest.status != SessionStatus::Recording {
                return None;
            }
            if mark {
                manifest.status = SessionStatus::Interrupted;
                manifest.errors.push(INTERRUPTED.to_string());
                annotate_interrupted_pauses(&mut manifest);
                write_manifest(&path, &manifest).ok()?;
            }
            Some(path)
        })
        .collect();
    found.sort();
    found
}

/// What to tell the user about `interrupted` sessions (see
/// [`interrupted_sessions`]), marked or not.
#[cfg(any(windows, target_os = "linux"))]
pub fn interrupted_note(interrupted: &[PathBuf], marked: bool) -> Option<String> {
    if interrupted.is_empty() {
        return None;
    }
    Some(format!(
        "earlier recordings {}: {}; inspect each manifest for verified media, unknown counts and any persistence errors",
        if marked {
            "were inspected for recovery"
        } else {
            "may have been interrupted"
        },
        interrupted
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", "),
    ))
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn interrupted_note(interrupted: &[PathBuf], marked: bool) -> Option<String> {
    let list = interrupted
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let (what, its, manifest, says, it) = match interrupted.len() {
        0 => return None,
        1 => (
            "an earlier recording was".to_string(),
            "its",
            "manifest",
            "says",
            "it",
        ),
        n => (
            format!("{n} earlier recordings were"),
            "their",
            "manifests",
            "say",
            "them",
        ),
    };
    let what = format!(
        "{what} interrupted (killed or crashed): {list}; recoverable duration is unknown. \
         Only completed fragments may play; files with no completed fragment may not open. \
         Inspect each file's audio/video tracks; pause/resume replies do not confirm durability"
    );
    Some(match marked {
        true => format!("{what}, and {its} {manifest} now {says} so (status \"interrupted\")"),
        false => format!(
            "{what}, but {its} {manifest} still {says} \"recording\" until the next recording \
             in this folder marks {it} as failed"
        ),
    })
}

/// Both launch-time and recording-start recovery use the pause policy. An
/// open pause stays open: the crash time cannot be reconstructed from its cut.
#[cfg(not(any(windows, target_os = "linux")))]
fn annotate_interrupted_pauses(manifest: &mut SessionManifest) {
    if manifest.pauses.is_empty() {
        return;
    }
    if let Some(open) = manifest.pauses.last().filter(|p| p.end_seconds.is_none()) {
        manifest
            .errors
            .push(match open.recoverable_seconds_at_pause {
                Some(seconds) => format!(
                    "this take was interrupted while paused; every file was on disk up to \
                 {seconds:.3} s of its timeline when the pause was answered"
                ),
                None => {
                    "this take was interrupted while paused; recoverable duration unknown".into()
                }
            });
    }
    // Crash-safe files' pause replies do state committed media.
    if manifest
        .pauses
        .iter()
        .all(|p| p.recoverable_seconds_at_pause.is_some())
    {
        return;
    }
    manifest.errors.push(
        "this take contains pauses: pause/resume replies checkpoint the cut \
         clock and manifest, not committed media. Pre-pause samples may still \
         be queued or in an unfinished fragment, including after resume; \
         recovered and lost durations cannot be inferred from the pause times"
            .into(),
    );
}

/// Repair orphaned manifests only under the recording folder's exclusive
/// lock. Probe durable media for duration; never infer frame/drop counts.
#[cfg(any(windows, target_os = "linux"))]
pub fn recover_interrupted<P: Platform>(
    _platform: &P,
    folder: &Path,
) -> Vec<isocast_protocol::manifest::InterruptedRecording> {
    match isocast_mux::recovery::recover_directory(folder) {
        Ok(recovery) => {
            for warning in recovery.warnings {
                isocast_diagnostics::diagnostic!("warn"; "recording recovery: {}: {warning}", folder.display());
            }
            recovery.sessions
        }
        Err(error) => {
            // A live recording owns a shared lease; leave it alone. All other
            // failures remain visible, rather than pretending repair succeeded.
            if !error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::WouldBlock)
            {
                isocast_diagnostics::diagnostic!("warn"; "recording recovery: {}: {error:#}", folder.display());
            }
            Vec::new()
        }
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn recover_interrupted<P: Platform>(
    platform: &P,
    folder: &Path,
) -> Vec<isocast_protocol::manifest::InterruptedRecording> {
    let Some(lock) = lock_file(folder) else {
        return Vec::new();
    };
    if lock.try_lock().is_err() {
        return Vec::new();
    }
    let mut recovered = Vec::new();
    let mut paths = std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name().is_some_and(|n| {
                n.to_string_lossy().ends_with(".session.json")
                    && !n.to_string_lossy().starts_with('.')
            })
        })
        .collect::<Vec<_>>();
    paths.sort();
    for path in paths.into_iter().rev().take(100) {
        if !path
            .symlink_metadata()
            .is_ok_and(|m| m.is_file() && m.len() <= (4 << 20))
        {
            continue;
        }
        let Some(mut manifest) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<SessionManifest>(&s).ok())
        else {
            continue;
        };
        // A manifest still saying "recording" is a crash; one saying
        // "failed" or "interrupted" may still hold a file a crash-safe
        // writer could not finish (a write error, or a repair that failed
        // last time): every such file is finished from its fragments now.
        // Files that are movies already are left alone.
        let newly_detected = manifest.status == SessionStatus::Recording;
        if !matches!(
            manifest.status,
            SessionStatus::Recording | SessionStatus::Interrupted | SessionStatus::Failed
        ) {
            continue;
        }
        let mut changed = newly_detected;
        let mut repaired = false;
        if newly_detected {
            manifest.status = SessionStatus::Interrupted;
            manifest.errors.push(INTERRUPTED.into());
            annotate_interrupted_pauses(&mut manifest);
        }
        for file in &mut manifest.files {
            // A file name in this folder, nothing else: recovery repairs
            // only regular files here (the platform refuses links), under
            // this folder's lock, which is held for the whole pass.
            let relative = Path::new(&file.path);
            let mut components = relative.components();
            let Some(std::path::Component::Normal(name)) = components.next() else {
                continue;
            };
            if components.next().is_some() {
                continue;
            }
            let full = folder.join(name);
            let recovered = platform.recover_file(folder, name);
            let probe = match recovered {
                Ok(Some(recovered)) => {
                    repaired = true;
                    if let Some(note) = manifest.errors.iter_mut().find(|e| *e == INTERRUPTED) {
                        *note = INTERRUPTED_RECOVERED.into();
                    }
                    // The movie's own contents replace counters that only
                    // described what the writer had accepted.
                    if let Some(frames) = recovered.frames {
                        file.frames = Some(frames);
                        file.repeated_frames = None;
                        file.dropped_frames = None;
                    }
                    manifest.errors.push(match recovered.frames {
                        Some(_) => format!(
                            "{}: finished at launch from the {} crash-safe fragments that \
                             reached the disk before the interruption: {}",
                            file.path, recovered.fragments, recovered.summary
                        ),
                        None => format!(
                            "{}: its final header was completed at launch (the recording was \
                             interrupted while finishing it)",
                            file.path
                        ),
                    });
                    true
                }
                Ok(None) => newly_detected,
                Err(err) => {
                    let note = format!(
                        "{}: could not be finished from its fragments: {err:#}",
                        file.path
                    );
                    if !manifest.errors.contains(&note) {
                        manifest.errors.push(note);
                        changed = true;
                    }
                    newly_detected
                }
            };
            if probe {
                match platform.media_info(&full) {
                    Ok(info) => file.duration_seconds = Some(info.duration.as_secs_f64()),
                    Err(err) => manifest.errors.push(format!(
                        "{}: durable media could not be probed: {err:#}",
                        file.path
                    )),
                }
            }
        }
        if newly_detected || repaired {
            manifest.duration_seconds = manifest
                .files
                .iter()
                .filter_map(|f| {
                    f.duration_seconds
                        .map(|d| d + f.start_offset_seconds.unwrap_or(0.0))
                })
                .reduce(f64::max);
        }
        if (changed || repaired)
            && let Err(err) = write_manifest(&path, &manifest)
        {
            manifest
                .errors
                .push(format!("could not persist interrupted marker: {err:#}"));
        }
        if manifest.status == SessionStatus::Interrupted || repaired {
            recovered.push(isocast_protocol::manifest::InterruptedRecording {
                newly_detected: newly_detected || repaired,
                manifest: path,
                recording: manifest,
            });
        }
    }
    recovered
}

/// A controlled emergency stop finished the media normally, but the take
/// was interrupted by a critical-state failure, not ended by the operator.
#[cfg(any(windows, target_os = "linux"))]
pub fn mark_interrupted(report: &Report, why: &str) {
    for path in &report.takes {
        if let Err(error) = isocast_mux::recovery::mark_interrupted(path, why) {
            isocast_diagnostics::diagnostic!("warn"; "could not mark emergency recording {}: {error:#}", path.display());
        }
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn mark_interrupted(report: &Report, why: &str) {
    for path in &report.takes {
        if let Some(mut manifest) = std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str::<SessionManifest>(&s).ok())
        {
            manifest.status = SessionStatus::Interrupted;
            manifest.errors.push(why.into());
            let _ = write_manifest(path, &manifest);
        }
    }
}

/// Fill in a file name template: {SOURCE}, {source}, {date}, {time}.
fn file_name(template: &str, source: &str, now: &DateTime<Local>) -> String {
    template
        .replace("{SOURCE}", &source.to_uppercase())
        .replace("{source}", source)
        .replace("{date}", &now.format("%Y-%m-%d").to_string())
        .replace("{time}", &now.format("%H-%M-%S").to_string())
}

/// Paths for every file of a session and its manifest, numbered so none of
/// them overwrites an existing file.
fn unique_paths(
    folder: &Path,
    names: &[(String, &str)],
    manifest_stem: &str,
) -> (Vec<PathBuf>, PathBuf) {
    (1..)
        .map(|n| {
            let suffix = if n == 1 {
                String::new()
            } else {
                format!(" {n}")
            };
            let files = names
                .iter()
                .map(|(stem, extension)| folder.join(format!("{stem}{suffix}.{extension}")))
                .collect::<Vec<_>>();
            let manifest = folder.join(format!("{manifest_stem}{suffix}.session.json"));
            (files, manifest)
        })
        // Any entry at a name takes it, a dangling link included (a link
        // followed would write the file wherever it points).
        .find(|(files, manifest)| {
            [manifest]
                .into_iter()
                .chain(files)
                .all(|p| p.symlink_metadata().is_err())
        })
        .expect("some numbered name is free")
}

pub fn expand_home(folder: &str) -> PathBuf {
    #[cfg(windows)]
    let home =
        Some(isocast_protocol::api::home_folder()).filter(|home| !home.as_os_str().is_empty());
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match (folder.strip_prefix('~'), home) {
        (Some(rest), Some(home))
            if rest.is_empty()
                || rest.starts_with('/')
                || (cfg!(windows) && rest.starts_with('\\')) =>
        {
            let rest = rest.trim_start_matches(|c| c == '/' || (cfg!(windows) && c == '\\'));
            if cfg!(windows) {
                // `~/Videos/Isocast` is the Videos known folder on Windows,
                // wherever it was moved (another drive, OneDrive).
                isocast_protocol::api::home_join(rest)
            } else {
                home.join(rest)
            }
        }
        _ => PathBuf::from(folder),
    }
}

#[cfg(all(test, windows))]
mod metadata_finalization {
    use super::*;
    use std::os::windows::fs::OpenOptionsExt;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    #[test]
    fn retries_after_the_short_budget_and_keeps_final_counts() {
        for hold_sidecar in [false, true] {
            let folder = std::env::temp_dir().join(format!(
                "isocast-final-metadata-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&folder).unwrap();
            let lock = FolderLock::hold(&folder).unwrap();
            let path = folder.join("take.session.json");
            let mut manifest: SessionManifest = serde_json::from_value(serde_json::json!({
                "version": 1, "setup": "held reader", "status": "recording",
                "started_at": "2026-10-25T12:00:00Z", "host_time_ns": 0,
                "files": [], "errors": []
            }))
            .unwrap();
            manifest.status = SessionStatus::Complete;
            manifest.duration_seconds = Some(18.0);
            manifest.files.push(
                serde_json::from_value(serde_json::json!({
                    "path": "program.mov", "source": "program", "kind": "program",
                    "width": 320, "height": 180, "fps": 30, "audio": [],
                    "start_offset_seconds": 0.0, "duration_seconds": 18.0, "frames": 540,
                    "repeated_frames": 0, "dropped_frames": 0, "audio_dropouts": 0
                }))
                .unwrap(),
            );
            let mut initial = manifest.clone();
            initial.status = SessionStatus::Recording;
            initial.duration_seconds = None;
            initial.files[0].frames = None;
            lock.write(&path, &initial, true).unwrap();
            let held_path = if hold_sidecar {
                let mut earlier = manifest.clone();
                earlier.files[0].frames = Some(1);
                lock.save_final(&path, &earlier).unwrap();
                folder.join("take.session.json.final")
            } else {
                path.clone()
            };
            let control = SessionControl::default();
            let reader = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(3)
                .open(&held_path)
                .unwrap();
            let (pending, result) = thread::scope(|scope| {
                let worker = scope.spawn(|| lock.finish(&path, &manifest, &control));
                let deadline = Instant::now() + Duration::from_secs(10);
                while control.metadata_pending().is_empty()
                    && !worker.is_finished()
                    && Instant::now() < deadline
                {
                    thread::sleep(Duration::from_millis(10));
                }
                let pending = control.metadata_pending();
                let split_kept_controls = !control.metadata_finalizing();
                control.finish_recording();
                let was_finalizing = control.metadata_finalizing();
                let before: SessionManifest =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                let was_recording = before.status == SessionStatus::Recording;
                let was_running = !worker.is_finished();
                // Always release the external blocker before asserting or joining.
                drop(reader);
                let result = worker.join().unwrap();
                assert!(
                    was_running && was_recording && split_kept_controls && was_finalizing,
                    "completion reported before publication"
                );
                (pending, result)
            });
            result.unwrap();
            assert_eq!(pending.len(), 1);
            assert!(pending[0].contains("retrying in the background"));
            assert!(control.metadata_pending().is_empty());
            assert!(!control.metadata_finalizing());
            let saved: SessionManifest =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(saved, manifest);
            // Hard links are invalid destinations, not contention to retry forever.
            std::fs::hard_link(&path, folder.join("alias")).unwrap();
            assert!(lock.finish(&path, &manifest, &control).is_err());
            assert!(control.metadata_pending().is_empty());
            drop(lock);
            std::fs::remove_dir_all(folder).unwrap();
        }
    }

    #[test]
    fn shutdown_with_a_blocked_sidecar_still_publishes_the_primary() {
        let folder = std::env::temp_dir().join(format!(
            "isocast-held-final-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&folder).unwrap();
        let path = folder.join("take.session.json");
        let lock = FolderLock::hold(&folder).unwrap();
        let mut manifest: SessionManifest = serde_json::from_value(serde_json::json!({
            "version":1,"setup":"shutdown","status":"recording",
            "started_at":"2026-10-25T12:00:00Z","host_time_ns":123,"files":[],"errors":[]
        }))
        .unwrap();
        lock.write(&path, &manifest, true).unwrap();
        manifest.status = SessionStatus::Interrupted;
        manifest.duration_seconds = Some(18.0);
        manifest
            .errors
            .push("recording interrupted: emergency stop".into());
        lock.save_final(&path, &manifest).unwrap();
        let reader = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(folder.join("take.session.json.final"))
            .unwrap();
        let control = SessionControl::default();
        control.shutdown_metadata();
        let began = Instant::now();
        lock.finish(&path, &manifest, &control).unwrap();
        assert!(began.elapsed() < Duration::from_secs(5));
        assert!(control.metadata_pending().is_empty());
        let saved: SessionManifest =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved, manifest);
        // If both names are held, shutdown remains bounded and reports the
        // unsaved outcome instead of claiming a durable fallback was saved.
        let primary = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(&path)
            .unwrap();
        let began = Instant::now();
        let error = lock.finish(&path, &manifest, &control).unwrap_err();
        assert!(began.elapsed() < Duration::from_secs(5));
        assert!(format!("{error:#}").contains("primary publication also failed"));
        assert!(control.metadata_pending().is_empty());
        drop(primary);
        drop(reader);
        drop(lock);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn shutdown_and_retry_deadline_leave_an_exact_recoverable_outcome() {
        for shutdown in [true, false] {
            let folder = std::env::temp_dir().join(format!(
                "isocast-final-exit-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&folder).unwrap();
            let path = folder.join("take.session.json");
            let lock = FolderLock::hold(&folder).unwrap();
            let mut manifest: SessionManifest = serde_json::from_value(serde_json::json!({
                "version": 1, "setup": "shutdown", "status": "recording",
                "started_at": "2026-10-25T12:00:00Z", "host_time_ns": 123,
                "files": [{"path":"program.mov", "source":"program", "kind":"program",
                    "width":320,"height":180,"fps":30,"audio":[]}], "errors":[]
            }))
            .unwrap();
            lock.write(&path, &manifest, true).unwrap();
            manifest.status = SessionStatus::Complete;
            manifest.duration_seconds = Some(18.0);
            manifest.files[0].frames = Some(540);
            manifest.files[0].dropped_frames = Some(0);
            manifest.files[0].repeated_frames = Some(0);
            manifest.files[0].audio_dropouts = Some(0);
            manifest.files[0].duration_seconds = Some(18.0);
            let control = SessionControl::default();
            if shutdown {
                control.shutdown_metadata();
            }
            let reader = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(3)
                .open(&path)
                .unwrap();
            let began = Instant::now();
            let deadline = began
                + if shutdown {
                    Duration::from_secs(300)
                } else {
                    Duration::ZERO
                };
            let error = lock
                .finish_until(&path, &manifest, &control, deadline)
                .unwrap_err();
            assert!(
                began.elapsed() < Duration::from_secs(5),
                "shutdown/deadline did not bound retries"
            );
            assert!(format!("{error:#}").contains("saved durably"));
            assert!(control.metadata_pending().is_empty());
            drop(lock); // model the process ending; recovery obtains a new lease
            assert!(
                isocast_mux::recovery::interrupted_sessions(&folder, false)
                    .unwrap()
                    .is_empty()
            );
            let held = isocast_mux::recovery::recover_directory(&folder).unwrap();
            assert!(held.sessions.is_empty());
            assert_eq!(
                held.warnings.len(),
                1,
                "the reader still blocks publication"
            );
            drop(reader);
            assert!(
                isocast_mux::recovery::interrupted_sessions(&folder, true)
                    .unwrap()
                    .is_empty()
            );
            let saved: SessionManifest =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(
                saved, manifest,
                "recovery must not downgrade complete to interrupted or lose counts"
            );
            std::fs::remove_dir_all(folder).unwrap();
        }
    }
}
