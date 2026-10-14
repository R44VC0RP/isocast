use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use isocast_client::DaemonError;
use isocast_engine::record::{LiveStatus, RecordOptions, Report};
use isocast_protocol::api::{
    Applied, ApplyParams, Capacity, CheckStatus, DoctorParams, EngineInfo, EngineState, Event,
    EventKind, Frame, FrameParams, FramesParams, ImportObsParams, Live, Marked, Method, Plan,
    Refreshed, RpcError, Snapshot, SnapshotParams, SourceRun, StartParams, Started, Status,
    StreamKey, StreamKeyParams, StreamKeyTarget, Switched, TuneParams, Tuning, WatchParams,
};
use isocast_protocol::change::{Change, Difference, DifferenceOp};
use isocast_protocol::devices::Devices;
use isocast_protocol::obs::Imported;
use isocast_protocol::stream::{StreamService, StreamState, StreamStatus};
use isocast_protocol::{QUICK_SOURCES, Setup, Transition};

use crate::client::Client;

mod client;
mod data;
mod hotkeys;
mod library;
mod mcp;
mod plugin;
#[cfg(target_os = "macos")]
mod shared_frames;
mod timeline;
mod virtual_camera;

#[cfg(target_os = "macos")]
fn shared_frames(options: shared_frames::Options) -> Result<ExitCode> {
    shared_frames::run(options)
}

#[cfg(not(target_os = "macos"))]
mod shared_frames {
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

#[cfg(not(target_os = "macos"))]
fn shared_frames(_options: shared_frames::Options) -> Result<ExitCode> {
    bail!("--shared is only available on macOS so far; leave it out to watch H.264")
}

/// Isocast: scene-based recording with per-source ISO files, controlled from
/// the command line or by agents.
#[derive(Parser)]
#[command(
    name = "isocast",
    version,
    after_help = "Getting started: `isocast permissions` shows what macOS allows, `isocast \
                  setup init --apply` makes a setup from this Mac's devices, `isocast frame` \
                  saves a picture of the program, `isocast start --seconds 10` records a test, \
                  and `isocast status` shows how it went. Agents: `isocast mcp`."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
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
    /// Timelines: takes in order, joined into one file by `export`. While
    /// one is current, every take that finishes (`stop`, or a `split`) is
    /// added to its end; `retake` records the last one again.
    Timeline {
        #[command(subcommand)]
        command: timeline::TimelineCommand,
    },
    /// Record the last take again. Not recording (or while a stop is
    /// finishing, once it has): take the current timeline's last clip off
    /// (its files stay) and start recording. Recording: drop the take being
    /// recorded from the timeline (its files stay) and begin a new take
    /// about a quarter second later, without stopping any device; returns
    /// once it has begun.
    Retake {
        /// Write the new take here; else where the take it replaces is.
        #[arg(long)]
        folder: Option<PathBuf>,
        /// Stop on its own after this many seconds.
        #[arg(long)]
        seconds: Option<f64>,
        #[arg(long)]
        json: bool,
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
        /// camera:NAME_OR_ID, mic[:NAME_OR_ID], system-audio,
        /// test-pattern[:WIDTHxHEIGHT[@FPS]] or tone[:HZ]. Repeat for more
        /// sources; every audio source goes into every file.
        #[arg(long = "source", value_name = "SPEC")]
        sources: Vec<String>,
        /// Stop after this many seconds.
        #[arg(long)]
        seconds: Option<f64>,
        /// Write files here instead of the setup's recording.folder.
        #[arg(long)]
        folder: Option<PathBuf>,
        /// Print the result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Start recording in the background (in isocastd, launched if needed)
    /// and return once every file is created. Same options as `record`;
    /// with neither a setup nor --source, records the live setup. A setup
    /// or --source given here becomes the live setup.
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
    /// Stop the background recording and print its report.
    Stop {
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
        #[arg(long)]
        json: bool,
    },
    /// Put the preview scene on air (with its transition unless --cut,
    /// --fade MS or --transition is given); the scene that was on air moves
    /// to preview.
    Take {
        #[command(flatten)]
        transition: TransitionArgs,
        #[arg(long)]
        json: bool,
    },
    /// Check this Mac and a setup: permissions, hardware encoders, whether
    /// the setup's devices are connected, and the recording disk's space and
    /// speed. Exits with an error if anything would stop a recording.
    Doctor {
        setup: Option<PathBuf>,
        /// Check this folder instead of the setup's recording.folder.
        #[arg(long)]
        folder: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Benchmark the encoders on a setup's files (the live setup if none is
    /// given), all at once as in a recording, and say whether this Mac keeps
    /// up (fits, keeps up with no margin, or can't keep up). If it doesn't
    /// fit, try changes closest first (a smaller program file, a lower
    /// quality preset, half the frame rate, one ISO file fewer) and
    /// recommend the closest that fits, as a patch for `plan` and `apply`.
    /// `start` warns when a setup exceeds what this measured. Runs in
    /// isocastd.
    Tune {
        setup: Option<PathBuf>,
        /// Benchmark writing to this folder instead of recording.folder.
        #[arg(long)]
        folder: Option<PathBuf>,
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
    /// Restart isocastd, for example after granting it screen recording
    /// permission. Refused while recording.
    Restart,
    /// Show the screen recording, camera and microphone permissions and
    /// what to do about each that isn't granted; with --request, ask macOS
    /// for the ones not decided yet (its prompts appear) and wait up to a
    /// minute for the answers; with --open, open the System Settings pane
    /// where one is turned on.
    Permissions {
        #[arg(long, conflicts_with = "open")]
        request: bool,
        /// screen_recording, camera or microphone.
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
    Stream {
        #[command(subcommand)]
        command: StreamCommand,
    },
    /// Feed a picture to Isocast Virtual Camera (extension approval required once).
    Vcam {
        #[command(subcommand)]
        command: virtual_camera::Command,
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
        /// plugin_changed, data_changed, timeline_changed, export_progress,
        /// export_finished).
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
        /// `program`, `preview` or a source id.
        #[arg(default_value = "program")]
        target: String,
        /// Where to save it; `<target>.jpg` if absent.
        #[arg(long, short)]
        output: Option<PathBuf>,
        /// Scale it down to at most this many pixels wide.
        #[arg(long)]
        width: Option<u32>,
        #[arg(long)]
        json: bool,
    },
    /// Record what a monitor shows: the live engine's low-latency H.264
    /// stream of the program (the default), studio mode's preview or a
    /// video source, saved as an H.264 elementary stream (Annex B, which
    /// ffprobe and ffplay read). Starts the engine if it isn't running.
    Frames {
        /// `program`, `preview` or a source id.
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
    /// Serve the control API to agents over MCP, on stdin and stdout.
    Mcp,
}

#[derive(Subcommand)]
enum StreamCommand {
    /// Go live: stream the program (the canvas and the program mix) to the
    /// live setup's [stream] service, starting the live engine if needed.
    /// Recordings start and stop without touching it. Returns once live,
    /// or says why not.
    Start {
        #[arg(long)]
        json: bool,
    },
    /// Stop streaming.
    Stop {
        #[arg(long)]
        json: bool,
    },
    /// Show the stream's state, bitrates, dropped frames, congestion and
    /// reconnects, and whether a stream key is set.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Keep or remove the stream key (in the Keychain, never in a setup).
    Key {
        #[command(subcommand)]
        command: StreamKeyCommand,
    },
}

#[derive(Subcommand)]
enum StreamKeyCommand {
    /// Store the stream key for the live setup's service (or --service).
    /// Reads it from stdin, or asks for it without echoing it in a
    /// terminal.
    Set {
        #[command(flatten)]
        target: KeyTargetArgs,
        #[arg(long)]
        json: bool,
    },
    /// Remove the stored stream key for the live setup's service (or
    /// --service).
    Clear {
        #[command(flatten)]
        target: KeyTargetArgs,
        #[arg(long)]
        json: bool,
    },
}

/// Which service's key; the live setup's [stream] if absent.
#[derive(clap::Args)]
struct KeyTargetArgs {
    /// custom, twitch, youtube, kick or facebook.
    #[arg(long, value_parser = parse_service)]
    service: Option<StreamService>,
    /// For custom: the server the key belongs to (rtmp://host/app).
    #[arg(long)]
    server: Option<String>,
}

impl KeyTargetArgs {
    fn target(self) -> StreamKeyTarget {
        StreamKeyTarget {
            service: self.service,
            server: self.server,
        }
    }
}

fn parse_service(text: &str) -> Result<StreamService, String> {
    StreamService::from_id(text)
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
        #[arg(long)]
        folder: Option<String>,
        /// The setup's name.
        #[arg(long)]
        name: Option<String>,
        /// Print the result (setup, notes, applied change) as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Import an OBS Studio scene collection as a setup: its sources,
    /// scenes and items, filters and transition, matched against this
    /// Mac's devices (in isocastd). Prints the setup as TOML, or writes it
    /// to --output, and lists what couldn't come over.
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
    // other tools do, not panic on the closed pipe. Not for an in-process
    // recording (its files must finish) or the MCP server.
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

fn run(cli: Cli) -> Result<ExitCode> {
    match cli.command {
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
                let (width, height, fps) = parse_canvas(canvas.as_deref())?;
                // The daemon may run in another folder: make a relative
                // folder absolute here.
                let folder = match folder {
                    Some(f) if !f.starts_with('~') && !f.starts_with('/') => {
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
        Command::Timeline { command } => timeline::run(command),
        Command::Retake {
            folder,
            seconds,
            json,
        } => timeline::retake(folder, seconds, json),
        Command::Hotkeys { command } => hotkeys::run(command),
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
        Command::Stop { json } => stop(json),
        Command::Pause { json } => pause(true, json),
        Command::Resume { json } => pause(false, json),
        Command::Status { json } => status(json),
        Command::Get { resolved, json } => get(resolved, json),
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
        Command::Tune {
            setup,
            folder,
            json,
        } => tune(setup.as_deref(), folder, json),
        Command::Split { json } => split(json),
        Command::Marker { label, json } => marker(label, json),
        Command::Refresh { source, json } => refresh(source, json),
        Command::Scene {
            scene,
            transition,
            json,
        } => {
            let transition = transition.get()?;
            switched(Method::SwitchScene { scene, transition }, json)
        }
        Command::Preview { scene, clear, json } => switched(
            if clear {
                Method::ClearPreview
            } else {
                Method::Preview {
                    scene: scene.expect("required by clap"),
                }
            },
            json,
        ),
        Command::Take { transition, json } => {
            let transition = transition.get()?;
            switched(Method::Take { transition }, json)
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
            json,
        } => frame(target, output, width, json),
        Command::Frames {
            target,
            output,
            width,
            height,
            fps,
            seconds,
            shared,
            slots,
            filtered,
            check,
            expect,
            touch,
            json,
        } => match shared {
            true => shared_frames(shared_frames::Options {
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
            false => frames(target, output, width, fps, seconds, json),
        },
        Command::Engine { command, json } => engine(command, json),
        Command::Vcam { command } => virtual_camera::run(command),
        Command::Stream { command } => match command {
            StreamCommand::Start { json } => {
                let status: StreamStatus =
                    Client::connect_or_launch()?.call(Method::StreamStart)?;
                print_stream(&status, json)
            }
            StreamCommand::Stop { json } => {
                let Some(mut client) = Client::connect()? else {
                    bail!("not streaming: isocastd isn't running");
                };
                let status: StreamStatus = client.call(Method::StreamStop)?;
                print_stream(&status, json)
            }
            StreamCommand::Status { json } => {
                let Some(mut client) = Client::connect()? else {
                    if json {
                        println!("null");
                    } else {
                        println!("Not streaming: isocastd isn't running");
                    }
                    return Ok(ExitCode::SUCCESS);
                };
                let status: Option<StreamStatus> = client.call::<Status>(Method::Status)?.stream;
                match status {
                    Some(status) => print_stream(&status, json),
                    None if json => {
                        println!("null");
                        Ok(ExitCode::SUCCESS)
                    }
                    None => {
                        println!(
                            "Not set up for streaming: the live setup has no [stream] section"
                        );
                        Ok(ExitCode::SUCCESS)
                    }
                }
            }
            StreamCommand::Key { command } => match command {
                StreamKeyCommand::Set { target, json } => {
                    let mut client = Client::connect_or_launch()?;
                    let key = read_stream_key()?;
                    let stored: StreamKey =
                        client.call(Method::StreamKeySet(Box::new(StreamKeyParams {
                            key,
                            target: target.target(),
                        })))?;
                    print_stream_key(&stored, json)
                }
                StreamKeyCommand::Clear { target, json } => {
                    let stored: StreamKey = Client::connect_or_launch()?
                        .call(Method::StreamKeyClear(Box::new(target.target())))?;
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
    if !folder.is_empty() && !folder.starts_with('/') && !folder.starts_with('~') {
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

    if json {
        let report = serde_json::json!({
            "valid": issues.is_empty(),
            "issues": issues,
            "setup": setup,
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
    Ok(code)
}

fn import_obs(
    collection: String,
    output: Option<PathBuf>,
    canvas: Option<&str>,
    profile: Option<String>,
    json: bool,
) -> Result<ExitCode> {
    let (width, height, fps) = parse_canvas(canvas)?;
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
fn parse_canvas(canvas: Option<&str>) -> Result<(Option<u32>, Option<u32>, Option<u32>)> {
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
    let mut header = vec!["Made by `isocast setup init` from this Mac's devices.".to_string()];
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
    let duration = duration(seconds)?;

    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst))
        .context("could not install the Ctrl-C handler")?;
    let announce = |paths: &[PathBuf], _manifest: &Path| {
        if json {
            return;
        }
        match duration {
            Some(d) => isocast_diagnostics::diagnostic!("Recording for {} s:", d.as_secs_f64()),
            None => isocast_diagnostics::diagnostic!("Recording, press Ctrl-C to stop:"),
        }
        for path in paths {
            isocast_diagnostics::diagnostic!("  {}", path.display());
        }
    };
    // Warn as soon as a file starts losing frames or audio.
    let reported = Mutex::new(Vec::<(u64, u64)>::new());
    let warn = |status: &LiveStatus| {
        if json {
            return;
        }
        for message in status.new_drops(&mut reported.lock().unwrap()) {
            isocast_diagnostics::diagnostic!("warn"; "warning: {message}");
        }
    };
    let options = RecordOptions {
        folder,
        duration,
        stop,
        on_start: Some(&announce),
        on_status: Some(&warn),
        on_take: None,
        control: None,
    };
    let report = record_with_platform(&setup, &options)?;
    print_report(&report, json)
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

fn stop(json: bool) -> Result<ExitCode> {
    let Some(mut client) = Client::connect()? else {
        bail!("not recording: isocastd isn't running");
    };
    let report: Report = match client.call(Method::Stop) {
        Ok(report) => report,
        Err(err) => {
            // Not recording: a recording with --seconds stops on its own, so
            // say how the last one ended. (Other errors are this stop's.)
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
                }) => bail!("{err:#}; the last recording failed: {error}"),
                Some(Status {
                    last: Some(last), ..
                }) => bail!(
                    "{err:#}; the last recording ({}) is done: {}",
                    last_summary(&last),
                    last.manifest.display()
                ),
                _ => return Err(err),
            }
        }
    };
    print_report(&report, json)
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

fn status(json: bool) -> Result<ExitCode> {
    let status = match Client::connect()? {
        Some(mut client) => client.call(Method::Status)?,
        None => Status {
            state: EngineState::Idle,
            scene: None,
            preview: None,
            live: None,
            files: Vec::new(),
            last: None,
            last_error: None,
            engine: None,
            setup_file: None,
            collection: None,
            profile: None,
            stream: None,
            cpu_percent: None,
            data: None,
            diagnostics: None,
            interrupted: Vec::new(),
            timeline: None,
            timeline_problem: None,
            export: None,
            virtual_camera: None,
            recording_free_bytes: None,
            program_bytes_per_second: None,
        },
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
        return Ok(ExitCode::SUCCESS);
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
        println!("CPU: {cpu:.1}% of this Mac (isocastd)");
    }
    if let Some(timeline) = &status.timeline {
        println!(
            "Timeline: {:?} ({}), {} clip{}, {:.2} s; finished takes are added to it",
            timeline.name,
            timeline.id,
            timeline.clips,
            if timeline.clips == 1 { "" } else { "s" },
            timeline.duration_seconds
        );
    }
    if let Some(problem) = &status.timeline_problem {
        println!("warning: {problem}");
    }
    if let Some(export) = &status.export {
        println!(
            "Exporting timeline {:?}: {:.0}% (export {})",
            export.timeline,
            export.progress * 100.0,
            export.export
        );
    }
    if let Some(free) = status.recording_free_bytes {
        println!("Recording disk: {:.1} GB free", free as f64 / 1e9);
    }
    if let Some(rate) = status.program_bytes_per_second {
        println!("Program file: up to ~{:.1} GB an hour", rate * 3600.0 / 1e9);
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
    if let Some(stream) = &status.stream {
        print_stream_lines(stream);
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
    }
    Ok(ExitCode::SUCCESS)
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
            && !path.starts_with('/')
            && !path.starts_with('~')
        {
            *file = std::env::current_dir()?.join(path).to_string_lossy().into();
        }
        if let Some(image) = value.get_mut("image")
            && let Some(path) = image.as_str()
            && !path.starts_with('/')
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
    let how = how(switched.transition.as_ref());
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
    request: bool,
    open: Option<isocast_protocol::devices::PermissionKind>,
    json: bool,
) -> Result<ExitCode> {
    // Ask the daemon: it records, so its permissions are the ones that count.
    let mut client = Client::connect_or_launch()?;
    if let Some(permission) = open {
        let opened: isocast_protocol::api::Opened =
            client.call(Method::OpenPermissionSettings { permission })?;
        if json {
            println!("{}", serde_json::to_string_pretty(&opened)?);
        } else {
            println!(
                "Opened System Settings at {}; turn isocastd's app on there.",
                permission.label()
            );
        }
        return Ok(ExitCode::SUCCESS);
    }
    if request && !json {
        // Only when macOS has something to ask.
        let now: isocast_protocol::devices::PermissionsReport = client.call(Method::Permissions)?;
        let undecided = now
            .details
            .iter()
            .any(|d| d.state == isocast_protocol::devices::Permission::NotDetermined);
        if undecided {
            isocast_diagnostics::diagnostic!(
                "Asking macOS; answer its prompts (waiting up to a minute)…"
            );
        }
    }
    let report: isocast_protocol::devices::PermissionsReport = if request {
        client.call(Method::RequestPermissions)?
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

fn get(resolved: bool, json: bool) -> Result<ExitCode> {
    let params = resolved.then_some(isocast_protocol::api::GetParams { resolved });
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
    seconds: f64,
    json: bool,
) -> Result<ExitCode> {
    use isocast_protocol::api::frames::{self, Record};
    use std::io::Write;
    let params = FramesParams {
        target: target.clone(),
        max_width: width,
        fps,
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
}

fn engine(command: EngineCommand, json: bool) -> Result<ExitCode> {
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
    let (live, events) = Client::connect_or_launch()?.watch_some(Some(params))?;
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
        let event = event?;
        if json {
            println!("{}", serde_json::to_string(&event)?);
            continue;
        }
        match event {
            Event::SetupChanged { revision } => println!("setup changed: revision {revision}"),
            Event::Started { files, begins_at } => {
                match begins_at {
                    Some(at) => println!("new take (from {at}):"),
                    None => println!("recording started:"),
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
            Event::RecordingPaused { recording } => println!(
                "{} at {:.2} s; {:.2} s paused",
                if recording.paused {
                    "paused"
                } else {
                    "resumed"
                },
                recording.elapsed_seconds,
                recording.paused_seconds
            ),
            Event::Stream { stream } => {
                print!("stream: ");
                print_stream_lines(&stream);
            }
            Event::VirtualCamera { camera } => virtual_camera::print(&camera),
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
            Event::PreviewChanged { scene } => println!("scene {scene:?} in preview"),
            Event::PreviewCleared => println!("preview cleared"),
            Event::Marker {
                label,
                take,
                time_seconds,
            } => println!("marker {label:?} at {time_seconds:.2} s in take {take}"),
            Event::Stopped { report, error } => match (report, error) {
                (Some(report), _) => println!(
                    "recording stopped after {:.2} s; manifest {}",
                    report.duration_seconds,
                    report.manifest.display()
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
                Some(error) => println!("source {}: {:?}: {error}", status.source, status.state),
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
            Event::TimelineChanged {
                timeline,
                action,
                clip,
                clips,
                ..
            } => {
                let action = serde_json::to_value(action)?;
                println!(
                    "timeline {timeline} {}{}: {clips} clip{}",
                    action.as_str().unwrap_or_default(),
                    clip.map(|clip| format!(" {clip}")).unwrap_or_default(),
                    if clips == 1 { "" } else { "s" }
                );
            }
            Event::ExportProgress {
                export, progress, ..
            } => println!("export {export}: {:.0}%", progress * 100.0),
            Event::ExportFinished {
                export,
                manifest,
                error,
                ..
            } => match error {
                None => println!("export {export} finished: {}", manifest.display()),
                Some(error) => println!("export {export} failed: {error}"),
            },
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
        }
    }
    // Events end only when isocastd hangs up; a watcher must not mistake
    // that for a quiet stretch.
    anyhow::bail!(
        "lost the connection to isocastd (it stopped or restarted, or this watcher fell too far \
         behind); events from now on are not shown, so run `isocast watch` again"
    )
}

/// Read a stream key: from stdin, or asked for without echo in a terminal.
fn read_stream_key() -> Result<String> {
    use std::io::{BufRead, IsTerminal, Read, Write};
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let mut key = String::new();
        stdin
            .lock()
            .read_to_string(&mut key)
            .context("could not read the stream key from stdin")?;
        return Ok(key.trim().to_string());
    }
    eprint!("Stream key (not shown): ");
    std::io::stderr().flush()?;
    let _echo = EchoOff::new();
    let mut key = String::new();
    stdin
        .lock()
        .read_line(&mut key)
        .context("could not read the stream key")?;
    isocast_diagnostics::diagnostic!();
    Ok(key.trim().to_string())
}

/// The terminal stops echoing typed characters while this lives.
struct EchoOff(#[cfg(target_os = "macos")] Option<libc::termios>);

impl EchoOff {
    #[cfg(target_os = "macos")]
    fn new() -> Self {
        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        unsafe {
            if libc::tcgetattr(0, original.as_mut_ptr()) != 0 {
                return Self(None);
            }
            let original = original.assume_init();
            let mut quiet = original;
            quiet.c_lflag &= !libc::ECHO;
            quiet.c_lflag |= libc::ECHONL;
            libc::tcsetattr(0, libc::TCSANOW, &quiet);
            Self(Some(original))
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn new() -> Self {
        Self()
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        if let Some(original) = &self.0 {
            unsafe { libc::tcsetattr(0, libc::TCSANOW, original) };
        }
    }
}

fn print_stream_key(stored: &StreamKey, json: bool) -> Result<ExitCode> {
    if json {
        println!("{}", serde_json::to_string_pretty(stored)?);
    } else if stored.key_set {
        println!(
            "Stream key for {} kept in the Keychain.",
            stored.service.id()
        );
    } else {
        println!("No stream key kept for {} now.", stored.service.id());
    }
    Ok(ExitCode::SUCCESS)
}

fn print_stream(status: &StreamStatus, json: bool) -> Result<ExitCode> {
    if json {
        println!("{}", serde_json::to_string_pretty(status)?);
    } else {
        print_stream_lines(status);
    }
    Ok(match status.state {
        StreamState::Failed => ExitCode::FAILURE,
        _ => ExitCode::SUCCESS,
    })
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
    println!(
        "Stream {state}: {} ({server}){}",
        stream.service.id(),
        if stream.key_set {
            ""
        } else {
            "; no stream key set (`isocast stream key set`)"
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
    let p = &devices.permissions;
    println!(
        "Permissions: screen recording {}, camera {}, microphone {}",
        p.screen_recording.label(),
        p.camera.label(),
        p.microphone.label()
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

/// Devices and permissions as the daemon sees them: it records, so its
/// permissions are the ones that count.
fn list_devices() -> Result<Devices> {
    Client::connect_or_launch()?.call(Method::Devices)
}

#[cfg(target_os = "macos")]
fn record_with_platform(setup: &Setup, options: &RecordOptions) -> Result<Report> {
    isocast_engine::record::record(&isocast_platform_macos::MacPlatform, setup, options)
}

#[cfg(not(target_os = "macos"))]
fn record_with_platform(_setup: &Setup, _options: &RecordOptions) -> Result<Report> {
    bail!("recording is only supported on macOS so far")
}
