use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use isocast_client::DaemonError;
use isocast_protocol::api::{
    Applied, ApplyParams, Capacity, CapacityEstimate, CapacityParams, CheckStatus, DoctorParams,
    EngineInfo, EngineState, Event, EventKind, Frame, FrameParams, FramesParams, ImportObsParams,
    Live, Marked, Method, Plan, Refreshed, Report, RpcError, Snapshot, SnapshotParams, SourceRun,
    StartParams, Started, Status, StopAccepted, StreamDestinationParams, StreamDestinations,
    StreamKey, StreamKeyParams, StreamKeyTarget, StreamStartParams, Streaming, Switched,
    TuneParams, Tuning, WatchParams,
};
use isocast_protocol::change::{Change, Difference, DifferenceOp};
use isocast_protocol::devices::Devices;
use isocast_protocol::obs::Imported;
use isocast_protocol::platform_text as wording;
use isocast_protocol::stream::{MAIN_DESTINATION, StreamService, StreamState, StreamStatus};
use isocast_protocol::{CanvasId, QUICK_SOURCES, Setup, Transition};

use crate::client::Client;

mod client;
mod data;
mod debug;
mod engines;
mod hotkeys;
mod library;
mod mcp;
mod optimize;
mod plugin;
mod project;
mod projector;
mod record;
mod remote;
mod replay;
mod roots;
#[cfg(any(target_os = "macos", windows))]
mod shared_frames;
mod virtual_camera;
mod websocket;

#[cfg(any(target_os = "macos", windows))]
fn shared_frames(options: shared_frames::Options) -> Result<ExitCode> {
    shared_frames::run(options)
}

#[cfg(not(any(target_os = "macos", windows)))]
mod shared_frames {
    #[expect(
        dead_code,
        reason = "parsed options retained while shared surfaces are unsupported"
    )]
    pub struct Options {
        pub target: String,
        pub max_width: Option<u32>,
        pub max_height: Option<u32>,
        pub fps: Option<u32>,
        pub slots: Option<u32>,
        pub filtered: bool,
        pub seconds: f64,
        pub check: bool,
        pub expect: Vec<String>,
        pub touch: bool,
        pub json: bool,
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn shared_frames(_options: shared_frames::Options) -> Result<ExitCode> {
    bail!("--shared is available on macOS and Windows; leave it out to watch H.264")
}

/// Isocast: scene-based recording with per-source ISO files, controlled from
/// the command line or by agents.
#[derive(Parser)]
#[command(
    name = "isocast",
    version,
    after_help = wording::select_linux("Getting started: `isocast permissions` shows what macOS allows, `isocast \
                  setup init --apply` makes a setup from this Mac's devices, `isocast frame` \
                  saves a picture of the program, `isocast start --seconds 10` records a test, \
                  and `isocast status` shows how it went. Agents: `isocast mcp`.",
                  "Getting started: `isocast permissions` reports Windows access, `isocast setup init --apply` makes a setup from this PC's devices, `isocast frame` saves a picture of the program, `isocast start --seconds 10` records a test, and `isocast status` shows how it went. Windows Camera and Microphone settings must allow desktop apps; the recording engine is isocastd.exe. Agents: `isocast mcp`.",
"Getting started: `isocast permissions` reports what can be captured, `isocast setup init --apply` makes a setup from this computer's devices, `isocast frame` saves a picture of the program, `isocast start --seconds 10` records a test, and `isocast status` shows how it went. Agents: `isocast mcp`.")
)]
struct Cli {
    /// Engine to control: local, a paired name/id, or host[:port]. Also ISOCAST_ENGINE.
    #[arg(long, global = true)]
    engine: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum MachineCommand {
    /// Show the profile and calibration state without starting a measurement.
    Show {
        #[arg(long)]
        json: bool,
    },
    /// Start an idle-only self-test (at most 30 seconds); poll show for results.
    Calibrate {
        #[arg(long)]
        json: bool,
    },
    /// Cancel a self-test, retaining the previous profile.
    Cancel {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum Command {
    /// Remote-control settings, pairing and client identities.
    Remote(remote::RemoteArgs),
    /// Read a bounded page from a redacted report preview.
    ReportFile {
        preview_id: String,
        file: String,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        json: bool,
    },
    /// Create a private, redacted bug-report zip. Nothing is uploaded
    /// unless you pass --submit.
    Report {
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        describe: Option<String>,
        #[arg(long)]
        preview: bool,
        /// Pseudonymize paths, titles, device names and device IDs per report.
        #[arg(long = "private", visible_alias = "hash-paths")]
        hash_paths: bool,
        #[arg(long)]
        launches: Option<usize>,
        /// Also send it: ISSUE.md becomes a public GitHub issue, and the zip
        /// is kept privately for the maintainers. Prints the issue URL.
        #[arg(long, conflicts_with = "preview")]
        submit: bool,
        /// With --submit: send exactly this preview (from --preview).
        #[arg(long, requires = "submit")]
        preview_id: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Read structured logs (also works when the daemon is stopped).
    Logs {
        #[arg(long)]
        follow: bool,
        #[arg(long)]
        level: Option<String>,
        #[arg(long)]
        component: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Customize profile hotkeys, including global keys and push-to-talk.
    Hotkeys {
        #[command(subcommand)]
        command: hotkeys::HotkeysCommand,
    },
    /// List recording codecs and the containers each supports.
    Encoders {
        #[arg(long)]
        json: bool,
    },
    /// Show the daemon's graphics adapter identity (Windows DX12 LUID).
    GpuAdapter {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Work with setup files.
    Setup {
        #[command(subcommand)]
        command: SetupCommand,
    },
    /// Scene collections, as in OBS: named sets of scenes, sources,
    /// transitions and the canvas size, swapped as a whole. The one in use
    /// is the live setup's (with the profile in use).
    Collection {
        #[command(subcommand)]
        command: library::LibraryCommand,
    },
    /// Profiles, as in OBS: named frame rate, recording, stream and audio
    /// settings, swapped as a whole. The one in use is the live setup's
    /// (with the scene collection in use).
    Profile {
        #[command(subcommand)]
        command: library::LibraryCommand,
    },
    /// Plugins: folders with an isocast-plugin.toml, installed from a local
    /// folder. A plugin's process runs sandboxed and may only call the
    /// methods it was granted.
    Plugin {
        #[command(subcommand)]
        command: plugin::PluginCommand,
    },
    /// The data store: values by key (like `score.home`) that text in scenes
    /// shows (`data = "score.home"` on a text source or text content), set
    /// by you, scripts, agents and plugins without changing the setup.
    Data {
        #[command(subcommand)]
        command: data::DataCommand,
    },
    /// Recording projects: a folder per recording session
    /// (`<date> <time> Untitled` in the recordings folder) with every
    /// take's files and manifests, and takes as projects for Final Cut Pro,
    /// Premiere Pro or DaVinci Resolve (`project export-nle`).
    Project {
        #[command(subcommand)]
        command: project::ProjectCommand,
    },
    /// List displays, windows, cameras, microphones and audio outputs (for
    /// monitoring), with their ids and formats, and the permissions
    /// recording them needs.
    Devices {
        /// Print the result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Record a setup's ISO sources, each to its own file, until --seconds
    /// runs out or Ctrl-C. Without a setup file, record --source specs.
    Record {
        /// Setup file (.toml or .json).
        setup: Option<PathBuf>,
        /// Record without a setup file: screen, display[:NAME_OR_ID],
        /// camera:NAME_OR_ID, mic[:NAME_OR_ID],
        /// system-audio[:except=APP[,APP]], window:APP, app-audio:APP,
        /// test-pattern[:WIDTHxHEIGHT[@FPS]] or tone[:HZ]. Repeat for more
        /// sources; every audio source goes into every file.
        #[arg(long = "source", value_name = "SPEC")]
        sources: Vec<String>,
        /// Stop after this many seconds.
        #[arg(long)]
        seconds: Option<f64>,
        /// Record into a new project folder in this folder instead of the
        /// setup's recording.folder (or into this folder, if it is a
        /// project's).
        #[arg(long)]
        folder: Option<PathBuf>,
        /// Print the result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Start recording in the background (in isocastd, launched if needed)
    /// and return once every file is created. Same options as `record`;
    /// with neither a setup nor --source, records the live setup. A setup
    /// or --source given here becomes the live setup. Its takes go into a
    /// new project folder in the recordings folder (or --folder).
    Start {
        setup: Option<PathBuf>,
        #[arg(long = "source", value_name = "SPEC")]
        sources: Vec<String>,
        /// Stop on its own after this many seconds.
        #[arg(long)]
        seconds: Option<f64>,
        #[arg(long)]
        folder: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Ask the background recording to stop and return at once, while
    /// isocastd finishes its files (`status` says finalizing until it has;
    /// the files aren't done yet). With --wait, wait until they are and
    /// print the report. An older daemon without acknowledgement support
    /// also waits and returns the completed report.
    Stop {
        /// Wait for the files to be finished and print the report; exits
        /// with failure if the recording had errors.
        #[arg(long)]
        wait: bool,
        /// Stop only this recording session (its id, from `isocast record
        /// --background --json` or `isocast status --json`): refused if
        /// another session is recording now.
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Pause this take without closing its files; streaming continues.
    Pause {
        #[arg(long)]
        json: bool,
    },
    /// Resume the paused take into the same files.
    Resume {
        #[arg(long)]
        json: bool,
    },
    /// Show whether isocastd is recording, with live counters.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Show what the engine is: its machine's operating system and name,
    /// its identity, build, protocol version and how many methods it
    /// offers (all of them with --json).
    Hello {
        #[arg(long)]
        json: bool,
    },
    /// Split the background recording into a new take at the next whole
    /// second, without stopping any device.
    Split {
        #[arg(long)]
        json: bool,
    },
    /// Mark this moment in the background recording, with a label.
    Marker {
        label: String,
        #[arg(long)]
        json: bool,
    },
    /// Reload a browser source's page in the background recording,
    /// bypassing the cache.
    Refresh {
        /// The browser source's id.
        source: String,
        #[arg(long)]
        json: bool,
    },
    /// Put a scene on air, by id, with its transition (the scene's, else the
    /// setup's) unless --cut, --fade MS or --transition is given. While
    /// recording, the switch goes into the manifest's scene timeline.
    Scene {
        scene: String,
        #[command(flatten)]
        transition: TransitionArgs,
        /// Which canvas: main, or vertical (the setup's [vertical] canvas,
        /// once it stops following the main one).
        #[arg(long, value_parser = parse_canvas, default_value = "main")]
        canvas: CanvasId,
        #[arg(long)]
        json: bool,
    },
    /// Stage a scene in preview, by id, to put on air with `take`.
    Preview {
        #[arg(required_unless_present = "clear", conflicts_with = "clear")]
        scene: Option<String>,
        /// Leave Studio Mode (clear its staged preview).
        #[arg(long)]
        clear: bool,
        /// Which canvas: main, or vertical (once it stops following the
        /// main one).
        #[arg(long, value_parser = parse_canvas, default_value = "main")]
        canvas: CanvasId,
        #[arg(long)]
        json: bool,
    },
    /// Put the preview scene on air (with its transition unless --cut,
    /// --fade MS or --transition is given); the scene that was on air moves
    /// to preview.
    Take {
        #[command(flatten)]
        transition: TransitionArgs,
        /// Which canvas: main, or vertical (once it stops following the
        /// main one).
        #[arg(long, value_parser = parse_canvas, default_value = "main")]
        canvas: CanvasId,
        #[arg(long)]
        json: bool,
    },
    /// Check this Mac and a setup: permissions, hardware encoders, whether
    /// the setup's devices are connected, and the recording disk's space and
    /// speed. Exits with an error if anything would stop a recording.
    #[cfg_attr(
        windows,
        command(
            about = "Check this PC and a setup: permissions, hardware encoders, connected devices and recording disk space/speed. Exits with an error if anything would stop a recording."
        )
    )]
    Doctor {
        setup: Option<PathBuf>,
        /// Check this folder instead of the setup's recording.folder.
        #[arg(long)]
        folder: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Measure this machine quietly, inspect its profile, or cancel calibration.
    Machine {
        #[command(subcommand)]
        command: MachineCommand,
    },
    /// Benchmark the encoders on a setup's files (the live setup if none is
    /// given), all at once as in a recording, and say whether this Mac keeps
    /// up (fits, keeps up with no margin, or can't keep up). If it doesn't
    /// fit, try changes closest first (a smaller program file, a lower
    /// quality preset, half the frame rate, one ISO file fewer) and
    /// recommend the closest that fits, as a patch for `plan` and `apply`.
    /// `start` warns when a setup exceeds what this measured. Runs in
    /// isocastd.
    #[cfg_attr(
        windows,
        command(
            about = "Benchmark this PC's encoders on a setup's files (the live setup if absent). Recommend the closest changes that fit as a patch for plan/apply. Runs in isocastd.exe; start warns when the setup exceeds the measurement."
        )
    )]
    Tune {
        setup: Option<PathBuf>,
        /// Benchmark writing to this folder instead of recording.folder.
        #[arg(long)]
        folder: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Predict, instantly and without benchmarking, whether a setup's files
    /// (the live setup's, at its sources' live formats, if none is given)
    /// fit this Mac's video encoders, from measured per-chip data, and list
    /// the closest changes that fit as patches for `plan` and `apply`.
    /// `tune` measures instead. Runs in isocastd.
    #[cfg_attr(
        windows,
        command(
            about = "Predict whether a setup's files fit this PC's video encoders, without benchmarking, and list the closest changes that fit. tune measures instead. Runs in isocastd.exe."
        )
    )]
    Capacity {
        setup: Option<PathBuf>,
        /// Count the stream's encode (default: the live setup's if it
        /// streams now; a given setup's if it has a stream).
        #[arg(long, conflicts_with = "no_stream")]
        stream: bool,
        /// Leave the stream's encode out.
        #[arg(long)]
        no_stream: bool,
        /// How to turn on this source's ISO file so the setup still fits.
        #[arg(long, value_name = "SOURCE")]
        add_iso: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Optimize the live setup for recording, streaming or both, like
    /// OBS's Auto-Configuration Wizard: recording favours quality and ISO
    /// files; streaming sets the service and a bitrate from the upload
    /// speed (tested unless --upload-kbps is given; a custom server only
    /// with `isocast stream speed-test`). Then `tune` benchmarks
    /// this Mac and steps down where it can't keep up. Prints a one-line
    /// summary and the changes; --apply makes them. Runs in isocastd.
    #[cfg_attr(
        windows,
        command(
            about = "Optimize recording, streaming or both: choose settings, benchmark this PC and step down where needed. Prints a summary and changes; --apply makes them. Runs in isocastd.exe."
        )
    )]
    Optimize {
        /// recording, streaming or both.
        #[arg(value_parser = optimize::parse_goal)]
        goal: isocast_protocol::optimize::Goal,
        /// Streaming: custom, twitch, youtube, kick or facebook; the live
        /// setup's if absent.
        #[arg(long, value_parser = parse_service)]
        service: Option<StreamService>,
        /// Streaming: the ingest server, rtmp://host/app or rtmps://…
        #[arg(long)]
        server: Option<String>,
        /// Streaming: the upload speed in kb/s, instead of testing it.
        #[arg(long)]
        upload_kbps: Option<u32>,
        /// Benchmark writing to this folder instead of recording.folder.
        #[arg(long)]
        folder: Option<PathBuf>,
        /// Make the changes.
        #[arg(long)]
        apply: bool,
        #[arg(long)]
        json: bool,
    },
    /// Save one frame as a PNG: of a video source (an id of the live setup,
    /// or a quick source like `screen` or `camera:NAME`), of --scene ID, of
    /// --preview, or with none of them the program (while recording, the
    /// exact frame it recorded last). Runs in isocastd.
    Snapshot {
        source: Option<String>,
        #[arg(long, conflicts_with = "source")]
        scene: Option<String>,
        /// The scene in preview.
        #[arg(long, conflicts_with_all = ["source", "scene"])]
        preview: bool,
        /// Where to save it; `<source or scene id>.png` if absent.
        #[arg(long, short)]
        output: Option<PathBuf>,
        /// Scale it down to at most this many pixels wide.
        #[arg(long)]
        width: Option<u32>,
        #[arg(long)]
        json: bool,
    },
    /// Change an audio source's program mix in the live setup: gain in dB,
    /// mute, sync offset in ms (positive is later), balance (-1 left to 1
    /// right) and monitoring (hearing it through `audio.monitoring_device`).
    /// A running recording follows all but the offset at once.
    Audio {
        source: String,
        /// Downmix to mono (before balance) in the program and monitor.
        #[arg(long, conflicts_with = "stereo")]
        mono: bool,
        /// Keep the source's stereo channels in the program and monitor.
        #[arg(long)]
        stereo: bool,
        #[arg(long, allow_hyphen_values = true)]
        gain: Option<f64>,
        #[arg(long, conflicts_with = "unmute")]
        mute: bool,
        #[arg(long)]
        unmute: bool,
        #[arg(long, allow_hyphen_values = true)]
        offset: Option<i32>,
        #[arg(long, allow_hyphen_values = true)]
        balance: Option<f64>,
        /// off, monitor_only (heard, left out of the program mix) or
        /// monitor_and_output.
        #[arg(long, value_parser = parse_monitoring)]
        monitor: Option<isocast_protocol::Monitoring>,
        #[arg(long)]
        json: bool,
    },
    /// Choose the audio sources an ISO file carries, in the live setup:
    /// `isocast iso-audio camera mic` gives the camera's ISO file the mic
    /// only, --none no audio, --default back to recording.iso.audio (or,
    /// without it, the source's own sound). Without a source, list each
    /// video source's ISO tracks.
    IsoAudio {
        /// A video source's id.
        source: Option<String>,
        /// Audio source ids, in track order.
        #[arg(requires = "source")]
        audio: Vec<String>,
        #[arg(long, requires = "source", conflicts_with_all = ["audio", "none"])]
        default: bool,
        #[arg(long, requires = "source", conflicts_with = "audio")]
        none: bool,
        #[arg(long)]
        json: bool,
    },
    /// Restart isocastd, for example after granting it screen recording
    /// permission. Refused, atomically by isocastd, while it records,
    /// streams, exports or feeds the virtual camera.
    Restart,
    /// Show the screen recording, camera, microphone and system audio
    /// recording permissions and what to do about each that isn't granted; with --request, ask macOS
    /// for the ones not decided yet (its prompts appear; --request camera
    /// asks for that one only) and wait up to a minute for the answers;
    /// with --open, open the System Settings pane where one is turned on.
    #[cfg_attr(
        windows,
        command(
            about = "Report Windows capture, camera, microphone and system audio access. --request checks/requests access supported by the backend; --open opens its Windows Settings page. Camera and Microphone must allow desktop apps (isocastd.exe), not an individual app switch."
        )
    )]
    #[cfg_attr(
        target_os = "linux",
        command(
            about = "Report what isocastd can capture on Linux. The screen is chosen in the desktop's screen-sharing dialog when capture starts (--request screen_recording lets a dismissed dialog ask again); cameras are /dev/video* devices your user must be able to open; PipeWire audio needs no permission. --open isn't available on Linux."
        )
    )]
    Permissions {
        /// screen_recording, camera, microphone or system_audio; all if
        /// left out.
        #[arg(
            long,
            conflicts_with = "open",
            value_name = "PERMISSION",
            num_args = 0..=1,
            value_parser = parse_permission
        )]
        request: Option<Option<isocast_protocol::devices::PermissionKind>>,
        /// screen_recording, camera, microphone or system_audio.
        #[arg(long, value_name = "PERMISSION", value_parser = parse_permission)]
        open: Option<isocast_protocol::devices::PermissionKind>,
        #[arg(long)]
        json: bool,
    },
    /// Print the live setup (TOML, or JSON with --json) and its revision;
    /// with --resolved, also where layout put every item (each item's rect
    /// in its parent and its box on the canvas).
    Get {
        #[arg(long)]
        resolved: bool,
        /// With --resolved: whose layout, main or vertical (the setup is
        /// the whole live setup either way).
        #[arg(long, value_parser = parse_canvas, default_value = "main")]
        canvas: CanvasId,
        #[arg(long)]
        json: bool,
    },
    /// Show what a change to the live setup would do, without doing it:
    /// a whole setup FILE, or --patch with a JSON merge patch (lists of
    /// sources, scenes and items merge by id).
    Plan {
        file: Option<PathBuf>,
        #[arg(long)]
        patch: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Make a change to the live setup, if it is still at --revision (from
    /// `plan` or `get`; 0 when there is no live setup yet).
    Apply {
        file: Option<PathBuf>,
        #[arg(long)]
        patch: Option<String>,
        #[arg(long)]
        revision: u64,
        /// One step of a gesture: apply at once and save to disk a moment
        /// later (for scripts sending many changes a second).
        #[arg(long)]
        live: bool,
        #[arg(long)]
        json: bool,
    },
    /// Stream the program live over RTMP or RTMPS to the live setup's
    /// [stream] service, and keep its stream key in the Keychain.
    #[cfg_attr(
        windows,
        command(
            about = "Stream the program live over RTMP or RTMPS; keep its stream key in Credential Manager."
        )
    )]
    #[cfg_attr(
        target_os = "linux",
        command(
            about = "Stream the program live over RTMP or RTMPS; keep its stream key in an owner-only 0600 file."
        )
    )]
    Stream {
        #[command(subcommand)]
        command: StreamCommand,
    },
    /// Canvases besides the main one (#84): the vertical canvas and
    /// `[[canvases]]`, each with its own size, frame rate, scene (or the
    /// main one's), recording and stream destinations.
    Canvas {
        #[command(subcommand)]
        command: CanvasCommand,
    },
    /// The replay buffer: keep the last seconds of the program in memory,
    /// and save them to a file on demand.
    Replay {
        #[command(subcommand)]
        command: replay::Command,
    },
    /// The obs-websocket v5 server, so OBS tools (Stream Deck, Touch
    /// Portal, Streamer.bot, scripts) drive Isocast. Off until enabled.
    Websocket {
        #[command(subcommand)]
        command: websocket::Command,
    },
    /// The folders other computers can use (remote control and
    /// obs-websocket clients): see and, on this machine, choose them.
    Roots {
        #[command(subcommand)]
        command: roots::Command,
    },
    /// Browse the engine's disk (names, sizes and dates only), as the
    /// app's engine file pickers do: on another computer, only inside the
    /// folders other computers can use.
    Files {
        #[command(subcommand)]
        command: roots::FilesCommand,
    },
    /// Projectors: the program, the preview, a scene, a source or a
    /// multiview grid, full screen on a display or in a window (the app
    /// shows them).
    Projector {
        #[command(subcommand)]
        command: projector::Command,
    },
    /// Feed a picture to Isocast Virtual Camera (extension approval required once).
    Vcam {
        #[command(subcommand)]
        command: virtual_camera::Command,
    },
    /// The engine API itself: its methods, params and results.
    Api {
        #[command(subcommand)]
        command: ApiCommand,
    },
    /// Developer mode, for testing a build locally: verbose logs of every
    /// API call and component, and frame timing traces while recording.
    Debug {
        #[command(subcommand)]
        command: debug::Command,
    },
    /// Print events from isocastd as they happen, until Ctrl-C.
    Watch {
        /// Also show audio levels (about 20 a second while the live engine
        /// runs); isocastd doesn't send them otherwise.
        #[arg(long)]
        levels: bool,
        /// Only these kinds of events, like `scene_changed,stopped`
        /// (setup_changed, started, status, warning, scene_changed,
        /// preview_changed, marker, stream, stopped, levels,
        /// source_changed, engine_changed, library_changed,
        /// plugin_changed, data_changed, take_finished, …).
        #[arg(long, value_delimiter = ',', value_name = "KINDS")]
        kinds: Vec<String>,
        /// Leave out these kinds of events, like `status,source_changed`.
        #[arg(long, value_delimiter = ',', value_name = "KINDS")]
        except: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Save the live engine's newest frame of the program (the default),
    /// studio mode's preview, or a video source as a JPEG. Starts the engine
    /// if it isn't running.
    Frame {
        /// `program`, `preview`, `scene:ID` or a source id.
        #[arg(default_value = "program")]
        target: String,
        /// Where to save it; `<target>.jpg` if absent.
        #[arg(long, short)]
        output: Option<PathBuf>,
        /// Scale it down to at most this many pixels wide.
        #[arg(long)]
        width: Option<u32>,
        /// Which canvas: main, or vertical (its program; vertical takes
        /// `program` only).
        #[arg(long, value_parser = parse_canvas, default_value = "main")]
        canvas: CanvasId,
        #[arg(long)]
        json: bool,
    },
    /// Record what a monitor shows: the live engine's low-latency H.264
    /// stream of the program (the default), studio mode's preview or a
    /// video source, saved as an H.264 elementary stream (Annex B, which
    /// ffprobe and ffplay read). Starts the engine if it isn't running.
    Frames {
        /// `program`, `preview`, `scene:ID` or a source id.
        #[arg(default_value = "program")]
        target: String,
        /// Where to save it; `<target>.h264` if absent.
        #[arg(long, short, conflicts_with = "shared")]
        output: Option<PathBuf>,
        /// Scale it down to at most this many pixels wide.
        #[arg(long)]
        width: Option<u32>,
        /// With --shared, scale it down to at most this many pixels high.
        #[arg(long, requires = "shared")]
        height: Option<u32>,
        /// The most frames a second (30 if absent, at most 60).
        #[arg(long)]
        fps: Option<u32>,
        /// The H.264 profile: high (the default) or constrained_baseline.
        #[arg(long, value_parser = parse_h264_profile, conflicts_with = "shared")]
        profile: Option<isocast_protocol::api::H264Profile>,
        /// Stop after this long (5 s if absent).
        #[arg(long, default_value_t = 5.0)]
        seconds: f64,
        /// Watch through GPU surfaces shared with the daemon (zero-copy,
        /// as the native app does) instead of saving H.264: reports the
        /// rate, how late frames arrive and the setup revisions they show.
        #[arg(long)]
        shared: bool,
        /// With --shared: surfaces in the ring (4 if absent, 2 to 8).
        #[arg(long, requires = "shared")]
        slots: Option<u32>,
        /// With --shared and a source: through the source's filters, as
        /// the program shows it.
        #[arg(long, requires = "shared")]
        filtered: bool,
        /// With --shared: read every frame back through this process's GPU
        /// and fail on black frames or pixels the daemon never drew.
        #[arg(long, requires = "shared")]
        check: bool,
        /// With --check: a pixel some frame must have, as X,Y=#RRGGBB (X
        /// and Y fractions of the width and height). Repeatable.
        #[arg(long, requires = "check")]
        expect: Vec<String>,
        /// With --check: halfway through, apply an empty live change (the
        /// setup stays the same, its revision goes up) and check frames
        /// show the new revision.
        #[arg(long, requires = "check")]
        touch: bool,
        /// Which canvas: main, or vertical (its program; vertical takes
        /// `program` only).
        #[arg(long, value_parser = parse_canvas, default_value = "main")]
        canvas: CanvasId,
        #[arg(long)]
        json: bool,
    },
    /// Keep the live engine running (`start`), or let it stop once nothing
    /// needs it (`stop`). It runs the live setup's sources, program and mix
    /// all the time, so recording starts at once; it also runs while
    /// recording or while a client shows frames.
    Engine {
        #[command(subcommand)]
        command: EngineCommand,
        #[arg(long, global = true)]
        json: bool,
    },
    /// Paired engines and engines advertising on the local network. Discovery
    /// is an unverified hint; the saved TLS pin establishes identity.
    Engines {
        /// Find engines on the local network.
        #[arg(long)]
        discover: bool,
        /// How long to listen for answers, in seconds (Windows confirms
        /// each engine with a fresh query, which can take a few seconds).
        #[arg(long, default_value_t = 5.0)]
        timeout: f64,
        /// With --discover: keep listening and print engines as they
        /// appear and go away, until Ctrl-C (JSON lines with --json).
        #[arg(long)]
        watch: bool,
        #[arg(long)]
        json: bool,
    },
    /// Serve the control API to agents over MCP, on stdin and stdout.
    Mcp,
}

#[derive(Subcommand)]
enum StreamCommand {
    /// Go live: stream the program (the canvas and the program mix) to the
    /// live setup's [stream] destinations (every enabled one), starting the
    /// live engine if needed. Recordings start and stop without touching
    /// it. Returns once each is live or failed, or says why not.
    Start {
        /// Only this destination (main, or an id from `stream status`);
        /// repeat for several. A turned-off destination starts too.
        #[arg(long = "only", value_name = "ID")]
        only: Vec<String>,
        /// Which canvas: main ([stream]), or vertical ([vertical.stream],
        /// with its own destinations and keys).
        #[arg(long, value_parser = parse_canvas, default_value = "main")]
        canvas: CanvasId,
        #[arg(long)]
        json: bool,
    },
    /// Stop streaming (to every destination of the canvas, or --only
    /// those); the other canvas's streams keep going.
    Stop {
        /// Only this destination; repeat for several. The others keep going.
        #[arg(long = "only", value_name = "ID")]
        only: Vec<String>,
        /// Which canvas: main ([stream]), or vertical ([vertical.stream],
        /// with its own destinations and keys).
        #[arg(long, value_parser = parse_canvas, default_value = "main")]
        canvas: CanvasId,
        #[arg(long)]
        json: bool,
    },
    /// Show each destination's state, bitrates, dropped frames, congestion
    /// and reconnects, and whether its stream key is set.
    Status {
        /// Which canvas: main ([stream]), or vertical ([vertical.stream],
        /// with its own destinations and keys).
        #[arg(long, value_parser = parse_canvas, default_value = "main")]
        canvas: CanvasId,
        #[arg(long)]
        json: bool,
    },
    /// Add, remove or change the places the program streams to.
    Destination {
        #[command(subcommand)]
        command: DestinationCommand,
    },
    /// Keep or remove the stream key (in the Keychain, never in a setup).
    #[cfg_attr(
        windows,
        command(about = "Keep or remove the stream key in Credential Manager, never in a setup.")
    )]
    #[cfg_attr(
        target_os = "linux",
        command(
            about = "Keep or remove the stream key in an owner-only 0600 file, never in a setup."
        )
    )]
    Key {
        #[command(subcommand)]
        command: StreamKeyCommand,
    },
    /// Measure the upload speed to the service's ingest (a short publish
    /// of filler: Twitch's bandwidth test mode, or a custom server, which
    /// shows it for a few seconds if it relays to a live service) and
    /// recommend a stream bitrate. YouTube, Facebook and Kick have no test
    /// mode, so they get a safe bitrate instead, with the reason.
    SpeedTest {
        #[command(flatten)]
        target: KeyTargetArgs,
        /// Seconds to send for, 3 to 20 (8 by default).
        #[arg(long)]
        seconds: Option<u32>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum CanvasCommand {
    /// Every canvas: size, frame rate, scene, recording and streams.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Add a canvas: `vertical` (the [vertical] canvas) or a new id
    /// (lowercase letters, digits and dashes), 1080×1920 following the main
    /// scene unless said otherwise. Add its streams next: `isocast stream
    /// destination add SERVICE --canvas ID`.
    Add {
        canvas: String,
        #[command(flatten)]
        fields: CanvasFields,
        #[arg(long)]
        json: bool,
    },
    /// Change a canvas.
    Set {
        canvas: String,
        #[command(flatten)]
        fields: CanvasFields,
        /// Reset a field: name, fps (to the main canvas's) or stream (no
        /// stream of its own). Repeat for several.
        #[arg(long, value_name = "FIELD")]
        clear: Vec<String>,
        /// Shorthand for --clear stream.
        #[arg(long)]
        no_stream: bool,
        #[arg(long)]
        json: bool,
    },
    /// Remove a canvas with its arrangements, recording and streams (not
    /// while it records or streams).
    Remove {
        canvas: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(clap::Args)]
struct CanvasFields {
    /// Display label, like "YouTube Shorts".
    #[arg(long)]
    name: Option<String>,
    /// Size as WIDTHxHEIGHT, optionally @FPS (1080x1920, 1080x1080@30).
    #[arg(long)]
    size: Option<String>,
    /// Its own frame rate (else the main canvas's).
    #[arg(long)]
    fps: Option<u32>,
    /// Show the main canvas's scene (true) or pick its own (false).
    #[arg(long)]
    follow_main: Option<bool>,
    /// Record its program with each recording (true or false).
    #[arg(long)]
    record: Option<bool>,
}

impl CanvasFields {
    fn params(self, canvas: &str) -> Result<isocast_protocol::api::CanvasParams> {
        let canvas = CanvasId::parse(canvas).map_err(anyhow::Error::msg)?;
        let (width, height, fps) = parse_canvas_size(self.size.as_deref())?;
        Ok(isocast_protocol::api::CanvasParams {
            canvas,
            name: self.name,
            width,
            height,
            fps: self.fps.or(fps),
            follow_main: self.follow_main,
            record: self.record,
            clear: Vec::new(),
        })
    }
}

/// What `canvas list` and the changes print.
fn print_canvases(
    canvases: &[isocast_protocol::api::CanvasStatus],
    main: Option<&isocast_protocol::Canvas>,
) {
    if let Some(main) = main {
        println!(
            "main      {}×{} at {} fps",
            main.width, main.height, main.fps
        );
    }
    if canvases.is_empty() {
        println!("no other canvases; add one with `isocast canvas add ID --size 1080x1920`");
    }
    for canvas in canvases {
        let live = canvas.streams.iter().filter(|s| s.is_active()).count();
        println!(
            "{:<9} {}×{} at {} fps, {}{}{}{}",
            canvas.id.as_str(),
            canvas.canvas.width,
            canvas.canvas.height,
            canvas.canvas.fps,
            if canvas.follow_main {
                "follows main".to_string()
            } else {
                format!("own scene {}", canvas.scene.as_deref().unwrap_or("-"))
            },
            if canvas.recorded { ", recorded" } else { "" },
            match canvas.streams.len() {
                0 => String::new(),
                n => format!(", {n} destination(s), {live} live"),
            },
            if canvas.name != canvas.id.as_str() {
                format!(" ({})", canvas.name)
            } else {
                String::new()
            }
        );
    }
}

#[derive(Subcommand)]
enum DestinationCommand {
    /// Add a destination (the first makes the setup's [stream] section,
    /// id main). Set its key next: `isocast stream key set --destination ID`.
    Add {
        /// custom, twitch, youtube, kick or facebook.
        #[arg(value_parser = parse_service)]
        service: StreamService,
        /// Its id; made from the service if absent (youtube, youtube-2).
        #[arg(long)]
        id: Option<String>,
        #[command(flatten)]
        fields: DestinationFields,
        /// Add it turned off (`stream start` skips it unless --only names it).
        #[arg(long)]
        disabled: bool,
        /// Which canvas: main ([stream]), or vertical ([vertical.stream],
        /// with its own destinations and keys).
        #[arg(long, value_parser = parse_canvas, default_value = "main")]
        canvas: CanvasId,
        #[arg(long)]
        json: bool,
    },
    /// Remove a destination (not main; not while it streams).
    Remove {
        id: String,
        /// Which canvas: main ([stream]), or vertical ([vertical.stream],
        /// with its own destinations and keys).
        #[arg(long, value_parser = parse_canvas, default_value = "main")]
        canvas: CanvasId,
        #[arg(long)]
        json: bool,
    },
    /// Change a destination (main is the [stream] section itself).
    Set {
        id: String,
        #[arg(long, value_parser = parse_service)]
        service: Option<StreamService>,
        #[command(flatten)]
        fields: DestinationFields,
        /// Turn it on or off for `stream start`.
        #[arg(long)]
        enabled: Option<bool>,
        /// Reset a field: server, video_bitrate_kbps, size, rate_control,
        /// keyframe_interval_seconds or audio_bitrate_kbps (a destination
        /// then uses main's; main the defaults). Repeat for several.
        #[arg(long, value_name = "FIELD")]
        clear: Vec<String>,
        /// Which canvas: main ([stream]), or vertical ([vertical.stream],
        /// with its own destinations and keys).
        #[arg(long, value_parser = parse_canvas, default_value = "main")]
        canvas: CanvasId,
        #[arg(long)]
        json: bool,
    },
}

#[derive(clap::Args)]
struct DestinationFields {
    /// The ingest server without the key (rtmp://host/app or rtmps://…);
    /// required for custom and kick.
    #[arg(long)]
    server: Option<String>,
    /// Its own video bitrate in kb/s (else main's). A bitrate or size of
    /// its own needs an encoder of its own.
    #[arg(long)]
    video_bitrate_kbps: Option<u32>,
    /// Its own size sent (with --height; else main's).
    #[arg(long, requires = "height")]
    width: Option<u32>,
    /// Its own height sent (with --width).
    #[arg(long, requires = "width")]
    height: Option<u32>,
    /// Its own rate control: constant or average (else main's).
    #[arg(long, value_parser = parse_rate_control)]
    rate_control: Option<isocast_protocol::stream::RateControl>,
    /// Its own seconds between keyframes (else main's).
    #[arg(long)]
    keyframe_interval_seconds: Option<u32>,
    /// Its own AAC bitrate in kb/s (else main's).
    #[arg(long)]
    audio_bitrate_kbps: Option<u32>,
}

#[derive(Subcommand)]
enum StreamKeyCommand {
    /// Store the stream key for the live setup's service (or --service).
    /// Reads it from stdin, or asks for it without echoing it in a
    /// terminal.
    Set {
        #[command(flatten)]
        target: KeyTargetArgs,
        /// Which canvas: main ([stream]), or vertical ([vertical.stream],
        /// with its own destinations and keys).
        #[arg(long, value_parser = parse_canvas, default_value = "main")]
        canvas: CanvasId,
        #[arg(long)]
        json: bool,
    },
    /// Remove the stored stream key for the live setup's service (or
    /// --service).
    Clear {
        #[command(flatten)]
        target: KeyTargetArgs,
        /// Which canvas: main ([stream]), or vertical ([vertical.stream],
        /// with its own destinations and keys).
        #[arg(long, value_parser = parse_canvas, default_value = "main")]
        canvas: CanvasId,
        #[arg(long)]
        json: bool,
    },
}

/// Which service's key; the live setup's [stream] if absent.
#[derive(clap::Args)]
struct KeyTargetArgs {
    /// A destination of the live setup (main, or an id from `stream
    /// status`): its own key.
    #[arg(long)]
    destination: Option<String>,
    /// custom, twitch, youtube, kick or facebook.
    #[arg(long, value_parser = parse_service)]
    service: Option<StreamService>,
    /// For custom: the server the key belongs to (rtmp://host/app).
    #[arg(long)]
    server: Option<String>,
}

impl KeyTargetArgs {
    fn target(self, canvas: CanvasId) -> StreamKeyTarget {
        StreamKeyTarget {
            destination: self.destination,
            service: self.service,
            server: self.server,
            canvas,
            revision: None,
        }
    }
}

fn parse_service(text: &str) -> Result<StreamService, String> {
    StreamService::from_id(text)
}

/// A canvas, as `--canvas` names it: main, vertical or a `[[canvases]]`
/// id (whether the live setup has it, the engine says).
fn parse_canvas(text: &str) -> Result<CanvasId, String> {
    CanvasId::parse(text)
}

fn parse_rate_control(text: &str) -> Result<isocast_protocol::stream::RateControl, String> {
    serde_json::from_value(serde_json::Value::String(text.to_string()))
        .map_err(|_| format!("{text:?} is not a rate control; use constant or average"))
}

/// The monitor target `--canvas` means: the target itself on the main
/// canvas; on the vertical one, its program or preview. Sources are shared.
fn canvas_target(canvas: CanvasId, target: String) -> Result<String> {
    if canvas.is_main() || target == canvas.program_target() || target == canvas.preview_target() {
        return Ok(target);
    }
    if target == isocast_protocol::PROGRAM {
        return Ok(canvas.program_target().to_string());
    }
    if target == "preview" {
        return Ok(canvas.preview_target());
    }
    bail!(
        "--canvas {canvas}: give `program` (the default) or `preview`, or watch source {target:?} without --canvas"
    )
}

#[derive(Subcommand)]
enum ApiCommand {
    /// Describe this build's method table as an OpenRPC 1.3.2 document
    /// (JSON Schema draft-07): every method's params and result. Without
    /// --json, list each method and what it returns. Needs no engine.
    Spec {
        /// Print the OpenRPC document.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum SetupCommand {
    /// Validate a setup file (.toml or .json) and summarize it.
    Check {
        path: PathBuf,
        /// Print the result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print the JSON Schema of the setup document.
    Schema,
    /// Make a good starting setup from this Mac's devices: the main
    /// display, the best camera and the default microphone, with desktop
    /// audio, in scenes "Screen + Camera", "Camera" and "Screen", recording
    /// the program and ISO files of the screen and camera (in isocastd).
    /// Prints it as TOML, writes it to --output, or makes it the live setup
    /// with --apply.
    #[cfg_attr(
        windows,
        command(
            about = "Make a starting setup from this PC's display, camera, microphone and desktop audio, with program and ISO recording. Print TOML, write --output, or make it live with --apply. Runs in isocastd.exe."
        )
    )]
    #[cfg_attr(
        target_os = "linux",
        command(
            about = "Make a starting setup from this computer's display, camera, microphone and desktop audio, with program and ISO recording. Print TOML, write --output, or make it live with --apply. Runs in isocastd; the desktop asks which screen to share when capture starts."
        )
    )]
    Init {
        /// Write the setup to this file (it must not exist yet).
        #[arg(long, short)]
        output: Option<PathBuf>,
        /// Make it the live setup, replacing any (kept in the setup file's
        /// .bak).
        #[arg(long)]
        apply: bool,
        /// The display: "main" (the default), or an id or name from
        /// `isocast devices`.
        #[arg(long)]
        display: Option<String>,
        /// The camera: an id or (part of a) name from `isocast devices`, or
        /// "none". The best real camera if absent.
        #[arg(long)]
        camera: Option<String>,
        /// The microphone: "default" (the default), an id or (part of a)
        /// name, or "none".
        #[arg(long)]
        microphone: Option<String>,
        /// Leave desktop (system) audio out of the program.
        #[arg(long)]
        no_system_audio: bool,
        /// Where the camera goes in "Screen + Camera": bottom_right (the
        /// default), bottom_left, top_right, top_left or side_by_side. A
        /// tall canvas stacks the screen and the camera.
        #[arg(long, value_parser = parse_layout)]
        layout: Option<isocast_protocol::init::Layout>,
        /// The canvas, WIDTHxHEIGHT or WIDTHxHEIGHT@FPS; 1920x1080@60 by
        /// default.
        #[arg(long, value_name = "WxH[@FPS]")]
        canvas: Option<String>,
        /// Recording folder; ~/Movies/Isocast by default.
        #[cfg_attr(
            not(target_os = "macos"),
            arg(help = "Recording folder; ~/Videos/Isocast by default.")
        )]
        #[arg(long)]
        folder: Option<String>,
        /// The setup's name.
        #[arg(long)]
        name: Option<String>,
        /// Print the result (setup, notes, applied change) as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Find OBS Studio's scene collections and profiles on this Mac (in
    /// ~/Library/Application Support/obs-studio, or --folder), without
    /// importing anything: each collection with the profile an import
    /// would use, and each profile's video, recording and stream settings.
    #[cfg_attr(
        windows,
        command(
            about = "Find OBS Studio's scene collections and profiles on this PC (%APPDATA%/obs-studio, or --folder), without importing anything. Reports collections and their profile's video, recording and stream settings."
        )
    )]
    #[cfg_attr(
        target_os = "linux",
        command(
            about = "Find OBS Studio's scene collections and profiles on this computer ($XDG_CONFIG_HOME/obs-studio, ~/.config/obs-studio by default, or --folder), without importing anything. Reports collections and their profile's video, recording and stream settings."
        )
    )]
    DiscoverObs {
        /// Look in this folder instead.
        #[arg(long)]
        folder: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Import an OBS Studio scene collection as a setup: its sources,
    /// scenes and items, filters and transition, matched against this
    /// Mac's devices (in isocastd). Prints the setup as TOML, or writes it
    /// to --output, and lists what couldn't come over.
    #[cfg_attr(
        windows,
        command(
            about = "Import an OBS scene collection, matching sources against this PC's devices in isocastd.exe. Print the setup as TOML or write --output, and list what could not come over."
        )
    )]
    #[cfg_attr(
        target_os = "linux",
        command(
            about = "Import an OBS scene collection, matching sources against this computer's devices in isocastd. Print the setup as TOML or write --output, and list what could not come over."
        )
    )]
    ImportObs {
        /// A collection's .json file, or its name in OBS's scenes folder
        /// (like "Recording").
        collection: String,
        /// Write the setup to this file (it must not exist yet).
        #[arg(long, short)]
        output: Option<PathBuf>,
        /// Import onto this canvas, WIDTHxHEIGHT or WIDTHxHEIGHT@FPS,
        /// scaling positions and sizes; the collection's own canvas and the
        /// profile's frame rate otherwise.
        #[arg(long, value_name = "WxH[@FPS]")]
        canvas: Option<String>,
        /// The OBS profile with the frame rate and recording settings: a
        /// name or a basic.ini path. Found automatically if absent.
        #[arg(long)]
        profile: Option<String>,
        /// Print the result (setup, what wasn't imported, notes) as JSON.
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    let _logs = isocast_diagnostics::init("isocast", true);
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let _ = error.print();
            return ExitCode::from(error.exit_code() as u8);
        }
    };
    // `isocast get | head` should end quietly when the reader goes away, as
    // other tools do, not panic on the closed pipe. Not for foreground
    // recording (wait for its files to finish) or the MCP server.
    #[cfg(unix)]
    if !matches!(cli.command, Command::Record { .. } | Command::Mcp) {
        // SAFETY: setting a signal's disposition before any threads start.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    }
    match run(cli) {
        Ok(code) => code,
        Err(err) => {
            isocast_diagnostics::diagnostic!("error"; "error: {err:#}");
            // A daemon from an older build doesn't know newer methods (it
            // says "unknown variant `library`").
            if err.downcast_ref::<DaemonError>().is_some_and(|err| {
                err.code() == RpcError::METHOD_NOT_FOUND
                    || err.code() == RpcError::INVALID_REQUEST
                        && err.to_string().contains("unknown variant")
            }) {
                isocast_diagnostics::diagnostic!(
                    "isocastd is older than this isocast and doesn't have that; restart it to run \
                     this build's (`isocast restart`)"
                );
            }
            ExitCode::FAILURE
        }
    }
}

fn api_spec(json: bool) -> Result<ExitCode> {
    let doc = isocast_protocol::openrpc::document();
    if json {
        println!("{}", serde_json::to_string_pretty(&doc)?);
        return Ok(ExitCode::SUCCESS);
    }
    let methods = doc["methods"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    for method in methods {
        println!(
            "{:28} {}",
            method["name"].as_str().unwrap_or_default(),
            method["result"]["name"].as_str().unwrap_or("(untyped)")
        );
    }
    println!(
        "{} methods, OpenRPC {}; `isocast api spec --json` prints the document.",
        methods.len(),
        isocast_protocol::openrpc::OPENRPC_VERSION
    );
    Ok(ExitCode::SUCCESS)
}

fn run(cli: Cli) -> Result<ExitCode> {
    let target = match cli.engine {
        Some(target) => isocast_client::Engine::parse(&target)?,
        None => isocast_client::Engine::selected()?,
    };
    if !target.is_local()
        && matches!(
            &cli.command,
            Command::Restart
                | Command::Logs { .. }
                | Command::Record { .. }
                | Command::Setup {
                    command: SetupCommand::Check { .. }
                }
        )
    {
        bail!(
            "this command is local-only and cannot run with --engine/ISOCAST_ENGINE; run it on the engine machine, or explicitly select --engine local"
        );
    }
    isocast_client::Engine::select(target)?;
    match cli.command {
        Command::Remote(args) => {
            remote::run(args)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::ReportFile {
            preview_id,
            file,
            offset,
            limit,
            json,
        } => {
            let page: isocast_protocol::diagnostics::ReportFile = Client::connect_or_launch()?
                .call(Method::ReportFile(
                    isocast_protocol::diagnostics::ReportFileParams {
                        preview_id,
                        file,
                        offset,
                        limit,
                    },
                ))?;
            if json {
                println!("{}", serde_json::to_string(&page)?);
            } else {
                print!("{}", page.content);
                if let Some(offset) = page.next_offset {
                    println!("\nNext offset: {offset}");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Report {
            out,
            describe,
            preview,
            hash_paths,
            launches,
            submit,
            preview_id,
            json,
        } => {
            let params = isocast_protocol::diagnostics::BugReportParams {
                out: out
                    .map(|p| {
                        if p.starts_with("~") {
                            Ok(p)
                        } else {
                            std::path::absolute(p)
                        }
                    })
                    .transpose()?,
                describe,
                preview,
                hash_paths,
                launches,
                submit,
                preview_id,
            };
            let report: isocast_protocol::diagnostics::BugReport =
                Client::connect_or_launch()?.call(Method::BugReport(params))?;
            if json || preview {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else if let Some(path) = &report.archive {
                println!("{}\n{}", path.display(), report.issue_body);
                if let Some(sent) = &report.submitted {
                    println!(
                        "Sent: {}{} (report {})",
                        sent.issue_url,
                        if sent.deduplicated {
                            ", added to an open issue with the same crash"
                        } else {
                            ""
                        },
                        sent.report_id
                    );
                }
            }
            if let Some(why) = &report.submit_error {
                eprintln!(
                    "isocast: report not sent: {why}. The zip is kept at {}; attach it to a new GitHub issue by hand.",
                    report
                        .archive
                        .as_deref()
                        .map_or("?".into(), |p| p.display().to_string())
                );
                return Ok(ExitCode::FAILURE);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Logs {
            follow,
            level,
            component,
            json,
        } => {
            let stopped = Arc::new(AtomicBool::new(false));
            if follow {
                let stopped = stopped.clone();
                ctrlc::set_handler(move || stopped.store(true, Ordering::Relaxed))?;
            }
            let mut params = isocast_protocol::diagnostics::LogsParams {
                level,
                component,
                ..Default::default()
            };
            loop {
                let logs =
                    isocast_diagnostics::bundle::logs(&params).map_err(anyhow::Error::msg)?;
                for record in logs.records {
                    if json {
                        println!("{record}");
                    } else {
                        println!(
                            "{} {:5} {} [{}] {} {}",
                            record["time"].as_str().unwrap_or(""),
                            record["level"].as_str().unwrap_or(""),
                            record["process"].as_str().unwrap_or(""),
                            record["component"].as_str().unwrap_or(""),
                            record["message"].as_str().unwrap_or(""),
                            record["fields"]
                        );
                    }
                }
                if !follow || stopped.load(Ordering::Relaxed) {
                    break;
                }
                params.cursor = Some(logs.cursor);
                std::thread::sleep(Duration::from_millis(500));
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Setup { command } => match command {
            SetupCommand::Check { path, json } => check(&path, json),
            SetupCommand::DiscoverObs { folder, json } => optimize::discover(folder, json),
            SetupCommand::Schema => {
                println!("{}", serde_json::to_string_pretty(&Setup::json_schema())?);
                Ok(ExitCode::SUCCESS)
            }
            SetupCommand::ImportObs {
                collection,
                output,
                canvas,
                profile,
                json,
            } => import_obs(collection, output, canvas.as_deref(), profile, json),
            SetupCommand::Init {
                output,
                apply,
                display,
                camera,
                microphone,
                no_system_audio,
                layout,
                canvas,
                folder,
                name,
                json,
            } => {
                let (width, height, fps) = parse_canvas_size(canvas.as_deref())?;
                // The daemon may run in another folder: make a relative
                // folder absolute here.
                let folder = match folder {
                    Some(f) if !f.starts_with('~') && !Path::new(&f).is_absolute() => {
                        Some(std::path::absolute(&f)?.display().to_string())
                    }
                    other => other,
                };
                let options = isocast_protocol::init::InitOptions {
                    display,
                    camera,
                    microphone,
                    system_audio: no_system_audio.then_some(false),
                    layout,
                    width,
                    height,
                    fps,
                    folder,
                    name,
                };
                init_setup(options, output, apply, json)
            }
        },
        Command::Collection { command } => {
            library::run(isocast_protocol::library::LibraryKind::Collection, command)
        }
        Command::Profile { command } => {
            library::run(isocast_protocol::library::LibraryKind::Profile, command)
        }
        Command::Plugin { command } => plugin::run(command),
        Command::Data { command } => data::run(command),
        Command::Project { command } => project::run(command),
        Command::Hotkeys { command } => hotkeys::run(command),
        Command::GpuAdapter { json } => {
            let luid: Option<(u32, i32)> = Client::connect_or_launch()?.call(Method::GpuAdapter)?;
            if json {
                println!("{}", serde_json::to_string(&luid)?);
            } else if let Some((low, high)) = luid {
                println!("DX12 adapter LUID: {low}, {high}");
            } else {
                println!("This platform does not require an explicit graphics adapter identity.");
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Encoders { json } => {
            let options = Client::connect_or_launch()?.encoder_options()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&options)?);
            } else {
                for option in options {
                    println!(
                        "{}: {}",
                        option.label,
                        option
                            .containers
                            .iter()
                            .map(|c| serde_json::to_value(c)
                                .unwrap()
                                .as_str()
                                .unwrap()
                                .to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Devices { json } => devices(json),
        Command::Record {
            setup,
            sources,
            seconds,
            folder,
            json,
        } => record(setup.as_deref(), &sources, seconds, folder, json),
        Command::Start {
            setup,
            sources,
            seconds,
            folder,
            json,
        } => start(setup.as_deref(), &sources, seconds, folder, json),
        Command::Stop {
            wait,
            session,
            json,
        } => stop(wait, session, json),
        Command::Pause { json } => pause(true, json),
        Command::Resume { json } => pause(false, json),
        Command::Status { json } => status(json),
        Command::Hello { json } => hello(json),
        Command::Get {
            resolved,
            canvas,
            json,
        } => get(resolved, canvas, json),
        Command::Permissions {
            request,
            open,
            json,
        } => permissions(request, open, json),
        Command::Restart => restart(),
        Command::Audio {
            source,
            mono,
            stereo,
            gain,
            mute,
            unmute,
            offset,
            balance,
            monitor,
            json,
        } => {
            let change = client::AudioChange {
                mono: (mono || stereo).then_some(mono),
                gain_db: gain,
                muted: (mute || unmute).then_some(mute),
                offset_ms: offset,
                balance,
                monitor,
            };
            let mut client = Client::connect_or_launch()?;
            let applied = client::set_audio(&mut client, &source, &change)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&applied)?);
            } else {
                println!("Live setup is now at revision {}.", applied.revision);
                for note in &applied.notes {
                    println!("note: {note}");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::IsoAudio {
            source,
            audio,
            default,
            none,
            json,
        } => {
            let mut client = Client::connect_or_launch()?;
            let Some(source) = source else {
                let live: Live = client.call(Method::Get(None))?;
                let Some(setup) = live.setup else {
                    bail!("there is no live setup; apply one first");
                };
                let rows = client::iso_audio(&setup);
                if json {
                    println!("{}", serde_json::to_string_pretty(&rows)?);
                } else {
                    for row in &rows {
                        let tracks = match row.audio.is_empty() {
                            true => "no audio".to_string(),
                            false => row.audio.join(", "),
                        };
                        let from = match row.from {
                            client::IsoAudioFrom::SourceAudio => "its own list",
                            client::IsoAudioFrom::Shared => "recording.iso.audio",
                            client::IsoAudioFrom::Own => "own sound",
                        };
                        let off = if row.recorded { "" } else { " (no ISO file)" };
                        println!("  {:<14} {tracks} — {from}{off}", row.source);
                    }
                }
                return Ok(ExitCode::SUCCESS);
            };
            if !(default || none) && audio.is_empty() {
                bail!(
                    "give the audio sources for {source}'s ISO file, --none for no audio, or \
                     --default for recording.iso.audio's choice"
                );
            }
            let list = (!default).then_some(audio.as_slice());
            let applied = client::set_iso_audio(&mut client, &source, list)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&applied)?);
            } else {
                println!("Live setup is now at revision {}.", applied.revision);
                for note in &applied.notes {
                    println!("note: {note}");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Snapshot {
            source,
            scene,
            preview,
            output,
            width,
            json,
        } => snapshot(
            SnapshotParams {
                source,
                scene,
                preview,
                width,
            },
            output,
            json,
        ),
        Command::Doctor {
            setup,
            folder,
            json,
        } => doctor(setup.as_deref(), folder, json),
        Command::Machine { command } => {
            let (method, json) = match command {
                MachineCommand::Show { json } => (Method::MachineProfile, json),
                MachineCommand::Calibrate { json } => (Method::MachineCalibrate, json),
                MachineCommand::Cancel { json } => (Method::MachineCancel, json),
            };
            let result: isocast_protocol::machine::MachineStatus =
                Client::connect_or_launch()?.call(method)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("{}", result.summary);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Tune {
            setup,
            folder,
            json,
        } => tune(setup.as_deref(), folder, json),
        Command::Capacity {
            setup,
            stream,
            no_stream,
            add_iso,
            json,
        } => capacity(
            setup.as_deref(),
            (stream || no_stream).then_some(stream),
            add_iso,
            json,
        ),
        Command::Optimize {
            goal,
            service,
            server,
            upload_kbps,
            folder,
            apply,
            json,
        } => optimize::optimize(
            isocast_protocol::optimize::OptimizeParams {
                goal,
                service,
                server,
                upload_kbps,
                folder: folder.map(std::path::absolute).transpose()?,
                apply,
            },
            json,
        ),
        Command::Split { json } => split(json),
        Command::Marker { label, json } => marker(label, json),
        Command::Refresh { source, json } => refresh(source, json),
        Command::Scene {
            scene,
            transition,
            canvas,
            json,
        } => {
            let transition = transition.get()?;
            switched(
                Method::SwitchScene {
                    scene,
                    transition,
                    canvas,
                },
                json,
            )
        }
        Command::Preview {
            scene,
            clear,
            canvas,
            json,
        } => switched(
            match (clear, canvas.is_main()) {
                (true, true) => Method::ClearPreview,
                (true, false) => Method::ClearCanvasPreview { canvas },
                (false, _) => Method::Preview {
                    scene: scene.expect("required by clap"),
                    canvas,
                },
            },
            json,
        ),
        Command::Take {
            transition,
            canvas,
            json,
        } => {
            let transition = transition.get()?;
            switched(Method::Take { transition, canvas }, json)
        }
        Command::Plan { file, patch, json } => plan(file.as_deref(), patch.as_deref(), json),
        Command::Apply {
            file,
            patch,
            revision,
            live,
            json,
        } => apply(file.as_deref(), patch.as_deref(), revision, live, json),
        Command::Watch {
            levels,
            kinds,
            except,
            json,
        } => watch(levels, &kinds, &except, json),
        Command::Frame {
            target,
            output,
            width,
            canvas,
            json,
        } => frame(canvas_target(canvas, target)?, output, width, json),
        Command::Frames {
            target,
            output,
            width,
            height,
            fps,
            profile,
            seconds,
            shared,
            slots,
            filtered,
            check,
            expect,
            touch,
            canvas,
            json,
        } => match (shared, canvas_target(canvas, target)?) {
            (true, target) => shared_frames(shared_frames::Options {
                target,
                max_width: width,
                max_height: height,
                fps,
                slots,
                filtered,
                seconds,
                check,
                expect,
                touch,
                json,
            }),
            (false, target) => frames(target, output, width, fps, profile, seconds, json),
        },
        Command::Engine { command, json } => engine(command, json),
        Command::Vcam { command } => virtual_camera::run(command),
        Command::Projector { command } => projector::run(command),
        Command::Replay { command } => replay::run(command),
        Command::Websocket { command } => websocket::run(command),
        Command::Roots { command } => roots::run(command),
        Command::Files { command } => roots::files(command),
        Command::Debug { command } => debug::run(command),
        Command::Api {
            command: ApiCommand::Spec { json },
        } => api_spec(json),
        Command::Engines {
            discover,
            timeout,
            watch,
            json,
        } => match watch {
            true => engines::watch(discover, json),
            false => engines::run(discover, timeout, json),
        },
        Command::Canvas { command } => {
            use isocast_protocol::api::Canvases;
            let mut client = Client::connect_or_launch()?;
            let (method, json) = match command {
                CanvasCommand::List { json } => {
                    let status: Status = client.call(Method::Status)?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&status.canvases)?);
                    } else {
                        let main = status.engine.as_ref().map(|engine| &engine.canvas);
                        print_canvases(&status.canvases, main);
                    }
                    return Ok(ExitCode::SUCCESS);
                }
                CanvasCommand::Add {
                    canvas,
                    fields,
                    json,
                } => (Method::CanvasAdd(Box::new(fields.params(&canvas)?)), json),
                CanvasCommand::Set {
                    canvas,
                    fields,
                    mut clear,
                    no_stream,
                    json,
                } => {
                    if no_stream {
                        clear.push("stream".into());
                    }
                    let params = isocast_protocol::api::CanvasParams {
                        clear,
                        ..fields.params(&canvas)?
                    };
                    (Method::CanvasSet(Box::new(params)), json)
                }
                CanvasCommand::Remove { canvas, json } => (
                    Method::CanvasRemove {
                        canvas: CanvasId::parse(&canvas).map_err(anyhow::Error::msg)?,
                    },
                    json,
                ),
            };
            let changed: Canvases = client.call(method)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&changed)?);
            } else {
                print_canvases(&changed.canvases, None);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Stream { command } => match command {
            StreamCommand::SpeedTest {
                target,
                seconds,
                json,
            } => optimize::speed_test(target.service, target.server, seconds, json),
            StreamCommand::Start { only, canvas, json } => {
                let mut client = Client::connect_or_launch()?;
                client::check_engine(&mut client);
                let started: Streaming =
                    client.call(Method::StreamStart(Some(StreamStartParams {
                        only,
                        canvas,
                    })))?;
                print_streaming(&started, json)
            }
            StreamCommand::Stop { only, canvas, json } => {
                let Some(mut client) = Client::connect()? else {
                    bail!("not streaming: isocastd isn't running");
                };
                let stopped: Streaming =
                    client.call(Method::StreamStop(Some(StreamStartParams { only, canvas })))?;
                print_streaming(&stopped, json)
            }
            StreamCommand::Status { canvas, json } => {
                let Some(mut client) = Client::connect()? else {
                    if json {
                        println!("null");
                    } else {
                        println!("Not streaming: isocastd isn't running");
                    }
                    return Ok(ExitCode::SUCCESS);
                };
                let status = client.call::<Status>(Method::Status)?;
                // The vertical canvas's: its main destination's (else its
                // first) as the stream.
                let (stream, destinations) = if canvas.is_main() {
                    (status.stream, status.streams)
                } else {
                    let first = status
                        .vertical_streams
                        .iter()
                        .find(|s| s.id == MAIN_DESTINATION)
                        .or(status.vertical_streams.first())
                        .cloned();
                    (first, status.vertical_streams)
                };
                match stream {
                    Some(stream) => print_streaming(
                        &Streaming {
                            stream,
                            destinations,
                        },
                        json,
                    ),
                    None if json => {
                        println!("null");
                        Ok(ExitCode::SUCCESS)
                    }
                    None if canvas.is_main() => {
                        println!(
                            "Not set up for streaming: the live setup has no [stream] section \
                             (add one: `isocast stream destination add SERVICE`)"
                        );
                        Ok(ExitCode::SUCCESS)
                    }
                    None => {
                        println!(
                            "The vertical canvas isn't set up for streaming: the live setup has \
                             no [vertical.stream] section (add one: `isocast stream destination \
                             add SERVICE --canvas vertical`)"
                        );
                        Ok(ExitCode::SUCCESS)
                    }
                }
            }
            StreamCommand::Destination { command } => {
                let (method, json) = match command {
                    DestinationCommand::Add {
                        service,
                        id,
                        fields,
                        disabled,
                        canvas,
                        json,
                    } => (
                        Method::StreamDestinationAdd(Box::new(StreamDestinationParams {
                            id,
                            service: Some(service),
                            enabled: disabled.then_some(false),
                            canvas,
                            ..fields.params()
                        })),
                        json,
                    ),
                    DestinationCommand::Remove { id, canvas, json } => {
                        (Method::StreamDestinationRemove { id, canvas }, json)
                    }
                    DestinationCommand::Set {
                        id,
                        service,
                        fields,
                        enabled,
                        clear,
                        canvas,
                        json,
                    } => (
                        Method::StreamDestinationSet(Box::new(StreamDestinationParams {
                            id: Some(id),
                            service,
                            enabled,
                            clear,
                            canvas,
                            ..fields.params()
                        })),
                        json,
                    ),
                };
                let changed: StreamDestinations = Client::connect_or_launch()?.call(method)?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&changed)?);
                } else {
                    print_destinations(&changed.destinations);
                    for note in &changed.notes {
                        println!("note: {note}");
                    }
                }
                Ok(ExitCode::SUCCESS)
            }
            StreamCommand::Key { command } => match command {
                StreamKeyCommand::Set {
                    target,
                    canvas,
                    json,
                } => {
                    let mut client = Client::connect_or_launch()?;
                    let key = read_stream_key()?;
                    let stored: StreamKey =
                        client.call(Method::StreamKeySet(Box::new(StreamKeyParams {
                            key,
                            target: target.target(canvas),
                        })))?;
                    print_stream_key(&stored, json)
                }
                StreamKeyCommand::Clear {
                    target,
                    canvas,
                    json,
                } => {
                    let stored: StreamKey = Client::connect_or_launch()?
                        .call(Method::StreamKeyClear(Box::new(target.target(canvas))))?;
                    print_stream_key(&stored, json)
                }
            },
        },
        Command::Mcp => mcp::run().map(|()| ExitCode::SUCCESS),
    }
}

fn load(path: &Path) -> Result<Setup> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("could not read {}", path.display()))?;
    let mut setup = match path.extension().and_then(|e| e.to_str()) {
        Some("json") => Setup::from_json(&text)?,
        _ => Setup::from_toml(&text)?,
    };
    // Image paths in the file are relative to it.
    let folder = std::path::absolute(path)?;
    if let Some(folder) = folder.parent() {
        setup.resolve_paths(folder);
    }
    absolute_recording_folder(&mut setup)?;
    Ok(setup)
}

/// Make a relative recording folder absolute, from the current folder (as
/// `record` uses it): isocastd runs in another folder, and refuses one.
fn absolute_recording_folder(setup: &mut Setup) -> Result<()> {
    let folder = &setup.recording.folder;
    if !folder.is_empty() && !Path::new(folder).is_absolute() && !folder.starts_with('~') {
        setup.recording.folder = std::path::absolute(folder)?.display().to_string();
    }
    Ok(())
}

/// Why a setup file couldn't be loaded, as an issue naming the path in it.
fn load_issue(err: &anyhow::Error) -> isocast_protocol::Issue {
    match err.downcast_ref::<isocast_protocol::LoadError>() {
        Some(load) => isocast_protocol::Issue {
            path: load.path().to_string(),
            message: load.message(),
        },
        None => isocast_protocol::Issue {
            path: String::new(),
            message: format!("{err:#}"),
        },
    }
}

/// "path: message", or the message alone for the document as a whole.
fn issue_line(issue: &isocast_protocol::Issue) -> String {
    match issue.path.as_str() {
        "" => issue.message.clone(),
        path => format!("{path}: {}", issue.message),
    }
}

fn check(path: &Path, json: bool) -> Result<ExitCode> {
    let setup = match load(path) {
        Ok(setup) => setup,
        // A file that doesn't parse is a problem to report like any other.
        Err(err) if json => {
            let report = serde_json::json!({ "valid": false, "issues": [load_issue(&err)] });
            println!("{}", serde_json::to_string_pretty(&report)?);
            return Ok(ExitCode::FAILURE);
        }
        Err(err) => return Err(err.context(format!("{} is not a valid setup", path.display()))),
    };
    let issues = setup.validate();
    let code = if issues.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    };

    // Whether its files fit this Mac's encoders (#66): a warning, never a
    // failure.
    let capacity = issues.is_empty().then(|| capacity_check(&setup)).flatten();
    if json {
        let report = serde_json::json!({
            "valid": issues.is_empty(),
            "issues": issues,
            "setup": setup,
            "capacity": capacity,
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(code);
    }

    for issue in &issues {
        isocast_diagnostics::diagnostic!("warn"; "{}", issue_line(issue));
    }
    if !issues.is_empty() {
        isocast_diagnostics::diagnostic!("error"; "{} problem(s) in {}", issues.len(), path.display());
        return Ok(code);
    }

    let c = &setup.canvas;
    println!("{} — {}×{} @ {} fps", setup.name, c.width, c.height, c.fps);
    println!("sources:");
    for s in &setup.sources {
        println!("  {:<14} {}", s.id, s.kind.label());
    }
    println!("scenes:");
    for s in &setup.scenes {
        let items: Vec<&str> = s.items.iter().map(|i| i.key().unwrap_or("box")).collect();
        println!("  {:<14} {}", s.id, items.join(", "));
    }
    let iso = &setup.recording.iso;
    if iso.enabled {
        println!("iso: {} → {:?}", iso.sources.join(", "), iso.encoder.codec);
    }
    if let Some(check) = &capacity {
        if check.status == CheckStatus::Ok {
            println!("encoders: {}", check.detail);
        } else {
            isocast_diagnostics::diagnostic!("warn"; "encoders: {}", check.detail);
            if let Some(fix) = &check.fix {
                isocast_diagnostics::diagnostic!("warn"; "  fix: {fix}");
            }
        }
    }
    Ok(code)
}

/// Use the daemon's estimate, including its profile and exact tune results;
/// setup validation remains local if the daemon cannot answer.
fn capacity_check(setup: &Setup) -> Option<isocast_protocol::api::Check> {
    let not_checked = |reason: String| {
        Some(isocast_protocol::api::Check {
            name: "encoder capacity".into(),
            status: CheckStatus::Warning,
            detail: format!("not checked: {reason}"),
            fix: Some(
                "start Isocast, then check the file again; file validation did not start an engine"
                    .into(),
            ),
        })
    };
    let mut client = match Client::connect() {
        Ok(Some(client)) => client,
        Ok(None) => return not_checked("isocastd is not running".into()),
        Err(error) => return not_checked(format!("could not reach isocastd: {error}")),
    };
    let estimate: CapacityEstimate =
        match client.call(Method::EstimateCapacity(Box::new(CapacityParams {
            setup: Some(setup.clone()),
            ..Default::default()
        }))) {
            Ok(estimate) => estimate,
            Err(error) => {
                return not_checked(format!("isocastd could not estimate capacity: {error}"));
            }
        };
    isocast_engine::estimate::check(&estimate)
}

fn import_obs(
    collection: String,
    output: Option<PathBuf>,
    canvas: Option<&str>,
    profile: Option<String>,
    json: bool,
) -> Result<ExitCode> {
    let (width, height, fps) = parse_canvas_size(canvas)?;
    if let Some(path) = &output
        && path.exists()
    {
        bail!(
            "{} already exists; choose another --output or remove it",
            path.display()
        );
    }
    // The daemon may run in another folder: give it an absolute path to a
    // file (even a missing one, so its error names the path meant), or a
    // collection's name as it is.
    let path_like = collection.contains('/') && !collection.starts_with('~');
    let collection = match Path::new(&collection).exists() || path_like {
        true => std::path::absolute(&collection)?.display().to_string(),
        false => collection,
    };
    let profile = match profile {
        Some(p) if Path::new(&p).exists() => Some(std::path::absolute(&p)?.display().to_string()),
        other => other,
    };
    let imported: Imported =
        Client::connect_or_launch()?.call(Method::ImportObs(Box::new(ImportObsParams {
            collection,
            profile,
            width,
            height,
            fps,
        })))?;
    let toml = format!(
        "{}{}",
        import_header(&imported),
        toml::to_string_pretty(&imported.setup)?
    );
    if let Some(path) = &output {
        std::fs::write(path, &toml)
            .with_context(|| format!("could not write {}", path.display()))?;
    }
    let code = if imported.problems.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&imported)?);
        return Ok(code);
    }
    if output.is_none() {
        print!("{toml}");
    }
    let c = &imported.counts;
    let mut report = vec![format!(
        "Imported {} of {} sources, {} scenes, {} of {} items and {} of {} filters{}.",
        c.sources,
        c.obs_sources,
        c.scenes,
        c.items,
        c.obs_items,
        c.filters,
        c.obs_filters,
        match &output {
            Some(path) => format!(" into {}", path.display()),
            None => String::new(),
        }
    )];
    if !imported.unsupported.is_empty() {
        report.push("Not imported:".into());
        for u in &imported.unsupported {
            report.push(format!("  {} ({}): {}", u.obs, u.kind, u.reason));
        }
    }
    for note in &imported.notes {
        report.push(format!("note: {note}"));
    }
    for problem in &imported.problems {
        report.push(format!("problem: {}", issue_line(problem)));
    }
    // The setup went to stdout, so the report goes to stderr.
    for line in report {
        if output.is_some() {
            println!("{line}");
        } else {
            isocast_diagnostics::diagnostic!("{line}");
        }
    }
    Ok(code)
}

/// `--canvas WIDTHxHEIGHT[@FPS]`.
fn parse_canvas_size(canvas: Option<&str>) -> Result<(Option<u32>, Option<u32>, Option<u32>)> {
    let Some(canvas) = canvas else {
        return Ok((None, None, None));
    };
    let (size, fps) = match canvas.split_once('@') {
        Some((size, fps)) => (size, Some(fps)),
        None => (canvas, None),
    };
    let parsed = size
        .split_once('x')
        .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)));
    let fps = fps.map(|fps| fps.parse::<u32>().ok());
    match (parsed, fps) {
        (Some((w, h)), None) => Ok((Some(w), Some(h), None)),
        (Some((w, h)), Some(Some(fps))) => Ok((Some(w), Some(h), Some(fps))),
        _ => bail!("--canvas {canvas:?}: expected WIDTHxHEIGHT or WIDTHxHEIGHT@FPS"),
    }
}

/// `--layout`: a layout preset by its setup name.
fn parse_layout(value: &str) -> Result<isocast_protocol::init::Layout, String> {
    serde_json::from_value(serde_json::Value::String(value.into())).map_err(|_| {
        format!(
            "{value:?} is not a layout; valid: {}",
            isocast_protocol::init::Layout::NAMES.join(", ")
        )
    })
}

fn init_setup(
    options: isocast_protocol::init::InitOptions,
    output: Option<PathBuf>,
    apply: bool,
    json: bool,
) -> Result<ExitCode> {
    if let Some(path) = &output
        && path.exists()
    {
        bail!(
            "{} already exists; choose another --output or remove it",
            path.display()
        );
    }
    let made: isocast_protocol::init::Initialized =
        Client::connect_or_launch()?.call(Method::InitSetup(Box::new(
            isocast_protocol::api::InitParams { options, apply },
        )))?;
    let mut header = vec![format!(
        "Made by `isocast setup init` from this {}'s devices.",
        wording::MACHINE
    )];
    header.extend(made.notes.iter().cloned());
    let mut toml = String::new();
    for line in &header {
        for (i, part) in textwrap(line, 76).iter().enumerate() {
            toml.push_str(if i == 0 { "# " } else { "#   " });
            toml.push_str(part);
            toml.push('\n');
        }
    }
    toml.push('\n');
    toml.push_str(&made.setup.to_toml()?);
    if let Some(path) = &output {
        std::fs::write(path, &toml)
            .with_context(|| format!("could not write {}", path.display()))?;
    }
    let code = if made.problems.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&made)?);
        return Ok(code);
    }
    let quiet = output.is_some() || apply;
    if !quiet {
        print!("{toml}");
    }
    // The setup went to stdout unless it went elsewhere.
    let say = |line: String| {
        if quiet {
            println!("{line}");
        } else {
            isocast_diagnostics::diagnostic!("{line}");
        }
    };
    if quiet {
        for note in &made.notes {
            say(note.clone());
        }
    }
    for problem in &made.problems {
        say(format!("problem: {}", issue_line(problem)));
    }
    if let Some(path) = &output {
        say(format!("Wrote {}", path.display()));
    }
    if !quiet && made.problems.is_empty() {
        say(
            "Not applied: run it again with --apply to make it the live setup, or with \
             --output FILE to save it."
                .into(),
        );
    }
    if let Some(applied) = &made.applied {
        for note in &applied.notes {
            say(format!("note: {note}"));
        }
        say(format!(
            "Live setup is now at revision {}; record with `isocast start`.",
            applied.revision
        ));
    }
    Ok(code)
}

/// Split `text` into lines of at most about `width` characters, at spaces.
fn textwrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        if !line.is_empty() && line.len() + 1 + word.len() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    lines.push(line);
    lines
}

/// Comments for the top of an imported setup file: where it came from and
/// what didn't come over.
fn import_header(imported: &Imported) -> String {
    let mut lines = vec![format!(
        "Imported from the OBS scene collection {:?} by `isocast setup import-obs`.",
        imported.setup.name
    )];
    if let Some(collection) = &imported.collection {
        lines.push(format!("Collection: {collection}"));
    }
    if let Some(profile) = &imported.profile {
        lines.push(format!("Profile: {profile}"));
    }
    if !imported.unsupported.is_empty() {
        lines.push("Not imported:".into());
        for u in &imported.unsupported {
            lines.push(format!("- {} ({}): {}", u.obs, u.kind, u.reason));
        }
    }
    if !imported.notes.is_empty() {
        lines.push("Check:".into());
        for note in &imported.notes {
            lines.push(format!("- {note}"));
        }
    }
    let mut out = String::new();
    for line in lines {
        for (i, part) in line.lines().enumerate() {
            out.push_str(if i == 0 { "# " } else { "#   " });
            out.push_str(part);
            out.push('\n');
        }
    }
    out.push('\n');
    out
}

/// The longest `--seconds` accepted: 30 days.
const MAX_SECONDS: f64 = 30.0 * 24.0 * 3600.0;

/// The setup to record: a setup file, or one built from `--source` specs.
fn resolve_setup(setup: Option<&Path>, sources: &[String]) -> Result<Setup> {
    match (setup, sources.is_empty()) {
        (Some(_), false) => bail!("give either a setup file or --source, not both"),
        (Some(path), true) => {
            load(path).with_context(|| format!("{} is not a valid setup", path.display()))
        }
        (None, false) => {
            let mut setup = Setup::quick(sources).map_err(anyhow::Error::msg)?;
            absolute_recording_folder(&mut setup)?;
            Ok(setup)
        }
        (None, true) => bail!(
            "give a setup file or at least one --source; sources: {}",
            QUICK_SOURCES.join(", ")
        ),
    }
}

fn duration(seconds: Option<f64>) -> Result<Option<Duration>> {
    match seconds {
        Some(s) if s > 0.0 && s <= MAX_SECONDS => Ok(Some(Duration::from_secs_f64(s))),
        Some(s) => {
            bail!("--seconds must be more than 0 and at most {MAX_SECONDS} (30 days), not {s}")
        }
        None => Ok(None),
    }
}

fn record(
    setup: Option<&Path>,
    sources: &[String],
    seconds: Option<f64>,
    folder: Option<PathBuf>,
    json: bool,
) -> Result<ExitCode> {
    let setup = resolve_setup(setup, sources)?;
    duration(seconds)?;
    let root = std::path::absolute(
        folder.unwrap_or_else(|| isocast_engine::record::expand_home(&setup.recording.folder)),
    )?;
    // Keep the parent's recordings registry, not just the isolated daemon's
    // temporary one. Project creation and all media I/O now belong to isocastd.
    let remembered = if isocast_protocol::project::read(&root).is_some() {
        root.parent().unwrap_or(&root)
    } else {
        &root
    };
    if let Err(err) = isocast_protocol::project::remember_root(remembered) {
        isocast_diagnostics::diagnostic!("warn";
            "warning: could not add {} to the recordings folders isocastd lists ({err}); \
             its projects show in list_projects while it is the setup's recording.folder",
            remembered.display()
        );
    }
    let (report, cleanup) = record::run(setup, root, seconds, json)?;
    let code = print_report(&report, json)?;
    if let Some(error) = cleanup {
        isocast_diagnostics::diagnostic!("error"; "recording finished, but isolated daemon cleanup failed: {error:#}");
        return Ok(ExitCode::FAILURE);
    }
    Ok(code)
}

/// Print a session report; failing if the session had errors.
fn print_report(report: &Report, json: bool) -> Result<ExitCode> {
    isocast_diagnostics::emit(
        if report.errors.is_empty() {
            "info"
        } else {
            "error"
        },
        "recording",
        "recording completed",
        serde_json::json!({"recording_report":report}),
    );
    let code = if report.errors.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    };
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
        return Ok(code);
    }
    for warning in &report.warnings {
        isocast_diagnostics::diagnostic!("warn"; "warning: {warning}");
    }
    println!(
        "Recorded {:.2} s; manifest {}",
        report.duration_seconds,
        report.manifest.display()
    );
    if report.takes.len() > 1 {
        println!("  {} takes", report.takes.len());
    }
    for file in &report.files {
        println!(
            "  {}{}  {} frames, {} dropped, {} audio dropouts",
            if report.takes.len() > 1 {
                format!("take {}: ", file.take)
            } else {
                String::new()
            },
            file.path.display(),
            file.frames,
            file.dropped_frames,
            file.audio_dropouts
        );
    }
    for capture in &report.captures {
        println!(
            "  {} delivered {} new frames",
            capture.source, capture.frames_captured
        );
    }
    for error in &report.errors {
        isocast_diagnostics::diagnostic!("error"; "error: {error}");
    }
    Ok(code)
}

fn start(
    setup: Option<&Path>,
    sources: &[String],
    seconds: Option<f64>,
    folder: Option<PathBuf>,
    json: bool,
) -> Result<ExitCode> {
    // Neither a setup nor sources: record the live setup.
    let setup = match (setup, sources.is_empty()) {
        (None, true) => None,
        _ => Some(resolve_setup(setup, sources)?),
    };
    duration(seconds)?;
    // The daemon may run in another folder; give it an absolute one.
    let folder = folder.map(std::path::absolute).transpose()?;
    let mut client = Client::connect_or_launch()?;
    client::check_engine(&mut client);
    let started: Started = client.call(Method::Start(Box::new(StartParams {
        setup,
        folder,
        seconds,
    })))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&started)?);
    } else {
        match seconds {
            Some(s) => println!(
                "Recording in the background for {s} s (or until `isocast stop`); `isocast \
                 status` shows it:"
            ),
            None => println!("Recording in the background; stop with `isocast stop`:"),
        }
        for file in &started.files {
            println!("  {}", file.display());
        }
        for note in &started.notes {
            isocast_diagnostics::diagnostic!("note: {note}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn stop(wait: bool, session: Option<String>, json: bool) -> Result<ExitCode> {
    let Some(mut client) = Client::connect()? else {
        bail!("not recording: isocastd isn't running");
    };
    let params = session.map(|session| isocast_protocol::api::StopParams {
        session: Some(session),
    });
    if !wait {
        // Only asked: the files are still being finished.
        let accepted: Option<StopAccepted> = match client.call(Method::RequestStop(params.clone()))
        {
            Ok(accepted) => Some(accepted),
            Err(err)
                if err
                    .downcast_ref::<DaemonError>()
                    .is_some_and(|err| err.code() == RpcError::METHOD_NOT_FOUND) =>
            {
                // The method was not supported, so no stop was accepted. A
                // transport error is NOT safe to retry this way.
                isocast_diagnostics::diagnostic!(
                    "This daemon waits for Stop to finish; requesting the completed report."
                );
                None
            }
            Err(err) => return Err(not_recording(&mut client, err)),
        };
        if let Some(accepted) = accepted {
            if json {
                println!("{}", serde_json::to_string_pretty(&accepted)?);
            } else {
                println!("Stop requested; finalizing recording.");
                for file in &accepted.files {
                    println!("  {}", file.display());
                }
            }
            return Ok(ExitCode::SUCCESS);
        }
    }
    let report: Report = match client.call(Method::Stop(params)) {
        Ok(report) => report,
        Err(err) => return Err(not_recording(&mut client, err)),
    };
    print_report(&report, json)
}

/// `stop`'s error `err`, saying how the last recording ended when it was
/// refused for not recording: a recording with --seconds stops on its own.
/// (Other errors are this stop's.)
fn not_recording(client: &mut Client, err: anyhow::Error) -> anyhow::Error {
    let not_recording = err
        .downcast_ref::<DaemonError>()
        .is_some_and(|err| err.code() == RpcError::WRONG_STATE);
    let status = client
        .call::<Status>(Method::Status)
        .ok()
        .filter(|status| not_recording && status.state == EngineState::Idle);
    match status {
        Some(Status {
            last_error: Some(error),
            ..
        }) => anyhow::anyhow!("{err:#}; the last recording failed: {error}"),
        Some(Status {
            last: Some(last), ..
        }) => anyhow::anyhow!(
            "{err:#}; the last recording ({}) is done: {}",
            last_summary(&last),
            last.manifest.display()
        ),
        _ => err,
    }
}

/// "20.00 s, 3 files, 0 dropped frames" for a finished recording.
fn last_summary(report: &Report) -> String {
    let dropped: u64 = report.files.iter().map(|f| f.dropped_frames).sum();
    let mut text = format!(
        "{:.2} s, {} file{}, {dropped} dropped frame{}",
        report.duration_seconds,
        report.files.len(),
        if report.files.len() == 1 { "" } else { "s" },
        if dropped == 1 { "" } else { "s" }
    );
    if !report.errors.is_empty() {
        text.push_str(&format!(", {} error(s)", report.errors.len()));
    }
    text
}

fn hello(json: bool) -> Result<ExitCode> {
    let hello: isocast_protocol::hello::Hello = Client::connect_or_launch()?.call(Method::Hello)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&hello)?);
        return Ok(ExitCode::SUCCESS);
    }
    let os = serde_json::to_value(hello.os)?;
    println!("{} ({})", hello.name, os.as_str().unwrap_or("unknown"));
    let build = &hello.build;
    println!(
        "  isocastd {} ({} build, commit {}), protocol {}",
        build.version,
        serde_json::to_value(build.provenance)?
            .as_str()
            .unwrap_or("unknown"),
        build.commit,
        hello.protocol
    );
    if let Some(id) = &hello.engine_id {
        println!("  engine id {id}");
    }
    println!("  {} methods", hello.capabilities.len());
    Ok(ExitCode::SUCCESS)
}

fn status(json: bool) -> Result<ExitCode> {
    let status = match Client::connect()? {
        Some(mut client) => {
            let status: Status = client.call(Method::Status)?;
            client::warn_if_stale(status.daemon.as_ref());
            status
        }
        None => Status {
            state: EngineState::Idle,
            finalizing: false,
            governor: None,
            machine_profile: None,
            load_warning: None,
            scene: None,
            preview: None,
            session: None,
            live: None,
            files: Vec::new(),
            last: None,
            last_error: None,
            engine: None,
            setup_file: None,
            collection: None,
            profile: None,
            stream: None,
            streams: Vec::new(),
            cpu_percent: None,
            data: None,
            diagnostics: None,
            interrupted: Vec::new(),
            virtual_camera: None,
            replay: None,
            recording_free_bytes: None,
            program_bytes_per_second: None,
            daemon: None,
            vertical_replay: None,
            canvases: Vec::new(),
            vertical_scene: None,
            vertical_preview: None,
            vertical_streams: Vec::new(),
        },
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(warning) = &status.load_warning {
        eprintln!("warning: {}", warning.message);
    }
    if let Some(machine) = &status.machine_profile {
        println!("Machine: {}", machine.summary);
    }
    if let Some(governor) = &status.governor
        && (governor.steps > 0 || governor.bottleneck.is_some() || !governor.enabled)
    {
        println!("{}", governor.reason);
        if let Some(error) = &governor.error {
            eprintln!("warning: automatic quality: {error}");
        }
    }
    if let (Some(collection), Some(profile)) = (&status.collection, &status.profile) {
        println!(
            "Scene collection: {:?} ({}), profile: {:?} ({})",
            collection.name, collection.id, profile.name, profile.id
        );
    }
    for take in &status.interrupted {
        println!(
            "Interrupted recording: {} (recoverable duration unknown; inspect its manifest)",
            take.manifest.display(),
        );
    }
    if let Some(diagnostics) = &status.diagnostics
        && let Some(previous) = diagnostics["previous_abnormal"].as_array()
        && !previous.is_empty()
    {
        println!(
            "Warning: {} previous daemon launch(es) ended abnormally; run `isocast report`.",
            previous.len()
        );
    }
    if let Some(warning) = status
        .diagnostics
        .as_ref()
        .and_then(|d| d["native_relay"]["last_warning"].as_str())
    {
        println!("Warning: {warning}");
    }
    if let Some(file) = &status.setup_file {
        print_setup_file(file);
    }
    match &status.engine {
        Some(engine) => print_engine(engine),
        None => println!("Live engine: stopped"),
    }
    if let Some(cpu) = status.cpu_percent {
        println!("CPU: {cpu:.1}% of this {} (isocastd)", wording::MACHINE);
    }
    if let Some(free) = status.recording_free_bytes {
        println!("Recording disk: {:.1} GB free", free as f64 / 1e9);
    }
    if let Some(rate) = status.program_bytes_per_second {
        println!("Program file: up to ~{:.1} GB an hour", rate * 3600.0 / 1e9);
    }
    if let Some(daemon) = &status.daemon {
        println!(
            "Engine: isocastd {} (build {}, pid {})",
            daemon.version, daemon.build, daemon.pid
        );
    }
    if let Some(data) = status.data.as_ref().filter(|data| data.keys > 0) {
        println!(
            "Data: {} keys ({} kept), {} bytes, revision {}{}",
            data.keys,
            data.persisted,
            data.bytes,
            data.revision,
            data.save_error
                .as_ref()
                .map(|error| format!("; kept keys couldn't be saved: {error}"))
                .unwrap_or_default()
        );
    }
    if let Some(preview) = &status.preview {
        println!("Preview: {preview}");
    }
    if let Some(scene) = &status.scene {
        println!("Scene on air: {scene}");
    }
    match (status.state, &status.live) {
        (EngineState::Idle, _) => match (&status.last_error, &status.last) {
            (Some(error), last) => {
                println!("Idle; the last recording failed: {error}");
                if let Some(last) = last {
                    println!(
                        "  the one before: {}, manifest {}",
                        last_summary(last),
                        last.manifest.display()
                    );
                }
            }
            (None, Some(last)) => println!(
                "Idle; last recording: {}, manifest {}",
                last_summary(last),
                last.manifest.display()
            ),
            (None, None) => println!("Idle"),
        },
        _ if status.finalizing => {
            println!("Finalizing: finishing the files; a new recording waits until they're done")
        }
        (EngineState::Starting, _) => println!("Starting"),
        (EngineState::Recording, None) => println!("Recording"),
        (EngineState::Recording, Some(live)) => {
            println!(
                "{} for {:.2} s; {:.2} s paused",
                if live.paused {
                    "Paused; recorded"
                } else {
                    "Recording"
                },
                live.elapsed_seconds,
                live.paused_seconds
            );
            if live.handover {
                println!("  Starting a new take: split, pause and resume wait until it has");
            }
            for file in &live.files {
                println!(
                    "  {}  {} frames, {} dropped, {} audio dropouts",
                    file.source, file.frames, file.dropped_frames, file.audio_dropouts
                );
            }
            for level in &live.levels {
                println!(
                    "  {} audio  {:.0} dB peak, {:.0} dB RMS",
                    level.source, level.peak_db, level.rms_db
                );
            }
            if let Some(monitor) = &live.monitor {
                print_monitor(monitor);
            }
        }
    }
    if status.streams.is_empty() {
        if let Some(stream) = &status.stream {
            print_stream_lines(stream);
        }
    } else {
        print_destinations(&status.streams);
    }
    if status.state != EngineState::Idle {
        for file in &status.files {
            println!("  {}", file.display());
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn pause(paused: bool, json: bool) -> Result<ExitCode> {
    let Some(mut client) = Client::connect()? else {
        bail!("recording: idle; start a recording first");
    };
    let recording: isocast_protocol::api::RecordingPause = client.call(if paused {
        Method::Pause
    } else {
        Method::Resume
    })?;
    if json {
        println!("{}", serde_json::to_string_pretty(&recording)?);
    } else {
        println!(
            "{} at {:.2} s; {:.2} s paused",
            if recording.paused {
                "Paused"
            } else {
                "Resumed"
            },
            recording.elapsed_seconds,
            recording.paused_seconds
        );
        if let Some(seconds) = recording.recoverable_seconds {
            println!("  on disk up to {seconds:.3} s of every file");
        }
        for error in &recording.errors {
            eprintln!("  error: {error}");
        }
    }
    if recording.errors.is_empty() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::FAILURE)
    }
}

fn split(json: bool) -> Result<ExitCode> {
    let Some(mut client) = Client::connect()? else {
        bail!("not recording: isocastd isn't running");
    };
    let started: Started = client.call(Method::Split)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&started)?);
    } else {
        match &started.begins_at {
            Some(at) => println!("New take, from {at}:"),
            None => println!("New take:"),
        }
        for file in &started.files {
            println!("  {}", file.display());
        }
        for note in &started.notes {
            println!("note: {note}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn refresh(source: String, json: bool) -> Result<ExitCode> {
    let Some(mut client) = Client::connect()? else {
        bail!("browser sources run while recording, and isocastd isn't running");
    };
    let refreshed: Refreshed = client.call(Method::RefreshBrowser { source })?;
    if json {
        println!("{}", serde_json::to_string_pretty(&refreshed)?);
    } else {
        println!("Reloading {}", refreshed.source);
    }
    Ok(ExitCode::SUCCESS)
}

fn marker(label: String, json: bool) -> Result<ExitCode> {
    let Some(mut client) = Client::connect()? else {
        bail!("markers need a recording in progress, and isocastd isn't running");
    };
    let marked: Marked = client.call(Method::Marker { label })?;
    if json {
        println!("{}", serde_json::to_string_pretty(&marked)?);
    } else {
        println!(
            "Marked {:?} at {:.2} s in take {}",
            marked.label, marked.time_seconds, marked.take
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// How a switch transitions, if not the scene's or the setup's.
#[derive(clap::Args)]
struct TransitionArgs {
    /// Cut straight to the scene.
    #[arg(long, conflicts_with_all = ["fade", "transition"])]
    cut: bool,
    /// Fade to the scene over this many milliseconds.
    #[arg(long, value_name = "MS", conflicts_with = "transition")]
    fade: Option<u32>,
    /// Another transition: a kind (cut, fade, swipe, slide, fade_to_color,
    /// luma_wipe, stinger, move) with its default settings, or a JSON
    /// object like '{"kind":"swipe","direction":"up"}'.
    #[arg(long, value_name = "KIND|JSON")]
    transition: Option<String>,
    /// How long --transition takes, in milliseconds (300 by default).
    #[arg(long, value_name = "MS", requires = "transition")]
    duration: Option<u32>,
}

impl TransitionArgs {
    fn get(&self) -> Result<Option<Transition>> {
        if self.cut {
            return Ok(Some(Transition::default()));
        }
        if let Some(ms) = self.fade {
            return Ok(Some(Transition::fade(ms)));
        }
        let Some(spec) = &self.transition else {
            return Ok(None);
        };
        let mut value: serde_json::Value = if spec.trim_start().starts_with('{') {
            serde_json::from_str(spec).context("--transition: not a JSON object")?
        } else {
            serde_json::json!({ "kind": spec })
        };
        if let Some(ms) = self.duration {
            value["duration_ms"] = ms.into();
        }
        // A stinger's video, given here, is relative to this folder.
        if let Some(file) = value.get_mut("file")
            && let Some(path) = file.as_str()
            && !Path::new(path).is_absolute()
            && !path.starts_with('~')
        {
            *file = std::env::current_dir()?.join(path).to_string_lossy().into();
        }
        if let Some(image) = value.get_mut("image")
            && let Some(path) = image.as_str()
            && !Path::new(path).is_absolute()
            && !path.starts_with('~')
        {
            *image = std::env::current_dir()?.join(path).to_string_lossy().into();
        }
        let transition = isocast_protocol::located::from_value(&value)
            .map_err(|err| anyhow::anyhow!("--transition: {err}"))?;
        Ok(Some(transition))
    }
}

/// How a scene came on air, for messages: " with a 500 ms swipe".
fn how(transition: Option<&Transition>) -> String {
    match transition {
        Some(t) => format!(" with a {} ms {}", t.duration_ms, t.kind.label()),
        None => String::new(),
    }
}

/// Call a scene method (switch, preview or take) and show the result.
fn switched(method: Method, json: bool) -> Result<ExitCode> {
    let switched: Switched = Client::connect_or_launch()?.call(method)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&switched)?);
        return Ok(ExitCode::SUCCESS);
    }
    let mut how = how(switched.transition.as_ref());
    if !switched.canvas.is_main() {
        how = format!(" on the vertical canvas{how}");
    }
    match switched.take.zip(switched.time_seconds) {
        Some((take, t)) => println!(
            "Scene {:?} on air{how} at {t:.2} s in take {take}",
            switched.scene
        ),
        None => println!("Scene {:?} on air{how}", switched.scene),
    }
    if let Some(preview) = &switched.preview {
        println!("Preview: {preview}");
    }
    Ok(ExitCode::SUCCESS)
}

fn doctor(setup: Option<&Path>, folder: Option<PathBuf>, json: bool) -> Result<ExitCode> {
    let setup = setup
        .map(|path| load(path).with_context(|| format!("{} is not a valid setup", path.display())))
        .transpose()?;
    let report = run_doctor(setup.as_ref(), folder.as_deref())?;
    let problems = report
        .checks
        .iter()
        .filter(|c| c.status == CheckStatus::Problem)
        .count();
    let warnings = report
        .checks
        .iter()
        .filter(|c| c.status == CheckStatus::Warning)
        .count();
    let code = if problems == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(code);
    }
    let m = &report.machine;
    println!(
        "{} · {:.0} GB · {} cores · {}",
        m.chip, m.memory_gb, m.cores, m.os
    );
    for check in &report.checks {
        let mark = match check.status {
            CheckStatus::Ok => "ok     ",
            CheckStatus::Warning => "warning",
            CheckStatus::Problem => "PROBLEM",
        };
        println!("{mark}  {}: {}", check.name, check.detail);
        if let Some(fix) = &check.fix {
            println!("         fix: {fix}");
        }
    }
    println!("{problems} problem(s), {warnings} warning(s)");
    Ok(code)
}

fn tune(setup: Option<&Path>, folder: Option<PathBuf>, json: bool) -> Result<ExitCode> {
    let setup = setup
        .map(|path| load(path).with_context(|| format!("{} is not a valid setup", path.display())))
        .transpose()?;
    let folder = folder.map(std::path::absolute).transpose()?;
    if !json {
        isocast_diagnostics::diagnostic!("Benchmarking encoders; this takes a little while…");
    }
    let tuning: Tuning =
        Client::connect_or_launch()?.call(Method::Tune(Box::new(TuneParams { setup, folder })))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&tuning)?);
        return Ok(ExitCode::SUCCESS);
    }
    let streams: Vec<String> = tuning
        .streams
        .iter()
        .map(|s| format!("{} {}×{}@{}", s.source, s.width, s.height, s.fps))
        .collect();
    println!("Streams: {}", streams.join(", "));
    println!(
        "Disk: {}/s, {} free",
        human(tuning.disk_bytes_per_second),
        human(tuning.free_bytes as f64)
    );
    let verdict = |capacity: Option<Capacity>| match capacity {
        Some(Capacity::Fits) => "fits",
        Some(Capacity::NoMargin) => "keeps up, no margin",
        Some(Capacity::CannotKeepUp) => "can't keep up",
        None => "not benchmarked",
    };
    println!(
        "As set up: {} ({:.2}× real time on hard content)",
        verdict(tuning.capacity),
        tuning.realtime_factor
    );
    println!("ISO encoders:");
    for c in &tuning.candidates {
        println!(
            "  {:<18} {:>5.2}× real time  {:>9}/min  {}",
            format!("{:?} {}-bit", c.encoder.codec, c.encoder.bit_depth),
            c.realtime_factor,
            human(c.bytes_per_second * 60.0),
            c.reason.as_deref().unwrap_or("feasible")
        );
    }
    if !tuning.variants.is_empty() {
        println!("Changes tried, closest first:");
        for v in &tuning.variants {
            println!(
                "  {:<40} {:>5.2}× real time  {:>9}/min  {}",
                v.description,
                v.realtime_factor,
                human(v.bytes_per_second * 60.0),
                v.reason.as_deref().unwrap_or("fits")
            );
        }
    }
    match &tuning.recommendation {
        Some(recommendation) => println!("Recommended: {recommendation}"),
        None => println!("Recommended: nothing tried fits"),
    }
    for note in &tuning.notes {
        println!("note: {note}");
    }
    if let Some(change) = &tuning.change {
        println!(
            "Plan it with: isocast plan --patch '{}'",
            serde_json::to_string(change)?
        );
    }
    // A closer change that keeps up, if only just.
    if let Some(closer) = tuning
        .variants
        .iter()
        .find(|v| v.capacity == Capacity::NoMargin && Some(&v.change) != tuning.change.as_ref())
        .filter(|_| tuning.change.is_some())
    {
        println!(
            "Or, closer but with no margin ({}): isocast plan --patch '{}'",
            closer.description,
            serde_json::to_string(&closer.change)?
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn capacity(
    setup: Option<&Path>,
    stream: Option<bool>,
    add_iso: Option<String>,
    json: bool,
) -> Result<ExitCode> {
    let setup = setup
        .map(|path| load(path).with_context(|| format!("{} is not a valid setup", path.display())))
        .transpose()?;
    let estimate: CapacityEstimate =
        Client::connect_or_launch()?.call(Method::EstimateCapacity(Box::new(CapacityParams {
            setup,
            stream,
            add_iso,
        })))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&estimate)?);
        return Ok(ExitCode::SUCCESS);
    }
    let software = estimate
        .resources
        .iter()
        .any(|r| r.resource == isocast_protocol::api::SOFTWARE_RENDERER_RESOURCE);
    if software {
        println!(
            "{}: software graphics adapter; local capacity unmeasured",
            estimate.chip
        );
    } else {
        println!(
            "{}: {} H.264/HEVC engine(s), {:.0} Mpx/s each",
            estimate.chip,
            estimate.encode_engines,
            estimate.engine_pixels_per_second / 1e6
        );
    }
    println!("  ({})", estimate.basis);
    for o in &estimate.outputs {
        println!(
            "  {:<14} {}×{}@{} {:?}{}  {:>5.0} Mpx/s{}",
            o.output,
            o.width,
            o.height,
            o.fps,
            o.codec,
            if o.protected { ", protected" } else { "" },
            o.pixels_per_second / 1e6,
            if o.assumed { " (size assumed)" } else { "" }
        );
    }
    for resource in &estimate.resources {
        let (scale, unit) = if resource.unit == "bytes/second" {
            (1e6, "MB/s")
        } else {
            (1.0, resource.unit.as_str())
        };
        println!(
            "  {}: {:.2} {}; capacity {}{}",
            resource.resource,
            resource.demand / scale,
            unit,
            resource
                .capacity
                .map_or_else(|| "unqualified".into(), |v| format!("{:.2}", v / scale)),
            resource
                .load
                .map_or_else(String::new, |v| format!(", {:.0}% load", v * 100.0))
        );
        println!("    {}", resource.basis);
    }
    let verdict = match estimate.capacity {
        Capacity::Fits => "fits",
        Capacity::NoMargin
            if estimate.resources.iter().any(|r| {
                matches!(
                    r.resource.as_str(),
                    "prores"
                        | "recording_disk"
                        | "startup_recovery"
                        | isocast_protocol::api::SOFTWARE_RENDERER_RESOURCE
                ) && r.capacity.is_none()
            }) =>
        {
            "not qualified"
        }
        Capacity::NoMargin => "keeps up, no margin",
        Capacity::CannotKeepUp => "can't keep up",
    };
    if software {
        println!("Verdict: {verdict}; local capacity is unmeasured");
    } else {
        println!(
            "Verdict: {verdict}, {} at {:.0}%{}",
            if estimate.resources.is_empty() {
                "busiest engine"
            } else {
                "largest modelled resource"
            },
            estimate.load * 100.0,
            if estimate.streaming {
                ", stream counted"
            } else {
                ""
            }
        );
    }
    let mut summary = estimate.summary.clone();
    if let Some(first) = summary.get(..1) {
        summary.replace_range(..1, &first.to_uppercase());
    }
    println!("{summary}.");
    if let Some(measured) = &estimate.measured {
        println!("note: {measured}");
    }
    for s in &estimate.suggestions {
        if software {
            println!(
                "Suggestion: {}{}",
                s.description,
                if s.after_take {
                    " (after this take)"
                } else {
                    ""
                }
            );
            continue;
        }
        let how = match (&s.change, &s.method) {
            (Some(change), _) => {
                format!("isocast plan --patch '{}'", serde_json::to_string(change)?)
            }
            (None, Some(method)) => format!("method {method}"),
            _ => String::new(),
        };
        println!(
            "Fix: {} ({:.0}%{}): {how}",
            s.description,
            s.load * 100.0,
            if s.after_take {
                ", after this take"
            } else {
                ""
            }
        );
    }
    if let Some(a) = &estimate.admission {
        println!("Add ISO: {}", a.description);
        if let Some(reason) = &a.reason {
            println!("  because {reason}");
        }
        if let Some(change) = &a.change {
            println!(
                "  isocast plan --patch '{}'",
                serde_json::to_string(change)?
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn human(bytes: f64) -> String {
    isocast_engine::space::human(bytes)
}

fn run_doctor(
    setup: Option<&Setup>,
    folder: Option<&Path>,
) -> Result<isocast_protocol::api::Doctor> {
    let folder = folder.map(std::path::absolute).transpose()?;
    Client::connect_or_launch()?.call(Method::Doctor(Box::new(DoctorParams {
        setup: setup.cloned(),
        folder,
    })))
}

fn snapshot(params: SnapshotParams, output: Option<PathBuf>, json: bool) -> Result<ExitCode> {
    use base64::Engine;
    let snapshot: Snapshot = Client::connect_or_launch()?.call(Method::Snapshot(params))?;
    let of = snapshot
        .source
        .clone()
        .or(snapshot.scene.clone())
        .unwrap_or_default();
    let png = base64::engine::general_purpose::STANDARD
        .decode(&snapshot.png_base64)
        .context("isocastd sent a picture that isn't base64")?;
    let path = output.unwrap_or_else(|| PathBuf::from(format!("{of}.png")));
    std::fs::write(&path, png).with_context(|| format!("could not write {}", path.display()))?;
    let path = std::path::absolute(&path)?;
    if json {
        let value = serde_json::json!({
            "source": snapshot.source,
            "scene": snapshot.scene,
            "path": path,
            "width": snapshot.width,
            "height": snapshot.height,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!(
            "Saved {of} ({}×{}) to {}",
            snapshot.width,
            snapshot.height,
            path.display()
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn restart() -> Result<ExitCode> {
    client::restart()?;
    println!("isocastd restarted");
    Ok(ExitCode::SUCCESS)
}

fn permissions(
    request: Option<Option<isocast_protocol::devices::PermissionKind>>,
    open: Option<isocast_protocol::devices::PermissionKind>,
    json: bool,
) -> Result<ExitCode> {
    let only = request.flatten();
    let request = request.is_some();
    // Ask the daemon: it records, so its permissions are the ones that count.
    let mut client = Client::connect_or_launch()?;
    if let Some(permission) = open {
        let opened: isocast_protocol::api::Opened =
            client.call(Method::OpenPermissionSettings { permission })?;
        if json {
            println!("{}", serde_json::to_string_pretty(&opened)?);
        } else {
            println!(
                "{}",
                if cfg!(windows) {
                    format!(
                        "Opened Windows Settings at {}; review access for desktop apps (isocastd.exe).",
                        permission.label()
                    )
                } else {
                    format!(
                        "Opened System Settings at {}; turn the Isocast engine's app on there.",
                        permission.label()
                    )
                }
            );
        }
        return Ok(ExitCode::SUCCESS);
    }
    if request && !json {
        // Only when macOS has something to ask.
        let now: isocast_protocol::devices::PermissionsReport = client.call(Method::Permissions)?;
        let undecided = now.details.iter().any(|d| {
            d.state == isocast_protocol::devices::Permission::NotDetermined
                && only.is_none_or(|only| only == d.permission)
        });
        if undecided {
            isocast_diagnostics::diagnostic!(
                "{}",
                wording::select_linux(
                    "Asking macOS; answer its prompts (waiting up to a minute)…",
                    "Checking Windows access (waiting up to a minute); Camera and Microphone settings must allow desktop apps (isocastd.exe)…",
                    "Checking access (waiting up to a minute)…"
                )
            );
        }
    }
    let report: isocast_protocol::devices::PermissionsReport = if request {
        client.call(Method::RequestPermissions(only.map(|permission| {
            isocast_protocol::api::RequestPermissionsParams {
                permission: Some(permission),
            }
        })))?
    } else {
        client.call(Method::Permissions)?
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(ExitCode::SUCCESS);
    }
    for detail in &report.details {
        println!(
            "{}: {} (for {})",
            detail.permission.label(),
            detail.state.label(),
            detail.needed_for
        );
        if let Some(step) = &detail.next_step {
            println!("  next: {step}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `--open`: a permission by its API name.
fn parse_permission(value: &str) -> Result<isocast_protocol::devices::PermissionKind, String> {
    serde_json::from_value(serde_json::Value::String(value.into())).map_err(|_| {
        format!(
            "{value:?} is not a permission; valid: {}",
            isocast_protocol::devices::PermissionKind::NAMES.join(", ")
        )
    })
}

fn get(resolved: bool, canvas: CanvasId, json: bool) -> Result<ExitCode> {
    if !resolved && !canvas.is_main() {
        bail!("--canvas says whose layout --resolved shows; give --resolved too");
    }
    let params = resolved.then_some(isocast_protocol::api::GetParams { resolved, canvas });
    let live: Live = Client::connect_or_launch()?.call(Method::Get(params))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&live)?);
        return Ok(ExitCode::SUCCESS);
    }
    let file = live.setup_file.as_ref();
    match &live.setup {
        Some(setup) => {
            println!("# Live setup, revision {}", live.revision);
            if let (Some(collection), Some(profile)) = (&live.collection, &live.profile) {
                println!(
                    "# Scene collection {:?} ({}), profile {:?} ({})",
                    collection.name, collection.id, profile.name, profile.id
                );
            }
            if let Some(file) = file {
                println!("# Kept in {}", file.path.display());
                if let Some(profile) = &file.profile_path {
                    println!("#     and {}", profile.display());
                }
            }
            print!("{}", setup.to_toml()?);
            if let Some(resolved) = &live.resolved {
                print_resolved(resolved);
            }
        }
        // A file that didn't load is explained below; making a new setup
        // isn't the first thing to suggest then.
        None if file.is_some_and(|file| file.load_error.is_some()) => {
            println!("No live setup (revision 0): the setup file couldn't be loaded.")
        }
        None => println!(
            "No live setup yet (revision 0); make one with `isocast setup init --apply`, or apply \
             one with `isocast apply FILE --revision 0`."
        ),
    }
    if let Some(file) = file
        && (file.load_error.is_some() || file.save_error.is_some())
    {
        print_setup_file(file);
    }
    Ok(ExitCode::SUCCESS)
}

/// Where layout put every item: its rect in its parent, and its box on
/// the canvas where that differs.
fn print_resolved(resolved: &isocast_protocol::api::Resolved) {
    use isocast_protocol::api::{ResolvedItem, ResolvedRect};
    fn rect(r: &ResolvedRect) -> String {
        format!("{} {} {}x{}", r.x, r.y, r.width, r.height)
    }
    fn walk(items: &[ResolvedItem], depth: usize) {
        for (index, item) in items.iter().enumerate() {
            let name = item.id.clone().unwrap_or_else(|| format!("#{index}"));
            let mut line = format!(
                "{:indent$}{name:<w$} {}",
                "",
                rect(&item.rect),
                indent = 2 + 2 * depth,
                w = 20usize.saturating_sub(2 * depth)
            );
            if item.canvas != item.rect {
                line.push_str(&format!("  (canvas {})", rect(&item.canvas)));
            }
            if !item.visible {
                line.push_str("  hidden");
            }
            println!("{line}");
            walk(&item.children, depth + 1);
        }
    }
    println!();
    match resolved.generation {
        0 => println!(
            "# Resolved layout (x y widthxheight in the parent; the live engine isn't running, so \
             sources' sizes are their settings')"
        ),
        generation => {
            println!("# Resolved layout, generation {generation} (x y widthxheight in the parent)")
        }
    }
    for scene in &resolved.scenes {
        println!("{}:", scene.id);
        walk(&scene.items, 0);
    }
}

/// Where the live setup is kept, and any trouble loading or saving it.
fn print_setup_file(file: &isocast_protocol::api::SetupFile) {
    println!("Setup file: {}", file.path.display());
    if let Some(profile) = &file.profile_path {
        println!("Profile file: {}", profile.display());
    }
    for note in &file.notes {
        println!("note: {note}");
    }
    if let Some(error) = &file.load_error {
        isocast_diagnostics::diagnostic!("warn"; "warning: {}", error.describe(&file.path));
    }
    if let Some(error) = &file.save_error {
        isocast_diagnostics::diagnostic!("warn"; "warning: {error}");
    }
}

/// A change from a setup file or a JSON merge patch.
fn change(file: Option<&Path>, patch: Option<&str>) -> Result<Change> {
    match (file, patch) {
        (Some(path), None) => {
            let setup =
                load(path).with_context(|| format!("{} is not a valid setup", path.display()))?;
            Ok(Change::Setup(Box::new(setup)))
        }
        (None, Some(patch)) => Ok(Change::Patch(
            serde_json::from_str(patch).context("--patch is not valid JSON")?,
        )),
        _ => bail!("give either a setup file or --patch with JSON"),
    }
}

fn plan(file: Option<&Path>, patch: Option<&str>, json: bool) -> Result<ExitCode> {
    let change = match (change(file, patch), file) {
        (Ok(change), _) => change,
        // A file that doesn't parse is a problem, like one in a patch.
        (Err(err), Some(path)) if json => {
            let live: Live = Client::connect_or_launch()?.call(Method::Get(None))?;
            let issue = load_issue(&err);
            let plan = Plan {
                revision: live.revision,
                changes: Vec::new(),
                problems: vec![isocast_protocol::Issue {
                    message: format!("{} is not a valid setup: {}", path.display(), issue.message),
                    ..issue
                }],
                notes: Vec::new(),
                setup: None,
            };
            println!("{}", serde_json::to_string_pretty(&plan)?);
            return Ok(ExitCode::FAILURE);
        }
        (Err(err), _) => return Err(err),
    };
    let plan: Plan = Client::connect_or_launch()?.call(Method::Plan(Box::new(change)))?;
    let code = if plan.problems.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
        return Ok(code);
    }
    print_differences(&plan.changes);
    for problem in &plan.problems {
        isocast_diagnostics::diagnostic!("error"; "problem: {}", issue_line(problem));
    }
    for note in &plan.notes {
        println!("note: {note}");
    }
    if plan.problems.is_empty() && !plan.changes.is_empty() {
        let what = match (file, patch) {
            (Some(path), _) => path.display().to_string(),
            (_, Some(patch)) => format!("--patch '{patch}'"),
            _ => String::new(),
        };
        println!(
            "Apply with: isocast apply {what} --revision {}",
            plan.revision
        );
    }
    Ok(code)
}

fn apply(
    file: Option<&Path>,
    patch: Option<&str>,
    revision: u64,
    live: bool,
    json: bool,
) -> Result<ExitCode> {
    let change = change(file, patch)?;
    let applied: Applied =
        Client::connect_or_launch()?.call(Method::Apply(Box::new(ApplyParams {
            revision,
            change,
            live,
        })))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&applied)?);
        return Ok(ExitCode::SUCCESS);
    }
    print_differences(&applied.changes);
    for note in &applied.notes {
        println!("note: {note}");
    }
    println!("Live setup is now at revision {}.", applied.revision);
    Ok(ExitCode::SUCCESS)
}

fn print_differences(changes: &[Difference]) {
    if changes.is_empty() {
        println!("No changes.");
    }
    let show = |value: &Option<serde_json::Value>| {
        value
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default())
            .unwrap_or_default()
    };
    for change in changes {
        match change.op {
            DifferenceOp::Add => println!("+ {}: {}", change.path, show(&change.to)),
            DifferenceOp::Remove => println!("- {}: {}", change.path, show(&change.from)),
            DifferenceOp::Change => println!(
                "~ {}: {} → {}",
                change.path,
                show(&change.from),
                show(&change.to)
            ),
        }
    }
}

fn print_engine(engine: &EngineInfo) {
    for error in &engine.worker_errors {
        println!("  Engine worker failed: {error}");
    }
    let canvas = &engine.canvas;
    let why = if engine.outputs.is_empty() {
        String::new()
    } else {
        format!(" ({})", engine.outputs.join(", "))
    };
    println!(
        "Live engine: {health}{why} for {:.0} s, {}×{} at {:.2} / {} fps, {} frames drawn, {} \
         missed",
        engine.uptime_seconds,
        canvas.width,
        canvas.height,
        engine.fps,
        canvas.fps,
        engine.program_frames,
        engine.missed_frames,
        health = if engine.worker_errors.is_empty() {
            "running"
        } else {
            "degraded"
        }
    );
    let compositor = &engine.compositor;
    println!(
        "  compositor: {:.1} µs CPU per frame (p99 {:.1}), {} draws, {} frames sent again \
         unchanged; scenes resolved in {:.1} µs",
        compositor.cpu_us.p50,
        compositor.cpu_us.p99,
        compositor.draws,
        compositor.reused,
        engine.scenes.resolve_us
    );
    if engine.scenes.content > 0 {
        println!(
            "  content: {} pictures ({:.1} MB), last drawn in {:.1} µs",
            engine.scenes.content, engine.scenes.content_mb, engine.scenes.raster_us
        );
    }
    for source in &engine.sources {
        let size = |w: u32, h: u32| match (source.fps, source.received_fps) {
            (Some(fps), Some(got)) => format!("{w}×{h} at {got:.1} fps (opened at {fps})"),
            _ => format!("{w}×{h}"),
        };
        let what = match (source.width.zip(source.height), source.channels) {
            (Some((w, h)), Some(c)) => format!("{}, {c} ch", size(w, h)),
            (Some((w, h)), None) => size(w, h),
            (None, Some(c)) => format!("{c} ch"),
            (None, None) => String::new(),
        };
        match (&source.state, &source.error) {
            (SourceRun::WaitingForPermission, Some(error)) => {
                println!("  {}  {error}", source.source)
            }
            (SourceRun::Failed, Some(error)) => println!("  {}  failed: {error}", source.source),
            (state, _) => println!("  {}  {state:?} {what}", source.source),
        }
    }
    for filter in &engine.audio_filters {
        let state = serde_json::to_value(filter.state)
            .ok()
            .and_then(|state| state.as_str().map(String::from))
            .unwrap_or_default();
        let reason = filter
            .reason
            .as_ref()
            .map(|reason| format!(": {reason}"))
            .unwrap_or_default();
        println!(
            "  {} {} ({}): {state}{reason}; {} blocks, {:.1} µs each (p99 {:.1}), fuel {:.0} \
             (p99 {:.0}) of {}, {} failures",
            filter.path,
            filter.kind,
            filter.runs_in,
            filter.calls,
            filter.call_us.p50,
            filter.call_us.p99,
            filter.fuel.p50,
            filter.fuel.p99,
            filter.fuel_budget,
            filter.failures
        );
    }
}

fn frame(
    target: String,
    output: Option<PathBuf>,
    width: Option<u32>,
    json: bool,
) -> Result<ExitCode> {
    use base64::Engine;
    let params = FrameParams {
        target: target.clone(),
        max_width: width,
        after: None,
        timeout_ms: Some(5000),
        quality: Some(0.9),
        fps: None,
    };
    let frame: Frame = Client::connect_or_launch()?.call(Method::Frame(params))?;
    let jpeg = base64::engine::general_purpose::STANDARD
        .decode(&frame.jpeg_base64)
        .context("isocastd sent a picture that isn't base64")?;
    let path = output.unwrap_or_else(|| PathBuf::from(format!("{target}.jpg")));
    std::fs::write(&path, jpeg).with_context(|| format!("could not write {}", path.display()))?;
    let path = std::path::absolute(&path)?;
    if json {
        let value = serde_json::json!({
            "target": frame.target,
            "path": path,
            "width": frame.width,
            "height": frame.height,
            "seq": frame.seq,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!(
            "Saved {target} ({}×{}) to {}",
            frame.width,
            frame.height,
            path.display()
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// Save `seconds` of `target`'s monitor stream as Annex B H.264, and say
/// how it went: frames, keyframes, size, bitrate and how late frames
/// arrived after their time.
fn frames(
    target: String,
    output: Option<PathBuf>,
    width: Option<u32>,
    fps: Option<u32>,
    profile: Option<isocast_protocol::api::H264Profile>,
    seconds: f64,
    json: bool,
) -> Result<ExitCode> {
    use isocast_protocol::api::frames::{self, Record};
    use std::io::Write;
    let params = FramesParams {
        target: target.clone(),
        max_width: width,
        fps,
        profile,
    };
    let (reply, mut reader) = Client::connect_or_launch()?.frames(params)?;
    let started = std::time::Instant::now();
    // The host clock now, from the reply's reading of it.
    let host_now =
        |at: std::time::Instant| reply.host_time + at.duration_since(started).as_nanos() as u64;
    // Hanging up at the end also ends a wait for a frame that never comes.
    let connection = reader.get_ref().try_clone()?;
    let length = Duration::try_from_secs_f64(seconds.max(0.0)).unwrap_or(Duration::ZERO);
    std::thread::spawn(move || {
        std::thread::sleep(length);
        let _ = connection.shutdown(std::net::Shutdown::Both);
    });
    let path = output.unwrap_or_else(|| PathBuf::from(format!("{target}.h264")));
    let file = std::fs::File::create(&path)
        .with_context(|| format!("could not create {}", path.display()))?;
    let mut file = std::io::BufWriter::new(file);
    let mut parameter_sets: Vec<Vec<u8>> = Vec::new();
    let (mut count, mut keyframes, mut bytes, mut size) = (0u64, 0u64, 0u64, (0, 0));
    let mut lags = Vec::new();
    let mut ended = None;
    loop {
        let record = match Record::read(&mut reader) {
            Ok(Some(record)) => record,
            Ok(None) => break,
            // Hung up at the end of the time.
            Err(_) if started.elapsed() >= length => break,
            Err(err) => return Err(err).context("lost the frame stream"),
        };
        match record.kind {
            frames::CONFIG => parameter_sets = avcc_parameter_sets(&record.payload)?,
            frames::KEY_FRAME | frames::FRAME => {
                let arrived = host_now(std::time::Instant::now());
                lags.push(arrived.saturating_sub(record.seq) as f64 / 1e6);
                if record.kind == frames::KEY_FRAME {
                    keyframes += 1;
                    for set in &parameter_sets {
                        file.write_all(&[0, 0, 0, 1])?;
                        file.write_all(set)?;
                    }
                }
                write_annex_b(&mut file, &record.payload)?;
                count += 1;
                bytes += record.payload.len() as u64;
                size = (record.width, record.height);
            }
            frames::END => {
                ended = Some(String::from_utf8_lossy(&record.payload).into_owned());
                break;
            }
            _ => {}
        }
    }
    file.flush()?;
    let elapsed = started.elapsed().as_secs_f64().max(0.001);
    // END carries a failure reason, even when a partial capture was saved.
    // The normal requested-duration stop simply closes our connection.
    let failed = ended.is_some();
    lags.sort_by(f64::total_cmp);
    let lag = |q: f64| {
        lags.get(((lags.len() as f64 - 1.0) * q).round() as usize)
            .copied()
    };
    let path = std::path::absolute(&path)?;
    let kbps = bytes as f64 * 8.0 / elapsed / 1000.0;
    if json {
        let value = serde_json::json!({
            "target": target,
            "path": path,
            "codec": reply.codec,
            "width": size.0,
            "height": size.1,
            "frames": count,
            "keyframes": keyframes,
            "seconds": elapsed,
            "fps": count as f64 / elapsed,
            "bitrate_kbps": kbps,
            "lag_ms": { "median": lag(0.5), "p90": lag(0.9), "max": lag(1.0) },
            "ended": ended,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!(
            "Saved {count} frames ({keyframes} keyframes) of {target}, {}×{} at {:.1} fps, {:.0} kb/s, to {}",
            size.0,
            size.1,
            count as f64 / elapsed,
            kbps,
            path.display()
        );
        if let (Some(median), Some(p90)) = (lag(0.5), lag(0.9)) {
            println!(
                "Frames arrived {median:.1} ms after their time (median; 90% within {p90:.1} ms)"
            );
        }
        if let Some(why) = ended {
            println!("The stream ended: {why}");
        }
    }
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// The SPS and PPS NAL units in an `AVCDecoderConfigurationRecord`.
fn avcc_parameter_sets(avcc: &[u8]) -> Result<Vec<Vec<u8>>> {
    let mut sets = Vec::new();
    let mut at = 5;
    for count_mask in [0x1f, 0xff] {
        let count = usize::from(*avcc.get(at).context("a short avcC record")? & count_mask);
        at += 1;
        for _ in 0..count {
            let len = usize::from(u16::from_be_bytes(
                avcc.get(at..at + 2)
                    .context("a short avcC record")?
                    .try_into()?,
            ));
            sets.push(
                avcc.get(at + 2..at + 2 + len)
                    .context("a short avcC record")?
                    .to_vec(),
            );
            at += 2 + len;
        }
    }
    Ok(sets)
}

/// Write length-prefixed NAL units (AVCC) with start codes instead.
fn write_annex_b(out: &mut impl std::io::Write, mut data: &[u8]) -> Result<()> {
    while data.len() >= 4 {
        let len = u32::from_be_bytes(data[..4].try_into()?) as usize;
        let nal = data
            .get(4..4 + len)
            .context("a frame with a cut-off NAL unit")?;
        out.write_all(&[0, 0, 0, 1])?;
        out.write_all(nal)?;
        data = &data[4 + len..];
    }
    Ok(())
}

#[derive(Subcommand)]
enum EngineCommand {
    /// Start the live engine and keep it running.
    Start,
    /// Let the live engine stop once nothing needs it.
    Stop,
    /// Shut down only if recording/output work is idle; cancels an idle calibration.
    Shutdown,
}

fn engine(command: EngineCommand, json: bool) -> Result<ExitCode> {
    if matches!(command, EngineCommand::Shutdown) {
        Client::shutdown_if_idle()?;
        if json {
            println!("{{\"stopped\":true}}");
        } else {
            println!("Daemon: stopped");
        }
        return Ok(ExitCode::SUCCESS);
    }
    let running = matches!(command, EngineCommand::Start);
    let mut client = match running {
        true => Client::connect_or_launch()?,
        false => match Client::connect()? {
            Some(client) => client,
            None => {
                println!("{}", if json { "null" } else { "Live engine: stopped" });
                return Ok(ExitCode::SUCCESS);
            }
        },
    };
    let info: Option<EngineInfo> = client.call(Method::Engine { running })?;
    if json {
        println!("{}", serde_json::to_string_pretty(&info)?);
    } else {
        match &info {
            Some(info) if running => print_engine(info),
            Some(_) => println!("Live engine: still running for a recording or a client"),
            None => println!("Live engine: stopped"),
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn watch(levels: bool, kinds: &[String], except: &[String], json: bool) -> Result<ExitCode> {
    let read = |names: &[String], option: &str| -> Result<Vec<EventKind>> {
        names
            .iter()
            .map(|name| {
                serde_json::from_value(serde_json::json!(name.trim())).map_err(|_| {
                    anyhow::anyhow!(
                        "--{option}: {name:?} isn't a kind of event; kinds: {}",
                        EventKind::NAMES.join(", ")
                    )
                })
            })
            .collect()
    };
    let mut kinds = read(kinds, "kinds")?;
    let mut except = read(except, "except")?;
    // Levels come 20 times a second: isocastd sends them only when asked.
    if levels {
        if !kinds.is_empty() && !kinds.contains(&EventKind::Levels) {
            kinds.push(EventKind::Levels);
        }
    } else if !kinds.contains(&EventKind::Levels) && !except.contains(&EventKind::Levels) {
        except.push(EventKind::Levels);
    }
    let params = WatchParams {
        kinds: (!kinds.is_empty()).then_some(kinds),
        except,
    };
    let remote = !isocast_client::Engine::selected()?.is_local();
    loop {
        let (live, events) = match Client::connect_or_launch()
            .and_then(|client| client.watch_some(Some(params.clone())))
        {
            Ok(watching) => watching,
            Err(error) if remote && error.downcast_ref::<DaemonError>().is_none() => {
                eprintln!("Reconnecting… {error}");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
            Err(error) => return Err(error),
        };
        if json {
            // What the method returns first: the live setup and its revision.
            let watching = serde_json::json!({ "type": "watching", "live": live });
            println!("{}", serde_json::to_string(&watching)?);
        } else {
            println!(
                "Watching isocastd (live setup revision {}); Ctrl-C to stop.",
                live.revision
            );
        }
        for event in events {
            let event = match event {
                Ok(event) => event,
                Err(error) if remote => {
                    eprintln!("Reconnecting… {error}");
                    break;
                }
                Err(error) => return Err(error),
            };
            if json {
                println!("{}", serde_json::to_string(&event)?);
                continue;
            }
            match event {
                Event::SetupChanged { revision, by } => println!(
                    "setup changed: revision {revision}{}",
                    by.map(|by| format!(" (by {by})")).unwrap_or_default()
                ),
                Event::Started {
                    files,
                    begins_at,
                    take,
                    folder,
                    ..
                } => {
                    let take = take.map(|n| format!(" {n}")).unwrap_or_default();
                    let folder = folder
                        .map(|f| format!(" in {}", f.display()))
                        .unwrap_or_default();
                    match begins_at {
                        Some(at) => println!("new take{take} (from {at}){folder}:"),
                        None => println!("recording started{folder}:"),
                    }
                    for file in files {
                        println!("  {}", file.display());
                    }
                }
                Event::Status { live } => {
                    let files: Vec<String> = live
                        .files
                        .iter()
                        .map(|f| format!("{} {} frames", f.source, f.frames))
                        .collect();
                    println!(
                        "recording {:.0} s: {}",
                        live.elapsed_seconds,
                        files.join(", ")
                    );
                }
                Event::Warning { message } => println!("warning: {message}"),
                Event::RecordingPaused { recording, .. } => println!(
                    "{} at {:.2} s; {:.2} s paused",
                    if recording.paused {
                        "paused"
                    } else {
                        "resumed"
                    },
                    recording.elapsed_seconds,
                    recording.paused_seconds
                ),
                Event::Stream { stream, .. } => {
                    print!("stream: ");
                    print_stream_lines(&stream);
                }
                Event::VirtualCamera { camera } => virtual_camera::print(&camera),
                Event::Replay { replay } => replay::print(&replay),
                Event::ReplaySaved { saved } => println!(
                    "{}replay saved: {}",
                    if saved.canvas.is_main() {
                        ""
                    } else {
                        "vertical "
                    },
                    saved.path
                ),
                Event::SceneChanged {
                    scene,
                    take,
                    time_seconds,
                    transition,
                } => {
                    let how = how(transition.as_ref());
                    match take.zip(time_seconds) {
                        Some((take, t)) => {
                            println!("scene {scene:?} on air{how} at {t:.2} s in take {take}")
                        }
                        None => println!("scene {scene:?} on air{how}"),
                    }
                }
                Event::CanvasSceneChanged {
                    canvas,
                    scene,
                    take,
                    time_seconds,
                    transition,
                } => {
                    let how = how(transition.as_ref());
                    let canvas = serde_json::to_value(canvas)?;
                    let canvas = canvas.as_str().unwrap_or("other");
                    match take.zip(time_seconds) {
                        Some((take, t)) => println!(
                            "scene {scene:?} on air on the {canvas} canvas{how} at {t:.2} s in take \
                         {take}"
                        ),
                        None => println!("scene {scene:?} on air on the {canvas} canvas{how}"),
                    }
                }
                Event::CanvasPreviewChanged { canvas, scene } => {
                    let canvas = serde_json::to_value(canvas)?;
                    let canvas = canvas.as_str().unwrap_or("other");
                    match scene {
                        Some(scene) => println!("scene {scene:?} in the {canvas} canvas's preview"),
                        None => println!("the {canvas} canvas's preview cleared"),
                    }
                }
                Event::PreviewChanged { scene } => println!("scene {scene:?} in preview"),
                Event::PreviewCleared => println!("preview cleared"),
                Event::Marker {
                    label,
                    take,
                    time_seconds,
                    ..
                } => println!("marker {label:?} at {time_seconds:.2} s in take {take}"),
                Event::Stopped { report, error, .. } => match (report, error) {
                    (Some(report), _) => println!(
                        "recording stopped after {:.2} s; manifest {}",
                        report.duration_seconds,
                        report.manifest.display(),
                    ),
                    (None, Some(error)) => println!("recording failed: {error}"),
                    (None, None) => println!("recording stopped"),
                },
                Event::Levels { levels } => {
                    let levels: Vec<String> = levels
                        .iter()
                        .map(|l| format!("{} {:.0} dB", l.source, l.peak_db))
                        .collect();
                    println!("levels: {}", levels.join(", "));
                }
                Event::SourceChanged { status } => match &status.error {
                    Some(error) => {
                        println!("source {}: {:?}: {error}", status.source, status.state)
                    }
                    None => println!("source {}: {:?}", status.source, status.state),
                },
                Event::EngineChanged { running } => println!(
                    "live engine {}",
                    if running { "started" } else { "stopped" }
                ),
                Event::LibraryChanged {
                    action,
                    kind,
                    id,
                    collection,
                    profile,
                } => {
                    let what = match (kind, id) {
                        (Some(kind), Some(id)) => format!("{} {id:?}", kind.label()),
                        _ => "scene collection and profile".into(),
                    };
                    let action = serde_json::to_value(action)?;
                    println!(
                        "{what} {}; in use: collection {}, profile {}",
                        action.as_str().unwrap_or_default(),
                        collection.as_deref().unwrap_or("none"),
                        profile.as_deref().unwrap_or("none")
                    );
                }
                Event::PluginChanged {
                    id,
                    action,
                    state,
                    process,
                } => {
                    let action = serde_json::to_value(action)?;
                    let state = serde_json::to_value(state)?;
                    let process = process
                        .map(serde_json::to_value)
                        .transpose()?
                        .map(|p| format!(", process {}", p.as_str().unwrap_or_default()))
                        .unwrap_or_default();
                    println!(
                        "plugin {id} {}: {}{process}",
                        action.as_str().unwrap_or_default(),
                        state.as_str().unwrap_or_default()
                    );
                }
                Event::DataChanged { revision, keys } => {
                    println!("data changed (revision {revision}): {}", keys.join(", "));
                }
                Event::HotkeyFired {
                    binding,
                    action,
                    source,
                    phase,
                    error,
                } => {
                    println!(
                        "hotkey {binding}: {action} ({source:?}, {phase:?}){}",
                        error.map(|e| format!(": {e}")).unwrap_or_default()
                    );
                }
                Event::TakeFinished {
                    take,
                    manifest,
                    status,
                    duration_seconds,
                    ..
                } => println!(
                    "take {take} finished ({}{}): {}",
                    serde_json::to_value(status)?.as_str().unwrap_or_default(),
                    duration_seconds
                        .map(|d| format!(", {d:.2} s"))
                        .unwrap_or_default(),
                    manifest.display()
                ),
                Event::ProjectCreated { project } => println!(
                    "project {} {:?} created: {}",
                    project.id,
                    project.name,
                    project.folder.display()
                ),
                Event::ProjectRenamed {
                    project, old_name, ..
                } => println!(
                    "project {} renamed from {old_name:?} to {:?}: {}",
                    project.id,
                    project.name,
                    project.folder.display()
                ),
                Event::DeviceChanged {
                    action,
                    kind,
                    id,
                    name,
                } => println!(
                    "{} {name:?} ({id}) {}",
                    serde_json::to_value(kind)?.as_str().unwrap_or_default(),
                    serde_json::to_value(action)?.as_str().unwrap_or_default()
                ),
                Event::Projectors { projectors, reason } => {
                    if let Some(reason) = reason {
                        println!("projector closed: {reason}");
                    }
                    println!("projectors: {} open", projectors.len());
                    for projector in &projectors {
                        print!("  ");
                        projector::print_one(projector);
                    }
                }
                Event::EncoderOverload { overload } => {
                    println!(
                        "encoder overload in take {}: {}",
                        overload.take, overload.reason
                    );
                    for fix in overload.estimate.iter().flat_map(|e| &e.suggestions) {
                        match &fix.change {
                            Some(change) => println!(
                                "  fix: {}: isocast plan --patch '{}'",
                                fix.description,
                                serde_json::to_string(change)?
                            ),
                            None => println!("  fix: {}", fix.description),
                        }
                    }
                }
                Event::GovernorChanged { governor } => println!("{}", governor.reason),
                Event::RemoteHeartbeat => {}
                Event::RemoteClientConnected { peer } => println!(
                    "remote client connected: {} ({})",
                    peer.name, peer.client_id
                ),
                Event::RemoteClientDisconnected { peer } => println!(
                    "remote client disconnected: {} ({})",
                    peer.name, peer.client_id
                ),
                Event::RemotePaired { client } => {
                    println!("paired: {} ({})", client.name, client.id)
                }
                Event::RemoteRevoked { client_id } => {
                    println!("remote client revoked: {client_id}")
                }
                Event::RemoteDisabled => println!("remote control disabled"),
                Event::Refused {
                    method,
                    message,
                    by,
                } => println!(
                    "refused {method}{}: {message}",
                    by.map(|by| format!(" from {by}")).unwrap_or_default()
                ),
            }
        }
        // Events end only when isocastd hangs up; a watcher must not mistake
        // that for a quiet stretch.
        if remote {
            eprintln!("Reconnecting…");
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        anyhow::bail!(
            "lost the connection to isocastd (it stopped or restarted, or this watcher fell too far \
         behind); events from now on are not shown, so run `isocast watch` again"
        )
    }
}

/// Read a stream key: from stdin, or asked for without echo in a terminal.
fn read_stream_key() -> Result<String> {
    read_secret("stream key", "Stream key")
}

/// Read a secret (`what`): from stdin, or asked for (`label`) without echo
/// in a terminal.
fn read_secret(what: &str, label: &str) -> Result<String> {
    use std::io::{BufRead, IsTerminal, Read, Write};
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let mut key = String::new();
        stdin
            .lock()
            .read_to_string(&mut key)
            .with_context(|| format!("could not read the {what} from stdin"))?;
        return Ok(key.trim().to_string());
    }
    let _echo = EchoOff::new()?;
    eprint!("{label} (not shown): ");
    std::io::stderr().flush()?;
    let mut key = String::new();
    stdin
        .lock()
        .read_line(&mut key)
        .with_context(|| format!("could not read the {what}"))?;
    isocast_diagnostics::diagnostic!();
    Ok(key.trim().to_string())
}

/// The terminal stops echoing typed characters while this lives.
struct EchoOff(
    #[cfg(target_os = "macos")] Option<libc::termios>,
    #[cfg(windows)]
    (
        windows::Win32::Foundation::HANDLE,
        windows::Win32::System::Console::CONSOLE_MODE,
    ),
);

impl EchoOff {
    #[cfg(target_os = "macos")]
    fn new() -> Result<Self> {
        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        unsafe {
            if libc::tcgetattr(0, original.as_mut_ptr()) != 0 {
                return Ok(Self(None));
            }
            let original = original.assume_init();
            let mut quiet = original;
            quiet.c_lflag &= !libc::ECHO;
            quiet.c_lflag |= libc::ECHONL;
            libc::tcsetattr(0, libc::TCSANOW, &quiet);
            Ok(Self(Some(original)))
        }
    }

    #[cfg(windows)]
    fn new() -> Result<Self> {
        use windows::Win32::System::Console::{
            CONSOLE_MODE, ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE,
            SetConsoleMode,
        };
        let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) }
            .context("could not open the console for hidden secret input")?;
        let mut original = CONSOLE_MODE::default();
        unsafe { GetConsoleMode(handle, &mut original) }
            .context("could not read the console mode; pipe the secret on stdin instead")?;
        unsafe { SetConsoleMode(handle, original & !ENABLE_ECHO_INPUT) }
            .context("could not disable console echo; secret input was not read")?;
        Ok(Self((handle, original)))
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    fn new() -> Result<Self> {
        Ok(Self())
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        if let Some(original) = &self.0 {
            unsafe { libc::tcsetattr(0, libc::TCSANOW, original) };
        }
        #[cfg(windows)]
        {
            let (handle, mode) = self.0;
            let _ = unsafe { windows::Win32::System::Console::SetConsoleMode(handle, mode) };
        }
    }
}

fn print_stream_key(stored: &StreamKey, json: bool) -> Result<ExitCode> {
    if json {
        println!("{}", serde_json::to_string_pretty(stored)?);
    } else if stored.key_set {
        println!(
            "Stream key for {} kept in the {}.",
            stored.service.id(),
            wording::CREDENTIAL_STORE
        );
    } else {
        println!("No stream key kept for {} now.", stored.service.id());
    }
    Ok(ExitCode::SUCCESS)
}

impl DestinationFields {
    fn params(self) -> StreamDestinationParams {
        StreamDestinationParams {
            server: self.server,
            video_bitrate_kbps: self.video_bitrate_kbps,
            width: self.width,
            height: self.height,
            rate_control: self.rate_control,
            keyframe_interval_seconds: self.keyframe_interval_seconds,
            audio_bitrate_kbps: self.audio_bitrate_kbps,
            ..Default::default()
        }
    }
}

/// What `stream start`, `stop` and `status` print: every destination (or
/// the one there is). Fails when the destination they touched failed.
fn print_streaming(streaming: &Streaming, json: bool) -> Result<ExitCode> {
    if json {
        println!("{}", serde_json::to_string_pretty(streaming)?);
    } else if streaming.destinations.is_empty() {
        print_stream_lines(&streaming.stream);
    } else {
        print_destinations(&streaming.destinations);
    }
    // Any destination failed (a start or stop names each one's state).
    let failed = streaming.stream.state == StreamState::Failed
        || streaming
            .destinations
            .iter()
            .any(|d| d.state == StreamState::Failed);
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn print_destinations(destinations: &[StreamStatus]) {
    for stream in destinations {
        print_stream_lines(stream);
    }
}

fn print_stream_lines(stream: &StreamStatus) {
    let state = match stream.state {
        StreamState::Offline => "offline",
        StreamState::Connecting => "connecting",
        StreamState::Live => "live",
        StreamState::Reconnecting => "reconnecting",
        StreamState::Failed => "failed",
    };
    let server = if stream.server.is_empty() {
        "no server".to_string()
    } else {
        stream.server.clone()
    };
    let main = stream.id == MAIN_DESTINATION;
    println!(
        "{}{} {state}: {} ({server}){}{}",
        if stream.canvas.is_main() {
            "Stream"
        } else {
            "Vertical stream"
        },
        if main {
            String::new()
        } else {
            format!(" to {}", stream.id)
        },
        stream.service.id(),
        if stream.enabled || stream.is_active() {
            ""
        } else {
            "; turned off"
        },
        match (stream.key_set, main) {
            (true, _) => String::new(),
            (false, true) => "; no stream key set (`isocast stream key set`)".into(),
            (false, false) => format!(
                "; no stream key set (`isocast stream key set --destination {}`)",
                stream.id
            ),
        }
    );
    let ended = matches!(stream.state, StreamState::Offline | StreamState::Failed);
    if ended && stream.frames_sent > 0 && stream.elapsed_seconds > 0.0 {
        // The current rates are zero once a stream ends; show what it
        // averaged instead.
        println!(
            "  {}×{} at {} fps for {:.0} s: {:.0} kb/s sent on average (target {} + {})",
            stream.width,
            stream.height,
            stream.fps,
            stream.elapsed_seconds,
            stream.bytes_sent as f64 * 8.0 / 1000.0 / stream.elapsed_seconds,
            stream.target_video_kbps,
            stream.target_audio_kbps
        );
    } else if stream.state != StreamState::Offline || stream.frames_sent > 0 {
        println!(
            "  {}×{} at {} fps for {:.0} s: {:.0} kb/s video + {:.0} kb/s audio ({:.0} kb/s \
             sent; target {} + {})",
            stream.width,
            stream.height,
            stream.fps,
            stream.elapsed_seconds,
            stream.video_kbps,
            stream.audio_kbps,
            stream.total_kbps,
            stream.target_video_kbps,
            stream.target_audio_kbps
        );
    }
    if stream.state != StreamState::Offline || stream.frames_sent > 0 {
        println!(
            "  {} frames sent, {} dropped, {} audio dropouts, congestion {:.0}%, {} reconnect(s)",
            stream.frames_sent,
            stream.dropped_frames,
            stream.audio_dropouts,
            stream.congestion * 100.0,
            stream.reconnects
        );
    }
    if let Some(error) = &stream.last_error {
        println!("  last error: {error}");
    }
    if let Some(warning) = &stream.bitrate_warning {
        println!("  warning: {warning}");
    }
}

/// Formats shown per camera before the rest are left to --json.
const MAX_FORMATS_SHOWN: usize = 8;

fn devices(json: bool) -> Result<ExitCode> {
    let devices = list_devices()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&devices)?);
        return Ok(ExitCode::SUCCESS);
    }

    println!("Displays");
    for d in &devices.displays {
        let main = if d.main { " (main)" } else { "" };
        println!(
            "  {}{main}  {}×{} @ {} Hz  id {}",
            d.name, d.width, d.height, d.refresh_rate, d.id
        );
    }
    if !devices.windows.is_empty() {
        println!("Windows");
        for w in &devices.windows {
            let title = w.title.as_deref().unwrap_or("(untitled)");
            println!(
                "  {} — {title}  {}×{}  id {}",
                w.app, w.width, w.height, w.id
            );
        }
    }
    println!("Cameras");
    if devices.cameras.is_empty() {
        println!("  (none)");
    }
    for c in &devices.cameras {
        println!("  {} [{}]  id {}", c.name, c.kind.label(), c.id);
        for f in c.formats.iter().take(MAX_FORMATS_SHOWN) {
            // Each range as "1–30", or its top rate if that's all it is.
            let rates: Vec<String> = if f.frame_rate_ranges.is_empty() {
                f.frame_rates.iter().map(|r| r.to_string()).collect()
            } else {
                f.frame_rate_ranges
                    .iter()
                    .map(|r| {
                        if r.max - r.min < 0.5 {
                            r.max.to_string()
                        } else {
                            format!("{}–{}", r.min, r.max)
                        }
                    })
                    .collect()
            };
            println!(
                "    {}×{} {}  {} fps",
                f.width,
                f.height,
                f.pixel_format,
                rates.join(", ")
            );
        }
        if c.formats.len() > MAX_FORMATS_SHOWN {
            println!(
                "    … {} more (see --json)",
                c.formats.len() - MAX_FORMATS_SHOWN
            );
        }
    }
    println!("Microphones");
    if devices.microphones.is_empty() {
        println!("  (none)");
    }
    for m in &devices.microphones {
        let default = if m.default { " (default)" } else { "" };
        let rates: Vec<String> = m.sample_rates.iter().map(|r| r.to_string()).collect();
        println!(
            "  {}{default}  {} Hz, {} ch  id {}",
            m.name,
            rates.join("/"),
            m.channels,
            m.id
        );
        // The channels a source's `channels` picks from, when named.
        if !m.channel_names.is_empty() {
            let names: Vec<String> = m
                .channel_names
                .iter()
                .enumerate()
                .map(|(i, name)| match name.as_str() {
                    "" => format!("{}", i + 1),
                    name => format!("{} {name}", i + 1),
                })
                .collect();
            println!("    channels: {}", names.join(", "));
        }
        // What a source without `channels` records.
        if m.channels > 2 {
            println!("    default: inputs 1 + 2 (stereo); `channels` picks others");
        }
    }
    println!("Audio outputs (for monitoring)");
    if devices.audio_outputs.is_empty() {
        println!("  (none)");
    }
    for o in &devices.audio_outputs {
        let default = if o.default { " (default)" } else { "" };
        println!(
            "  {}{default}  {} Hz, {} ch  id {}",
            o.name, o.sample_rate, o.channels, o.id
        );
    }
    if !devices.audio_apps.is_empty() {
        println!("Apps with audio (for app-audio and system-audio's exclude)");
        for a in &devices.audio_apps {
            let playing = if a.playing { " (playing)" } else { "" };
            let id = if a.id.is_empty() {
                a.bundle_id.as_deref().unwrap_or("-")
            } else {
                &a.id
            };
            print!("  {}{playing}  {id}  pid {}", a.name, a.pid);
            if let Some(binary) = &a.binary {
                print!("  binary {binary}");
            }
            println!();
        }
    }
    let p = &devices.permissions;
    println!(
        "Permissions: screen recording {}, camera {}, microphone {}, system audio recording {}",
        p.screen_recording.label(),
        p.camera.label(),
        p.microphone.label(),
        p.system_audio.label()
    );
    for note in &devices.notes {
        println!("note: {note}");
    }
    Ok(ExitCode::SUCCESS)
}

/// One status line for monitoring.
fn print_monitor(monitor: &isocast_protocol::api::MonitorStatus) {
    let device = monitor.output.as_deref().unwrap_or(&monitor.device);
    if let Some(error) = &monitor.error {
        println!("  monitor  off: {error}");
        return;
    }
    let silent = if monitor.silent { " (silent)" } else { "" };
    println!(
        "  monitor  {} on {device}{silent}  L {:.0} / R {:.0} dB peak, {:.0} ms buffered, \
         {} underruns",
        monitor.sources.join(", "),
        monitor.peak_db[0],
        monitor.peak_db[1],
        monitor.buffered_ms,
        monitor.underruns
    );
}

/// `--monitor`: a monitoring type by its setup name.
fn parse_monitoring(value: &str) -> Result<isocast_protocol::Monitoring, String> {
    serde_json::from_value(serde_json::Value::String(value.into())).map_err(|_| {
        format!("{value:?} is not a monitoring type; valid: off, monitor_only, monitor_and_output")
    })
}

fn parse_h264_profile(value: &str) -> Result<isocast_protocol::api::H264Profile, String> {
    serde_json::from_value(serde_json::Value::String(value.into())).map_err(|_| {
        format!("{value:?} is not an H.264 profile; valid: high, constrained_baseline")
    })
}

/// Devices and permissions as the daemon sees them: it records, so its
/// permissions are the ones that count.
fn list_devices() -> Result<Devices> {
    Client::connect_or_launch()?.call(Method::Devices)
}
