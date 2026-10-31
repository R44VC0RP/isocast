//! The control API: one method table for the CLI, MCP and the app.
//!
//! Clients speak JSON-RPC 2.0 to the daemon (`isocastd`) over a local
//! socket, one message per line:
//!
//! ```text
//! → {"jsonrpc":"2.0","id":1,"method":"status"}
//! ← {"jsonrpc":"2.0","id":1,"result":{"state":"idle",...}}
//! ```

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::change::{Change, Difference};
use crate::data::{DataDeleteParams, DataGetParams, DataSetParams, DataStatus};
use crate::library::{
    LibraryAction, LibraryCreateParams, LibraryImportObsParams, LibraryImportParams, LibraryKind,
    LibraryRenameParams, LibrarySwitchParams, LibraryTarget,
};
use crate::plugin::{
    PluginAction, PluginAddParams, PluginEnableParams, PluginState, PluginTarget, ProcessState,
};
use crate::stream::{StreamService, StreamStatus};
use crate::{Canvas, CanvasId, Issue, Setup};

/// Every method the engine offers. Serialized as JSON-RPC `method` and
/// `params`. Params a method doesn't have are refused, naming the ones it
/// has, so a typo fails instead of being ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "method",
    content = "params",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Method {
    /// What the engine is doing. Returns [`Status`].
    Status,
    /// Displays, windows, cameras, microphones, audio outputs and
    /// permissions. Returns
    /// [`Devices`](crate::devices::Devices).
    Devices,
    /// The live setup and its revision; with `resolved`, also where layout
    /// put every item. Returns [`Live`]. Its params are optional:
    /// `{"method": "get"}` asks for the setup alone.
    Get(Option<GetParams>),
    /// Work out what a change would do without making it: the differences
    /// and any problems. Returns [`Plan`].
    Plan(Box<Change>),
    /// Make a change, if the live setup is still at `revision` and the
    /// result has no problems. Returns [`Applied`].
    Apply(Box<ApplyParams>),
    /// Turn this connection into a stream of [`Event`] notifications until
    /// it closes. Returns [`Live`] first. Its params are optional and pick
    /// the kinds of events sent (`{"except": ["levels"]}` for all but the
    /// audio levels that come 20 times a second); `{"method": "watch"}`
    /// sends every kind.
    Watch(Option<WatchParams>),
    /// Start recording the ISO sources of `setup`, or of the live setup.
    /// Returns [`Started`] once every file has been created.
    Start(Box<StartParams>),
    /// Stop recording and finish the files. Returns the session's
    /// [`Report`]. Its params are optional: with `session`, it stops only
    /// that session (refused with `WRONG_STATE` if another is running), so
    /// a retried stop never ends a recording someone else started since.
    Stop(Option<StopParams>),
    /// Ask the running session to stop (or cancel one still starting) and
    /// answer at once, without waiting for its files to be finished.
    /// Returns [`StopAccepted`]: an acknowledgement, not a finished
    /// recording. Until the files are done, `status` says `finalizing`
    /// and a new `start` is refused; the [`Event::Stopped`] event (and
    /// `status`'s `last` or `last_error`) then gives the outcome. Asking
    /// again while finalizing answers the same way; refused when idle.
    /// Its params are optional: with `session`, only that session (as
    /// `stop`).
    RequestStop(Option<StopParams>),
    /// Split the running recording into a new take at the next whole
    /// second, without stopping any device: the current files end there and
    /// new ones (with their own manifest) begin. Returns the new take's
    /// files as [`Started`].
    Split,
    /// Cut a paused span out of every recording file; streaming continues.
    /// Returns [`RecordingPause`] once the shared A/V boundary is reached.
    Pause,
    /// Resume the same take and files. Returns [`RecordingPause`].
    Resume,
    /// Mark this moment in the running recording. Returns [`Marked`].
    Marker {
        label: String,
    },
    /// Reload a browser source's page in the running recording, bypassing
    /// the cache (OBS's "Refresh cache of current page"). Returns
    /// [`Refreshed`].
    RefreshBrowser {
        source: String,
    },
    /// Put a scene on air (by id). While recording, the switch is written to
    /// the manifest's scene timeline. Returns [`Switched`].
    SwitchScene {
        scene: String,
        /// How it comes on air; the setup's transition if absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transition: Option<crate::Transition>,
        /// Which canvas: `main` (the default) or `vertical` (the setup's
        /// `[vertical]` canvas, which must not follow the main one).
        #[serde(default, skip_serializing_if = "CanvasId::is_main")]
        canvas: CanvasId,
    },
    /// Stage a scene in preview (by id), to go on air with `take`. Returns
    /// [`Switched`] (with no time).
    Preview {
        scene: String,
        /// Which canvas: `main` (the default) or `vertical` (which must not
        /// follow the main one).
        #[serde(default, skip_serializing_if = "CanvasId::is_main")]
        canvas: CanvasId,
    },
    /// Put the preview scene on air, with a transition (the setup's if
    /// absent); the scene that was on air moves to preview. Returns
    /// [`Switched`].
    Take {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transition: Option<crate::Transition>,
        /// Which canvas: `main` (the default) or `vertical` (which must not
        /// follow the main one).
        #[serde(default, skip_serializing_if = "CanvasId::is_main")]
        canvas: CanvasId,
    },
    /// Check this machine and a setup (the live setup if none is given):
    /// permissions, hardware encoders, devices and the recording disk.
    /// Returns [`Doctor`].
    Doctor(Box<DoctorParams>),
    /// Benchmark encoders on a setup's streams (the live setup if none is
    /// given) and recommend settings. Refused while recording. Returns
    /// [`Tuning`].
    Tune(Box<TuneParams>),
    /// One frame of a video source or a scene, drawn by the GPU as a PNG.
    /// Returns [`Snapshot`].
    Snapshot(SnapshotParams),
    /// The live engine's newest frame of the program, studio mode's
    /// preview or a source, as a JPEG, waiting a while for one newer than
    /// `after` (so a client can show live video by asking again as each
    /// arrives). Asking keeps the engine running, and starts it if needed.
    /// Returns [`Frame`].
    Frame(FrameParams),
    /// Turn this connection into a live video stream of the program,
    /// studio mode's preview or a source, for monitors: low-latency H.264
    /// from the hardware encoder, shared by everyone watching the same
    /// target at the same size and H.264 profile, at the highest rate any of them wants,
    /// running only while someone watches. Returns [`FrameStream`] first;
    /// after that line the connection carries binary [`frames`] records
    /// until either side closes it, and the client may send [`FrameRate`]
    /// lines to change its rate. Keeps the engine running, and starts it
    /// if needed.
    Frames(FramesParams),
    /// Turn this connection into a zero-copy video stream for a monitor in
    /// a native client on the same machine: the daemon's GPU draws the
    /// program, studio mode's preview or a source, scaled to the size the
    /// client asks for, into a small ring of shared GPU surfaces (8-bit
    /// RGBA), and the client's GPU shows them in place. Returns
    /// [`SharedFrameStream`] first; after that line the connection carries
    /// [`shared::Event`] lines (the ring once, then a slot per frame) and
    /// the client sends [`shared::Control`] lines (releasing each slot
    /// when its GPU is done with it, changing rate or size). Drawn only
    /// while someone watches, at the highest rate any viewer of the
    /// program asks for. macOS only so far (IOSurfaces handed over Mach).
    /// Keeps the engine running, and starts it if needed.
    SharedFrames(SharedFramesParams),
    /// Keep the live engine running (`true`) or let it stop once nothing
    /// needs it (`false`). It runs the live setup's sources, program and
    /// mix all the time, as OBS does, so recording and streaming start at
    /// once; it also runs while a recording or stream does, or while a
    /// client asks for frames. Returns [`EngineInfo`], or null once
    /// stopped.
    Engine {
        running: bool,
    },
    /// Read an OBS Studio scene collection into a setup, matching its
    /// devices and sizes against this Mac, without changing the live setup.
    /// Returns [`Imported`](crate::obs::Imported): the setup and what
    /// couldn't come over.
    ImportObs(Box<ImportObsParams>),
    /// Make a good starting setup from this Mac's displays, cameras and
    /// microphones (scenes "Screen + Camera", "Camera" and "Screen";
    /// program and ISO recording), and apply it to the live setup if asked.
    /// Returns [`Initialized`](crate::init::Initialized): the setup, notes
    /// on what was picked and what to do, and the change if applied.
    InitSetup(Box<InitParams>),
    /// The screen recording, camera and microphone permissions, without
    /// asking, with the next step for each that isn't granted (ask, or
    /// where in System Settings to turn it on). Returns
    /// [`PermissionsReport`](crate::devices::PermissionsReport).
    Permissions,
    /// Ask macOS for screen recording, camera and microphone access where
    /// it hasn't been decided (its prompts appear on screen) and wait up to
    /// a minute for the answers; with `permission`, only for that one.
    /// Returns [`PermissionsReport`](crate::devices::PermissionsReport).
    /// Its params are optional: `{"method": "request_permissions"}` asks
    /// for all three.
    RequestPermissions(Option<RequestPermissionsParams>),
    /// Open the System Settings pane where a permission is turned on by
    /// hand. Returns [`Opened`].
    OpenPermissionSettings {
        permission: crate::devices::PermissionKind,
    },
    /// Start streaming the program (the canvas and the program mix) to the
    /// live setup's `[stream]` destinations (every enabled one, or those in
    /// `only`), starting the live engine if needed. Destinations already
    /// streaming are left alone. Recordings start and stop independently
    /// of it. Returns [`Streaming`] once each has gone live or failed, or
    /// an error saying what to do when none could start. Its params are
    /// optional: `{"method": "stream_start"}` starts every enabled one.
    StreamStart(Option<StreamStartParams>),
    /// Stop streaming (to every destination, or those in `only`); the
    /// others keep going. Returns their final [`Streaming`] status. Its
    /// params are optional.
    StreamStop(Option<StreamStartParams>),
    /// Keep a stream key in the Keychain for the live setup's service (or
    /// the one given). It is never logged, shown or returned. Returns
    /// [`StreamKey`].
    StreamKeySet(Box<StreamKeyParams>),
    /// Remove a stored stream key. Returns [`StreamKey`].
    StreamKeyClear(Box<StreamKeyTarget>),
    /// Add a stream destination to the live setup (making its `[stream]`
    /// section, as the main destination, if it has none). Returns
    /// [`StreamDestinations`].
    StreamDestinationAdd(Box<StreamDestinationParams>),
    /// Remove a stream destination from the live setup (not `main`: turn it
    /// off instead, or remove the whole `[stream]`). Returns
    /// [`StreamDestinations`].
    StreamDestinationRemove {
        id: String,
        /// Which canvas's destinations: `main` (the default, `[stream]`) or
        /// `vertical` (`[vertical.stream]`).
        #[serde(default, skip_serializing_if = "CanvasId::is_main")]
        canvas: CanvasId,
    },
    /// Change a stream destination (`id` required; `main` changes the
    /// `[stream]` section itself). Returns [`StreamDestinations`].
    StreamDestinationSet(Box<StreamDestinationParams>),
    /// Add a canvas to the live setup (#84): `vertical` turns on its
    /// `[vertical]` canvas, any other id adds a `[[canvases]]` entry
    /// (1080×1920 at the main frame rate, following the main scene, unless
    /// the params say otherwise). Returns [`Canvases`].
    CanvasAdd(Box<CanvasParams>),
    /// Change a canvas of the live setup (`vertical` or a `[[canvases]]`
    /// id). Returns [`Canvases`].
    CanvasSet(Box<CanvasParams>),
    /// Remove a canvas from the live setup, with its arrangements, stream
    /// and recording (not while it records or streams). Returns
    /// [`Canvases`].
    CanvasRemove {
        canvas: CanvasId,
    },
    /// The scene collections and profiles (the two parts the live setup is
    /// kept in, as in OBS) and which are current. Returns
    /// [`Library`](crate::library::Library).
    Library,
    /// A scene collection or profile as a document (to export it, or to
    /// read one that isn't current). Returns
    /// [`LibraryDocument`](crate::library::LibraryDocument).
    LibraryGet(LibraryTarget),
    /// Make a scene collection or profile, blank or a copy of another, and
    /// switch to it if asked. Returns
    /// [`LibraryChange`](crate::library::LibraryChange).
    LibraryCreate(Box<LibraryCreateParams>),
    /// Rename a scene collection or profile (its id stays). Returns
    /// [`LibraryChange`](crate::library::LibraryChange).
    LibraryRename(Box<LibraryRenameParams>),
    /// Delete a scene collection or profile that isn't current; its file
    /// is kept in the library's `deleted` folder. Returns
    /// [`LibraryChange`](crate::library::LibraryChange).
    LibraryDelete(LibraryTarget),
    /// Use another scene collection, profile or both: the live setup
    /// becomes them, applied to the live engine at once. Refused while
    /// recording or streaming. Returns
    /// [`LibraryChange`](crate::library::LibraryChange).
    LibrarySwitch(LibrarySwitchParams),
    /// Add a scene collection and/or profile from documents or a whole
    /// setup. Returns [`LibraryChange`](crate::library::LibraryChange).
    LibraryImport(Box<LibraryImportParams>),
    /// Add an OBS Studio scene collection as a collection and/or an OBS
    /// profile as a profile, matching devices against this Mac. Returns
    /// [`LibraryChange`](crate::library::LibraryChange) with what couldn't
    /// come over.
    LibraryImportObs(Box<LibraryImportObsParams>),
    /// The installed plugins: version, kinds, state, process and granted
    /// permissions. Returns [`Plugins`](crate::plugin::Plugins).
    PluginList,
    /// Install a plugin from a folder with an `isocast-plugin.toml`: its
    /// manifest is checked, the folder copied into the plugins folder (or
    /// linked), and its process started if it has one, at once. Its
    /// permissions must be granted (`grant`, exactly what it asks for); a
    /// newer version replaces the installed one. Returns
    /// [`PluginChange`](crate::plugin::PluginChange).
    PluginAdd(Box<PluginAddParams>),
    /// Stop a plugin's process and remove it. Returns
    /// [`PluginChange`](crate::plugin::PluginChange).
    PluginRemove(PluginTarget),
    /// Turn a plugin off (its process stops) or on again. Returns
    /// [`PluginChange`](crate::plugin::PluginChange).
    PluginSetEnabled(PluginEnableParams),
    /// Values from the data store (every key, the `keys` named, or those
    /// under a `prefix`) and its revision. Its params are optional:
    /// `{"method": "data_get"}` reads every key. Returns
    /// [`DataValues`](crate::data::DataValues).
    DataGet(Option<DataGetParams>),
    /// Set data keys to JSON values, several at once (all of them, or none
    /// if one is refused): text showing a key (`data = "key"`) changes at
    /// the next frames, without a setup change. With `persist`, they are
    /// kept with the scene collection across restarts. Returns
    /// [`DataRevision`](crate::data::DataRevision).
    DataSet(Box<DataSetParams>),
    /// Delete data keys. Returns [`DataRevision`](crate::data::DataRevision).
    DataDelete(DataDeleteParams),
    /// Create or preview a sanitized offline bug report. Never uploads.
    BugReport(crate::diagnostics::BugReportParams),
    /// Read structured logs with optional severity/component filters and cursor.
    Logs(crate::diagnostics::LogsParams),
    /// Bounded read of a file from a redacted bug-report preview.
    ReportFile(crate::diagnostics::ReportFileParams),
    /// Current profile bindings and global registration warnings.
    HotkeysGet,
    /// Replace or patch profile bindings. Returns hotkeys::Hotkeys.
    HotkeysSet(Box<crate::hotkeys::SetParams>),
    /// Bindable actions, labels, groups, argument schemas and suggested defaults.
    HotkeysActions,
    /// Focused client's key input (press AND release), including globally
    /// registered chords. Matching global/focused presses within 100 ms fire
    /// once; registration alone is not evidence of native delivery.
    HotkeysInput(crate::hotkeys::InputParams),
    /// Feed the configured picture to the installed macOS camera extension.
    /// Returns [`crate::virtual_camera::VirtualCameraStatus`]. Does not install it.
    VirtualCameraStart,
    /// Stop feeding the camera; applications see its Isocast waiting card.
    VirtualCameraStop,
    /// Camera state, output format, counters and any failure.
    VirtualCameraStatus,
    /// Remove Isocast Virtual Camera from macOS (deactivate the release
    /// app's camera extension) so Isocast can be deleted cleanly: stops the
    /// camera first; macOS asks for an administrator password, and the call
    /// waits for it (up to about 4 minutes). Returns
    /// [`crate::virtual_camera::VirtualCameraRemoval`]. Refused by a
    /// development engine (scripts/vcam-uninstall.sh removes its camera).
    VirtualCameraRemove,
    /// Stop recording if needed, then exit the daemon.
    Shutdown,
    /// Recording codec labels and supported containers; returns EncoderOption[].
    EncoderOptions,
    /// Leave Studio Mode by clearing the staged preview. Returns Switched.
    ClearPreview,
    /// Exit the daemon only if nothing is running: refused, atomically,
    /// while it records (or pauses), streams, exports, feeds the virtual
    /// camera or benchmarks encoders; a start racing it is refused instead.
    /// `isocast restart` and app updates use it. Returns null.
    ShutdownIfIdle,
    /// Developer mode: the profile's setting, whether isocastd is writing
    /// verbose records and traces, when it turns itself off, its logs and
    /// the latest per-second trace. Returns
    /// [`DeveloperStatus`](crate::developer::DeveloperStatus).
    DebugStatus,
    /// Turn developer mode on or off (and set when it turns itself off) in
    /// the current profile: verbose logs of every API call and component,
    /// and per-second frame timing traces while recording, in their own
    /// log segments. Changing it never touches a running recording. Returns
    /// [`DeveloperStatus`](crate::developer::DeveloperStatus).
    DebugSet(crate::developer::DebugSetParams),
    /// The recording projects (a folder per recording session) in the
    /// recordings folders, newest first, each with its takes and files.
    /// Its params are optional. Returns
    /// [`Projects`](crate::project::Projects).
    ListProjects(Option<crate::project::ListProjectsParams>),
    /// Rename a project (by id): its folder becomes `<date> <time>
    /// <name>`, numbered if taken. Refused while a recording writes into
    /// it. Manifests keep resolving. Returns
    /// [`ProjectRenamed`](crate::project::ProjectRenamed).
    RenameProject(crate::project::RenameProjectParams),
    /// Predict, without benchmarking, whether a setup's files (the live
    /// setup's, at its sources' live formats, if none is given) fit this
    /// Mac's media engines, from measured per-chip data (and what `tune`
    /// measured here, if it did), with the closest changes that fit. With
    /// `add_iso`, how to turn on that source's ISO file so it fits. Allowed
    /// while recording. Returns [`CapacityEstimate`].
    EstimateCapacity(Box<CapacityParams>),
    /// Look for OBS Studio's scene collections and profiles on this Mac
    /// (its settings folder, or `folder`), without importing anything:
    /// each collection with its scene and source counts and the profile an
    /// import would take its settings from, each profile's video,
    /// recording and stream settings (never a stream key; `key_in_file`
    /// says whether OBS keeps one in plain text), and files that couldn't
    /// be read. Its params are optional. Returns
    /// [`Discovery`](crate::obs::Discovery).
    ObsDiscover(Option<crate::optimize::ObsDiscoverParams>),
    /// Measure the upload speed to a streaming service's ingest with a
    /// short publish of filler data (Twitch's bandwidth test mode, or a
    /// custom server, which shows it if it relays to a live service), and
    /// recommend a stream bitrate. Services that would
    /// put a test on air (YouTube, Facebook, Kick), or a missing stream
    /// key, give a documented fallback instead (`measured` false, with
    /// the reason). Returns [`SpeedTest`](crate::optimize::SpeedTest).
    SpeedTest(Box<crate::optimize::SpeedTestParams>),
    /// Optimize the live setup for recording, streaming or both (OBS's
    /// Auto-Configuration Wizard): favour quality and ISO files for
    /// recording; for streaming, set the service and a bitrate from the
    /// upload speed (measured if not given); then benchmark this Mac
    /// (`tune`) and step down where it can't keep up. A custom server is
    /// tested only through `speed_test`, never implicitly. Applies it if
    /// asked (refused if the setup changed meanwhile).
    /// Refused while recording. Returns
    /// [`Optimized`](crate::optimize::Optimized), with a one-line summary.
    Optimize(Box<crate::optimize::OptimizeParams>),
    /// Start the replay buffer: keep the last `recording.replay.seconds`
    /// of the encoded program (video and the program mix) in memory, apart
    /// from any recording or stream. Shares the stream's encoder when one
    /// runs with the same format. With `canvas` "vertical", the vertical
    /// canvas's own buffer (sharing a `[vertical.stream]`'s encoder). Its
    /// params are optional. Returns
    /// [`ReplayStatus`](crate::replay::ReplayStatus).
    ReplayStart(Option<crate::replay::ReplayParams>),
    /// Stop a canvas's replay buffer (the main one's unless `canvas`
    /// says), discarding what it holds. Returns its final
    /// [`ReplayStatus`](crate::replay::ReplayStatus).
    ReplayStop(Option<crate::replay::ReplayParams>),
    /// Save what a canvas's replay buffer holds, up to this moment, to a
    /// new file in the recordings folder (the running recording's project
    /// folder, if one records). Never interrupts a recording or stream.
    /// Returns [`ReplaySaved`](crate::replay::ReplaySaved).
    ReplaySave(Option<crate::replay::ReplayParams>),
    /// A canvas's replay buffer's state, memory and counters. Returns
    /// [`ReplayStatus`](crate::replay::ReplayStatus).
    ReplayStatus(Option<crate::replay::ReplayParams>),
    /// The obs-websocket v5 compatibility server: whether it is on and
    /// listening, where, whether it asks for a password, and how many
    /// clients are connected. Returns
    /// [`WebsocketStatus`](crate::websocket::WebsocketStatus).
    WebsocketStatus,
    /// Turn on the obs-websocket v5 server (OBS tools such as Stream Deck
    /// and Touch Portal then drive Isocast), optionally changing its port,
    /// address or password. Without a stored password one is generated
    /// and returned once. Kept across restarts. Returns
    /// [`WebsocketStatus`](crate::websocket::WebsocketStatus).
    WebsocketEnable(Option<crate::websocket::WebsocketEnableParams>),
    /// Turn off the obs-websocket server, disconnecting its clients.
    /// Returns [`WebsocketStatus`](crate::websocket::WebsocketStatus).
    WebsocketDisable,
    RemoteStatus,
    RemoteEnable(Option<crate::remote::RemoteEnableParams>),
    RemoteDisable,
    RemotePairBegin,
    /// Dedicated, pre-parser PAKE exchange; JSON calls explain the pairing flow.
    RemotePairFinish,
    RemoteClients,
    RemoteRevoke(crate::remote::RemoteRevokeParams),
    /// Clear a canvas's staged preview: `clear_preview` for `main`, or the
    /// vertical canvas's own preview. Returns [`Switched`].
    ClearCanvasPreview {
        #[serde(default, skip_serializing_if = "CanvasId::is_main")]
        canvas: CanvasId,
    },
    /// Open a projector (G.4): the program, studio mode's preview, a
    /// scene, a video source or a multiview grid, full screen on a display
    /// or in a window. The app shows it (and opens it when it next starts
    /// if none runs now). Opening a view that is already open moves that
    /// projector. Returns [`Projector`](crate::projector::Projector).
    ProjectorOpen(Box<crate::projector::OpenParams>),
    /// Close a projector, or all of them; where each was is remembered for
    /// the next time its view opens. Returns
    /// [`Projectors`](crate::projector::Projectors).
    ProjectorClose(crate::projector::CloseParams),
    /// The open projectors and the displays they can fill. Returns
    /// [`Projectors`](crate::projector::Projectors).
    ProjectorList,
    /// What the app showing a projector reports: where its window is and
    /// whether it shows it. Returns
    /// [`Projector`](crate::projector::Projector).
    ProjectorPlace(Box<crate::projector::PlaceParams>),
    /// Measured machine capabilities and calibration state. Returns MachineStatus.
    MachineProfile,
    /// Start a silent, idle-only calibration (at most 30 seconds). Returns MachineStatus.
    MachineCalibrate,
    /// Cancel calibration, retaining the last valid profile. Returns MachineStatus.
    MachineCancel,
    /// Select/return the daemon's graphics adapter before opening a client
    /// renderer. Windows returns its DX12 LUID as [low: u32, high: i32];
    /// other platforms return null. Does not start capture or recording.
    GpuAdapter,
    /// What this engine is: its machine's operating system and name, its
    /// identity, build, protocol version and methods. Remote clients call
    /// it first; local ones may. Returns [`Hello`](crate::hello::Hello).
    Hello,
    /// The folders other computers can use: every path a client on another
    /// computer (or an obs-websocket client) gives, read or written, must
    /// be inside one, symlinks resolved. Returns
    /// [`FolderRoots`](crate::roots::FolderRoots).
    FolderRoots,
    /// Choose the folders other computers can use (kept across restarts),
    /// or go back to the defaults. Only on this machine. Returns
    /// [`FolderRoots`](crate::roots::FolderRoots).
    FolderRootsSet(crate::roots::FolderRootsSetParams),
    /// Write a take as a project Final Cut Pro (`fcpxml`), Premiere Pro
    /// (`xmeml`), DaVinci Resolve (`xmeml` with app `resolve`) or any
    /// OpenTimelineIO reader (`otio`) opens (#104): every ISO source on its
    /// own video track with the program on top (the one track on), markers,
    /// and each file's audio on its own tracks, all lined up from the take's
    /// manifest. It points at the take's own files (nothing is encoded,
    /// copied or changed). Returns [`NleExported`](crate::nle::NleExported).
    TakeExportNle(crate::nle::TakeExportNleParams),
    /// Where an engine file picker starts: the folders other computers can
    /// use that exist, and the engine user's home folder. Returns
    /// [`FilesRoots`](crate::roots::FilesRoots).
    FilesRoots,
    /// A folder's entries on the engine's disk (names, sizes, dates; never
    /// contents), hidden ones left out. A client on another computer may
    /// list only inside the folders other computers can use. Returns
    /// [`FilesList`](crate::roots::FilesList).
    FilesList(crate::roots::FilesListParams),
    /// Negotiate a program WebRTC viewer on this dedicated connection.
    /// Returns [`WhepAnswer`]. The connection owns the viewer; closing it
    /// (including remote revocation) stops media, never recording.
    WhepOffer(WhepOfferParams),
    /// End this connection's WebRTC viewer (WHEP DELETE semantics).
    /// Returns null, then closes the connection.
    WhepClose,
    /// Choose the screen or window a `display` or `window` source records,
    /// in the desktop's sharing dialog, where apps can't list screens and
    /// windows themselves (Linux). Waits up to 3 minutes for the choice.
    /// The source then opens that screen or window without asking again
    /// while the desktop remembers it; a running source switches to it.
    /// Returns [`ScreenPicked`]. Only on this machine; refused where
    /// `devices` lists screens and windows instead (macOS, Windows).
    ScreenPick(ScreenPickParams),
}

#[derive(Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WhepOfferParams {
    /// SDP offer, at most 64 KiB, with gathered host candidates and recvonly
    /// H.264 video and Opus audio. No STUN/TURN servers or trickle ICE.
    pub sdp: String,
}

#[derive(Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WhepAnswer {
    /// Sendonly SDP answer, with all engine host candidates.
    pub sdp: String,
    /// Opaque connection-owned viewer id; not a credential or another
    /// connection's authority to close it.
    pub session: String,
}

impl std::fmt::Debug for WhepOfferParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WhepOfferParams")
            .field("sdp", &"[omitted]")
            .finish()
    }
}

impl std::fmt::Debug for WhepAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WhepAnswer")
            .field("sdp", &"[omitted]")
            .field("session", &self.session)
            .finish()
    }
}

/// Every method's name, as requests and plugin grants spell it
/// (`switch_scene`), in the method table's order.
pub fn method_names() -> &'static [String] {
    static NAMES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    NAMES.get_or_init(|| {
        let schema = serde_json::to_value(schemars::schema_for!(Method)).unwrap_or_default();
        let mut names = Vec::new();
        for variant in schema["oneOf"].as_array().into_iter().flatten() {
            let method = &variant["properties"]["method"];
            match (&method["const"], &method["enum"]) {
                (serde_json::Value::String(name), _) => names.push(name.clone()),
                (_, serde_json::Value::Array(list)) => names.extend(
                    list.iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(String::from),
                ),
                _ => {}
            }
        }
        names
    })
}

/// A stream key to store. Its `Debug` output leaves the key out.
#[derive(Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(remote = "Self", deny_unknown_fields)]
pub struct StreamKeyParams {
    /// The stream key from the service's dashboard.
    pub key: String,
    #[serde(flatten)]
    pub target: StreamKeyTarget,
}

crate::located::flattened_fields!(StreamKeyParams, |value| {
    crate::located::from_value::<StreamKeyTarget>(&crate::located::without(value, &["key"])).err()
});

impl std::fmt::Debug for StreamKeyParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamKeyParams")
            .field("key", &"<hidden>")
            .field("target", &self.target)
            .finish()
    }
}

/// Which stream key: the live setup's `[stream]` service (and server, for
/// `custom`) unless given, or a destination's.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StreamKeyTarget {
    /// A destination (`main` or an id in `stream.destinations`): its own
    /// key. Alone, the live setup's destination of that id; with
    /// `service` (and `server` for custom), one not applied yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<StreamService>,
    /// For `custom`, the server the key belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// Which canvas's stream: `main` (the default, `[stream]`) or
    /// `vertical` (`[vertical.stream]`). Each canvas keeps its own keys,
    /// so the vertical canvas's Twitch key is not the main one's.
    #[serde(default, skip_serializing_if = "CanvasId::is_main")]
    pub canvas: CanvasId,
    /// Store it only if the live setup is still at this revision (the one
    /// the client showed the destination from), so a key never lands on a
    /// destination someone moved meanwhile; refused with `WRONG_STATE`
    /// otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
}

/// Which stream destinations `stream_start` and `stream_stop` mean.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StreamStartParams {
    /// Destination ids (`main` and the ids in `stream.destinations`); every
    /// enabled one (or for a stop, every one streaming) if empty. A
    /// destination named here starts even if it is turned off.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub only: Vec<String>,
    /// Which canvas streams: `main` (the default: the program to
    /// `[stream]`) or `vertical` (the vertical program to
    /// `[vertical.stream]`). A stop leaves the other canvas's streams
    /// alone.
    #[serde(default, skip_serializing_if = "CanvasId::is_main")]
    pub canvas: CanvasId,
}

/// What `stream_start` and `stream_stop` did: the first destination they
/// touched (the main one if it was), flattened so a single-destination
/// client reads it as before, and every destination's status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Streaming {
    #[serde(flatten)]
    pub stream: StreamStatus,
    /// Every destination of the live setup, the main one first.
    #[serde(default)]
    pub destinations: Vec<StreamStatus>,
}

/// A stream destination to add or change. Absent fields are left as they
/// are (for `add`: the main destination's, or the service's defaults).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StreamDestinationParams {
    /// The destination's id. For `add`, made from the service if absent
    /// (`youtube`, `youtube-2`); required for `set`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Required for `add`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<StreamService>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_bitrate_kbps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Its own rate control (#84); the main destination's if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_control: Option<crate::stream::RateControl>,
    /// Its own seconds between keyframes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyframe_interval_seconds: Option<u32>,
    /// Its own AAC bitrate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_bitrate_kbps: Option<u32>,
    /// Fields to reset: `server` (to the service's), `video_bitrate_kbps`,
    /// `size`, `rate_control`, `keyframe_interval_seconds` and
    /// `audio_bitrate_kbps` (to the main destination's; for `main`, the
    /// defaults).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clear: Vec<String>,
    /// Which canvas's destinations: `main` (the default, `[stream]`),
    /// `vertical` (`[vertical.stream]`) or a `[[canvases]]` id (its
    /// `stream`), a canvas the live setup must have.
    #[serde(default, skip_serializing_if = "CanvasId::is_main")]
    pub canvas: CanvasId,
}

/// A canvas to add or change (`canvas_add`, `canvas_set`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CanvasParams {
    /// Which canvas: `vertical`, or a `[[canvases]]` id (lowercase letters,
    /// digits and dashes).
    pub canvas: CanvasId,
    /// Display label, like "YouTube Shorts".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Size: give both (each even, 16 to 8192).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Its own frame rate; the main canvas's if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
    /// Show the main canvas's scene (true), or pick its own (false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_main: Option<bool>,
    /// Record its program with each recording (true: a `recording` with
    /// default settings unless it has one; false: none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<bool>,
    /// Fields to reset: `name`, `fps` (to the main canvas's) and `stream`
    /// (no stream of its own).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clear: Vec<String>,
}

/// The fields `CanvasParams::clear` takes.
pub const CANVAS_CLEARABLE: [&str; 3] = ["name", "fps", "stream"];

/// The live setup's canvases after a change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Canvases {
    /// The new live revision.
    pub revision: u64,
    /// Every canvas besides the main one.
    pub canvases: Vec<CanvasStatus>,
}

/// The fields `StreamDestinationParams::clear` takes.
pub const DESTINATION_CLEARABLE: [&str; 6] = [
    "server",
    "video_bitrate_kbps",
    "size",
    "rate_control",
    "keyframe_interval_seconds",
    "audio_bitrate_kbps",
];

/// The live setup's stream destinations after a change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StreamDestinations {
    /// The new live revision.
    pub revision: u64,
    /// The destination added or changed (absent after a removal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Every destination, the main one first.
    pub destinations: Vec<StreamStatus>,
    /// Things to know, like a stream key cleared because the server
    /// changed from another computer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// Where a stream key is kept, and whether one is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StreamKey {
    pub service: StreamService,
    /// The Keychain item's account: the service, or a custom server.
    pub account: String,
    pub key_set: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StartParams {
    /// The setup to record; the live setup if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<Setup>,
    /// Record into a new project folder in this folder instead of the
    /// setup's `recording.folder` (or into this folder, if it is a
    /// project's: the new take joins that project).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<PathBuf>,
    /// Stop on its own after this many seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds: Option<f64>,
}

/// Which recording session `stop` and `request_stop` are meant for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StopParams {
    /// The session's id (`session` in `start`'s answer, `status` and the
    /// `started` event); refused with `WRONG_STATE` when it isn't the one
    /// running. Any session if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DoctorParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<Setup>,
    /// Check this folder instead of the setup's `recording.folder`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportObsParams {
    /// The collection: a .json file (absolute or `~/...`), or the name of
    /// one in OBS's scenes folder, like "Recording".
    pub collection: String,
    /// The OBS profile with the frame rate and recording settings: its
    /// name or a basic.ini path. If absent, the one named like the
    /// collection, OBS's current one, or one with the collection's canvas.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Import onto a canvas this wide and high, scaling positions and sizes
    /// about the centre; the collection's own canvas if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// The canvas frame rate; the profile's if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(remote = "Self", deny_unknown_fields)]
pub struct InitParams {
    #[serde(flatten)]
    pub options: crate::init::InitOptions,
    /// Make it the live setup (replacing any live one, which stays in the
    /// setup file's `.bak`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub apply: bool,
}

crate::located::flattened_fields!(InitParams, |value| {
    crate::located::from_value::<crate::init::InitOptions>(value).err()
});

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Opened {
    /// What was opened.
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Marked {
    pub label: String,
    /// The take (1 for the first) whose manifest holds the marker.
    pub take: u32,
    /// Where the marker sits on that take's timeline.
    pub time_seconds: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Refreshed {
    /// The browser source whose page is reloading.
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Switched {
    /// The scene on air.
    pub scene: String,
    /// The scene in preview, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// The take (1 for the first) whose manifest holds the switch, if
    /// recording.
    pub take: Option<u32>,
    /// Where the switch sits on that take's timeline, if recording.
    pub time_seconds: Option<f64>,
    /// How the scene came on air, unless it was a cut.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transition: Option<crate::Transition>,
    /// The canvas it went on air on, unless the main one.
    #[serde(default, skip_serializing_if = "CanvasId::is_main")]
    pub canvas: CanvasId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(remote = "Self", deny_unknown_fields)]
pub struct ApplyParams {
    /// The revision the change was planned against.
    pub revision: u64,
    #[serde(flatten)]
    pub change: Change,
    /// One step of a gesture (dragging a source, moving a slider): applied
    /// at once like any change, but saved to the setup file a moment after
    /// the gesture's last step rather than before replying, so steps can
    /// come many times a second. A failed save is reported as a `warning`
    /// event instead of in the reply's notes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub live: bool,
}

crate::located::flattened_fields!(ApplyParams, |value| {
    if let Some(message) = Change::neither_or_both(value) {
        return Some(crate::located::Located {
            path: String::new(),
            message: message.into(),
        });
    }
    let key = if value.get("setup").is_some() {
        "setup"
    } else {
        "patch"
    };
    let change = serde_json::json!({ key: value[key] });
    crate::located::from_value::<Change>(&change).err()
});

/// Parameters of `get`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetParams {
    /// Also return where every item is drawn, as laid out (`resolved`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub resolved: bool,
    /// Whose layout `resolved` is: `main` (the default) or `vertical` (the
    /// vertical canvas's, which the live setup must have). The setup is
    /// the whole live setup either way.
    #[serde(default, skip_serializing_if = "CanvasId::is_main")]
    pub canvas: CanvasId,
}

/// What `request_permissions` asks for.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestPermissionsParams {
    /// Only this permission (screen_recording, camera, microphone or
    /// system_audio); all that haven't been decided if left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<crate::devices::PermissionKind>,
}

/// Parameters of `watch`: which kinds of events (their `type`) to send.
/// An event is sent when `kinds` (if given) lists its kind and `except`
/// doesn't.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WatchParams {
    /// Only events of these kinds, like `["scene_changed", "stopped"]`;
    /// every kind if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kinds: Option<Vec<EventKind>>,
    /// Leave out events of these kinds, like `["levels"]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub except: Vec<EventKind>,
}

impl WatchParams {
    /// Every kind but those in `except`.
    pub fn except(except: &[EventKind]) -> Self {
        Self {
            kinds: None,
            except: except.to_vec(),
        }
    }

    /// The kinds it lets through, as a set.
    pub fn wanted(&self) -> EventKinds {
        let mut wanted = match &self.kinds {
            Some(kinds) => kinds
                .iter()
                .fold(EventKinds::NONE, |set, kind| set.with(*kind)),
            None => EventKinds::ALL,
        };
        for kind in &self.except {
            wanted = wanted.without(*kind);
        }
        wanted
    }

    /// What is wrong with the params, if anything: a filter that lets no
    /// event through.
    pub fn problem(&self) -> Option<crate::located::Located> {
        let path = match &self.kinds {
            Some(kinds) if kinds.is_empty() => "kinds",
            _ if self.wanted() == EventKinds::NONE => "except",
            _ => return None,
        };
        Some(crate::located::Located {
            path: path.into(),
            message: format!(
                "this lets no event through; list the kinds to send (leave `kinds` out for \
                 all): {}",
                EventKind::NAMES.join(", ")
            ),
        })
    }
}

/// A set of event kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EventKinds(u64);

impl EventKinds {
    pub const NONE: Self = Self(0);
    pub const ALL: Self = Self((1u64 << EventKind::NAMES.len()) - 1);

    pub fn with(self, kind: EventKind) -> Self {
        Self(self.0 | 1 << kind as u64)
    }

    pub fn without(self, kind: EventKind) -> Self {
        Self(self.0 & !(1 << kind as u64))
    }

    pub fn contains(self, kind: EventKind) -> bool {
        self.0 & 1 << kind as u64 != 0
    }
}

/// The live setup. Revision 0 means there is none: none was applied since
/// the daemon started and none was loaded from its file. A setup loaded at
/// start gets a revision above any an earlier run handed out (the time in
/// milliseconds), so revisions never repeat across restarts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Live {
    pub revision: u64,
    pub setup: Option<Setup>,
    /// Where every item of every scene is drawn, as laid out (asked for with
    /// `resolved`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<Resolved>,
    /// The canvas `resolved` lays out, unless the main one.
    #[serde(default, skip_serializing_if = "CanvasId::is_main")]
    pub canvas: CanvasId,
    /// Where the live setup is kept between runs, and whether it loaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_file: Option<SetupFile>,
    /// The scene collection the live setup's scenes and sources are kept
    /// in, once there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection: Option<Current>,
    /// The profile its recording, stream and audio settings are kept in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<Current>,
}

/// Where layout put every item of the live setup's scenes, with the
/// sources' picture sizes the live engine has (or, while it isn't running,
/// the sizes their settings name).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Resolved {
    /// Counts the layouts the live engine has made that differ from the one
    /// before (a change, or a source's picture changing size). Program and
    /// preview frames carry the generation they were drawn at
    /// (`layout_generation`), so a client can tell whether these rects are
    /// the ones a picture shows. 0: worked out for this reply, as the live
    /// engine isn't running.
    pub generation: u64,
    /// The setup revision it was laid out from.
    pub revision: u64,
    pub scenes: Vec<ResolvedScene>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ResolvedScene {
    pub id: String,
    pub items: Vec<ResolvedItem>,
}

/// Where an item is drawn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ResolvedItem {
    /// The item's id (else its source's or scene's), as patches name it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Its box in its parent's pixels, as `rect` says it or layout placed
    /// it (whole pixels).
    pub rect: ResolvedRect,
    /// Its box on the canvas after every transform of it and its parents:
    /// the bounding box when something rotates.
    pub canvas: ResolvedRect,
    /// Maps its own pixels (0,0 to its width and height) to canvas pixels:
    /// x' = a·x + b·y + tx, y' = c·x + d·y + ty, as [a, b, tx, c, d, ty].
    pub transform: [f32; 6],
    /// Whether it is shown (it and its parents are visible). A hidden item
    /// that takes part in layout keeps the place it was last given.
    pub visible: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<ResolvedItem>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ResolvedRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// The scene collection or profile in use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Current {
    pub id: String,
    pub name: String,
}

/// The files the daemon keeps the live setup in, the current scene
/// collection's and profile's: they are loaded when the daemon starts and
/// saved (atomically, the previous version kept as `<file>.bak`) after
/// every applied change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SetupFile {
    /// The current scene collection's file (before there is one, the file
    /// an older Isocast kept the whole setup in).
    pub path: PathBuf,
    /// The current profile's file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_path: Option<PathBuf>,
    /// The file couldn't be loaded when the daemon started, so it started
    /// with no setup. The file is left as it was until a change is
    /// applied, which first moves it aside (the change's notes say where).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_error: Option<SetupLoadError>,
    /// The last applied change couldn't be saved: it is live, but a restart
    /// would lose it unless a later change saves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save_error: Option<String>,
    /// What happened to the files when isocastd started, like the setup
    /// file of an older Isocast being taken in as a collection and profile.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// Why the setup file couldn't be loaded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SetupLoadError {
    pub message: String,
    /// The problems `setup check` finds in it, if it is a readable setup.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<Issue>,
}

impl SetupLoadError {
    /// One message with the file's path and every problem, for warnings.
    pub fn describe(&self, path: &std::path::Path) -> String {
        let mut text = format!(
            "the setup file {} couldn't be loaded, so isocastd started without a setup (the \
             file is kept as it was): {}",
            path.display(),
            self.message
        );
        for issue in &self.issues {
            text.push_str(&format!("\n  {}: {}", issue.path, issue.message));
        }
        text.push_str(
            "\nFix the file and restart isocastd (`isocast restart`) to load it, switch to \
             another scene collection or profile (`isocast collection list`), or apply a new \
             setup (a file that can't be read is then moved aside to <name>.broken.<ext>).",
        );
        text
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Plan {
    /// The live revision the plan was made against; pass it to `apply`.
    pub revision: u64,
    pub changes: Vec<Difference>,
    /// Problems with the resulting setup; `apply` refuses while any exist.
    pub problems: Vec<Issue>,
    /// Things to know, like a recording in progress keeping its setup.
    pub notes: Vec<String>,
    /// The setup the change would produce, if it is a valid document.
    pub setup: Option<Setup>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Applied {
    /// The new live revision.
    pub revision: u64,
    pub changes: Vec<Difference>,
    pub notes: Vec<String>,
}

/// What `watch` streams, as `{"jsonrpc":"2.0","method":"event","params":…}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// The live setup changed.
    SetupChanged {
        revision: u64,
        /// The client on another computer that changed it (its name).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<String>,
    },
    /// A recording started writing these files, or a split began a new
    /// take's; `begins_at` is when a new take's files begin
    /// (local time, RFC 3339 with milliseconds), a moment after the event.
    Started {
        files: Vec<PathBuf>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        begins_at: Option<String>,
        /// The take (1 for the first) these files belong to.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        take: Option<u32>,
        /// The take's manifest (`….session.json`), written as it records.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        manifest: Option<PathBuf>,
        /// The folder the files are in: the project's.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        folder: Option<PathBuf>,
        /// The project (its id) the recording writes into.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<String>,
        /// What began the take: `start` or `split`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cause: Option<TakeCause>,
        /// The recording session's id (as `stop` takes it).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        /// The client on another computer that started it (its name).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<String>,
    },
    /// Live counters, about once a second while recording.
    Status {
        live: LiveStatus,
    },
    /// Something needs attention, like a file dropping frames.
    Warning {
        message: String,
    },
    /// A scene went on air.
    SceneChanged {
        scene: String,
        take: Option<u32>,
        time_seconds: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transition: Option<crate::Transition>,
    },
    /// A scene was staged in preview.
    PreviewChanged {
        scene: String,
    },
    /// A marker was added to the recording.
    Marker {
        label: String,
        take: u32,
        time_seconds: f64,
        /// The project (its id) being recorded.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<String>,
    },
    /// The stream's state changed (connecting, live, reconnecting, failed,
    /// offline).
    Stream {
        stream: StreamStatus,
        /// The client on another computer that last started or stopped
        /// this destination's stream (its name).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<String>,
    },
    /// A recording ended: its report, or why it failed to start.
    Stopped {
        report: Option<Box<Report>>,
        error: Option<String>,
        /// The project (its id) it recorded into, if it got one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<String>,
        /// The project's folder.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        folder: Option<PathBuf>,
        /// The recording session's id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        /// The client on another computer that stopped it (its name).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<String>,
    },
    /// Every audio source's and the program mix's level (`program`), about
    /// 20 times a second while the live engine runs.
    Levels {
        levels: Vec<AudioLevel>,
    },
    /// One of the live engine's sources started opening, is running, or
    /// failed.
    SourceChanged {
        status: SourceStatus,
    },
    /// The live engine started or stopped.
    EngineChanged {
        running: bool,
    },
    /// A scene collection or profile was made, renamed, deleted, imported
    /// or switched to (`kind` and `id` say which, unless both were
    /// switched); `collection` and `profile` are the current ones after.
    LibraryChanged {
        action: LibraryAction,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<LibraryKind>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        collection: Option<String>,
        profile: Option<String>,
    },
    /// A plugin was added, removed, enabled or disabled, or its process
    /// crashed or restarted; `state` and `process` are how it is after.
    PluginChanged {
        id: String,
        action: PluginAction,
        state: PluginState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        process: Option<ProcessState>,
    },
    /// Data keys were set or deleted (`data_get` reads them): every key
    /// changed since the last such event, at most 20 events a second;
    /// `revision` is the store's after them.
    DataChanged {
        revision: u64,
        keys: Vec<String>,
    },
    /// A recording crossed a pause/resume boundary; its files stay open.
    RecordingPaused {
        recording: RecordingPause,
        /// The project (its id) being recorded.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<String>,
    },
    HotkeyFired {
        binding: String,
        action: String,
        source: crate::hotkeys::InputSource,
        phase: crate::hotkeys::Phase,
        /// None on success; failure also emits a warning.
        error: Option<String>,
    },
    /// Studio preview was cleared; absence is not an empty scene id.
    PreviewCleared,
    VirtualCamera {
        camera: crate::virtual_camera::VirtualCameraStatus,
    },
    /// A take's files are finished (a stop or a split ended it,
    /// or the recording failed): its manifest, every file and how it
    /// ended. Comes once per take, after its last `started`.
    TakeFinished {
        take: u32,
        manifest: PathBuf,
        /// Program and ISO files, as the manifest lists them.
        files: Vec<PathBuf>,
        folder: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<String>,
        status: crate::manifest::SessionStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_seconds: Option<f64>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        errors: Vec<String>,
    },
    /// A recording made a new project folder, as its first take began.
    ProjectCreated {
        project: crate::project::Project,
    },
    /// A project was renamed: its folder moved from `old_folder`.
    ProjectRenamed {
        project: crate::project::Project,
        old_name: String,
        old_folder: PathBuf,
    },
    /// A camera, microphone, display or audio output appeared or went.
    DeviceChanged {
        action: DeviceAction,
        kind: DeviceKind,
        id: String,
        name: String,
    },
    /// A replay buffer started, stopped or failed (`replay.canvas` says
    /// whose: absent for the main canvas's).
    Replay {
        replay: crate::replay::ReplayStatus,
    },
    /// A replay buffer saved a file (`saved.canvas` says whose).
    ReplaySaved {
        saved: crate::replay::ReplaySaved,
    },
    /// The hardware encoders can't keep up: files of the take are dropping
    /// frames (ISO files first; the program and stream are protected), and
    /// the disk isn't the cause. Comes at most once per take.
    EncoderOverload {
        overload: Box<Overload>,
    },
    /// A projector opened, closed or moved: the open projectors now, and
    /// why one closed if it wasn't asked to (a display went away).
    Projectors {
        projectors: Vec<crate::projector::Projector>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// A scene went on air on a canvas other than the main one (the main
    /// canvas's switches are `scene_changed`).
    CanvasSceneChanged {
        canvas: CanvasId,
        scene: String,
        take: Option<u32>,
        time_seconds: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transition: Option<crate::Transition>,
    },
    /// A scene was staged in (or, `scene` null, cleared from) the preview
    /// of a canvas other than the main one (the main canvas's are
    /// `preview_changed` and `preview_cleared`).
    CanvasPreviewChanged {
        canvas: CanvasId,
        scene: Option<String>,
    },
    /// Automatic-quality policy changed an effective limit or recovery state.
    GovernorChanged {
        governor: crate::governor::GovernorStatus,
    },
    /// A network caller's request was refused by the engine's policy
    /// (machine control stays on the machine, folders other computers can
    /// use, credentials stay with configured servers; docs/remote-engines.md
    /// decisions 4 and 25). Also in the audit log.
    Refused {
        /// The method, as requests spell it.
        method: String,
        /// Why, as the caller was told.
        message: String,
        /// Who asked (a client's name, or `obs-websocket client ADDRESS`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<String>,
    },
    RemoteClientConnected {
        peer: crate::remote::RemoteAuditPeer,
    },
    RemoteClientDisconnected {
        peer: crate::remote::RemoteAuditPeer,
    },
    RemotePaired {
        client: crate::remote::RemotePeer,
    },
    RemoteRevoked {
        client_id: String,
    },
    RemoteDisabled,
    /// Remote watch liveness; not published to local subscribers.
    RemoteHeartbeat,
}

/// What began a take, in `started` events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TakeCause {
    Start,
    Split,
}

/// Whether a device appeared or went, in `device_changed` events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeviceAction {
    Added,
    Removed,
}

/// The kind of device in `device_changed` events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    Camera,
    Microphone,
    Display,
    AudioOutput,
}

/// The kind of an [`Event`], as its `type` says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    SetupChanged,
    Started,
    Status,
    Warning,
    SceneChanged,
    PreviewChanged,
    Marker,
    Stream,
    Stopped,
    Levels,
    SourceChanged,
    EngineChanged,
    LibraryChanged,
    PluginChanged,
    DataChanged,
    RecordingPaused,
    HotkeyFired,
    PreviewCleared,
    VirtualCamera,
    TakeFinished,
    ProjectCreated,
    ProjectRenamed,
    DeviceChanged,
    Replay,
    ReplaySaved,
    EncoderOverload,
    Projectors,
    CanvasSceneChanged,
    CanvasPreviewChanged,
    GovernorChanged,
    Refused,
    RemoteClientConnected,
    RemoteClientDisconnected,
    RemotePaired,
    RemoteRevoked,
    RemoteDisabled,
    RemoteHeartbeat,
}

impl EventKind {
    /// Every kind's name, as events and `watch` spell it, in this order.
    pub const NAMES: [&str; 37] = [
        "setup_changed",
        "started",
        "status",
        "warning",
        "scene_changed",
        "preview_changed",
        "marker",
        "stream",
        "stopped",
        "levels",
        "source_changed",
        "engine_changed",
        "library_changed",
        "plugin_changed",
        "data_changed",
        "recording_paused",
        "hotkey_fired",
        "preview_cleared",
        "virtual_camera",
        "take_finished",
        "project_created",
        "project_renamed",
        "device_changed",
        "replay",
        "replay_saved",
        "encoder_overload",
        "projectors",
        "canvas_scene_changed",
        "canvas_preview_changed",
        "governor_changed",
        "refused",
        "remote_client_connected",
        "remote_client_disconnected",
        "remote_paired",
        "remote_revoked",
        "remote_disabled",
        "remote_heartbeat",
    ];

    /// The kind's name, as events and `watch` spell it.
    pub fn name(self) -> &'static str {
        Self::NAMES[self as usize]
    }
}

impl Event {
    pub fn kind(&self) -> EventKind {
        match self {
            Self::SetupChanged { .. } => EventKind::SetupChanged,
            Self::Started { .. } => EventKind::Started,
            Self::Status { .. } => EventKind::Status,
            Self::Warning { .. } => EventKind::Warning,
            Self::SceneChanged { .. } => EventKind::SceneChanged,
            Self::PreviewChanged { .. } => EventKind::PreviewChanged,
            Self::Marker { .. } => EventKind::Marker,
            Self::Stream { .. } => EventKind::Stream,
            Self::Stopped { .. } => EventKind::Stopped,
            Self::Levels { .. } => EventKind::Levels,
            Self::SourceChanged { .. } => EventKind::SourceChanged,
            Self::EngineChanged { .. } => EventKind::EngineChanged,
            Self::LibraryChanged { .. } => EventKind::LibraryChanged,
            Self::PluginChanged { .. } => EventKind::PluginChanged,
            Self::DataChanged { .. } => EventKind::DataChanged,
            Self::RecordingPaused { .. } => EventKind::RecordingPaused,
            Self::HotkeyFired { .. } => EventKind::HotkeyFired,
            Self::PreviewCleared => EventKind::PreviewCleared,
            Self::VirtualCamera { .. } => EventKind::VirtualCamera,
            Self::TakeFinished { .. } => EventKind::TakeFinished,
            Self::ProjectCreated { .. } => EventKind::ProjectCreated,
            Self::ProjectRenamed { .. } => EventKind::ProjectRenamed,
            Self::DeviceChanged { .. } => EventKind::DeviceChanged,
            Self::Replay { .. } => EventKind::Replay,
            Self::ReplaySaved { .. } => EventKind::ReplaySaved,
            Self::EncoderOverload { .. } => EventKind::EncoderOverload,
            Self::Projectors { .. } => EventKind::Projectors,
            Self::CanvasSceneChanged { .. } => EventKind::CanvasSceneChanged,
            Self::CanvasPreviewChanged { .. } => EventKind::CanvasPreviewChanged,
            Self::GovernorChanged { .. } => EventKind::GovernorChanged,
            Self::Refused { .. } => EventKind::Refused,
            Self::RemoteClientConnected { .. } => EventKind::RemoteClientConnected,
            Self::RemoteClientDisconnected { .. } => EventKind::RemoteClientDisconnected,
            Self::RemotePaired { .. } => EventKind::RemotePaired,
            Self::RemoteRevoked { .. } => EventKind::RemoteRevoked,
            Self::RemoteDisabled => EventKind::RemoteDisabled,
            Self::RemoteHeartbeat => EventKind::RemoteHeartbeat,
        }
    }
}

/// A JSON-RPC notification carrying an [`Event`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    pub params: Event,
}

impl Notification {
    pub fn event(event: Event) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            method: "event".into(),
            params: event,
        }
    }
}

/// [`Method::RequestStop`]'s answer: the stop was asked for, nothing more.
/// The files are still being finished; wait for [`Event::Stopped`] (or a
/// `status` no longer `finalizing`) to learn whether they were.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StopAccepted {
    /// The session asked to stop (this isocastd's own count).
    pub session_id: u64,
    /// The session's id, as `start`, `status` and the events give it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The session was finishing its files when the request was taken (a
    /// snapshot: it may have finished since).
    pub finalizing: bool,
    /// The files it was writing, as known then (empty while still
    /// starting); provisional until it has stopped.
    pub files: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Started {
    pub files: Vec<PathBuf>,
    /// When a split's new take begins (local time, RFC 3339 with
    /// milliseconds): the next whole second, a moment after the split.
    /// Frames and audio before then are in the take before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub begins_at: Option<String>,
    /// Things to know, like a setup given to `start` replacing the live
    /// setup.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// The recording session's id: give it to `stop` and `request_stop`
    /// so they can only end this session. Absent for a split's new take
    /// (the session is the same).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// `start` was asked again with the same params while this session
    /// was already recording (or starting): nothing new started, and this
    /// is the running session.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub already_recording: bool,
}

/// A warning about current headroom, not the setup's estimated capacity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LoadWarning {
    /// `encoder_contention` is inferred: macOS does not identify the other app.
    pub cause: String,
    pub message: String,
    pub encode_latency_ms: f64,
    pub pending_frames: u64,
    pub system_cpu_percent: Option<f64>,
    /// macOS ProcessInfo: 0 nominal, 1 fair, 2 serious, 3 critical.
    pub thermal_state: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Status {
    pub state: EngineState,
    /// A stop has been requested for the running session (or its start
    /// cancelled) and it hasn't ended yet. This says the request was
    /// accepted, not where the files were cut or that they are on disk.
    /// Until it ends, `start`, `pause`, `resume` and `split` are refused;
    /// then `last` (or `last_error`) and the `stopped` event give the
    /// outcome.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub finalizing: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub governor: Option<crate::governor::GovernorStatus>,
    /// A compact calibration summary; machine_profile returns the measurements.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_profile: Option<crate::machine::MachineStatus>,
    /// A current episode of reduced system/encoder headroom; clears after
    /// recovery, not on a new take. Events are limited to one `warning` per
    /// cause every three minutes; status still shows suppressed episodes.
    /// Latency and CPU detection are blind for five seconds after our encoder
    /// sessions change (including a split), while a new baseline is learned.
    /// Thermal and encoder-allocation failure warnings do not need that baseline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_warning: Option<LoadWarning>,
    /// The scene on air (an id), if the setup has scenes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scene: Option<String>,
    /// The scene staged in preview, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// The running session's id (while starting, recording or
    /// finalizing), as `stop` and `request_stop` take it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The running session's live counters, once it has started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live: Option<LiveStatus>,
    /// Files the running session is writing.
    #[serde(default)]
    pub files: Vec<PathBuf>,
    /// The report of the most recent finished session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<Report>,
    /// Why the most recent session ended without a report: it failed to
    /// start (after `start` returned, too) or was stopped while starting.
    /// `last` is then an earlier session's. Cleared when a session ends
    /// with a report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// The live engine, while it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<EngineInfo>,
    /// Where the live setup is kept between runs, and whether it loaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_file: Option<SetupFile>,
    /// The scene collection and profile in use, once there is a live setup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection: Option<Current>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<Current>,
    /// The stream, if the live setup streams or a stream is running: the
    /// main destination's (see `streams` for every destination).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<StreamStatus>,
    /// Every stream destination, the main one first (empty without a
    /// `[stream]` section or a running stream).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub streams: Vec<StreamStatus>,
    /// The scene on air on the vertical canvas (an id), if the live setup
    /// has one: the main scene while it follows the main canvas.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vertical_scene: Option<String>,
    /// The scene staged in the vertical canvas's own preview, if any
    /// (while it follows the main canvas, the main preview's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vertical_preview: Option<String>,
    /// Every vertical stream destination (`[vertical.stream]`), the main
    /// one first, each with `canvas: vertical`; kept apart from `streams`
    /// so clients of the main canvas see only its streams.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vertical_streams: Vec<StreamStatus>,
    /// How much of this Mac's processor isocastd used lately, in percent
    /// of all its cores (as OBS's status bar shows it): measured between
    /// readings at least a second apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_percent: Option<f64>,
    /// The data store: its keys, size, writes and events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<DataStatus>,
    /// Log health and previous launches lacking a clean-exit marker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interrupted: Vec<crate::manifest::InterruptedRecording>,
    /// Camera output state (installation is explicit, never automatic).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtual_camera: Option<crate::virtual_camera::VirtualCameraStatus>,
    /// The replay buffer: running or not, what it holds, its last save.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay: Option<crate::replay::ReplayStatus>,
    /// Free space on the disk the live setup records to (its
    /// `recording.folder`, or the nearest folder above it that exists;
    /// with no setup yet, `~/Movies/Isocast`'s, where a new one records),
    /// in bytes, as the daemon may use it: measured with each status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_free_bytes: Option<u64>,
    /// How fast the live setup's program file fills that disk, at most:
    /// the generous estimate the free-space check plans for, in bytes per
    /// second (`ProgramRecording::bytes_per_second`). Absent without a
    /// setup, or when the program isn't recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_bytes_per_second: Option<f64>,
    /// Which isocastd answered: its version and build, so a client can
    /// tell an engine left running from before an app update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon: Option<DaemonBuild>,
    /// The vertical canvas's replay buffer (`canvas: vertical`), while the
    /// setup has a vertical canvas or its buffer ran; kept apart from
    /// `replay` so clients of the main canvas see only its buffer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vertical_replay: Option<crate::replay::ReplayStatus>,
    /// Every canvas besides the main one (`[vertical]` first, then
    /// `[[canvases]]`, #84): its size, scene, preview, streams, replay
    /// buffer and recording. The `vertical_*` fields above stay for clients
    /// of the vertical canvas alone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub canvases: Vec<CanvasStatus>,
}

/// One extra canvas in [`Status::canvases`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CanvasStatus {
    pub id: CanvasId,
    /// Its display label.
    pub name: String,
    pub canvas: Canvas,
    /// Whether it shows the main canvas's scene.
    pub follow_main: bool,
    /// The scene on air on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scene: Option<String>,
    /// The scene staged in its preview.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// Whether a recording writes its program.
    pub recorded: bool,
    /// Its stream destinations, the main one first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub streams: Vec<StreamStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay: Option<crate::replay::ReplayStatus>,
}

/// An isocastd build: `version` is the product version (`0.0.2`), `build`
/// the bundle build number that updates compare (`CFBundleVersion`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DaemonBuild {
    pub version: String,
    pub build: String,
    pub commit: String,
    /// The running executable, as it was when the daemon started.
    pub executable: String,
    pub pid: u32,
    /// This isolated daemon finishes and exits when its launching client's
    /// lifetime pipe closes. Older daemons cannot safely own a `record` run.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub parent_owned: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EngineState {
    Idle,
    /// Opening devices and creating files.
    Starting,
    Recording,
}

/// The engine's executable inside a release app's
/// `Contents/Helpers/Isocast Engine.app/Contents/MacOS/` (#83): macOS names
/// it in prompts such as the camera extension's approval alert. Cargo,
/// development builds and the development app still call it `isocastd`.
pub const ENGINE_EXECUTABLE: &str = "Isocast Engine";

/// What the engine's executable was called in release apps before #83,
/// and is still called in development builds.
pub const OLD_ENGINE_EXECUTABLE: &str = "isocastd";

/// Whether `name` is an engine executable's file name, either one.
pub fn is_engine_executable(name: &std::ffi::OsStr) -> bool {
    name == ENGINE_EXECUTABLE || name == OLD_ENGINE_EXECUTABLE
}

/// Where the daemon listens: `$ISOCAST_SOCKET`, or else
/// `~/Library/Application Support/Isocast/isocastd.sock` on macOS,
/// `$XDG_RUNTIME_DIR/isocast/isocastd.sock` on Linux (in the support
/// folder without a runtime directory), and in the support folder
/// elsewhere.
pub fn socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os("ISOCAST_SOCKET") {
        return PathBuf::from(path);
    }
    default_socket_path()
}

/// The user's own engine's socket, whatever `$ISOCAST_SOCKET` says.
pub fn default_socket_path() -> PathBuf {
    if cfg!(target_os = "linux") {
        return linux_socket_path(
            std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
            |dir| dir.is_dir(),
            &support_folder(),
        );
    }
    support_folder().join("isocastd.sock")
}

/// Linux's socket: in `$XDG_RUNTIME_DIR/isocast` when that is an absolute
/// path to a folder that exists (`is_dir`), else in `support`. The CLI,
/// app and daemon must see the same `XDG_RUNTIME_DIR` to find each other.
fn linux_socket_path(
    runtime: Option<&std::ffi::OsStr>,
    is_dir: impl Fn(&std::path::Path) -> bool,
    support: &std::path::Path,
) -> PathBuf {
    match runtime
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute() && is_dir(dir))
    {
        Some(runtime) => runtime.join("isocast").join("isocastd.sock"),
        None => support.join("isocastd.sock"),
    }
}

/// A file of the user's own engine that must outlive a login (its
/// bootstrap log `log`, the diagnostics `install-id`): beside the socket
/// and named after it (`isocastd.sock` → `isocastd.log`), except on Linux,
/// whose default socket is in `$XDG_RUNTIME_DIR` (cleared at logout): there
/// in the support folder. A socket of its own (`$ISOCAST_SOCKET`) keeps
/// its files beside it.
pub fn daemon_file(extension: &str) -> PathBuf {
    if cfg!(target_os = "linux") && std::env::var_os("ISOCAST_SOCKET").is_none() {
        return support_folder().join("isocastd").with_extension(extension);
    }
    socket_path().with_extension(extension)
}

/// The file a running daemon holds locked (and writes its pid in), next to
/// its socket and named after it (`isocastd.sock` → `isocastd.lock`): one
/// daemon per socket, even while that daemon is too busy to accept.
pub fn lock_path() -> PathBuf {
    socket_path().with_extension("lock")
}

/// Where the daemon keeps the live setup between runs: `$ISOCAST_SETUP`;
/// else, for a daemon on a socket of its own (`$ISOCAST_SOCKET`), a file
/// named after the socket beside it (`/tmp/x.sock` → `/tmp/x.setup.toml`),
/// so test daemons never touch the real one; else `setup.toml` beside the
/// default socket. Its scene collections and profiles are kept in a folder
/// named after this file (`setup.toml` → `setup/`); the file itself is the
/// whole setup as earlier versions kept it, taken in from there.
pub fn setup_path() -> PathBuf {
    if let Some(path) = std::env::var_os("ISOCAST_SETUP") {
        return PathBuf::from(path);
    }
    if let Some(socket) = std::env::var_os("ISOCAST_SOCKET") {
        let socket = PathBuf::from(socket);
        let stem = socket
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "isocastd".into());
        return socket.with_file_name(format!("{stem}.setup.toml"));
    }
    support_folder().join("setup.toml")
}

/// Where the daemon keeps installed plugins: `$ISOCAST_PLUGINS`; else, for
/// a daemon on a socket of its own (`$ISOCAST_SOCKET`), a folder named
/// after the socket beside it (`/tmp/x.sock` → `/tmp/x.plugins`), so test
/// daemons never touch the real one; else `plugins` beside the default
/// socket.
pub fn plugins_path() -> PathBuf {
    if let Some(path) = std::env::var_os("ISOCAST_PLUGINS") {
        return PathBuf::from(path);
    }
    if let Some(socket) = std::env::var_os("ISOCAST_SOCKET") {
        let socket = PathBuf::from(socket);
        let stem = socket
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "isocastd".into());
        return socket.with_file_name(format!("{stem}.plugins"));
    }
    support_folder().join("plugins")
}

/// The user's home folder. Windows does not normally set `HOME` outside a
/// Unix-like shell: there it is `USERPROFILE`, else the profile known
/// folder (an app started by Explorer or a service may have neither
/// variable set the way a shell does).
pub fn home_folder() -> PathBuf {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or_else(|| known_folder::path(known_folder::Folder::Profile))
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME").map(PathBuf::from);
    home.unwrap_or_default()
}

/// `~/Library/Application Support/Isocast` on macOS, `%LOCALAPPDATA%/Isocast`
/// on Windows (the LocalAppData known folder when the variable is unset),
/// `$XDG_DATA_HOME/isocast` (`~/.local/share/isocast`) on Linux, where a
/// `~/.isocast` from earlier builds is still used if it is there, and
/// `~/.isocast` elsewhere.
pub fn support_folder() -> PathBuf {
    let home = home_folder();
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Isocast")
    } else if cfg!(windows) {
        windows_folder("LOCALAPPDATA", "AppData/Local").join("Isocast")
    } else if cfg!(target_os = "linux") {
        linux_support_folder(
            &home,
            std::env::var_os("XDG_DATA_HOME").as_deref(),
            home.join(".isocast").is_dir(),
        )
    } else {
        home.join(".isocast")
    }
}

/// Linux's support folder: `~/.isocast` if it exists (`legacy`, from
/// earlier builds), else `$XDG_DATA_HOME/isocast` when that is absolute
/// (the XDG spec ignores relative and empty values), else
/// `~/.local/share/isocast`.
fn linux_support_folder(
    home: &std::path::Path,
    data_home: Option<&std::ffi::OsStr>,
    legacy: bool,
) -> PathBuf {
    if legacy {
        return home.join(".isocast");
    }
    data_home
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(|| home.join(".local/share"))
        .join("isocast")
}

/// A per-user Windows folder: its environment variable when set (so
/// isolated engines and tests can point it elsewhere), else the known
/// folder, else `home`/`fallback`. Only meaningful on Windows.
pub fn windows_folder(variable: &str, fallback: &str) -> PathBuf {
    if let Some(path) = std::env::var_os(variable).filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    #[cfg(windows)]
    {
        let folder = match variable {
            "APPDATA" => Some(known_folder::Folder::RoamingAppData),
            "LOCALAPPDATA" => Some(known_folder::Folder::LocalAppData),
            _ => None,
        };
        if let Some(path) = folder.and_then(known_folder::path) {
            return path;
        }
    }
    home_folder().join(fallback)
}

/// `~/rest` as a path. On Windows a library folder (`~/Videos`,
/// `~/Documents`, `~/Desktop`…) is its known folder, wherever it was moved
/// (another drive, OneDrive), as long as `~` is the real profile: an
/// isolated engine whose USERPROFILE points elsewhere keeps its own.
pub fn home_join(rest: &str) -> PathBuf {
    let home = home_folder();
    #[cfg(windows)]
    {
        let mut parts = rest.splitn(2, ['/', '\\']);
        let first = parts.next().unwrap_or_default();
        let tail = parts.next().unwrap_or_default();
        if let Some(folder) = known_folder::Folder::library(first)
            && known_folder::path(known_folder::Folder::Profile)
                .is_some_and(|profile| profile.as_os_str().eq_ignore_ascii_case(home.as_os_str()))
            && let Some(path) = known_folder::path(folder)
        {
            return if tail.is_empty() {
                path
            } else {
                path.join(tail)
            };
        }
    }
    home.join(rest)
}

/// Windows known folders (`SHGetKnownFolderPath`), as their current paths.
#[cfg(windows)]
pub mod known_folder {
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;

    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{
        FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_LocalAppData,
        FOLDERID_Music, FOLDERID_Pictures, FOLDERID_Profile, FOLDERID_RoamingAppData,
        FOLDERID_Videos, SHGetKnownFolderPath,
    };

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Folder {
        Profile,
        LocalAppData,
        RoamingAppData,
        Videos,
        Documents,
        Desktop,
        Downloads,
        Music,
        Pictures,
    }

    impl Folder {
        /// The library folder a profile subfolder's name stands for
        /// (`Videos`, any case).
        pub fn library(name: &str) -> Option<Self> {
            [
                ("Videos", Self::Videos),
                ("Documents", Self::Documents),
                ("Desktop", Self::Desktop),
                ("Downloads", Self::Downloads),
                ("Music", Self::Music),
                ("Pictures", Self::Pictures),
            ]
            .into_iter()
            .find(|(known, _)| known.eq_ignore_ascii_case(name))
            .map(|(_, folder)| folder)
        }
    }

    /// The folder's path for this user, if Windows has one.
    pub fn path(folder: Folder) -> Option<PathBuf> {
        let id = match folder {
            Folder::Profile => &FOLDERID_Profile,
            Folder::LocalAppData => &FOLDERID_LocalAppData,
            Folder::RoamingAppData => &FOLDERID_RoamingAppData,
            Folder::Videos => &FOLDERID_Videos,
            Folder::Documents => &FOLDERID_Documents,
            Folder::Desktop => &FOLDERID_Desktop,
            Folder::Downloads => &FOLDERID_Downloads,
            Folder::Music => &FOLDERID_Music,
            Folder::Pictures => &FOLDERID_Pictures,
        };
        let mut path = std::ptr::null_mut();
        // SAFETY: a valid folder id, default flags and no token (this user);
        // the returned string is freed with CoTaskMemFree even on failure.
        let status = unsafe { SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut path) };
        let result = (status >= 0 && !path.is_null()).then(|| {
            // SAFETY: a NUL-terminated wide string from the shell.
            let len = (0..).take_while(|&i| unsafe { *path.add(i) } != 0).count();
            PathBuf::from(std::ffi::OsString::from_wide(unsafe {
                std::slice::from_raw_parts(path, len)
            }))
        });
        unsafe { CoTaskMemFree(path.cast()) };
        result.filter(|path| !path.as_os_str().is_empty())
    }
}

/// A JSON-RPC request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub id: u64,
    #[serde(flatten)]
    pub method: Method,
}

impl Request {
    pub fn new(id: u64, method: Method) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            method,
        }
    }

    /// What is wrong with a request line that didn't parse, naming the
    /// field (`params.transition.direction: unknown variant ...`): serde
    /// can't, as the method is flattened into the request. None if it isn't
    /// JSON, or nothing is found wrong.
    pub fn located_error(line: &str) -> Option<crate::located::Located> {
        let request: serde_json::Value = serde_json::from_str(line).ok()?;
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct Envelope {
            jsonrpc: String,
            id: u64,
        }
        if let Err(err) = crate::located::from_value::<Envelope>(&request) {
            return Some(err);
        }
        let method = request.get("method")?;
        let params = request.get("params").filter(|params| !params.is_null());
        // A method without params alone; then with them, the tag first so
        // serde reads them as it goes rather than buffering them.
        let alone = serde_json::json!({ "method": method });
        let takes_params =
            serde_json::from_value::<Method>(serde_json::json!({ "method": method, "params": {} }))
                .is_ok();
        if let (Ok(_), Some(_), false) = (
            serde_json::from_value::<Method>(alone.clone()),
            params,
            takes_params,
        ) {
            return Some(crate::located::Located {
                path: "params".into(),
                message: format!("`{}` takes no params", method.as_str().unwrap_or_default()),
            });
        }
        if method == "plan"
            && let Some(params) = params.filter(|p| p.as_object().is_some_and(|o| o.len() != 1))
            && let Some(message) = Change::neither_or_both(params)
        {
            return Some(crate::located::Located {
                path: "params".into(),
                message: message.into(),
            });
        }
        let mut full = alone;
        if let Some(params) = params {
            full["params"] = params.clone();
        }
        crate::located::from_value::<Method>(&full)
            .err()
            .map(|mut err| {
                crate::located::identify_source(&mut err, &request["params"]["setup"]);
                err
            })
    }
}

/// A JSON-RPC response: exactly one of `result` and `error` is set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn ok(id: u64, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: Some(id),
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Option<u64>, error: RpcError) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    /// The line isn't JSON.
    pub const PARSE_ERROR: i64 = -32700;
    /// The request isn't valid JSON-RPC (no `id`, say).
    pub const INVALID_REQUEST: i64 = -32600;
    /// isocastd has no such method; the message lists the methods.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// The method's params are wrong; the message names the field.
    pub const INVALID_PARAMS: i64 = -32602;
    /// The method failed; `message` says why.
    pub const FAILED: i64 = -32000;
    /// The method doesn't apply in the engine's current state, like `stop`
    /// while idle.
    pub const WRONG_STATE: i64 = -32001;
    /// Not permitted: a plugin called a method it wasn't granted, or a
    /// plugin's permissions weren't granted; the message says which.
    pub const NOT_PERMITTED: i64 = -32002;
    /// This engine can't do it on this platform (or build) at all, so
    /// asking again won't help; the message says what to use instead.
    pub const UNSUPPORTED: i64 = -32003;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FrameParams {
    /// `program` (the default), `preview` (studio mode's preview scene),
    /// `vertical-program` (the vertical canvas's program, with a
    /// `[vertical]` canvas), `scene:ID` (a scene, drawn while watched, on
    /// air or not) or a video source's id.
    #[serde(default = "program_target")]
    pub target: String,
    /// Scale down to at most this wide, keeping the aspect ratio; full size
    /// if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_width: Option<u32>,
    /// Wait for a frame newer than this `seq` (from an earlier reply).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<u64>,
    /// How long to wait, in milliseconds: 1000 if absent, at most 10000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u32>,
    /// JPEG quality from 0 to 1: 0.75 if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<f32>,
    /// How many frames a second the client shows: while it keeps asking,
    /// the program or studio mode's preview is drawn at least that often
    /// (up to the canvas's rate); 30 if absent. Recordings and streams get
    /// every canvas frame regardless.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
}

fn program_target() -> String {
    crate::PROGRAM.to_string()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FramesParams {
    /// `program` (the default), `preview` (studio mode's preview scene),
    /// `vertical-program` (the vertical canvas's program, with a
    /// `[vertical]` canvas), `scene:ID` (a scene, drawn while watched, on
    /// air or not) or a video source's id.
    #[serde(default = "program_target")]
    pub target: String,
    /// Scale down to at most this wide, keeping the aspect ratio; full size
    /// if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_width: Option<u32>,
    /// The most frames a second to send: 30 if absent, at most 60. Change
    /// it while watching by sending [`FrameRate`] lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
    /// The H.264 profile of the records: `high` if absent (as before), or
    /// `constrained_baseline` for decoders that take nothing else, like
    /// WebRTC peers that only offer `42e01f`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<H264Profile>,
}

/// The H.264 profile a `frames` stream is encoded in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum H264Profile {
    /// High profile, the default.
    #[default]
    High,
    /// Constrained Baseline profile (`42e0..`): no B-frames or CABAC, for
    /// decoders that take nothing else.
    ConstrainedBaseline,
}

/// A line a `frames` client may send while watching, `{"fps": 60}`: the
/// most frames a second it wants from now on (1 to 60). The stream runs at
/// the highest rate any of its viewers wants and changes rate without a
/// new keyframe, so a monitor can follow the canvas's rate while the user
/// drags and go back to 30 after.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FrameRate {
    pub fps: u32,
}

/// The answer to `frames`, before its binary records.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FrameStream {
    pub target: String,
    /// The records' codec: `h264`.
    pub codec: String,
    /// The most frames a second this viewer asked for (the stream may send
    /// more if another viewer of it wants more).
    pub fps: u32,
    /// The host clock now, in nanoseconds: the clock record `seq`s are on,
    /// so a client can tell how late frames arrive.
    pub host_time: u64,
}

/// Parameters of `shared_frames`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SharedFramesParams {
    /// `program` (the default), `preview` (studio mode's preview scene),
    /// `vertical-program` (the vertical canvas's program, with a
    /// `[vertical]` canvas), `scene:ID` (a scene, drawn while watched, on
    /// air or not) or a video source's id.
    #[serde(default = "program_target")]
    pub target: String,
    /// The picture fits in this many pixels across and down, keeping its
    /// shape (never larger than the target itself); full size if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_height: Option<u32>,
    /// The most frames a second: 30 if absent, at most 60. Change it while
    /// watching with a [`shared::Control::Fps`] line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
    /// Surfaces in the ring: 4 if absent, 2 to 8. The client may hold all
    /// but one; with none free, frames are skipped until it releases one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slots: Option<u32>,
    /// With a source as the target: its picture through its filters, as
    /// the program shows it (they're read live, so a change shows at
    /// once), rather than as captured.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub filtered: bool,
}

/// The answer to `shared_frames`, before its [`shared::Event`] lines.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SharedFrameStream {
    pub target: String,
    /// How surfaces are handed over: `iosurface-mach` (macOS: IOSurfaces
    /// as Mach send rights from the Mach service `service`).
    pub transport: String,
    /// Where the client fetches each ring's surfaces: on macOS the daemon's
    /// Mach service name in the client's bootstrap namespace.
    pub service: String,
    /// The surfaces' pixel format: `rgba8unorm` (Rec. 709 primaries and
    /// transfer, as the canvas; not sRGB-decoded by the GPU).
    pub format: String,
    pub fps: u32,
    pub slots: u32,
    /// The host clock now, in nanoseconds: the clock frame times are on.
    pub host_time: u64,
}

/// The lines a `shared_frames` connection carries after its reply, and
/// the lines its client sends back. Each is one JSON object per line.
pub mod shared {
    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};

    /// From the daemon.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    pub enum Event {
        /// A new ring of surfaces, which replaces any before (at the start,
        /// and whenever the picture's size changes). Fetch its surfaces
        /// with `token` from the stream's `service`, once; slots number
        /// them in the order they come.
        Ring(Ring),
        /// A frame is ready in a slot of the current ring: the GPU has
        /// finished drawing it. Send [`Control::Release`] when done with it.
        Frame(Frame),
        /// The stream stopped, and why; the connection closes after.
        End(String),
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
    pub struct Ring {
        /// Counts rings in this stream, from 1.
        pub generation: u64,
        pub width: u32,
        pub height: u32,
        pub slots: u32,
        /// 32 hex digits, good for one fetch.
        pub token: String,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
    pub struct Frame {
        /// The ring it is in.
        pub generation: u64,
        pub slot: u32,
        /// When the picture was captured or drawn, on the host clock, in
        /// nanoseconds.
        pub time: u64,
        /// For the program and the preview, the setup revision (as `get`
        /// and `apply` report it) the frame was drawn from, so a client can
        /// draw its own marks (a selection box) at the layout the picture
        /// shows.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub revision: Option<u64>,
        /// And the layout generation it was drawn at (as `get` with
        /// `resolved` reports it).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub layout_generation: Option<u64>,
    }

    /// From the client.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    pub enum Control {
        /// The client's GPU is done with this slot: the daemon may draw
        /// into it again.
        Release { generation: u64, slot: u32 },
        /// The most frames a second it wants from now on (1 to 60).
        Fps(u32),
        /// Fit the picture in a new size from the next frame (a new ring).
        Size {
            #[serde(default, skip_serializing_if = "Option::is_none")]
            max_width: Option<u32>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            max_height: Option<u32>,
        },
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn lines_look_as_documented() {
            let frame = Event::Frame(Frame {
                generation: 1,
                slot: 2,
                time: 3,
                revision: Some(4),
                layout_generation: Some(5),
            });
            assert_eq!(
                serde_json::to_string(&frame).unwrap(),
                r#"{"frame":{"generation":1,"slot":2,"time":3,"revision":4,"layout_generation":5}}"#
            );
            let release: Control =
                serde_json::from_str(r#"{"release":{"generation":1,"slot":2}}"#).unwrap();
            assert_eq!(
                release,
                Control::Release {
                    generation: 1,
                    slot: 2
                }
            );
            let fps: Control = serde_json::from_str(r#"{"fps":60}"#).unwrap();
            assert_eq!(fps, Control::Fps(60));
            let size: Control = serde_json::from_str(r#"{"size":{"max_width":800}}"#).unwrap();
            assert_eq!(
                size,
                Control::Size {
                    max_width: Some(800),
                    max_height: None
                }
            );
        }
    }
}

/// The binary records a `frames` connection carries after its JSON reply.
///
/// Each record is a 24-byte header, big-endian, then a payload:
///
/// ```text
/// u32  length of the rest of the record (20 + payload)
/// u8   kind: 1 config, 2 key frame, 3 frame, 4 end, 5 revision
/// u8×3 zero
/// u64  seq: a frame's time on the host clock, in nanoseconds (else 0)
/// u32  width, u32 height: the picture's size (else 0)
/// …    payload
/// ```
///
/// - **config**: an `AVCDecoderConfigurationRecord` (`avcC`: profile, level,
///   SPS and PPS, 4-byte NAL lengths). Comes before the first key frame
///   and again whenever the size or settings change; WebCodecs takes it as
///   the decoder's `description`.
/// - **key frame**, **frame**: one H.264 access unit, its NAL units each
///   after a 4-byte length (AVCC). No B-frames: decode order is display
///   order. A stream starts with a config and a key frame.
/// - **end**: why the stream stopped (UTF-8); the connection closes after.
/// - **revision**: for the program and the preview, the setup revision (a
///   u64, as `get` and `apply` report it) the next frame was drawn from,
///   then the layout generation it was drawn at (a u64, as `get` with
///   `resolved` reports it); `seq` is that frame's. Sent before a frame
///   whenever either differs from what this client was last told, so a
///   client can draw its own marks (an editor's selection box) at the
///   layout the picture shows. Clients read the payload's first 8 bytes
///   (older daemons send only those) and ignore kinds they don't know.
pub mod frames {
    use std::io::{self, Read};

    pub const CONFIG: u8 = 1;
    pub const KEY_FRAME: u8 = 2;
    pub const FRAME: u8 = 3;
    pub const END: u8 = 4;
    pub const REVISION: u8 = 5;
    /// The header's size, its length field included.
    pub const HEADER: usize = 24;
    /// Records longer than this are refused as corrupt.
    const MAX_RECORD: usize = 64 << 20;

    /// One record.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Record {
        pub kind: u8,
        pub seq: u64,
        pub width: u32,
        pub height: u32,
        pub payload: Vec<u8>,
    }

    /// A record's bytes, ready to send.
    pub fn encode(kind: u8, seq: u64, width: u32, height: u32, payload: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(HEADER + payload.len());
        bytes.extend_from_slice(&((HEADER - 4 + payload.len()) as u32).to_be_bytes());
        bytes.extend_from_slice(&[kind, 0, 0, 0]);
        bytes.extend_from_slice(&seq.to_be_bytes());
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    impl Record {
        /// The next record, or `None` at the end of the stream.
        pub fn read(reader: &mut impl Read) -> io::Result<Option<Self>> {
            let mut header = [0u8; HEADER];
            match reader.read_exact(&mut header[..4]) {
                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
                result => result?,
            }
            reader.read_exact(&mut header[4..])?;
            let field = |at: usize| u32::from_be_bytes(header[at..at + 4].try_into().unwrap());
            let length = field(0) as usize;
            if !(HEADER - 4..=MAX_RECORD).contains(&length) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("a frame record of {length} bytes"),
                ));
            }
            let mut payload = vec![0u8; length - (HEADER - 4)];
            reader.read_exact(&mut payload)?;
            Ok(Some(Self {
                kind: header[4],
                seq: u64::from_be_bytes(header[8..16].try_into().unwrap()),
                width: field(16),
                height: field(20),
                payload,
            }))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn records_read_back_as_written() {
            let mut bytes = encode(KEY_FRAME, 1 << 40, 1664, 936, &[0, 0, 0, 1, 0x65]);
            bytes.extend(encode(END, 0, 0, 0, b"bye"));
            let mut reader = &bytes[..];
            let first = Record::read(&mut reader).unwrap().unwrap();
            assert_eq!(
                (first.kind, first.seq, first.width, first.height),
                (KEY_FRAME, 1 << 40, 1664, 936)
            );
            assert_eq!(first.payload, [0, 0, 0, 1, 0x65]);
            assert_eq!(Record::read(&mut reader).unwrap().unwrap().payload, b"bye");
            assert_eq!(Record::read(&mut reader).unwrap(), None);
        }
    }
}

/// One live frame as a JPEG.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Frame {
    pub target: String,
    /// Orders frames: pass it as `after` to wait for the next one. (The
    /// frame's time on the host clock, in nanoseconds.)
    pub seq: u64,
    pub width: u32,
    pub height: u32,
    pub jpeg_base64: String,
    /// For the program and the preview: the setup revision the frame was
    /// drawn from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    /// And the layout generation it was drawn at (as `get` with `resolved`
    /// reports it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout_generation: Option<u64>,
}

/// The live engine: what it runs and how it keeps up.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EngineInfo {
    /// The second canvas, only while it is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vertical: Option<CanvasEngineInfo>,
    /// Every extra canvas while it is enabled, the vertical one included
    /// (#84), each with its `id`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub canvases: Vec<CanvasEngineInfo>,
    /// Active media clocks, including per-source generators and ISO outputs.
    #[serde(default)]
    pub clocks: Vec<ClockPacingInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub worker_errors: Vec<String>,
    /// Seconds since it started.
    pub uptime_seconds: f64,
    pub canvas: Canvas,
    /// Every source it runs, and how each is doing.
    pub sources: Vec<SourceStatus>,
    /// Program frames drawn, and those missed because drawing fell behind.
    pub program_frames: u64,
    pub missed_frames: u64,
    /// Program frames drawn per second over the last second: the canvas's
    /// frame rate while the engine keeps up (0 for its first second).
    #[serde(default)]
    pub fps: f64,
    /// What it is running for, like `recording`, `stream`, `frames` or
    /// `held` (asked to keep running).
    #[serde(default)]
    pub outputs: Vec<String>,
    /// How much work the program's compositor does per canvas tick.
    #[serde(default)]
    pub compositor: CompositorInfo,
    /// Resolving the setup's scenes into what the compositor draws.
    #[serde(default)]
    pub scenes: ScenesInfo,
    /// Plugins' audio filters (WebAssembly) it runs, and what they cost.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audio_filters: Vec<PluginFilterInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CanvasEngineInfo {
    /// Which canvas (in `EngineInfo.canvases`).
    #[serde(default, skip_serializing_if = "CanvasId::is_main")]
    pub id: CanvasId,
    pub canvas: Canvas,
    pub program_frames: u64,
    pub missed_frames: u64,
    pub fps: f64,
    /// Why this canvas's program stopped, after a failure contained to it
    /// (the main canvas and its recording keep going). Its streams end; it
    /// stays stopped, and won't record or stream, until the canvas is
    /// turned off and on again (or resized) or the live engine restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<String>,
    pub compositor: CompositorInfo,
    pub scenes: ScenesInfo,
}

/// A plugin's audio filter in a running chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PluginFilterInfo {
    /// Where the setup has it, like `sources[2].audio_filters[1]`.
    pub path: String,
    /// `<plugin id>/<kind id>`.
    pub kind: String,
    /// What it filters for: `mix` (the program mix) or `iso` (a source's
    /// filtered ISO file).
    pub runs_in: String,
    pub state: PluginFilterState,
    /// Why it is off or restarting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Blocks (usually 10 ms) it filtered.
    pub calls: u64,
    /// Wall time per block, in microseconds, over its last 1,000 blocks.
    pub call_us: Percentiles,
    /// Work per block in fuel (about one WebAssembly instruction each),
    /// over its last 1,000 blocks; a call past `fuel_budget` is stopped.
    pub fuel: Percentiles,
    pub fuel_budget: u64,
    /// Calls that trapped, ran out of fuel or went over its memory.
    pub failures: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PluginFilterState {
    Running,
    /// A call failed; a fresh instance is being made, and the sound passes
    /// through meanwhile.
    Restarting,
    /// Bypassed (the sound passes through): not installed, its plugin
    /// turned off, its settings refused, or three failures in a minute.
    Off,
}

/// The program's compositor: what drawing a canvas frame costs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CompositorInfo {
    /// Lateness after the requested media wake deadline, including missed
    /// ticks. Microseconds over the last 600 ticks, not time spent drawing.
    #[serde(default)]
    pub wake_lateness_us: Percentiles,
    /// Wall time drawing a program frame (including submission and GPU wait).
    /// Reused frames are included; microseconds over the last 600 draws.
    #[serde(default)]
    pub draw_wall_us: Percentiles,
    /// Wall time from the final GPU submission to its completion hand-off,
    /// not GPU execution timestamps or time blocked on the drawing thread.
    /// Zero for reuse; the last 600 completed program frames.
    #[serde(default)]
    pub gpu_wait_us: Percentiles,
    /// CPU time the program's thread spent on each canvas tick that gave
    /// the program a frame (waiting for the GPU not included), in
    /// microseconds, over the last 600 such ticks (10 s at 60 fps).
    #[serde(default)]
    pub cpu_us: Percentiles,
    /// Ticks so far on which nothing in the picture had changed (no new
    /// source frame, nothing animating, no setup change), so the last
    /// program frame was sent again instead of drawing it.
    #[serde(default)]
    pub reused: u64,
    /// Draws in the last program frame drawn: each item's picture or
    /// fill, and its shadow, in the scene on air and the scenes it shows.
    #[serde(default)]
    pub draws: u32,
    /// Animated properties evaluated for the last program frame drawn: each
    /// property of each animation of an item in the scenes it drew.
    #[serde(default)]
    pub animated: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PacingMode {
    #[default]
    Ordinary,
    Critical,
}

/// Each clock measures its own wakes and inherits critical pacing when another
/// clock in the process has measured stretching. No policy is inferred from
/// the process name or launch path.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ClockPacingInfo {
    pub id: String,
    pub period_ms: f64,
    pub mode: PacingMode,
    /// Adopted measured process-wide evidence at creation or a subsequent wait.
    #[serde(default)]
    pub inherited_critical: bool,
    /// Escalations from this clock's own measured stretching (not inheritance).
    pub escalations: u64,
    pub wakes: u64,
    /// All deadlines, including overdue catch-up ticks, over the last 600.
    pub wake_lateness_us: Percentiles,
    /// Lifetime maximum; includes rare blackouts that p99 can miss.
    pub wake_max_us: f64,
    /// Lifetime count more than one nominal period late (not output drops).
    pub late_wakes: u64,
}

/// A median and a 99th percentile.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Percentiles {
    pub p50: f64,
    pub p99: f64,
}

/// The scene thread, which resolves the setup's scenes into draw lists
/// whenever they change (never per frame).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ScenesInfo {
    /// How long resolving the scenes took the last time they changed, in
    /// microseconds (drawing new content included).
    #[serde(default)]
    pub resolve_us: f64,
    /// How long drawing content (text and images of content items) took
    /// the last time any had to be drawn, in microseconds.
    #[serde(default)]
    pub raster_us: f64,
    /// Pictures of content items the scenes show, and the memory they
    /// take (as RGBA), in megabytes.
    #[serde(default)]
    pub content: u32,
    #[serde(default)]
    pub content_mb: f64,
}

/// One of the live engine's sources.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SourceStatus {
    pub source: String,
    pub state: SourceRun,
    /// Why it couldn't open, or stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Its picture's size and rate, if it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
    /// Frames it actually delivered per second over the last second,
    /// measured (`fps` is the rate it was opened at); 0 for a still
    /// picture or a device that stopped sending. In `status` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub received_fps: Option<f64>,
    /// Its audio channels, if it has sound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SourceRun {
    Opening,
    Running,
    /// It couldn't open, or its device went away; retried when the setup
    /// next changes.
    Failed,
    /// Opening protected data or a device whose permission is not granted.
    /// `error` names the permission and the next step; cleared on success.
    WaitingForPermission,
}

/// A snapshot of a running session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LiveStatus {
    pub elapsed_seconds: f64,
    /// True while recording is paused; elapsed time excludes all pauses.
    #[serde(default)]
    pub paused: bool,
    /// Total time cut out so far, including the current pause.
    #[serde(default)]
    pub paused_seconds: f64,
    /// A split's handover is under way: the new take's files are starting
    /// or the previous take's are finishing. Split, pause and resume are
    /// refused (busy) until it ends, about a second or two later.
    #[serde(default)]
    pub handover: bool,
    pub files: Vec<LiveFile>,
    /// Each audio source's level since the last status, as captured
    /// (before gain), and the program mix's ("program").
    #[serde(default)]
    pub levels: Vec<AudioLevel>,
    /// What the monitoring device plays, once a source has been monitored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor: Option<MonitorStatus>,
    /// How the disk holding the files keeps up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<LiveDisk>,
}

/// Recording-only clock, independent of the live engine and stream clock.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RecordingPause {
    pub paused: bool,
    pub elapsed_seconds: f64,
    pub paused_seconds: f64,
    /// A pause reply of crash-safe files: seconds of the take's saved
    /// timeline up to which every file was written and flushed to the
    /// disk before this reply (the same number the manifest's pause entry
    /// records). Absent when the files can't say (AVAssetWriter), when a
    /// file's commit failed or timed out (see `errors`), and on resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recoverable_seconds: Option<f64>,
    /// What went wrong committing the files at this pause: the pause
    /// itself took effect, but nothing durable is promised for the files
    /// named here, and they are reported failed when the recording ends.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

/// How the disk holding a running session's files keeps up.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LiveDisk {
    /// How fast the files reached the disk over the last few seconds.
    pub write_bytes_per_second: f64,
    /// How long a small test write beside the files took (written and
    /// synced) when the take started, in ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_at_start_ms: Option<f64>,
    /// The latest such test write, measured while frames were being
    /// dropped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<f64>,
    /// Governor clock milliseconds when the latest probe completed; a stale
    /// reading is not evidence that disk headroom returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measured_at_ms: Option<u64>,
}

impl LiveDisk {
    /// A test write now takes 10× as long as at the start, and at least
    /// 5 ms: the disk is behind (busy with more than it can write), and
    /// frames dropped meanwhile are its doing.
    pub fn behind(&self) -> bool {
        match (self.latency_at_start_ms, self.latency_ms) {
            (Some(start), Some(now)) => now >= 5.0 && now >= 10.0 * start,
            _ => false,
        }
    }
}

/// Audio monitoring in a running session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MonitorStatus {
    /// The output device the setup asks for (`audio.monitoring_device`).
    pub device: String,
    /// The device playing, by name, once it has been opened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Why the device couldn't be opened; monitoring is off until the next
    /// recording.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The sources being heard.
    pub sources: Vec<String>,
    /// Loudest left and right samples played since the last status, in
    /// dBFS (-120 is silence).
    pub peak_db: [f64; 2],
    /// Average left and right levels played, in dBFS.
    pub rms_db: [f64; 2],
    /// Sample frames the device has played so far.
    pub frames: u64,
    /// Times a source's audio ran out before the device needed it (heard
    /// as a gap); each makes that source keep more audio in hand.
    pub underruns: u64,
    /// The most audio waiting to be played for one source, in ms: how far
    /// monitoring runs behind capture, before the device's own latency.
    pub buffered_ms: f64,
    /// The device plays silence (`ISOCAST_MONITOR_SILENT` is set); the
    /// levels and counters show what it would have played.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub silent: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AudioLevel {
    pub source: String,
    /// Loudest sample, in dBFS (0 is full scale; -120 is silence).
    pub peak_db: f64,
    /// Average level, in dBFS.
    pub rms_db: f64,
    /// Left and right on their own, for the program mix's level (stereo);
    /// absent for a source's, which is one level for all its channels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<[ChannelLevel; 2]>,
}

/// One channel's level (see `AudioLevel::channels`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ChannelLevel {
    /// Loudest sample, in dBFS (-120 is silence).
    pub peak_db: f64,
    /// Average level, in dBFS.
    pub rms_db: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LiveFile {
    pub source: String,
    /// Codec of this take, not the editable setup's next-take settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<crate::Codec>,
    /// The native encoder's actual selection; unknown is not hardware.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hardware_accelerated: Option<bool>,
    /// Whether the muxer has written this file's first encoded video packet.
    /// Not encoder admission or a disk durability guarantee. Absent
    /// when the backend cannot observe this milestone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_active: Option<bool>,
    pub frames: u64,
    /// Frames due so far that never reached the file. Approximate while
    /// recording; the final report has the exact count.
    pub dropped_frames: u64,
    pub audio_dropouts: u64,
    /// Seconds of the file, from its start, that a crash can no longer
    /// lose: every track's samples up to there are in completed fragments
    /// on disk. Only crash-safe writers report it (absent otherwise).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committed_seconds: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Report {
    pub folder: PathBuf,
    pub disk: DiskReport,
    /// The manifest of the last take, which lines its files up on one
    /// timeline.
    pub manifest: PathBuf,
    /// Every take's manifest, in order; one unless the recording was split.
    pub takes: Vec<PathBuf>,
    pub duration_seconds: f64,
    pub files: Vec<FileReport>,
    /// Captures (displays, cameras) and how many new frames each delivered.
    /// A display only delivers a frame when the picture changes; files
    /// repeat the last one in between.
    pub captures: Vec<CaptureReport>,
    pub warnings: Vec<String>,
    /// Problems during recording. Files listed in `files` were still
    /// finished and are playable.
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FileReport {
    pub path: PathBuf,
    /// The take the file belongs to, from 1.
    pub take: u32,
    /// The video source recorded in this file.
    pub source: String,
    /// Audio sources, one track each, in track order.
    pub audio: Vec<String>,
    /// Seconds from the session start to the file's first frame.
    pub start_offset_seconds: f64,
    pub duration_seconds: f64,
    /// Encoded pictures stored, including explicit tail repeats.
    pub frames: u64,
    /// Tail pictures repeated to keep audio intact; also counted as drops.
    #[serde(default)]
    pub repeated_frames: u64,
    /// Original pictures missing: expected - frames + repeated_frames.
    pub dropped_frames: u64,
    /// Times audio was missing and filled with silence, or dropped by the
    /// encoder, across all tracks.
    pub audio_dropouts: u64,
    /// Audio sources that stopped while this file recorded (as in its
    /// manifest entry); absent when none did.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audio_stops: Vec<crate::manifest::AudioStop>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DiskReport {
    pub file_system: String,
    pub network: bool,
    /// Free space when the recording started.
    pub free_bytes: u64,
    /// Measured just before recording.
    pub write_bytes_per_second: f64,
    /// The generous estimate the checks used.
    pub estimated_bytes_per_second: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CaptureReport {
    pub source: String,
    pub frames_captured: u64,
}

impl LiveStatus {
    /// Warnings for files whose drop counts rose since `reported`, which
    /// holds each file's (frames, audio) counts already warned about.
    pub fn new_drops(&self, reported: &mut Vec<(u64, u64)>) -> Vec<String> {
        reported.resize(self.files.len(), (0, 0));
        // Said with each drop while it holds, so no warning reads as an
        // unexplained loss.
        let cause = match &self.disk {
            Some(disk) if disk.behind() => format!(
                ": the disk can't keep up (a test write takes {:.0} ms, {:.1} ms when the take \
                 started; the files reach it at {:.0} MB/s); record to a faster disk, or use a \
                 lighter codec or quality",
                disk.latency_ms.unwrap_or_default(),
                disk.latency_at_start_ms.unwrap_or_default(),
                disk.write_bytes_per_second / 1e6
            ),
            _ => String::new(),
        };
        let mut messages = Vec::new();
        for (file, (frames, audio)) in self.files.iter().zip(reported.iter_mut()) {
            if file.dropped_frames > *frames {
                messages.push(format!(
                    "{} dropped {} frame(s) at {:.0} s ({} so far){cause}",
                    file.source,
                    file.dropped_frames - *frames,
                    self.elapsed_seconds,
                    file.dropped_frames
                ));
                *frames = file.dropped_frames;
            }
            if file.audio_dropouts > *audio {
                messages.push(format!(
                    "{} had {} audio dropout(s) at {:.0} s ({} so far){cause}",
                    file.source,
                    file.audio_dropouts - *audio,
                    self.elapsed_seconds,
                    file.audio_dropouts
                ));
                *audio = file.audio_dropouts;
            }
        }
        messages
    }
}

/// What `doctor` found about this machine and a setup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Doctor {
    pub machine: Machine,
    pub checks: Vec<Check>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Machine {
    pub chip: String,
    pub memory_gb: f64,
    pub cores: u32,
    pub os: String,
    /// Video encoders that run on the media engines, by name.
    pub hardware_encoders: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Check {
    /// What was checked, like `permission: camera` or `source: facecam`.
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
    /// What to do about a warning or problem.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Ok,
    /// Works, but worth attention.
    Warning,
    /// Recording this setup will fail or lose something.
    Problem,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TuneParams {
    /// The setup to tune; the live setup if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<Setup>,
    /// Benchmark writing to this folder instead of the setup's
    /// `recording.folder`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<PathBuf>,
}

/// What `tune` measured and recommends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Tuning {
    /// The video streams benchmarked together: each ISO file's and the
    /// program's, at the sizes their files are written.
    pub streams: Vec<TunedStream>,
    /// The recording disk's measured write speed, bytes per second.
    pub disk_bytes_per_second: f64,
    pub free_bytes: u64,
    /// Whether the encoders keep up with the setup as it is; absent if it
    /// couldn't be benchmarked (see the first candidate's reason).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<Capacity>,
    /// How many times faster than real time the setup's own encoders ran
    /// on hard content, all streams at once (0 if not measured).
    #[serde(default)]
    pub realtime_factor: f64,
    /// The setup's streams with other ISO encoders.
    pub candidates: Vec<Candidate>,
    /// When the setup doesn't fit: changes to it that were benchmarked,
    /// closest first (a smaller program file, a lower quality preset, a
    /// lower frame rate, fewer ISO files), until one fit.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<Variant>,
    /// The ISO encoder settings recommended for this Mac; none if nothing
    /// tried fits.
    pub recommended: Option<crate::Encoder>,
    /// The recommendation in words, like "record the program at
    /// 1920×1080": the closest change that fits, or the setup as it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<String>,
    /// A patch for `plan`/`apply` that makes the recommendation, if it
    /// differs from the setup.
    pub change: Option<serde_json::Value>,
    pub notes: Vec<String>,
    /// ISO sources that couldn't be opened, so their files weren't in the
    /// benchmark (the rest were measured): each with why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedSource>,
}

/// A source `tune` couldn't open to learn its format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SkippedSource {
    /// Its id.
    pub source: String,
    /// Why it couldn't be opened.
    pub reason: String,
}

/// Whether the hardware encoders keep up with a set of streams, judged on
/// hard content (busy, noisy pictures; real screens and cameras are
/// easier).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Capacity {
    /// At least 1.25× real time: keeps up with room for the Mac being busy.
    Fits,
    /// From 1× to 1.25× real time: keeps up, but anything else using the
    /// encoders (a stream, a video call, an export) may cost frames.
    NoMargin,
    /// Under 1× real time: a recording drops frames, about the share it
    /// falls short by.
    CannotKeepUp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TunedStream {
    pub source: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Candidate {
    pub encoder: crate::Encoder,
    /// How many times faster than real time all streams encoded together.
    pub realtime_factor: f64,
    /// Data rate of all streams together on hard-to-compress content.
    pub bytes_per_second: f64,
    /// Whether the encoders keep up; absent if it wasn't benchmarked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<Capacity>,
    /// Keeps up with room to spare, and the disk takes it.
    pub feasible: bool,
    /// Why it isn't feasible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// A change to the setup that `tune` benchmarked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Variant {
    /// What it changes, like "program at 1920×1080, 30 fps".
    pub description: String,
    /// A patch for `plan`/`apply` that makes it.
    pub change: serde_json::Value,
    pub streams: Vec<TunedStream>,
    pub realtime_factor: f64,
    pub bytes_per_second: f64,
    pub capacity: Capacity,
    /// Keeps up with room to spare, and the disk takes it.
    pub feasible: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CapacityParams {
    /// The setup to judge; the live setup, at its sources' live formats,
    /// if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<Setup>,
    /// Count the stream's encode too. If absent: for the live setup,
    /// whether it streams now; for a given setup, whether it has a stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// A source id whose ISO file would be turned on: `admission` says how
    /// to add it so the setup still fits (at a smaller size or frame rate,
    /// or not at all), and why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add_iso: Option<String>,
}

/// Whether a setup's files fit this Mac's media engines, predicted from
/// per-chip data without benchmarking (`tune` measures).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CapacityEstimate {
    /// This Mac's chip, as macOS names it.
    pub chip: String,
    /// H.264/HEVC encode engines on the chip. Each file's encoder runs on
    /// one of them for its whole life. ProRes uses separate resource estimates,
    /// including native hardware/software selection where observed.
    pub encode_engines: u32,
    /// Pixels a second one engine encodes in real time (8-bit HEVC at
    /// Isocast's settings) before files drop frames.
    pub engine_pixels_per_second: f64,
    /// Where those numbers come from: which Mac they were measured on, or
    /// that they are assumed from the chip's family.
    pub basis: String,
    /// The video encodes the setup asks for.
    pub outputs: Vec<EstimatedOutput>,
    /// Separate ProRes/native, CPU, graphics and disk observations. Empty
    /// for the legacy H.26x-only estimate; unknown capacity is never zero.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<ResourceEstimate>,
    /// The largest qualified resource load (legacy: busiest H.26x engine): up to
    /// 0.85 fits, up to 1 keeps up with no margin, over 1 drops frames.
    pub load: f64,
    pub capacity: Capacity,
    /// Whether the stream's encode is counted.
    pub streaming: bool,
    /// H.26x-only loss model in percent. ProRes resource estimates do not
    /// supply a numeric loss forecast (0; see resources and summary), which
    /// must not be interpreted as a promise of zero loss.
    pub iso_drop_percent: f64,
    /// The verdict in words.
    pub summary: String,
    /// When it doesn't fit (or has no margin): the closest changes that
    /// fit, closest first; or, if none does, those that come closest.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggestions: Vec<CapacitySuggestion>,
    /// With `add_iso`: how to turn that ISO file on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission: Option<IsoAdmission>,
    /// What `tune` measured for these files on this Mac, if it did; a
    /// measured "can't keep up" overrules the prediction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measured: Option<String>,
    /// The same for setups that ask the same of the encoders (the files'
    /// sizes, rates and codecs, and the stream): what a client remembers
    /// for "don't show again for this setup".
    pub workload_key: String,
}

/// [`ResourceEstimate::resource`] on a software graphics adapter (#128):
/// compositing and frame conversion run on the CPU, and the estimate is
/// judged against an advisory reference, never measured capacity.
pub const SOFTWARE_RENDERER_RESOURCE: &str = "software_renderer";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ResourceEstimate {
    pub resource: String,
    pub unit: String,
    pub demand: f64,
    /// None means unqualified/unknown, not unlimited capacity.
    pub capacity: Option<f64>,
    pub load: Option<f64>,
    pub basis: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OutputKind {
    Program,
    Stream,
    Iso,
}

/// One video encode of a setup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EstimatedOutput {
    /// `program`, `stream`, or the ISO file's source id.
    pub output: String,
    pub kind: OutputKind,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub codec: crate::Codec,
    /// Kept whole under overload (the program and the stream); ISO files
    /// drop pictures first.
    pub protected: bool,
    /// Its work on the H.264/HEVC engines, in HEVC-equivalent pixels a
    /// second; 0 for ProRes in this H.26x field, not zero overall work (see
    /// the estimate's separate resources).
    pub pixels_per_second: f64,
    /// The size or rate wasn't known (the source isn't running), so the
    /// canvas's was assumed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub assumed: bool,
}

/// A change that makes a setup ask less of the encoders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CapacitySuggestion {
    /// What it changes, like "capture facecam at 1280×720".
    pub description: String,
    /// A patch for `plan`/`apply` that makes it, if it's a setup change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<serde_json::Value>,
    /// A method that makes it instead, like `stream_stop`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// The load with it (see [`CapacityEstimate::load`]).
    pub load: f64,
    pub capacity: Capacity,
    /// Applying it now would change a running recording's sources or
    /// canvas, so apply it once the take ends (a setup change applied
    /// while recording takes effect from the next take anyway).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub after_take: bool,
}

/// How to turn on a source's ISO file so the setup still fits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct IsoAdmission {
    pub source: String,
    /// Whether the ISO file goes on at all.
    pub on: bool,
    /// Whether it goes on as the source is (no smaller size or rate).
    pub as_is: bool,
    /// A patch for `plan`/`apply`: turns the ISO file on (maybe with the
    /// source at a smaller size or rate), or nothing if it stays off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<serde_json::Value>,
    /// What it does, like "facecam's ISO file at 1280×720, 30 fps".
    pub description: String,
    /// The ISO file's size and rate when it goes on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
    /// Why it isn't added as it is, if it isn't.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Files dropping frames because the encoders are at capacity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Overload {
    pub take: u32,
    /// Seconds into the take when it was noticed.
    pub elapsed_seconds: f64,
    /// The files dropping frames so far, worst first.
    pub dropping: Vec<OverloadedFile>,
    /// Whether the program or stream drops too, not just ISO files.
    pub protected_dropping: bool,
    /// Why, in words.
    pub reason: String,
    /// The live setup's estimate, with the fixes: apply a suggestion's
    /// `change` with `plan`/`apply` (once the take ends if `after_take`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimate: Option<CapacityEstimate>,
    /// See [`CapacityEstimate::workload_key`].
    pub workload_key: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct OverloadedFile {
    /// `program`, `stream` or the ISO file's source id.
    pub output: String,
    pub dropped_frames: u64,
    /// Of the pictures due so far.
    pub percent: f64,
}

/// The source `screen_pick` chooses for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScreenPickParams {
    /// The id of a `display` or `window` source in the live setup. A window
    /// source keeps its own window; a display source's choice is kept for
    /// its `display` value, shared by display sources that give the same.
    pub source: String,
}

/// What was chosen in the desktop's sharing dialog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ScreenPicked {
    pub source: String,
    /// The size the desktop shares, in pixels; 0 if it didn't say.
    pub width: u32,
    pub height: u32,
    /// Whether the desktop remembers the choice. If not, its dialog asks
    /// again each time the source starts (on Hyprland, tick "Allow a
    /// restore token" in it).
    pub remembered: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SnapshotParams {
    /// A source id in the live setup (the recording's, while recording), or
    /// a quick source like `screen`, `display:NAME`, `camera:NAME` or
    /// `test-pattern`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Or a scene id. With neither, the program (while recording, exactly
    /// the frame it recorded last, mid-transition included; else the scene
    /// on air).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scene: Option<String>,
    /// Or, with neither, the scene in preview.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub preview: bool,
    /// Scale the picture down to at most this many pixels wide; full size
    /// if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
}

/// One frame of a source or a scene as a PNG.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Snapshot {
    /// The source's id, for a source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The scene's id, for a scene.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scene: Option<String>,
    pub width: u32,
    pub height: u32,
    /// The PNG file, base64-encoded.
    pub png_base64: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_home_and_support_folders() {
        #[cfg(windows)]
        {
            let profile = std::env::var_os("USERPROFILE")
                .filter(|p| !p.is_empty())
                .or_else(|| std::env::var_os("HOME"))
                .unwrap_or_default();
            assert_eq!(home_folder(), PathBuf::from(profile));
            let local = std::env::var_os("LOCALAPPDATA")
                .filter(|p| !p.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home_folder().join("AppData/Local"));
            assert_eq!(support_folder(), local.join("Isocast"));
            let roaming = std::env::var_os("APPDATA")
                .filter(|p| !p.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home_folder().join("AppData/Roaming"));
            let obs = std::env::var_os(crate::obs::FOLDER_ENV)
                .filter(|p| !p.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| roaming.join("obs-studio"));
            assert_eq!(crate::obs::obs_folder(&home_folder()), obs);
        }
        #[cfg(not(windows))]
        {
            let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
            assert_eq!(home_folder(), home);
            let expected = if cfg!(target_os = "macos") {
                home.join("Library/Application Support/Isocast")
            } else if cfg!(target_os = "linux") {
                linux_support_folder(
                    &home,
                    std::env::var_os("XDG_DATA_HOME").as_deref(),
                    home.join(".isocast").is_dir(),
                )
            } else {
                home.join(".isocast")
            };
            assert_eq!(support_folder(), expected);
        }
    }

    // Unix paths (absolute only there); the rule is Linux's.
    #[cfg(unix)]
    #[test]
    fn linux_support_folder_follows_xdg_data_home() {
        use std::ffi::OsStr;
        use std::path::Path;
        let home = Path::new("/home/me");
        let xdg =
            |value: Option<&str>, legacy| linux_support_folder(home, value.map(OsStr::new), legacy);
        assert_eq!(xdg(None, false), Path::new("/home/me/.local/share/isocast"));
        assert_eq!(xdg(Some("/data"), false), Path::new("/data/isocast"));
        // Relative and empty values are ignored, as the XDG spec says.
        assert_eq!(
            xdg(Some("data"), false),
            Path::new("/home/me/.local/share/isocast")
        );
        assert_eq!(
            xdg(Some(""), false),
            Path::new("/home/me/.local/share/isocast")
        );
        // An existing ~/.isocast from earlier builds wins.
        assert_eq!(xdg(Some("/data"), true), Path::new("/home/me/.isocast"));
        assert_eq!(xdg(None, true), Path::new("/home/me/.isocast"));
    }

    // Unix paths (absolute only there); the rule is Linux's.
    #[cfg(unix)]
    #[test]
    fn linux_socket_in_runtime_dir_when_it_exists() {
        use std::ffi::OsStr;
        use std::path::Path;
        let support = Path::new("/home/me/.local/share/isocast");
        let exists = |_: &Path| true;
        let missing = |_: &Path| false;
        assert_eq!(
            linux_socket_path(Some(OsStr::new("/run/user/1000")), exists, support),
            Path::new("/run/user/1000/isocast/isocastd.sock")
        );
        // Unset, a folder that isn't there, empty or relative: the support folder.
        let fallback = Path::new("/home/me/.local/share/isocast/isocastd.sock");
        assert_eq!(linux_socket_path(None, exists, support), fallback);
        assert_eq!(
            linux_socket_path(Some(OsStr::new("/run/user/1000")), missing, support),
            fallback
        );
        assert_eq!(
            linux_socket_path(Some(OsStr::new("")), exists, support),
            fallback
        );
        assert_eq!(
            linux_socket_path(Some(OsStr::new("run")), exists, support),
            fallback
        );
    }

    #[test]
    fn get_takes_resolved_or_no_params() {
        let parse = |line: &str| serde_json::from_str::<Request>(line).map(|r| r.method);
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","id":1,"method":"get"}"#).unwrap(),
            Method::Get(None)
        );
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","id":1,"method":"get","params":{"resolved":true}}"#).unwrap(),
            Method::Get(Some(GetParams {
                resolved: true,
                ..Default::default()
            }))
        );
        let wrong = r#"{"jsonrpc":"2.0","id":1,"method":"get","params":{"resolve":true}}"#;
        assert!(parse(wrong).is_err());
        let located = Request::located_error(wrong).unwrap();
        assert_eq!(located.path, "params.resolve", "{located}");
        assert!(located.message.contains("resolved"), "{located}");
        // Methods without params still say so.
        let status = r#"{"jsonrpc":"2.0","id":1,"method":"status","params":{}}"#;
        assert!(parse(status).is_err());
        assert_eq!(
            Request::located_error(status).unwrap().message,
            "`status` takes no params"
        );
    }

    #[test]
    fn frames_profile_is_optional_and_named() {
        let parse = |line: &str| serde_json::from_str::<Request>(line).map(|r| r.method);
        let Method::Frames(params) =
            parse(r#"{"jsonrpc":"2.0","id":1,"method":"frames","params":{}}"#).unwrap()
        else {
            panic!("not frames");
        };
        assert_eq!(params.profile, None);
        assert_eq!(params.profile.unwrap_or_default(), H264Profile::High);
        // Absent stays absent on the wire, so older daemons still parse it.
        let json = serde_json::to_value(&params).unwrap();
        assert!(json.get("profile").is_none(), "{json}");
        let line = r#"{"jsonrpc":"2.0","id":1,"method":"frames","params":
            {"target":"program","profile":"constrained_baseline"}}"#;
        let Method::Frames(params) = parse(&line.replace('\n', "")).unwrap() else {
            panic!("not frames");
        };
        assert_eq!(params.profile, Some(H264Profile::ConstrainedBaseline));
        assert_eq!(
            serde_json::to_value(&params).unwrap()["profile"],
            "constrained_baseline"
        );
        // A wrong profile names its place and the profiles there are.
        let wrong = r#"{"jsonrpc":"2.0","id":1,"method":"frames","params":{"profile":"main"}}"#;
        assert!(parse(wrong).is_err());
        let located = Request::located_error(wrong).unwrap();
        assert_eq!(located.path, "params.profile", "{located}");
        assert!(located.message.contains("`high`"), "{located}");
        assert!(
            located.message.contains("`constrained_baseline`"),
            "{located}"
        );
    }

    #[test]
    fn watch_takes_kinds_or_no_params() {
        let parse = |line: &str| serde_json::from_str::<Request>(line).map(|r| r.method);
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","id":1,"method":"watch"}"#).unwrap(),
            Method::Watch(None)
        );
        let line = r#"{"jsonrpc":"2.0","id":1,"method":"watch","params":
            {"kinds":["scene_changed","levels"],"except":["levels"]}}"#;
        let Method::Watch(Some(params)) = parse(&line.replace('\n', "")).unwrap() else {
            panic!("not watch");
        };
        let wanted = params.wanted();
        assert!(wanted.contains(EventKind::SceneChanged));
        assert!(!wanted.contains(EventKind::Levels));
        assert!(!wanted.contains(EventKind::Status));
        assert!(params.problem().is_none());
        let all_but = WatchParams::except(&[EventKind::Levels]).wanted();
        assert!(all_but.contains(EventKind::PluginChanged));
        assert!(!all_but.contains(EventKind::Levels));
        // A misspelt kind names its place and the kinds there are.
        let wrong =
            r#"{"jsonrpc":"2.0","id":1,"method":"watch","params":{"kinds":["stopped","level"]}}"#;
        assert!(parse(wrong).is_err());
        let located = Request::located_error(wrong).unwrap();
        assert_eq!(located.path, "params.kinds[1]", "{located}");
        assert!(located.message.contains("`levels`"), "{located}");
        // A filter that lets nothing through is refused.
        let none = WatchParams {
            kinds: Some(vec![]),
            except: vec![],
        };
        assert_eq!(none.problem().unwrap().path, "kinds");
        let none = WatchParams {
            kinds: Some(vec![EventKind::Levels]),
            except: vec![EventKind::Levels],
        };
        assert_eq!(none.problem().unwrap().path, "except");
    }

    #[test]
    fn event_kinds_match_the_events_types() {
        let schema = serde_json::to_value(schemars::schema_for!(Event)).unwrap();
        let types: Vec<String> = schema["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .map(|variant| {
                variant["properties"]["type"]["const"]
                    .as_str()
                    .unwrap()
                    .into()
            })
            .collect();
        assert_eq!(types, EventKind::NAMES);
        for (i, name) in EventKind::NAMES.iter().enumerate() {
            let kind: EventKind = serde_json::from_value(serde_json::json!(name)).unwrap();
            assert_eq!(kind as usize, i);
            assert_eq!(kind.name(), *name);
        }
        let event = Event::Warning {
            message: "x".into(),
        };
        assert_eq!(
            serde_json::to_value(&event).unwrap()["type"],
            event.kind().name()
        );
    }
}
