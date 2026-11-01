//! isocastd: the headless Isocast engine.
//!
//! It owns the recording session and answers JSON-RPC 2.0 requests from the
//! CLI, MCP and the app on a local socket, one message per line (see
//! `isocast_protocol::api`). `isocast start` launches it when it isn't
//! running.

// Linux frames retain their Vulkan device for software-encoder readback;
// proving Send/Sync through wgpu's resource graph needs a deeper trait walk.
#![cfg_attr(target_os = "linux", recursion_limit = "256")]

use isocast_ipc::{Listener as UnixListener, Stream as UnixStream};
use parking_lot::{Condvar, Mutex};
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use isocast_engine::Platform;
use isocast_engine::Time;
use isocast_engine::capacity;
use isocast_engine::critical::{Critical, Transaction};
use isocast_engine::data::DataStore;
use isocast_engine::record::{
    DEFAULT_SLOTS, Engine as LiveEngineOf, EngineEvent, FrameFeed, JpegFrame, OnEngineEvent,
    RecordOptions, SessionControl, SharedEvent, SharedFeed, SplitTake, expand_home,
    interrupted_note, interrupted_sessions,
};
use isocast_engine::stream::{KEY_ENV, LEGACY_KEY_SERVICE, OnChange, Stream};
use isocast_protocol::api::InitParams;
use isocast_protocol::api::{
    Applied, ApplyParams, CapacityEstimate, CapacityParams, EngineInfo, EngineState, Event,
    EventKind, EventKinds, Frame, FrameParams, FrameRate, FrameStream, FramesParams, GetParams,
    H264Profile, ImportObsParams, Live, LiveStatus, Marked, Method, Notification, Overload,
    OverloadedFile, Plan, Refreshed, Report, Request, Resolved, Response, RpcError,
    SharedFrameStream, SharedFramesParams, Snapshot, SnapshotParams, StartParams, Started, Status,
    StopAccepted, StreamKey, StreamKeyParams, StreamKeyTarget, Switched, TakeCause, TuneParams,
    Tuning, WatchParams, lock_path, shared, socket_path,
};
use isocast_protocol::caller::Caller;
use isocast_protocol::change::{Change, diff, merge_patch, missing_removals};
use isocast_protocol::devices::{PermissionKind, PermissionsReport};
use isocast_protocol::init::{self, Initialized};
use isocast_protocol::manifest::SessionManifest;
use isocast_protocol::obs::{self, Imported};
use isocast_protocol::stream::{
    MAIN_DESTINATION, STREAM_SERVICES, StreamService, StreamSettings, canvas_key_account,
};
use isocast_protocol::{Canvas, CanvasId, Issue, PROGRAM, Setup, SourceKind, Transition};
use isocast_surfaces::{Exporter, Surface, Token};

/// How long `start` waits for devices to open and files to be created.
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// How many of the newest project folders `start` checks for recordings
/// that were interrupted (startup recovery checks more).
const START_SCAN_PROJECTS: usize = 5;
/// How long `stop` waits for the files to be finished.
const STOP_TIMEOUT: Duration = Duration::from_secs(60);
/// Why a recording stopped while it was starting had no report.
const STOPPED_BEFORE_START: &str =
    "the recording was stopped before it started, so nothing was recorded";
/// How long the live engine keeps running after a client last asked for
/// frames, when nothing else needs it.
const ENGINE_IDLE: Duration = Duration::from_secs(15);
/// The longest a `frame` request waits for a new frame.
const MAX_FRAME_WAIT: Duration = Duration::from_secs(10);
/// How long `stream_start` waits for the stream to go live.
const STREAM_START_TIMEOUT: Duration = Duration::from_secs(15);
/// Live changes (`apply` with `live`) are saved once none has come for this
/// long, or at the latest this long after the first unsaved one.
const SAVE_QUIET: Duration = Duration::from_millis(300);
const SAVE_AT_MOST: Duration = Duration::from_secs(1);
/// A slow event client must reconnect instead of retaining an unbounded
/// history. At 20 level events/s this allows about three seconds of lag.
const WATCH_QUEUE_DEPTH: usize = 64;
const WATCH_WRITE_TIMEOUT: Duration = Duration::from_secs(2);
/// How often a watcher with nothing to send checks that its client is
/// still there (one asking for rare kinds may get no event for hours).
const WATCH_HANGUP_CHECK: Duration = Duration::from_secs(1);
/// The longest request line read, so one client can't make the daemon hold
/// any amount of memory. A setup is far smaller.
const MAX_REQUEST: usize = 16 << 20;

use platform::{NativePlatform, native};

mod callers;
mod canvases;
mod data;
mod debug;
mod diagnostics;
mod governor;
mod hotkeys;
mod library;
mod machine;
mod nle;
mod optimize;
mod platform;
mod plugins;
mod projectors;
mod projects;
mod recovery;
mod remote;
mod replay;
mod roots;
mod store;
mod streams;
mod virtual_camera;
mod websocket;
mod whep;

static PARENT_OWNED: AtomicBool = AtomicBool::new(false);

fn main() -> ExitCode {
    if let Some(code) = machine::worker() {
        return code;
    }
    #[cfg(unix)]
    if isocast_diagnostics::native::requested() {
        return if isocast_diagnostics::native::run().is_ok() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    #[cfg(target_os = "macos")]
    if let Some(code) = isocast_platform_macos::preflight_permissions() {
        return ExitCode::from(code);
    }
    #[cfg(target_os = "macos")]
    if let Some(mode) = std::env::args().nth(1) {
        use isocast_platform_macos::virtual_camera;
        if mode == "--virtual-camera-sink-child" {
            let args = std::env::args().skip(2).collect::<Vec<_>>();
            isocast_platform_macos::will_run_main_queue();
            thread::spawn(move || {
                let code = match virtual_camera::run_child(&args) {
                    Ok(()) => 0,
                    Err(error) => {
                        eprintln!("virtual_camera helper: {error:#}");
                        1
                    }
                };
                std::process::exit(code);
            });
            // Framework callbacks retain a real main run loop, as in the
            // host, while every potentially blocking CMIO call is on a worker.
            // The helper holds no recordings, so a quit request just exits.
            isocast_platform_macos::run_main_queue(|| std::process::exit(0));
        }
        let result = match mode.as_str() {
            "--install-camera-extension" => Some(virtual_camera::activate(
                std::env::args().nth(2).as_deref() == Some("--wait-for-approval"),
            )),
            "--uninstall-camera-extension" => Some(virtual_camera::deactivate()),
            "--camera-extension-state" => Some(virtual_camera::print_state()),
            "--list-virtual-camera" => Some(virtual_camera::camera_probe(false)),
            "--check-virtual-camera" => Some(virtual_camera::camera_probe(true)),
            _ => None,
        };
        if let Some(result) = result {
            return ExitCode::from(result as u8);
        }
    }
    // Before anything opens files or starts helpers (they inherit it): the
    // 256 macOS starts with runs out under many clients and plugins (#92).
    let (files_before, files_now) = isocast_diagnostics::fds::raise();
    let _logs = isocast_diagnostics::init("isocastd", false);
    isocast_diagnostics::diagnostic!("isocastd: open-file limit {files_now} (was {files_before})");
    let socket = socket_path();
    match hold_instance_lock().and_then(|lock| Ok((lock, listen(&socket)?))) {
        // The lock is held until the process exits.
        Ok((_lock, listener)) => {
            isocast_diagnostics::ready();
            isocast_diagnostics::diagnostic!("isocastd listening on {}", socket.display());
            let daemon = Arc::new(Daemon::default());
            if std::env::args_os().any(|arg| arg == "--exit-on-stdin-close") {
                let owned = daemon.clone();
                // stdin is a lifetime pipe, not a second control protocol.
                // Closing it (also on CLI crash/kill) uses the same durable
                // shutdown as the API. No PID polling/reuse or forced kill.
                if let Err(error) =
                    thread::Builder::new()
                        .name("record owner".into())
                        .spawn(move || {
                            use std::io::Read;
                            let mut byte = [0];
                            let mut input = std::io::stdin().lock();
                            loop {
                                match input.read(&mut byte) {
                                    Err(error)
                                        if error.kind() == std::io::ErrorKind::Interrupted =>
                                    {
                                        continue;
                                    }
                                    _ => break,
                                }
                            }
                            owned.graceful_shutdown();
                            std::process::exit(0);
                        })
                {
                    isocast_diagnostics::diagnostic!("error"; "isocastd: could not watch record owner: {error}");
                    return ExitCode::FAILURE;
                }
                PARENT_OWNED.store(true, Ordering::SeqCst);
            }
            daemon.load_setup();
            daemon.load_machine();
            daemon.recover_recordings();
            daemon.load_data();
            daemon.load_projectors();
            diagnostics::start(&daemon);
            start_plugins(&daemon);
            daemon.websocket.load(&daemon);
            daemon.remote.load(&daemon);
            let saver = daemon.clone();
            thread::spawn(move || saver.contain("setup saver", || saver.save_live_changes()));
            // On Ctrl-C, SIGTERM or SIGHUP, finish the files before exiting.
            let on_signal = daemon.clone();
            let handler = ctrlc::set_handler(move || {
                on_signal.graceful_shutdown();
                std::process::exit(0);
            });
            let idle = daemon.clone();
            thread::spawn(move || idle.contain("idle monitor", || idle.stop_idle_engine()));
            if let Err(err) = handler {
                isocast_diagnostics::diagnostic!("error"; "isocastd: could not handle signals: {err}");
            }
            #[cfg(target_os = "macos")]
            isocast_platform_macos::will_run_main_queue();
            daemon.start_hotkeys();
            let on_quit = daemon.clone();
            thread::spawn(move || {
                // The first client's focused edge must not race initial native
                // registration. Main continues pumping while this waits.
                daemon.refresh_hotkeys();
                serve(listener, daemon)
            });
            run_main_queue(on_quit)
        }
        Err(err) => {
            isocast_diagnostics::diagnostic!("error"; "isocastd: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Become the one daemon on this socket: lock [`lock_path`] (released by
/// the kernel when the process exits, however it exits) and write this
/// pid in it. Fails if another daemon holds it, even one too busy to
/// accept connections, whose socket would otherwise look stale. Waits a
/// moment first for a daemon that is exiting, or a client checking it.
fn hold_instance_lock() -> Result<std::fs::File> {
    let path = lock_path();
    if let Some(folder) = path.parent() {
        std::fs::create_dir_all(folder)
            .with_context(|| format!("could not create {}", folder.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("could not open {}", path.display()))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                let mut pid = String::new();
                let _ = std::io::Read::read_to_string(&mut file, &mut pid);
                bail!(
                    "another isocastd (pid {}) is already running on {}",
                    pid.trim(),
                    socket_path().display()
                );
            }
            Err(std::fs::TryLockError::Error(err)) => {
                return Err(err).with_context(|| format!("could not lock {}", path.display()));
            }
        }
    }
    file.set_len(0)?;
    write!(file, "{}", std::process::id())?;
    Ok(file)
}

/// Bind the socket, replacing a stale one left by a daemon that crashed
/// (this one holds the instance lock, so no other daemon owns it).
#[cfg(unix)]
fn listen(socket: &Path) -> Result<UnixListener> {
    if UnixStream::connect(socket).is_ok() {
        bail!(
            "another isocastd is already listening on {}",
            socket.display()
        );
    }
    if let Some(folder) = socket.parent() {
        std::fs::create_dir_all(folder)
            .with_context(|| format!("could not create {}", folder.display()))?;
    }
    let _ = std::fs::remove_file(socket);
    // listen runs before daemon threads start: umask makes the socket private
    // at creation, eliminating the bind→chmod connection window.
    let mask = unsafe { libc::umask(0o177) };
    let listener = UnixListener::bind(socket);
    unsafe {
        libc::umask(mask);
    }
    listener.with_context(|| format!("could not listen on {}", socket.display()))
}

#[cfg(windows)]
fn listen(socket: &Path) -> Result<UnixListener> {
    // The instance lock created the parent. Pipes have no socket file to unlink;
    // the transport reserves the kernel name and authenticates both peers.
    UnixListener::bind(socket).with_context(|| format!("could not listen on {}", socket.display()))
}

/// Keep the main thread serving the main queue, which macOS needs for
/// AppKit calls (display names). Never returns.
#[cfg(target_os = "macos")]
fn run_main_queue(daemon: Arc<Daemon>) -> ExitCode {
    isocast_platform_macos::run_main_queue(move || daemon.graceful_shutdown())
}

#[cfg(not(target_os = "macos"))]
fn run_main_queue(_daemon: Arc<Daemon>) -> ExitCode {
    loop {
        thread::park();
    }
}

fn serve(listener: UnixListener, daemon: Arc<Daemon>) {
    for connection in listener.incoming().flatten() {
        #[cfg(target_os = "macos")]
        {
            use std::os::fd::AsRawFd;
            let mut uid = 0;
            let mut gid = 0;
            // Darwin does not provide a security boundary from socket mode
            // alone. Check the kernel peer uid before any RPC/plugin dispatch.
            if unsafe {
                libc::getpeereid(connection.local().unwrap().as_raw_fd(), &mut uid, &mut gid)
            } != 0
                || uid != unsafe { libc::geteuid() }
            {
                continue;
            }
        }
        let daemon = daemon.clone();
        // A plugin's process (unsandboxed, so it could reach this socket)
        // is held to its grant here too.
        let scope = daemon.plugins.scope_of_peer(&connection);
        thread::spawn(move || {
            daemon.contain("API connection", || {
                let _ = handle(connection, &daemon, scope, Caller::local());
            });
        });
    }
}

/// Load the installed plugins and start their processes; each plugin's
/// connections are served like any client's, but held to its grant.
fn start_plugins(daemon: &Arc<Daemon>) {
    let publish = Arc::downgrade(daemon);
    let serve = Arc::downgrade(daemon);
    let roots = Arc::downgrade(daemon);
    daemon.plugins.start(plugins::Hooks {
        publish: Box::new(move |event| {
            if let Some(daemon) = publish.upgrade() {
                daemon.publish(event);
            }
        }),
        recording_roots: Box::new(move || {
            roots
                .upgrade()
                .map(|daemon| daemon.recording_roots())
                .unwrap_or_default()
        }),
        serve: Box::new(move |connection, scope| {
            if let Some(daemon) = serve.upgrade() {
                thread::spawn(move || {
                    daemon.contain("plugin connection", || {
                        let _ = handle(connection, &daemon, Some(scope), Caller::local());
                    });
                });
            }
        }),
    });
}

/// Answer requests on one connection until the client hangs up. A
/// plugin's connection (`scope`) may only call what it was granted, and a
/// caller on another computer (`caller`) nothing that controls the machine
/// rather than the show (`isocast_protocol::caller::local_only_reason`).
fn handle(
    connection: UnixStream,
    daemon: &Arc<Daemon>,
    scope: Option<Arc<plugins::Scope>>,
    caller: Caller,
) -> std::io::Result<()> {
    if diagnostics::CONNECTION_PANIC.swap(false, Ordering::SeqCst) {
        panic!("requested API connection panic");
    }
    if daemon.emergency.load(Ordering::SeqCst) {
        return Ok(());
    }
    let _tracked = scope.as_ref().and_then(|scope| scope.track(&connection));
    #[cfg(target_os = "macos")]
    let peer = match connection.local() {
        Some(local) => isocast_platform_macos::diagnostics::peer(local),
        None => serde_json::Value::Null,
    };
    #[cfg(windows)]
    let peer =
        serde_json::json!({"transport":"named_pipe","pid":isocast_ipc::peer_pid(&connection).ok()});
    #[cfg(all(not(target_os = "macos"), not(windows)))]
    let peer = serde_json::json!({"transport":"unix"});
    // A remote client's audit peer names it (decision 15).
    let peer = caller.peer().unwrap_or(peer);
    let _closed = diagnostics::PeerClosed(peer.clone());
    let remote = connection.peer().is_some();
    let max_request = if remote {
        isocast_ipc::MAX_REMOTE_REQUEST
    } else {
        MAX_REQUEST
    };
    let mut focused = None;
    let mut writer = connection.try_clone()?;
    let mut reader = BufReader::new(connection);
    let mut line = String::new();
    loop {
        // Revocation also invalidates requests already buffered by BufReader.
        if writer.aborted() {
            return Ok(());
        }
        line.clear();
        let read = (&mut reader)
            .take(max_request as u64 + 1)
            .read_line(&mut line)?;
        if read == 0 || writer.aborted() {
            return Ok(());
        }
        if read > max_request {
            let mut audit = diagnostics::ApiCall::new(r#"{"method":"[oversized request]"}"#, &peer);
            // The rest of the line can't be skipped without reading it, so
            // the connection ends here.
            let response = Response::err(
                None,
                RpcError {
                    code: RpcError::INVALID_REQUEST,
                    message: format!(
                        "the request is longer than {} MB; requests are one JSON object per line",
                        max_request >> 20
                    ),
                },
            );
            audit.response(&response);
            writeln!(writer, "{}", serde_json::to_string(&response)?)?;
            return Ok(());
        }
        if line.trim().is_empty() {
            continue;
        }
        let request = serde_json::from_str::<Request>(&line);
        let mut audit = diagnostics::ApiCall::new(&line, &peer);
        if audit.pairing() && request.is_err() {
            // A mistaken JSON pairing call must not echo PINs or PAKE proofs
            // through serde's detailed type errors into either endpoint's log.
            let response = Response::err(id_of(&line), RpcError { code: RpcError::INVALID_PARAMS, message: "params: remote_pair_begin takes no parameters; remote_pair_finish requires the dedicated TLS-bound PAKE exchange (isocast remote pair HOST --code-stdin)".into() });
            writeln!(writer, "{}", serde_json::to_string(&response)?)?;
            audit.response(&response);
            continue;
        }
        if let (Some(scope), Ok(request)) = (&scope, &request)
            && let Some(message) = scope.refusal(&request.method)
        {
            let refused = Response::err(
                Some(request.id),
                RpcError {
                    code: RpcError::NOT_PERMITTED,
                    message,
                },
            );
            writeln!(writer, "{}", serde_json::to_string(&refused)?)?;
            audit.response(&refused);
            continue;
        }
        // A network caller's method, for refusal events (decision 15).
        let refusable = match &request {
            Ok(request) if !caller.is_local() => Some(callers::method_name(&request.method)),
            _ => None,
        };
        if let Ok(request) = &request
            && let Some(error) = callers::refusal(&caller, &request.method)
        {
            daemon.note_refusal(&caller, refusable.as_deref().unwrap_or_default(), &error);
            let refused = Response::err(Some(request.id), error);
            writeln!(writer, "{}", serde_json::to_string(&refused)?)?;
            audit.response(&refused);
            continue;
        }
        let pairing_reply = matches!(
            request.as_ref().map(|request| &request.method),
            Ok(Method::RemotePairBegin)
        );
        let _caller = scope.as_ref().map(|scope| PluginCaller::set(&scope.id));
        let _entered = callers::enter(caller.clone());
        let response = match request {
            Ok(Request {
                id,
                method: Method::HotkeysInput(params),
                ..
            }) => {
                let owner = focused
                    .get_or_insert_with(|| daemon.focused_connection())
                    .id;
                match daemon.hotkeys_input(params, owner) {
                    Ok(value) => Response::ok(id, value),
                    Err(error) => Response::err(Some(id), error),
                }
            }
            Ok(Request {
                id,
                method: Method::Watch(params),
                ..
            }) => {
                let params = params.unwrap_or_default();
                match params.problem() {
                    None => {
                        if remote && !reader.buffer().is_empty() {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "watch is one-way; unexpected trailing input",
                            ));
                        }
                        audit.value(serde_json::json!({"subscribed":"events"}));
                        return watch(writer, daemon, id, &params);
                    }
                    Some(problem) => Response::err(
                        Some(id),
                        RpcError {
                            code: RpcError::INVALID_PARAMS,
                            message: problem.within("params").to_string(),
                        },
                    ),
                }
            }
            Ok(Request {
                id,
                method: Method::WhepOffer(params),
                ..
            }) => match whep::prepare(daemon, &params, &caller) {
                Ok(viewer) => {
                    audit.value(serde_json::json!({"subscribed":"webrtc"}));
                    return viewer.run(reader, writer, daemon, id);
                }
                Err(error) => Response::err(Some(id), error),
            },
            Ok(Request {
                id,
                method: Method::Frames(params),
                ..
            }) => match daemon.frames(&params) {
                Ok((reply, feed)) => {
                    audit.value(serde_json::to_value(&reply).unwrap_or_default());
                    return stream_frames(reader, writer, daemon, id, reply, feed);
                }
                Err(error) => Response::err(Some(id), error),
            },
            Ok(Request {
                id,
                method: Method::SharedFrames(params),
                ..
            }) => match daemon.shared_frames(&params) {
                Ok((reply, feed, exporter)) => {
                    audit.value(serde_json::to_value(&reply).unwrap_or_default());
                    return stream_shared_frames(reader, writer, daemon, id, reply, feed, exporter);
                }
                Err(error) => Response::err(Some(id), error),
            },
            Ok(request) => {
                let shutdown = request.method == Method::Shutdown;
                let if_idle = request.method == Method::ShutdownIfIdle;
                let response = daemon.call(request);
                if shutdown || (if_idle && response.error.is_none()) {
                    daemon.graceful_shutdown();
                    writeln!(writer, "{}", serde_json::to_string(&response)?)?;
                    audit.response(&response);
                    std::process::exit(0);
                }
                response
            }
            Err(err) => Response::err(
                id_of(&line),
                RpcError {
                    code: request_error_code(&line),
                    message: isocast_protocol::hotkeys::bounded(
                        &Request::located_error(&line)
                            .map_or_else(|| err.to_string(), |located| located.to_string()),
                        8192,
                    ),
                },
            ),
        };
        if let (Some(method), Some(error)) = (&refusable, &response.error) {
            daemon.note_refusal(&caller, method, error);
        }
        let bytes = serde_json::to_vec(&response)?;
        if pairing_reply && response.error.is_none() {
            audit.value(serde_json::json!({"pairing":"opened"}));
        } else {
            audit.response_bytes(&response, bytes.len());
        }
        writer.write_all(&bytes)?;
        writer.write_all(b"\n")?;
    }
}

/// Send `reply`, then the stream's records as they come, until the client
/// hangs up or the stream ends.
fn stream_frames(
    reader: BufReader<UnixStream>,
    mut writer: UnixStream,
    daemon: &Daemon,
    id: u64,
    reply: FrameStream,
    feed: FrameFeed,
) -> std::io::Result<()> {
    writeln!(
        writer,
        "{}",
        serde_json::to_string(&Response::ok(id, to_json(reply).unwrap_or_default()))?
    )?;
    // The client may send `{"fps": N}` lines to change its rate (the app
    // does while the user drags on the canvas); hanging up ends the stream
    // even when no frames come (a still screen).
    let rate = feed.rate();
    let left = Arc::new(AtomicBool::new(false));
    thread::spawn({
        let left = left.clone();
        move || {
            if reader.get_ref().peer().is_some() {
                let mut reader = reader;
                let mut line = String::new();
                loop {
                    line.clear();
                    let read = (&mut reader)
                        .take(isocast_ipc::MAX_REMOTE_REQUEST as u64 + 1)
                        .read_line(&mut line);
                    if !matches!(read, Ok(n) if n > 0 && n <= isocast_ipc::MAX_REMOTE_REQUEST) {
                        break;
                    }
                    if let Ok(change) = serde_json::from_str::<FrameRate>(&line) {
                        rate.set(change.fps);
                    }
                }
            } else {
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    if let Ok(change) = serde_json::from_str::<FrameRate>(&line) {
                        rate.set(change.fps);
                    }
                }
            }
            left.store(true, Ordering::SeqCst);
        }
    });
    // A client that stops reading without leaving is let go.
    writer.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut kept = std::time::Instant::now();
    let result = loop {
        match feed.records.recv_timeout(Duration::from_millis(250)) {
            Ok(record) => {
                if let Err(err) = writer.write_all(&record) {
                    break Err(err);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break Ok(()),
        }
        if left.load(Ordering::SeqCst) {
            break Ok(());
        }
        // Watching keeps the engine running.
        if kept.elapsed() >= Duration::from_secs(1) {
            kept = std::time::Instant::now();
            daemon.state.lock().wanted_at = Some(kept);
        }
    };
    // The reader stops once the client sees the end.
    let _ = writer.shutdown(std::net::Shutdown::Both);
    result
}

/// Send `reply`, then the ring's surfaces (offered to `exporter`) and each
/// frame's slot as JSON lines, while taking the client's releases, rate and
/// size changes, until the client hangs up or the stream ends.
fn stream_shared_frames(
    reader: BufReader<UnixStream>,
    mut writer: UnixStream,
    daemon: &Daemon,
    id: u64,
    reply: SharedFrameStream,
    feed: SharedFeed<Surface>,
    exporter: &Exporter,
) -> std::io::Result<()> {
    writeln!(
        writer,
        "{}",
        serde_json::to_string(&Response::ok(id, to_json(reply).unwrap_or_default()))?
    )?;
    let control = feed.control();
    let left = Arc::new(AtomicBool::new(false));
    thread::spawn({
        let left = left.clone();
        move || {
            let mut reader = reader;
            let mut line = Vec::new();
            loop {
                line.clear();
                let read = std::io::Read::take(&mut reader, 4097).read_until(b'\n', &mut line);
                if !matches!(read, Ok(n) if n > 0)
                    || line.len() > 4096
                    || line.last() != Some(&b'\n')
                {
                    break;
                }
                match serde_json::from_slice::<shared::Control>(&line) {
                    Ok(shared::Control::Release { generation, slot }) => {
                        control.release(generation, slot);
                    }
                    Ok(shared::Control::Fps(fps)) => control.set_fps(fps),
                    Ok(shared::Control::Size {
                        max_width,
                        max_height,
                    }) => control.set_size(max_width, max_height),
                    // Lines it doesn't know are ignored.
                    Err(_) => {}
                }
            }
            left.store(true, Ordering::SeqCst);
        }
    });
    writer.set_write_timeout(Some(Duration::from_secs(5)))?;
    // The ring offered and not yet fetched.
    let mut offered: Option<Token> = None;
    let mut kept = std::time::Instant::now();
    let send = |writer: &mut UnixStream, event: &shared::Event| -> std::io::Result<()> {
        let mut line = serde_json::to_vec(event).map_err(std::io::Error::other)?;
        line.push(b'\n');
        writer.write_all(&line)
    };
    let result = loop {
        let event = match feed.events.recv_timeout(Duration::from_millis(250)) {
            Ok(SharedEvent::Ring {
                generation,
                width,
                height,
                surfaces,
            }) => {
                if let Some(token) = offered.take() {
                    exporter.withdraw(token);
                }
                let slots = surfaces.len() as u32;
                let token = exporter.offer(surfaces);
                offered = Some(token);
                Some(shared::Event::Ring(shared::Ring {
                    generation,
                    width,
                    height,
                    slots,
                    token: token.to_string(),
                }))
            }
            Ok(SharedEvent::Frame {
                generation,
                slot,
                time,
                revision,
                layout_generation,
            }) => Some(shared::Event::Frame(shared::Frame {
                generation,
                slot,
                time: time.as_nanos().max(0) as u64,
                revision,
                layout_generation,
            })),
            Ok(SharedEvent::End(why)) => {
                let _ = send(&mut writer, &shared::Event::End(why));
                break Ok(());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => break Ok(()),
        };
        if let Some(event) = event
            && let Err(err) = send(&mut writer, &event)
        {
            break Err(err);
        }
        if left.load(Ordering::SeqCst) {
            break Ok(());
        }
        // Watching keeps the engine running.
        if kept.elapsed() >= Duration::from_secs(1) {
            kept = std::time::Instant::now();
            daemon.state.lock().wanted_at = Some(kept);
        }
    };
    if let Some(token) = offered {
        exporter.withdraw(token);
    }
    drop(feed);
    let _ = writer.shutdown(std::net::Shutdown::Both);
    result
}

/// Send the live setup, then every event of the kinds `params` asks for,
/// until the client hangs up.
fn watch(
    mut writer: UnixStream,
    daemon: &Arc<Daemon>,
    id: u64,
    params: &WatchParams,
) -> std::io::Result<()> {
    writer.set_write_timeout(Some(WATCH_WRITE_TIMEOUT))?;
    let (tx, rx) = mpsc::sync_channel(WATCH_QUEUE_DEPTH);
    let wanted = params.wanted();
    let key = next_watcher_key();
    daemon.watchers.lock().push(Watcher { key, wanted, tx });
    // However this ends, the watcher goes (else one asking for rare kinds
    // would stay listed until the next event of its kind).
    struct Leave<'a>(&'a Daemon, u64);
    impl Drop for Leave<'_> {
        fn drop(&mut self) {
            let mut watchers = self.0.watchers.lock();
            watchers.retain(|watcher| watcher.key != self.1);
        }
    }
    let _leave = Leave(daemon, key);
    let live = daemon
        .get(GetParams::default())
        .and_then(to_json)
        .unwrap_or_default();
    writeln!(
        writer,
        "{}",
        serde_json::to_string(&Response::ok(id, live))?
    )?;
    // A setup file that didn't load stays a warning until a change fixes it.
    if wanted.contains(EventKind::Warning)
        && let Some(message) = daemon.state.lock().file.load_warning()
    {
        let warning = Notification::event(Event::Warning { message });
        writeln!(writer, "{}", serde_json::to_string(&warning)?)?;
    }
    if wanted.contains(EventKind::Warning) {
        for previous in isocast_diagnostics::previous_abnormal() {
            let warning = Notification::event(Event::Warning {
                message: format!(
                    "previous daemon launch ended abnormally: {}; use bug_report",
                    previous["launch"]
                ),
            });
            writeln!(writer, "{}", serde_json::to_string(&warning)?)?;
        }
    }
    let mut heartbeat = std::time::Instant::now();
    loop {
        if let UnixStream::Tls(stream) = &writer
            && stream.readable()
        {
            // A watch client never sends control messages. Do not leave its
            // unexpected input parked behind a full plaintext pipe.
            return Ok(());
        }
        if writer.peer().is_some() && heartbeat.elapsed() >= Duration::from_secs(2) {
            writeln!(
                writer,
                "{}",
                serde_json::to_string(&Notification::event(Event::RemoteHeartbeat))?
            )?;
            heartbeat = std::time::Instant::now();
        }
        match rx.recv_timeout(WATCH_HANGUP_CHECK) {
            Ok(event) => writer.write_all(&event)?,
            // A client that left is noticed here even when no event it
            // wants comes.
            Err(mpsc::RecvTimeoutError::Timeout) if hung_up(&writer) => return Ok(()),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

/// Whether the client at the other end of `stream` has closed it, without
/// reading anything it sent or waiting.
fn hung_up(stream: &UnixStream) -> bool {
    isocast_ipc::hung_up(stream)
}

/// A key for a new [`Watcher`], unique in this process.
fn next_watcher_key() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// A connection watching events, and the kinds it wants.
struct Watcher {
    key: u64,
    wanted: EventKinds,
    tx: mpsc::SyncSender<Arc<[u8]>>,
}

/// The JSON-RPC code for a request that didn't parse: not JSON, not a
/// JSON-RPC request, a method isocastd doesn't have, or wrong params.
fn request_error_code(line: &str) -> i64 {
    let Ok(request) = serde_json::from_str::<serde_json::Value>(line) else {
        return RpcError::PARSE_ERROR;
    };
    let method = request.get("method").and_then(serde_json::Value::as_str);
    let (Some(method), Some(_), Some(_)) = (
        method,
        request.get("id").and_then(serde_json::Value::as_u64),
        request.get("jsonrpc").and_then(serde_json::Value::as_str),
    ) else {
        return RpcError::INVALID_REQUEST;
    };
    // Named alone, a method isocastd has parses or asks for its params;
    // one it doesn't have is an unknown variant.
    match serde_json::from_value::<Method>(serde_json::json!({ "method": method })) {
        Err(err) if err.to_string().starts_with("unknown variant") => RpcError::METHOD_NOT_FOUND,
        _ => RpcError::INVALID_PARAMS,
    }
}

/// The id of a request that didn't parse, if it has one.
fn id_of(line: &str) -> Option<u64> {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()?
        .get("id")?
        .as_u64()
}

#[derive(Default)]
struct Daemon {
    machine: Mutex<machine::Calibration>,
    emergency: AtomicBool,
    interrupted: Mutex<Vec<isocast_protocol::manifest::InterruptedRecording>>,
    /// Manifests startup recovery marked that no `start` has mentioned yet.
    interrupted_unnoted: Mutex<Vec<PathBuf>>,
    /// Critical (`isocast_engine::critical`): a panic that cuts a change
    /// short finalizes recordings through the emergency stop.
    state: Critical<State>,
    /// Signalled when a session ends.
    ended: Condvar,
    /// Signalled when the engine has started (or failed to).
    engine_ready: Condvar,
    /// The revision of the live setup last written to each collection and
    /// profile file. Held while writing them, so library changes and saves
    /// don't overlap.
    saved: Mutex<HashMap<PathBuf, u64>>,
    /// The newest live change not yet saved (revision, setup, files), for
    /// the saver thread; signalled when it changes.
    live_save: Mutex<Option<(u64, Setup, store::Target)>>,
    live_saved: Condvar,
    /// Connections watching events.
    watchers: Mutex<Vec<Watcher>>,
    /// isocastd's processor use, for `status`.
    cpu: Mutex<CpuMeter>,
    /// Installed plugins and their processes.
    plugins: plugins::Registry,
    /// The data store bound text shows (`data_*`).
    data: data::Data,
    /// Held while a recording picks its project folder and while a project
    /// is renamed, so a rename never races a recording into it.
    projects: Mutex<()>,
    hotkeys: hotkeys::HotkeyRuntime,
    shutting_down: AtomicBool,
    /// Serialize process shutdown with idle engine teardown, including
    /// native workers no longer reachable through `state.engine`.
    shutdown_lock: Mutex<()>,
    /// Read-held by every call that starts something (a take, a stream,
    /// the virtual camera); `shutdown_if_idle` write-holds it,
    /// so no start can slip between its idle check and the shutdown.
    starting: parking_lot::RwLock<()>,
    /// The obs-websocket compatibility server (`websocket_*`).
    websocket: websocket::Runtime,
    remote: remote::Runtime,
    /// Open projectors and where each view was last shown (`projector_*`).
    projectors: projectors::ProjectorState,
    /// Who last started or stopped each stream slot from another computer
    /// (`by` in its `stream` events); its own lock, since streams report
    /// while the state lock is held.
    stream_by: Mutex<HashMap<String, Option<String>>>,
    /// The folders other computers can use (`folder_roots`).
    roots: roots::Runtime,
    /// Held from a `start`'s first check until its session is reserved, so
    /// two starts (each maybe replacing the setup) can't interleave.
    start_admission: Mutex<()>,
    /// Whether a take's software-adapter warning within the advisory
    /// reference was said this run (#128): said once; above it, or with
    /// unqualified outputs, it is said every take.
    software_reference_warned: AtomicBool,
}

/// isocastd's processor use, as `status` reports it: the share of all the
/// cores the process used between two readings at least a second apart.
struct CpuMeter {
    /// The reading the next one is measured from: when, and the processor
    /// seconds used by then.
    from: Option<(std::time::Instant, f64)>,
    percent: Option<f64>,
}

/// The first reading is taken when the daemon starts.
impl Default for CpuMeter {
    fn default() -> Self {
        Self {
            from: process_cpu_seconds().map(|cpu| (std::time::Instant::now(), cpu)),
            percent: None,
        }
    }
}

impl CpuMeter {
    fn read(&mut self) -> Option<f64> {
        let now = (std::time::Instant::now(), process_cpu_seconds()?);
        let Some((at, cpu)) = self.from else {
            self.from = Some(now);
            return None;
        };
        let wall = now.0.duration_since(at).as_secs_f64();
        if wall >= 1.0 {
            let cores = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
            self.percent = Some(((now.1 - cpu) / wall / cores * 100.0).clamp(0.0, 100.0));
            self.from = Some(now);
        }
        self.percent
    }
}

/// The processor time this process has used, user and system, in seconds.
fn process_cpu_seconds() -> Option<f64> {
    native().process_cpu_seconds()
}

#[derive(Default)]
struct State {
    camera: virtual_camera::CameraOutput,
    /// The replay buffer, running or the last (for its final status).
    replay: replay::ReplayOutput,
    /// The recording, if one runs (on the live engine).
    session: Option<Session>,
    /// The live engine, while something needs it.
    engine: Option<Arc<dyn LiveEngine>>,
    /// A client asked to keep the engine running.
    hold: bool,
    /// The engine is starting (outside the lock).
    starting: bool,
    /// When a client last asked for frames.
    wanted_at: Option<std::time::Instant>,
    /// The report of the most recent session that finished with one.
    last: Option<Report>,
    /// Why the most recent session ended without a report (it failed to
    /// start, or was stopped before it started); cleared when one ends
    /// with a report.
    last_error: Option<String>,
    /// The id the next session gets, and the id of the last that ended.
    next_session: u64,
    ended_session: u64,
    /// The live setup; revision 0 until one is applied.
    setup: Option<Setup>,
    revision: u64,
    /// The scene on air.
    scene: Option<String>,
    /// The scene staged for `take`.
    preview: Option<String>,
    /// The scene last put on air on each extra canvas (`[vertical]`,
    /// `[[canvases]]`), while it doesn't follow the main one (the engine
    /// starts it there).
    canvas_scenes: BTreeMap<CanvasId, String>,
    /// The scene staged in each extra canvas's own preview, while it
    /// doesn't follow the main one.
    canvas_previews: BTreeMap<CanvasId, String>,
    /// A `tune` benchmark is running; it and a recording would compete for
    /// the encoders.
    tuning: bool,
    /// Where the live setup is kept between runs: the current scene
    /// collection and profile.
    file: store::Store,
    /// Each stream destination's stream, running or ended (kept for its
    /// final status), by destination id, the main one first; the vertical
    /// canvas's after, as `vertical:<id>` (see `streams::slot`).
    streams: Vec<(String, streams::Output)>,
    /// `stream_start` calls getting the engine ready for streams that
    /// aren't in `streams` yet; the engine mustn't idle out under them.
    streams_starting: u32,
    /// Stream losses already warned about, per destination.
    stream_reported: streams::ReportedMap,
    /// Temporary push-key overrides, never persisted.
    hotkey_holds: HashMap<String, (String, bool)>,
}

impl State {
    /// Scene ids of the live setup.
    fn scene_ids(&self) -> Vec<String> {
        self.setup
            .iter()
            .flat_map(|setup| &setup.scenes)
            .map(|s| s.id.clone())
            .collect()
    }

    /// The live engine's control, while it runs.
    fn control(&self) -> Option<Arc<SessionControl>> {
        self.engine.as_ref().map(|engine| engine.control())
    }

    /// Why `what` ("no setup to record") can't happen without a live
    /// setup, and the next step: fixing the setup file if it didn't load,
    /// else making a first setup.
    fn no_setup(&self, what: &str) -> RpcError {
        let file = self.file.info();
        let message = match &file.load_error {
            Some(_) => format!(
                "{what}: the setup file {} couldn't be loaded (`isocast status` or get_status \
                 says why); fix it and restart isocastd (`isocast restart`), switch to another \
                 scene collection or profile (`isocast collection list`), or apply a new setup",
                file.path.display()
            ),
            None => format!(
                "{what}: there is no live setup yet; make one with `isocast setup init --apply` \
                 (MCP: init_setup with apply), or apply one"
            ),
        };
        RpcError {
            code: RpcError::WRONG_STATE,
            message,
        }
    }
}

/// A session's outcome once it has ended, and the signal that it has.
type SessionDone = (Mutex<Option<Result<Report, String>>>, Condvar);

struct Session {
    /// Counts up from 1, so `stop` knows when this session has ended.
    id: u64,
    /// Its id for clients (`session`), unique across runs.
    token: String,
    /// What `start` was asked with, so the same `start` again answers
    /// with this session (`already_recording`).
    asked: StartParams,
    /// Who first asked it to stop: `Some(None)` for a local caller.
    stopped_by: Arc<Mutex<Option<Option<String>>>>,
    /// Its outcome once it has ended, for whoever waits for it (a blocking
    /// `stop`), whatever starts and ends after it.
    done: Arc<SessionDone>,
    stop: Arc<AtomicBool>,
    files: Arc<Mutex<Option<Vec<PathBuf>>>>,
    live: Arc<Mutex<Option<LiveStatus>>>,
    /// The take it records now: from 1, 0 before it has started.
    take: Arc<AtomicU32>,
    /// The project folder it records into, once picked.
    project: Option<projects::ProjectRef>,
}

/// The live engine, whatever the platform.
trait LiveEngine: Send + Sync {
    fn update(&self, setup: &Setup, revision: u64) -> Result<(), String>;
    fn info(&self) -> EngineInfo;
    fn program_failure(&self) -> Option<String>;
    fn canvas_stopped(&self, canvas: CanvasId) -> Option<String>;
    fn control(&self) -> Arc<SessionControl>;
    fn set_preview(&self, scene: Option<String>);
    fn frame_jpeg(
        &self,
        target: &str,
        after: Option<u64>,
        timeout: Duration,
        max_width: Option<u32>,
        quality: f32,
        fps: Option<u32>,
    ) -> Result<Option<JpegFrame>, String>;
    fn monitor_h264_constrained_high(&self) -> bool;
    /// `target`'s picture as low-latency H.264 for a monitor.
    fn frames(
        &self,
        target: &str,
        max_width: Option<u32>,
        fps: u32,
        profile: H264Profile,
    ) -> Result<FrameFeed, String>;
    fn opus(&self) -> Result<isocast_engine::record::OpusFeed, String>;
    /// `target`'s picture in surfaces shared with a client on this machine.
    fn shared_frames(
        &self,
        target: &str,
        max_size: (Option<u32>, Option<u32>),
        fps: u32,
        slots: u32,
        filtered: bool,
    ) -> Result<SharedFeed<Surface>, String>;
    fn record(&self, options: &RecordOptions) -> Result<Report>;
    /// Where layout put every item, once the scenes are resolved.
    fn resolved(&self) -> Option<Resolved>;
    /// Why the canvas can't change to `canvas` now, if it can't.
    fn canvas_refusal(&self, canvas: &Canvas) -> Option<String>;
    /// Stream the program with `settings` under `key`, as destination `id`.
    fn stream(
        &self,
        id: &str,
        settings: StreamSettings,
        key: String,
        on_change: OnChange,
    ) -> Result<Stream>;
    /// Stream `canvas`'s program (the vertical one's) with `settings`
    /// under `key`, as that canvas's destination `id`.
    fn stream_canvas(
        &self,
        canvas: CanvasId,
        id: &str,
        settings: StreamSettings,
        key: String,
        on_change: OnChange,
    ) -> Result<Stream>;
    /// Put `scene` on air on `canvas` (not the main one, which switches
    /// through `control`): where it sits on the recording's timeline, if
    /// one records.
    fn switch_canvas_scene(
        &self,
        canvas: CanvasId,
        at: Time,
        scene: String,
        transition: Transition,
    ) -> Result<Option<(u32, f64)>, String>;
    /// The scene on air on `canvas`.
    fn canvas_scene(&self, canvas: CanvasId) -> Option<String>;
    /// Stage `scene` (or none) in `canvas`'s preview.
    fn set_canvas_preview(&self, canvas: CanvasId, scene: Option<String>) -> Result<(), String>;
    /// Where layout put every item on `canvas`, once the scenes are
    /// resolved.
    fn resolved_canvas(&self, canvas: CanvasId) -> Option<Resolved>;
    /// Why the engine would refuse `setup` now (a canvas it records or
    /// streams changing), if it would.
    fn setup_refusal(&self, setup: &Setup) -> Option<Issue>;
    fn virtual_camera(
        &self,
        on_change: isocast_engine::record::OnCameraChange,
    ) -> Result<isocast_engine::record::VirtualCamera>;
    /// Keep `canvas`'s program in a replay buffer.
    fn replay(
        &self,
        canvas: CanvasId,
        settings: &isocast_protocol::replay::ReplaySettings,
        stream: Option<&StreamSettings>,
        on_change: isocast_engine::replay::OnChange,
    ) -> Result<isocast_engine::replay::ReplayBuffer>;
    fn stop(&self);
}

impl<P: Platform<SharedSurface = Surface>> LiveEngine for LiveEngineOf<P> {
    fn update(&self, setup: &Setup, revision: u64) -> Result<(), String> {
        LiveEngineOf::update(self, setup, revision)
    }

    fn info(&self) -> EngineInfo {
        LiveEngineOf::info(self)
    }

    fn program_failure(&self) -> Option<String> {
        LiveEngineOf::program_failure(self)
    }

    fn canvas_stopped(&self, canvas: CanvasId) -> Option<String> {
        LiveEngineOf::canvas_stopped(self, canvas)
    }

    fn control(&self) -> Arc<SessionControl> {
        LiveEngineOf::control(self).clone()
    }

    fn set_preview(&self, scene: Option<String>) {
        LiveEngineOf::set_preview(self, scene);
    }

    fn frame_jpeg(
        &self,
        target: &str,
        after: Option<u64>,
        timeout: Duration,
        max_width: Option<u32>,
        quality: f32,
        fps: Option<u32>,
    ) -> Result<Option<JpegFrame>, String> {
        LiveEngineOf::frame_jpeg(self, target, after, timeout, max_width, quality, fps)
    }

    fn monitor_h264_constrained_high(&self) -> bool {
        LiveEngineOf::monitor_h264_constrained_high(self)
    }

    fn frames(
        &self,
        target: &str,
        max_width: Option<u32>,
        fps: u32,
        profile: H264Profile,
    ) -> Result<FrameFeed, String> {
        LiveEngineOf::frames(self, target, max_width, fps, profile)
    }

    fn opus(&self) -> Result<isocast_engine::record::OpusFeed, String> {
        LiveEngineOf::opus(self).map_err(|error| error.to_string())
    }

    fn shared_frames(
        &self,
        target: &str,
        max_size: (Option<u32>, Option<u32>),
        fps: u32,
        slots: u32,
        filtered: bool,
    ) -> Result<SharedFeed<Surface>, String> {
        LiveEngineOf::shared_frames(self, target, max_size, fps, slots, filtered)
    }

    fn record(&self, options: &RecordOptions) -> Result<Report> {
        LiveEngineOf::record(self, options)
    }

    fn resolved(&self) -> Option<Resolved> {
        LiveEngineOf::resolved(self)
    }

    fn canvas_refusal(&self, canvas: &Canvas) -> Option<String> {
        LiveEngineOf::canvas_refusal(self, canvas)
    }

    fn stream(
        &self,
        id: &str,
        settings: StreamSettings,
        key: String,
        on_change: OnChange,
    ) -> Result<Stream> {
        isocast_engine::stream::start(self, id, settings, key, on_change)
    }

    fn stream_canvas(
        &self,
        canvas: CanvasId,
        id: &str,
        settings: StreamSettings,
        key: String,
        on_change: OnChange,
    ) -> Result<Stream> {
        LiveEngineOf::stream_canvas(self, canvas, id, settings, key, on_change)
    }

    fn switch_canvas_scene(
        &self,
        canvas: CanvasId,
        at: Time,
        scene: String,
        transition: Transition,
    ) -> Result<Option<(u32, f64)>, String> {
        LiveEngineOf::switch_canvas_scene(self, canvas, at, scene, transition)
    }

    fn canvas_scene(&self, canvas: CanvasId) -> Option<String> {
        LiveEngineOf::canvas_scene(self, canvas)
    }

    fn set_canvas_preview(&self, canvas: CanvasId, scene: Option<String>) -> Result<(), String> {
        LiveEngineOf::set_canvas_preview(self, canvas, scene)
    }

    fn resolved_canvas(&self, canvas: CanvasId) -> Option<Resolved> {
        LiveEngineOf::resolved_canvas(self, canvas)
    }

    fn setup_refusal(&self, setup: &Setup) -> Option<Issue> {
        LiveEngineOf::setup_refusal(self, setup)
    }

    fn stop(&self) {
        LiveEngineOf::stop(self);
    }

    fn replay(
        &self,
        canvas: CanvasId,
        settings: &isocast_protocol::replay::ReplaySettings,
        stream: Option<&StreamSettings>,
        on_change: isocast_engine::replay::OnChange,
    ) -> Result<isocast_engine::replay::ReplayBuffer> {
        isocast_engine::replay::start_canvas(self, canvas, settings, stream, on_change)
    }

    fn virtual_camera(
        &self,
        on_change: isocast_engine::record::OnCameraChange,
    ) -> Result<isocast_engine::record::VirtualCamera> {
        LiveEngineOf::virtual_camera(self, on_change)
    }
}

/// Where layout put every item of `setup` (the live setup at `revision`):
/// from the live engine, once it has laid out this revision (it does
/// within moments of an apply); else worked out now, with sources' sizes
/// from their settings (generation 0).
fn resolved(
    setup: &Setup,
    revision: u64,
    engine: Option<Arc<dyn LiveEngine>>,
    data: &isocast_engine::data::DataStore,
) -> Resolved {
    if let Some(engine) = engine {
        let deadline = std::time::Instant::now() + Duration::from_millis(250);
        loop {
            match engine.resolved() {
                Some(resolved) if resolved.revision >= revision => return resolved,
                _ if std::time::Instant::now() >= deadline => break,
                _ => std::thread::sleep(Duration::from_millis(1)),
            }
        }
    }
    Resolved {
        generation: 0,
        revision,
        scenes: isocast_engine::layout::resolve_setup(setup, data),
    }
}

/// Where layout put every item of `setup`'s vertical canvas, as
/// [`resolved`] does for the main one; none without a vertical canvas.
fn resolved_vertical(
    setup: &Setup,
    canvas: CanvasId,
    revision: u64,
    engine: Option<Arc<dyn LiveEngine>>,
    data: &isocast_engine::data::DataStore,
) -> Option<Resolved> {
    let view = setup.for_canvas(canvas)?;
    if let Some(engine) = engine {
        let deadline = std::time::Instant::now() + Duration::from_millis(250);
        loop {
            match engine.resolved_canvas(canvas) {
                Some(resolved) if resolved.revision >= revision => return Some(resolved),
                _ if std::time::Instant::now() >= deadline => break,
                _ => std::thread::sleep(Duration::from_millis(1)),
            }
        }
    }
    Some(Resolved {
        generation: 0,
        revision,
        // Sources make their pictures at the main canvas's size, as in
        // the engine (#112).
        scenes: isocast_engine::layout::resolve_canvas(
            &view,
            (setup.canvas.width, setup.canvas.height),
            data,
        ),
    })
}

fn start_engine(
    setup: &Setup,
    control: Arc<SessionControl>,
    on_event: OnEngineEvent,
) -> Result<Arc<dyn LiveEngine>> {
    Ok(Arc::new(LiveEngineOf::start(
        native(),
        setup,
        Some(control),
        Some(on_event),
    )?))
}

impl Daemon {
    /// Signals, quit Apple Events and the shutdown API share this path. A
    /// timeout is diagnostic, never permission to exit during an active take.
    /// Finished media can leave blocked metadata in its durable final sidecar.
    fn graceful_shutdown(&self) {
        let _shutdown = self.begin_shutdown();
        #[cfg(windows)]
        if let Some(control) = self.state.read().control() {
            control.shutdown_metadata();
        }
        let _ = self.cancel_calibration("Calibration cancelled by shutdown");
        loop {
            let result = self.stop(None);
            if self.state.lock().session.is_none() {
                break;
            }
            if let Err(error) = result {
                eprintln!(
                    "isocastd: shutdown waiting for recording: {}",
                    error.message
                );
            }
        }
        let _ = self.stream_stop(Default::default());
        self.stop_engine();
        self.flush_live_save();
        self.data.flush();
        // After the recording's end went out: OBS clients hear ExitStarted
        // and a close, not a dropped socket.
        self.websocket.stop();
        self.plugins.stop_all();
        #[cfg(unix)]
        let _ = std::fs::remove_file(socket_path());
        isocast_diagnostics::clean_exit();
    }

    /// The start of `graceful_shutdown`: refuse new work, then wait for an
    /// idle engine teardown already under way. Hold the guard to the end.
    fn begin_shutdown(&self) -> parking_lot::MutexGuard<'_, ()> {
        self.shutting_down.store(true, Ordering::SeqCst);
        self.shutdown_lock.lock()
    }

    /// Agree to shut down only while nothing runs. Leaves `shutting_down`
    /// set on success (the connection then runs `graceful_shutdown`).
    fn shutdown_if_idle(&self) -> Result<(), RpcError> {
        let _gate = self.starting.write();
        if self.shutting_down.swap(true, Ordering::SeqCst) {
            return Err(RpcError {
                code: RpcError::WRONG_STATE,
                message: "the daemon is already shutting down".into(),
            });
        }
        if let Some(busy) = self.busy() {
            self.shutting_down.store(false, Ordering::SeqCst);
            return Err(RpcError {
                code: RpcError::WRONG_STATE,
                message: format!("isocastd keeps running while {busy}"),
            });
        }
        Ok(())
    }

    /// What a shutdown would interrupt, with how to end it.
    fn busy(&self) -> Option<String> {
        let state = self.state.read();
        if state.session.is_some() {
            return Some(
                "recording; stop it first (`isocast stop`, or MCP's stop_recording)".into(),
            );
        }
        if state.streaming_on(CanvasId::Main) {
            return Some(
                "streaming; stop it first (`isocast stream stop`, or MCP's stop_stream)".into(),
            );
        }
        if let Some(canvas) = state.streaming_canvas() {
            return Some(format!(
                "streaming the {canvas} canvas; stop it first (`isocast stream stop --canvas \
                 {canvas}`, or MCP's stop_stream with canvas \"{canvas}\")"
            ));
        }
        if state.camera.active() {
            return Some(
                "feeding the virtual camera (a call may be using it); stop it first (`isocast vcam stop`, or MCP's virtual_camera_stop)".into(),
            );
        }
        if state.replay.active() {
            return Some(format!(
                "keeping a replay buffer; save it if needed, then stop it ({})",
                state.replay.stop_hint()
            ));
        }
        if state.tuning {
            return Some("benchmarking the encoders (`tune`); wait for it to finish".into());
        }
        None
    }

    fn call(self: &Arc<Self>, mut request: Request) -> Response {
        let id = request.id;
        // A network caller's paths must be inside the folder roots, and are
        // used in the form checked (decisions 25, 26); local callers pass
        // at once.
        if let Err(error) = self.check_params(&mut request.method) {
            return Response::err(Some(id), error);
        }
        let _starting = matches!(
            request.method,
            Method::Start(_)
                | Method::StreamStart(_)
                | Method::VirtualCameraStart
                | Method::ReplayStart(_)
                | Method::Resume
                | Method::Optimize(_)
        )
        .then(|| self.starting.read());
        if _starting.is_some()
            && let Err(error) =
                self.cancel_calibration("Calibration deferred for an output or tune")
        {
            return Response::err(Some(id), error);
        }
        if self.shutting_down.load(Ordering::SeqCst)
            && !matches!(
                request.method,
                Method::Status
                    | Method::Get(_)
                    | Method::Stop(_)
                    | Method::RequestStop(_)
                    | Method::Shutdown
                    | Method::Hello
            )
        {
            return Response::err(
                Some(id),
                RpcError {
                    code: RpcError::WRONG_STATE,
                    message: "the daemon is shutting down; wait for it to finish".into(),
                },
            );
        }
        let shutdown = request.method == Method::Shutdown;
        let result = match request.method {
            Method::WhepOffer(_) | Method::WhepClose => Err(RpcError {
                code: RpcError::WRONG_STATE,
                message: "whep_offer opens a dedicated connection; whep_close ends its viewer".into(),
            }),
            Method::GpuAdapter => native()
                .gpu_adapter_luid()
                .map_err(failed)
                .and_then(to_json),
            Method::MachineProfile => to_json(self.machine_status(true)),
            Method::MachineCalibrate => self.machine_calibrate().and_then(to_json),
            Method::MachineCancel => self.machine_cancel().and_then(to_json),
            Method::Status => to_json(self.status()),
            Method::Hello => {
                let mut hello = callers::hello();
                hello.engine_id = self.remote.status().identity.map(|identity| identity.id);
                to_json(hello)
            },
            Method::FolderRoots => to_json(self.folder_roots()),
            Method::FolderRootsSet(params) => self.folder_roots_set(params).and_then(to_json),
            Method::FilesRoots => to_json(self.files_roots()),
            Method::FilesList(params) => self.files_list(params).and_then(to_json),
            Method::EncoderOptions => to_json(isocast_protocol::Codec::options()),
            Method::BugReport(params) => diagnostics::bug_report(self, &params).and_then(to_json),
            Method::ReportFile(params) => isocast_diagnostics::preview::read(&params)
                .map_err(diagnostics::error)
                .and_then(to_json),
            Method::Logs(params) => isocast_diagnostics::bundle::logs(&params)
                .map_err(diagnostics::error)
                .and_then(to_json),
            Method::Pause => self.set_paused(true).and_then(to_json),
            Method::Resume => self.set_paused(false).and_then(to_json),
            Method::Devices => devices().map_err(failed).and_then(to_json),
            Method::Get(params) => self.get(params.unwrap_or_default()).and_then(to_json),
            Method::Plan(change) => {
                let state = self.state.read();
                plan(&state, *change, &self.plugins).and_then(to_json)
            }
            Method::Apply(params) => self.apply(*params).and_then(to_json),
            Method::Split => self
                .split_take()
                .map(|split| Started {
                    begins_at: Some(split_text(&split)),
                    files: split.files,
                    notes: split.notes,
                    session: None,
                    already_recording: false,
                })
                .and_then(to_json),
            Method::Tune(params) => self.tune(*params).and_then(to_json),
            Method::Snapshot(params) => self.snapshot(params).and_then(to_json),
            Method::Frame(params) => self.frame(params).and_then(to_json),
            // Handled by the connection, which turns into a video stream.
            Method::Frames(_) => Err(RpcError {
                code: RpcError::INVALID_REQUEST,
                message: "frames must be the last request on its connection".into(),
            }),
            Method::SharedFrames(_) => Err(RpcError {
                code: RpcError::INVALID_REQUEST,
                message: "shared_frames must be the last request on its connection".into(),
            }),
            Method::Engine { running } => self.hold_engine(running).and_then(to_json),
            Method::Permissions => permissions().map_err(failed).and_then(to_json),
            Method::RequestPermissions(params) => {
                request_permissions(params.and_then(|p| p.permission))
                    .map_err(failed)
                    .and_then(to_json)
            }
            Method::OpenPermissionSettings { permission } => open_permission_settings(permission)
                .map(|url| isocast_protocol::api::Opened { url })
                .map_err(failed)
                .and_then(to_json),
            Method::ImportObs(params) => import_obs(*params).and_then(to_json),
            Method::ObsDiscover(params) => self.obs_discover(params).and_then(to_json),
            Method::SpeedTest(params) => self.speed_test(*params).and_then(to_json),
            Method::Optimize(params) => self.optimize(*params).and_then(to_json),
            Method::InitSetup(params) => self.init_setup(*params).and_then(to_json),
            Method::Doctor(params) => check_folder(params.folder.as_deref()).and_then(|()| {
                let setup = params.setup.or_else(|| self.state.lock().setup.clone());
                // The disk test writes in the nearest folder that exists: for
                // a network caller only inside the folder roots (decision 26).
                let may_write = |existing: &Path| {
                    self.check_path("folder", existing, roots::Access::Write)
                        .is_ok()
                };
                let mut report = doctor(setup.as_ref(), params.folder.as_deref(), &may_write);
                if let Some(control) = self.state.read().control() {
                    for detail in control.metadata_pending() {
                        report.checks.push(isocast_protocol::api::Check {
                            name: "recording metadata publication".into(),
                            status: isocast_protocol::api::CheckStatus::Problem,
                            detail,
                            fix: Some("Close readers or restore folder write permission; keep Isocast running while it retries.".into()),
                        });
                    }
                }
                if let Some(setup) = setup
                    && let Ok(estimate) = self.estimate_capacity(CapacityParams {
                        setup: Some(setup),
                        ..Default::default()
                    })
                {
                    report
                        .checks
                        .extend(isocast_engine::estimate::check(&estimate));
                }
                report.checks.extend(self.plugins.doctor_check());
                to_json(report)
            }),
            Method::Marker { label } => self.marker(label).and_then(to_json),
            Method::RefreshBrowser { source } => self.refresh_browser(source).and_then(to_json),
            Method::SwitchScene {
                scene,
                transition,
                canvas,
            } => self
                .switch_scene(scene, transition, canvas)
                .and_then(to_json),
            Method::Preview { scene, canvas } => self.preview(scene, canvas).and_then(to_json),
            Method::ClearPreview => to_json(self.clear_preview()),
            Method::Take { transition, canvas } => self.take(transition, canvas).and_then(to_json),
            Method::ClearCanvasPreview { canvas } => {
                self.clear_canvas_preview(canvas).and_then(to_json)
            }
            // Handled by the connection, which turns into an event stream.
            Method::Watch(_) => Err(RpcError {
                code: RpcError::INVALID_REQUEST,
                message: "watch must be the first request on its connection".into(),
            }),
            Method::Start(params) => self.start(*params).and_then(to_json),
            Method::StreamStart(params) => self
                .stream_start(params.unwrap_or_default())
                .and_then(to_json),
            Method::StreamStop(params) => self
                .stream_stop(params.unwrap_or_default())
                .and_then(to_json),
            Method::StreamDestinationAdd(params) => {
                self.stream_destination_add(*params).and_then(to_json)
            }
            Method::StreamDestinationRemove { id, canvas } => {
                self.stream_destination_remove(id, canvas).and_then(to_json)
            }
            Method::StreamDestinationSet(params) => {
                self.stream_destination_set(*params).and_then(to_json)
            }
            Method::CanvasAdd(params) => self.canvas_add(*params).and_then(to_json),
            Method::CanvasSet(params) => self.canvas_set(*params).and_then(to_json),
            Method::CanvasRemove { canvas } => self.canvas_remove(canvas).and_then(to_json),
            Method::VirtualCameraStart => self.virtual_camera_start().and_then(to_json),
            Method::VirtualCameraStop => to_json(self.virtual_camera_stop()),
            Method::VirtualCameraStatus => to_json(self.virtual_camera_status()),
            Method::VirtualCameraRemove => self.virtual_camera_remove().and_then(to_json),
            Method::ReplayStart(params) => self
                .replay_start(params.unwrap_or_default().canvas)
                .and_then(to_json),
            Method::ReplayStop(params) => self
                .replay_stop(params.unwrap_or_default().canvas)
                .and_then(to_json),
            Method::ReplaySave(params) => self
                .replay_save(params.unwrap_or_default().canvas)
                .and_then(to_json),
            Method::ReplayStatus(params) => {
                to_json(self.replay_status(params.unwrap_or_default().canvas))
            }
            Method::ProjectorOpen(params) => self.projector_open(*params).and_then(to_json),
            Method::ProjectorClose(params) => self.projector_close(params).and_then(to_json),
            Method::ProjectorList => to_json(self.projector_list()),
            Method::ProjectorPlace(params) => self.projector_place(*params).and_then(to_json),
            Method::WebsocketStatus => to_json(self.websocket.status()),
            Method::RemoteStatus => to_json(self.remote.status()),
            Method::RemoteEnable(params) => self.remote.enable(self, params.unwrap_or_default()).and_then(to_json),
            Method::RemoteDisable => self.remote.disable(self).and_then(to_json),
            Method::RemotePairBegin => self.remote.begin().and_then(to_json),
            Method::RemotePairFinish => Err(RpcError { code: RpcError::NOT_PERMITTED, message: "remote_pair_finish requires the dedicated TLS-bound PAKE exchange; use `isocast remote pair HOST --code-stdin`".into() }),
            Method::RemoteClients => self.remote.clients().and_then(to_json),
            Method::RemoteRevoke(params) => self.remote.revoke(self, &params.client_id).and_then(to_json),
            Method::WebsocketEnable(params) => self
                .websocket
                .enable(self, params.unwrap_or_default())
                .and_then(to_json),
            Method::WebsocketDisable => self.websocket.disable().and_then(to_json),
            Method::StreamKeySet(params) => self.stream_key_set(*params).and_then(to_json),
            Method::StreamKeyClear(target) => self.stream_key_clear(*target).and_then(to_json),
            Method::Library => to_json(self.library()),
            Method::LibraryGet(target) => self.library_get(target).and_then(to_json),
            Method::LibraryCreate(params) => self.library_create(*params).and_then(to_json),
            Method::LibraryRename(params) => self.library_rename(*params).and_then(to_json),
            Method::LibraryDelete(target) => self.library_delete(target).and_then(to_json),
            Method::LibrarySwitch(params) => self.library_switch(params).and_then(to_json),
            Method::LibraryImport(params) => self.library_import(*params).and_then(to_json),
            Method::LibraryImportObs(params) => self.library_import_obs(*params).and_then(to_json),
            Method::PluginList => to_json(self.plugins.list()),
            Method::PluginAdd(params) => self.plugins.add(*params).and_then(to_json),
            Method::PluginRemove(target) => self.plugin_remove(&target).and_then(to_json),
            Method::PluginSetEnabled(params) => self.plugins.set_enabled(params).and_then(to_json),
            Method::DataGet(params) => self
                .data
                .store
                .read(&params.unwrap_or_default())
                .map_err(refused)
                .and_then(to_json),
            Method::DataSet(params) => {
                let params = *params;
                self.data
                    .store
                    .set(params.values, params.persist)
                    .map_err(refused)
                    .and_then(|changed| {
                        if changed.persisted {
                            self.data.changed_kept();
                        }
                        to_json(changed.reply())
                    })
            }
            Method::DataDelete(params) => self
                .data
                .store
                .delete(&params.keys)
                .map_err(refused)
                .and_then(|changed| {
                    if changed.persisted {
                        self.data.changed_kept();
                    }
                    to_json(changed.reply())
                }),
            Method::HotkeysGet => to_json(self.hotkeys_get()),
            Method::HotkeysSet(params) => self.hotkeys_set(*params).and_then(to_json),
            Method::HotkeysActions => to_json(isocast_protocol::hotkeys::catalogue()),
            Method::HotkeysInput(_) => Err(RpcError {
                code: RpcError::WRONG_STATE,
                message: "hotkeys_input needs a client connection".into(),
            }),
            Method::ShutdownIfIdle => self.shutdown_if_idle().and_then(to_json),
            Method::DebugStatus => to_json(debug::status(self)),
            Method::DebugSet(params) => debug::set(self, params).and_then(to_json),
            Method::ListProjects(params) => to_json(self.list_projects(params.unwrap_or_default())),
            Method::RenameProject(params) => self.rename_project(params).and_then(to_json),
            Method::TakeExportNle(params) => self.take_export_nle(params).and_then(to_json),
            Method::EstimateCapacity(params) => self.estimate_capacity(*params).and_then(to_json),
            Method::RequestStop(params) => self
                .request_stop(params.unwrap_or_default().session.as_deref())
                .and_then(to_json),
            Method::Stop(params) => self
                .stop(params.unwrap_or_default().session.as_deref())
                .and_then(to_json),
            Method::Shutdown => {
                self.shutting_down.store(true, Ordering::SeqCst);
                #[cfg(windows)]
                if let Some(control) = self.state.read().control() {
                    control.shutdown_metadata();
                }
                match self.stop(None) {
                    // Shutting down while idle is fine.
                    Err(err) if shutdown && err.code == RpcError::WRONG_STATE => {
                        to_json(serde_json::Value::Null)
                    }
                    result => result.and_then(to_json),
                }
            }
        };
        match result {
            Ok(value) => Response::ok(id, value),
            Err(error) => Response::err(Some(id), error),
        }
    }

    fn publish(&self, event: Event) {
        if matches!(event, Event::SetupChanged { .. }) {
            self.refresh_hotkeys();
            // `recording.folder` may name a new recordings folder.
            self.plugins.refresh_sandboxes();
            debug::sync(self, "setup changed");
        }
        diagnostics::event(&event);
        // A display went away: full-screen projectors on it close (after
        // this event goes out).
        let display_gone = match &event {
            Event::DeviceChanged {
                action: isocast_protocol::api::DeviceAction::Removed,
                kind: isocast_protocol::api::DeviceKind::Display,
                id,
                name,
            } => Some((id.clone(), name.clone())),
            _ => None,
        };
        let display_came = matches!(
            &event,
            Event::DeviceChanged {
                action: isocast_protocol::api::DeviceAction::Added,
                kind: isocast_protocol::api::DeviceKind::Display,
                ..
            }
        );
        self.send_event(event);
        if let Some((id, name)) = display_gone {
            self.projectors_display_gone(&id, &name);
        }
        if display_came {
            self.projectors_display_came();
        }
    }

    /// Hand `event` to the watchers that want it.
    fn send_event(&self, event: Event) {
        let mut watchers = self.watchers.lock();
        // Nobody wants it (no one watches, or every watcher left its kind
        // out): not even serialized.
        let kind = event.kind();
        if !watchers.iter().any(|watcher| watcher.wanted.contains(kind)) {
            return;
        }
        // Serialize once, including the line delimiter, and share the
        // immutable bytes across clients. Never wait on a client's queue.
        let mut bytes = match serde_json::to_vec(&Notification::event(event)) {
            Ok(bytes) => bytes,
            Err(error) => {
                isocast_diagnostics::diagnostic!("error"; "isocastd: could not serialize an event: {error}");
                return;
            }
        };
        bytes.push(b'\n');
        let bytes: Arc<[u8]> = bytes.into();
        watchers.retain(|watcher| {
            !watcher.wanted.contains(kind) || watcher.tx.try_send(bytes.clone()).is_ok()
        });
    }

    /// Remove a plugin; refused while the live setup uses its kinds, unless
    /// forced (the setup then keeps them, bypassed).
    fn plugin_remove(
        &self,
        target: &isocast_protocol::plugin::PluginTarget,
    ) -> Result<isocast_protocol::plugin::PluginChange, RpcError> {
        let used = match &self.state.lock().setup {
            Some(setup) => isocast_protocol::plugin::uses(setup, &target.id),
            None => Vec::new(),
        };
        if !used.is_empty() && !target.force {
            return Err(RpcError {
                code: RpcError::WRONG_STATE,
                message: format!(
                    "the live setup uses {}: {}; remove those first, or give --force (MCP: \
                     force) to remove it anyway: the setup keeps them, bypassed (the sound \
                     passes through), until the plugin is added again",
                    target.id,
                    used.join(", ")
                ),
            });
        }
        let mut change = self.plugins.remove(&target.id)?;
        if !used.is_empty() {
            change.notes.push(format!(
                "the live setup still has {}: kept as they are and bypassed until {} is added \
                 again",
                used.join(", "),
                target.id
            ));
        }
        Ok(change)
    }

    fn get(&self, params: GetParams) -> Result<Live, RpcError> {
        let state = self.state.read();
        if let Some(setup) = state.setup.as_ref()
            && !setup.has_canvas(params.canvas)
        {
            return Err(RpcError {
                code: RpcError::WRONG_STATE,
                message: streams::no_canvas(setup, params.canvas),
            });
        }
        let (collection, profile) = state.file.current_names(state.setup.as_ref());
        let mut live = Live {
            revision: state.revision,
            setup: state.setup.clone(),
            resolved: None,
            canvas: CanvasId::Main,
            setup_file: Some(state.file.info()),
            collection,
            profile,
        };
        let engine = state.engine.clone();
        // Not holding the state while the engine lays out.
        drop(state);
        if params.resolved
            && let Some(setup) = &live.setup
        {
            live.canvas = params.canvas;
            live.resolved = if params.canvas.is_main() {
                Some(resolved(setup, live.revision, engine, &self.data.store))
            } else {
                resolved_vertical(
                    setup,
                    params.canvas,
                    live.revision,
                    engine,
                    &self.data.store,
                )
            };
        }
        Ok(live)
    }

    /// Load the setup saved by the last run, at [`first_revision`]. A file
    /// that can't be loaded is left alone and reported (in `get`, `status`
    /// and to watchers), and the daemon starts without a setup.
    fn load_setup(&self) {
        let mut state = self.state.lock();
        let loaded = state.file.load();
        for note in state.file.notes() {
            isocast_diagnostics::diagnostic!("isocastd: {note}");
        }
        let path = state.file.info().path;
        match loaded {
            Some(setup) => {
                isocast_diagnostics::diagnostic!(
                    "isocastd: loaded the setup from {}",
                    path.display()
                );
                let view = state
                    .file
                    .current()
                    .map(|current| {
                        let file = state.file.path(
                            isocast_protocol::library::LibraryKind::Collection,
                            &current.collection,
                        );
                        store::read_view(&file).unwrap_or_else(|err| {
                            isocast_diagnostics::diagnostic!("warn"; "isocastd: {err:#}");
                            store::CollectionView::default()
                        })
                    })
                    .unwrap_or_default()
                    .restore(&setup);
                state.scene = view.scene;
                state.preview = view.preview;
                state.setup = Some(setup.clone());
                state.revision = first_revision();
                // The setup this machine starts with records where it was
                // set to here.
                self.note_local_recording_folder(&setup.recording.folder);
                // An unlinked vertical canvas starts from the main canvas's
                // scene and preview (no engine or watchers yet).
                let _ = sync_canvases(&mut state, &[], true);
            }
            None => match state.file.load_warning() {
                Some(warning) => isocast_diagnostics::diagnostic!("warn"; "isocastd: {warning}"),
                None => isocast_diagnostics::diagnostic!(
                    "isocastd: no saved setup yet at {}",
                    path.display()
                ),
            },
        }
    }

    /// Load the kept data keys of the scene collection in use, and start
    /// announcing data changes and saving kept keys.
    fn load_data(self: &Arc<Self>) {
        // Revisions never repeat across runs, as the setup's don't.
        self.data.store.start_at(first_revision());
        match self.collection_file() {
            Some(file) => {
                for note in self.data.load_collection(&file) {
                    isocast_diagnostics::diagnostic!("isocastd: {note}");
                }
            }
            None => self.data.use_collection(None),
        }
        let daemon = Arc::downgrade(self);
        self.data.start(Box::new(move |event| {
            if let Some(daemon) = daemon.upgrade() {
                daemon.publish(event);
            }
        }));
    }

    /// The file of the scene collection in use, once there is one.
    fn collection_file(&self) -> Option<PathBuf> {
        let state = self.state.read();
        let current = state.file.current()?;
        Some(state.file.path(
            isocast_protocol::library::LibraryKind::Collection,
            &current.collection,
        ))
    }

    /// Kept data keys go beside the scene collection in use, once there is
    /// one (the first setup applied makes it).
    fn follow_collection(&self) {
        if self.data.path().is_none()
            && let Some(file) = self.collection_file()
        {
            self.data.use_collection(Some(&file));
        }
    }

    fn apply(&self, params: ApplyParams) -> Result<Applied, RpcError> {
        // A network caller's change is checked first, outside the lock
        // (it may look a browser source's host up): developer settings,
        // files, URLs and folders (decision 25). The change is then made
        // only if the setup is still the one checked.
        let checked = if callers::current().is_local() {
            None
        } else {
            let (revision, old, planned) = {
                let state = self.state.read();
                let planned = plan(&state, params.change.clone(), &self.plugins)?;
                (state.revision, state.setup.clone(), planned)
            };
            if let Some(new) = &planned.setup {
                self.check_setup(new, old.as_ref())?;
            }
            Some(revision)
        };
        let mut state = self.state.lock();
        if checked.is_some_and(|revision| revision != state.revision) {
            return Err(RpcError {
                code: RpcError::WRONG_STATE,
                message: format!(
                    "the live setup changed (revision {}) while the change was checked; plan \
                     it again",
                    state.revision
                ),
            });
        }
        let mut plan = plan(&state, params.change, &self.plugins)?;
        if params.revision != state.revision {
            return Err(RpcError {
                code: RpcError::WRONG_STATE,
                message: format!(
                    "the live setup is at revision {}, not {}; plan the change again",
                    state.revision, params.revision
                ),
            });
        }
        if !plan.problems.is_empty() {
            let problems: Vec<String> = plan
                .problems
                .iter()
                .map(|p| format!("{}: {}", p.path, p.message))
                .collect();
            return Err(RpcError {
                code: RpcError::FAILED,
                message: format!(
                    "the change leaves problems, so nothing was applied:\n  {}",
                    problems.join("\n  ")
                ),
            });
        }
        // Nothing changes: no new revision, so other clients' plans stay
        // valid, and nothing to save or announce.
        if plan.changes.is_empty() {
            return Ok(Applied {
                revision: state.revision,
                changes: Vec::new(),
                notes: plan.notes,
            });
        }
        // A plugin may only move `recording.folder` to a recordings folder
        // there is already, nor plant a hotkey that records elsewhere
        // (#103).
        if PluginCaller::current().is_some()
            && let Some(setup) = &plan.setup
        {
            let folder_of = |setup: &Setup| expand_home(&setup.recording.folder);
            let mut roots = self.remembered_recording_folders();
            roots.extend(state.setup.as_ref().map(folder_of));
            let new = folder_of(setup);
            if state.setup.as_ref().map(folder_of).as_ref() != Some(&new) {
                plugin_folder_refusal("recording.folder", &new, &roots)?;
            }
            plugin_hotkeys_refusal(
                "hotkeys",
                &setup.hotkeys,
                state.setup.as_ref().map(|s| &s.hotkeys),
                &roots,
            )?;
        }
        // Whether the vertical canvas was there, and linked, before.
        let vertical_before = canvas_links(&state);
        // A network caller's browser sources load the URL as checked
        // (WHATWG-normalized; decision 26).
        if !callers::current().is_local()
            && let Some(setup) = plan.setup.as_mut()
        {
            roots::normalize_browser_urls(setup);
        }
        // A network caller moving a destination clears its key before
        // anything else changes; refused (engine untouched) if it can't.
        let cleared = self.clear_moved_keys(state.setup.as_ref(), plan.setup.as_ref())?;
        // The live engine runs it from the next frame (or refuses, like a
        // canvas change while recording).
        if let (Some(engine), Some(setup)) = (&state.engine, &plan.setup) {
            engine
                .update(&state.with_hotkey_holds(setup), state.revision + 1)
                .map_err(|message| RpcError {
                    code: RpcError::WRONG_STATE,
                    message: callers::with_cleared(message, &cleared),
                })?;
        }
        state.setup = plan.setup;
        // A local caller's recordings folder is one set on this machine: a
        // folder root for network callers (decision 26). Nobody else's is.
        if let Some(setup) = &state.setup {
            self.note_local_recording_folder(&setup.recording.folder);
        }
        state.prune_hotkey_holds();
        state.revision += 1;
        let mut notes = plan.notes;
        notes.extend(cleared.iter().cloned());
        // Saved after the lock is released: writing (two fsyncs) can take
        // a while, and status, switches and frames shouldn't wait for it.
        let (prepared, target) = state.file.prepare();
        notes.extend(prepared);
        let to_save = match (&state.setup, target) {
            (Some(setup), Some(target)) => Some((setup.clone(), target)),
            _ => None,
        };
        if !state
            .scene_ids()
            .contains(&state.scene.clone().unwrap_or_default())
        {
            state.scene = state.scene_ids().into_iter().next();
            if let (Some(control), Some(scene)) = (state.control(), &state.scene) {
                let _ = control.switch_scene(now(), scene.clone(), Transition::default());
            }
        }
        if state
            .preview
            .as_ref()
            .is_some_and(|preview| !state.scene_ids().contains(preview))
        {
            state.preview = None;
            if let Some(engine) = &state.engine {
                engine.set_preview(None);
            }
        }
        let vertical_events = sync_canvases(&mut state, &vertical_before, false);
        let revision = state.revision;
        drop(state);
        self.publish(Event::SetupChanged {
            revision,
            by: callers::by(),
        });
        for message in cleared {
            self.publish(Event::Warning { message });
        }
        for event in vertical_events {
            self.publish(event);
        }
        if let Some((setup, target)) = to_save {
            if params.live {
                // Saved by the saver thread once the gesture pauses.
                *self.live_save.lock() = Some((revision, setup, target));
                self.live_saved.notify_all();
                return Ok(Applied {
                    revision,
                    changes: plan.changes,
                    notes,
                });
            }
            notes.extend(self.save(revision, &setup, &target));
        }
        if let Some(message) = self.state.lock().file.info().save_error {
            self.publish(Event::Warning { message });
        }
        self.follow_collection();
        Ok(Applied {
            revision,
            changes: plan.changes,
            notes,
        })
    }

    /// Write `setup` (at `revision`) to the collection and profile files of
    /// `target`, in revision order per file: an older change that lost a
    /// race never overwrites a newer one. Returns notes about a failure.
    fn save(&self, revision: u64, setup: &Setup, target: &store::Target) -> Vec<String> {
        let mut saved = self.saved.lock();
        let newer = |path: &PathBuf| saved.get(path).is_none_or(|&last| revision > last);
        let parts = store::Parts {
            collection: newer(&target.collection),
            profile: newer(&target.profile),
        };
        if !parts.collection && !parts.profile {
            return Vec::new();
        }
        let result = store::write(target, setup, parts);
        for (path, written) in [
            (&target.collection, parts.collection),
            (&target.profile, parts.profile),
        ] {
            if written {
                saved.insert(path.clone(), revision);
            }
        }
        self.state.lock().file.saved(result)
    }

    /// The saver thread: writes the newest live change once changes pause.
    fn save_live_changes(&self) {
        loop {
            let mut pending = self.live_save.lock();
            while pending.is_none() {
                self.live_saved.wait(&mut pending);
            }
            let first = std::time::Instant::now();
            loop {
                let wait = self.live_saved.wait_for(&mut pending, SAVE_QUIET);
                if wait.timed_out() || first.elapsed() >= SAVE_AT_MOST {
                    break;
                }
            }
            let Some((revision, setup, target)) = pending.take() else {
                continue;
            };
            drop(pending);
            if !self.save(revision, &setup, &target).is_empty()
                && let Some(message) = self.state.lock().file.info().save_error
            {
                self.publish(Event::Warning { message });
            }
        }
    }

    /// Save a live change still waiting for the saver (before exiting).
    fn flush_live_save(&self) {
        let pending = self.live_save.lock().take();
        if let Some((revision, setup, target)) = pending {
            let _ = self.save(revision, &setup, &target);
        }
    }

    /// A starting setup from this Mac's devices, applied if asked.
    fn init_setup(&self, params: InitParams) -> Result<Initialized, RpcError> {
        let devices = devices().map_err(failed)?;
        let mut made = init::init(&devices, &params.options).map_err(|message| RpcError {
            code: RpcError::FAILED,
            message,
        })?;
        if params.options.fps.is_none()
            && this_machine().is_some_and(|machine| init::starting_fps(machine) == 30)
            && made.setup.canvas.fps > 30
        {
            made = init::init(
                &devices,
                &init::InitOptions {
                    fps: Some(30),
                    ..params.options.clone()
                },
            )
            .map_err(|message| RpcError {
                code: RpcError::FAILED,
                message,
            })?;
            made.notes.push(
                "Starts at 30 fps on machines with at most 8 CPU cores or 8 GB memory, leaving room for ISO files and streaming. An explicit fps choice is kept; this is a starting point, not a guarantee for every source or outside workload".into(),
            );
        }
        if !params.apply {
            return Ok(made);
        }
        let revision = self.state.lock().revision;
        let mut applied = self.apply(ApplyParams {
            revision,
            change: Change::Setup(Box::new(made.setup.clone())),
            live: false,
        })?;
        if revision > 0 {
            let file = self.state.lock().file.info();
            applied.notes.push(format!(
                "replaced the live setup (revision {revision}), in the current scene collection \
                 and profile; the previous versions are kept in {}.bak{}",
                file.path.display(),
                file.profile_path
                    .map(|path| format!(" and {}.bak", path.display()))
                    .unwrap_or_default()
            ));
        }
        // A new setup starts on its first scene, not on a scene of the old
        // one that happens to share its id ("screen").
        if let Some(first) = made.setup.scenes.first() {
            let state = self.state.lock();
            if state.scene.as_ref() != Some(&first.id) {
                self.go_on_air(state, first.id.clone(), Some(Transition::default()), false)?;
            }
        }
        made.applied = Some(applied);
        Ok(made)
    }

    /// Where `tune` writes its benchmark files: as given for a local
    /// caller; for a network caller the folder (given, or the setup's
    /// recordings folder) resolved inside the folder roots.
    fn tune_folder(
        &self,
        folder: Option<&Path>,
        setup: &Setup,
    ) -> Result<Option<PathBuf>, RpcError> {
        if callers::current().is_local() {
            return Ok(folder.map(Path::to_path_buf));
        }
        let folder = folder
            .map(Path::to_path_buf)
            .unwrap_or_else(|| expand_home(&setup.recording.folder));
        self.checked_path("folder", &folder.to_string_lossy(), roots::Access::Write)
            .map(Some)
    }

    fn tune(&self, params: TuneParams) -> Result<Tuning, RpcError> {
        check_folder(params.folder.as_deref())?;
        // Optimize already holds a read guard; recursive acquisition must
        // not deadlock behind a queued shutdown writer.
        let gate = self.starting.read_recursive();
        self.cancel_calibration("Calibration deferred for tune")?;
        let mut state = self.state.lock();
        if state.session.is_some() || state.tuning || state.camera.active() {
            return Err(RpcError {
                code: RpcError::WRONG_STATE,
                message: "not while recording, tuning or using the virtual camera: the benchmark would compete for the \
                          encoders"
                    .into(),
            });
        }
        let Some(setup) = params.setup.or_else(|| state.setup.clone()) else {
            return Err(state.no_setup("no setup to tune (or give one)"));
        };
        // Kept beside the setup file, for `start` to check against.
        let memory = capacity::memory_path(state.file.legacy());
        // Not under the `starting` gate (a benchmark takes minutes, and a
        // restart would wait for it rather than be refused, #45): checked
        // under the state lock instead, which `shutdown_if_idle` takes
        // after setting `shutting_down`, so one of the two sees the other.
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(RpcError {
                code: RpcError::WRONG_STATE,
                message: "the daemon is shutting down; tune once it has restarted".into(),
            });
        }
        // Where the benchmark writes, however chosen (given, or the setup's
        // recordings folder): for a network caller inside the folder roots,
        // checked right before and used as checked (decision 26).
        let folder = self.tune_folder(params.folder.as_deref(), &setup)?;
        state.tuning = true;
        drop(state);
        drop(gate);
        // Recording stays blocked until `tuning` clears, so clear it even if
        // the benchmark panics.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tune(&setup, folder.as_deref(), &memory)
        }))
        .unwrap_or_else(|_| {
            Err(anyhow::anyhow!(
                "the benchmark panicked; use `isocast logs --level error` or `isocast report`"
            ))
        })
        .map_err(failed);
        self.state.lock().tuning = false;
        result
    }

    fn snapshot(&self, params: SnapshotParams) -> Result<Snapshot, RpcError> {
        let state = self.state.read();
        // While the live engine runs, frames come from it.
        let setup = state.setup.clone();
        let control = state.control();
        let on_air = state.scene.clone();
        let preview = state.preview.clone();
        drop(state);
        let whose = "the live";
        match (params.source, params.scene) {
            (Some(_), Some(_)) => Err(failed(anyhow::anyhow!(
                "give a source or a scene, not both"
            ))),
            (Some(source), None) => {
                let (setup, id) = match setup {
                    Some(setup) if setup.source(&source).is_some() => (setup, source),
                    setup => {
                        let quick = Setup::quick(std::slice::from_ref(&source)).map_err(|err| {
                            failed(anyhow::anyhow!(
                                "{source:?} is neither a source of {whose} setup ({}) nor a \
                                 quick source: {err}",
                                ids_or_none(setup.iter().flat_map(|s| &s.sources).map(|s| &s.id))
                            ))
                        })?;
                        let id = quick.sources[0].id.clone();
                        (quick, id)
                    }
                };
                check_cameras(&setup, std::slice::from_ref(&id), control.as_deref())?;
                let data = &self.data.store;
                let (png, width, height) =
                    source_png(&setup, &id, control.as_deref(), params.width, data)
                        .map_err(failed)?;
                Ok(Snapshot {
                    source: Some(id),
                    scene: None,
                    width,
                    height,
                    png_base64: base64::engine::general_purpose::STANDARD.encode(png),
                })
            }
            (None, scene) => {
                let Some(setup) = setup else {
                    return Err(RpcError {
                        code: RpcError::WRONG_STATE,
                        message: "no live setup, so no scenes; give a source, or apply a setup"
                            .into(),
                    });
                };
                // The program while the engine runs: the canvas it drew last.
                let recorded = control
                    .as_ref()
                    .filter(|control| control.captured().iter().any(|id| id == PROGRAM));
                if scene.is_none() && !params.preview && recorded.is_some() {
                    let (png, width, height) = source_png(
                        &setup,
                        PROGRAM,
                        recorded.map(|c| &**c),
                        params.width,
                        &self.data.store,
                    )
                    .map_err(failed)?;
                    return Ok(Snapshot {
                        source: None,
                        scene: on_air,
                        width,
                        height,
                        png_base64: base64::engine::general_purpose::STANDARD.encode(png),
                    });
                }
                let id = match (scene, params.preview) {
                    (Some(scene), _) => Some(scene),
                    (None, true) => preview,
                    (None, false) => on_air,
                };
                let Some(id) = id else {
                    return Err(failed(anyhow::anyhow!(
                        "no scene {}; give one of: {}",
                        if params.preview {
                            "in preview"
                        } else {
                            "on air"
                        },
                        ids_or_none(setup.scenes.iter().map(|s| &s.id))
                    )));
                };
                let Some(found) = setup.scenes.iter().find(|s| s.id == id) else {
                    return Err(failed(anyhow::anyhow!(
                        "no scene {id:?} in {whose} setup; scenes: {}",
                        ids_or_none(setup.scenes.iter().map(|s| &s.id))
                    )));
                };
                let shown = found.video_sources(&setup.scenes);
                // And what their masks show (#115).
                let masks = setup.mask_needs(&shown);
                let sources: Vec<String> = (shown.iter().chain(&masks))
                    .map(|id| id.to_string())
                    .collect();
                check_cameras(&setup, &sources, control.as_deref())?;
                let (png, width, height) = scene_png(
                    &setup,
                    &id,
                    &sources,
                    control.as_deref(),
                    params.width,
                    &self.data.store,
                )
                .map_err(failed)?;
                Ok(Snapshot {
                    source: None,
                    scene: Some(id),
                    width,
                    height,
                    png_base64: base64::engine::general_purpose::STANDARD.encode(png),
                })
            }
        }
    }

    /// Shared handler for CLI/MCP Pause and Resume, and the hotkey
    /// actions recording.pause / recording.resume / recording.pause_toggle.
    fn set_paused(&self, paused: bool) -> Result<isocast_protocol::api::RecordingPause, RpcError> {
        let wrong = |message: String| RpcError {
            code: RpcError::WRONG_STATE,
            message,
        };
        let (control, stop, id, epoch) = {
            let state = self.state.read();
            if !paused
                && state.setup.as_ref().is_some_and(|s| s.records_a_program())
                && let Some(reason) = state.engine.as_ref().and_then(|e| e.program_failure())
            {
                return Err(wrong(reason));
            }
            let session = state
                .session
                .as_ref()
                .ok_or_else(|| wrong("recording: idle; valid action: start".into()))?;
            if session.stop.load(Ordering::SeqCst) {
                return Err(wrong(
                    "recording: finalizing (a stop was requested); it can't pause or resume".into(),
                ));
            }
            if session.files.lock().is_none() {
                return Err(wrong(
                    "recording: starting; wait for recording state".into(),
                ));
            }
            let control = state.control().expect("recording engine");
            let epoch = control
                .recording_epoch()
                .expect("announced recording clock");
            (control, session.stop.clone(), session.id, epoch)
        };
        // Never hold the daemon state while waiting for the recording clock.
        // Stop/status and unrelated outputs must remain independently usable.
        let (at, generation) = control.set_paused(paused, now(), epoch).map_err(wrong)?;
        thread::sleep(at.saturating_since(now()));
        let deadline = std::time::Instant::now() + START_TIMEOUT;
        while !control
            .pause_saved(generation)
            .map_err(|message| RpcError {
                code: RpcError::FAILED,
                message,
            })?
        {
            if stop.load(Ordering::SeqCst) || std::time::Instant::now() >= deadline {
                return Err(wrong(
                    "recording.pauses: stopped or timed out waiting for the manifest checkpoint"
                        .into(),
                ));
            }
            thread::sleep(Duration::from_millis(2));
        }
        let state = self.state.read();
        if stop.load(Ordering::SeqCst) || state.session.as_ref().is_none_or(|s| s.id != id) {
            return Err(wrong("recording: stopped during pause/resume".into()));
        }
        if !paused
            && state.setup.as_ref().is_some_and(|s| s.records_a_program())
            && let Some(reason) = state.engine.as_ref().and_then(|e| e.program_failure())
        {
            return Err(wrong(reason));
        }
        let mut recording = control.pause_status(now()).expect("recording clock");
        let project = state
            .session
            .as_ref()
            .and_then(|s| s.project.as_ref().map(|p| p.id.clone()));
        drop(state);
        if paused && let Some((recoverable, errors)) = control.pause_outcome(generation) {
            recording.recoverable_seconds = recoverable;
            recording.errors = errors;
        }
        self.publish(Event::RecordingPaused {
            recording: recording.clone(),
            project,
        });
        Ok(recording)
    }

    /// Split the running recording into a new take.
    fn split_take(&self) -> Result<SplitTake, RpcError> {
        let refused = |code: i64, message: &str| RpcError {
            code,
            message: message.into(),
        };
        let state = self.state.read();
        let (Some(session), Some(control)) = (&state.session, state.control()) else {
            return Err(refused(RpcError::WRONG_STATE, "not recording"));
        };
        if session.stop.load(Ordering::SeqCst) {
            return Err(refused(
                RpcError::WRONG_STATE,
                "the recording is stopping, so no new take can begin",
            ));
        }
        if session.files.lock().is_none() {
            return Err(refused(
                RpcError::WRONG_STATE,
                "the recording is still starting; split once it has started (`isocast status` \
                 says when)",
            ));
        }
        let reply = control.split();
        let current = session.files.clone();
        let take = session.take.clone();
        let project = session.project.clone();
        let token = session.token.clone();
        drop(state);
        match reply.recv_timeout(START_TIMEOUT) {
            Ok(Ok(split)) => {
                *current.lock() = Some(split.files.clone());
                let begins_at = split_text(&split);
                take.store(split.take, Ordering::SeqCst);
                self.publish(Event::Started {
                    files: split.files.clone(),
                    begins_at: Some(begins_at),
                    take: Some(split.take),
                    manifest: Some(split.manifest.clone()),
                    folder: split.manifest.parent().map(Path::to_path_buf),
                    project: project.map(|p| p.id),
                    cause: Some(TakeCause::Split),
                    session: Some(token),
                    by: callers::by(),
                });
                Ok(split)
            }
            Ok(Err(why)) => Err(refused(
                match why.ending || why.busy {
                    true => RpcError::WRONG_STATE,
                    false => RpcError::FAILED,
                },
                &why.message,
            )),
            // Every split is answered, even once the recording has ended;
            // this is only isocastd's engine having gone.
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(refused(
                RpcError::WRONG_STATE,
                "the recording stopped before it could split",
            )),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(refused(
                RpcError::FAILED,
                "the recording did not split in time",
            )),
        }
    }

    fn refresh_browser(&self, source: String) -> Result<Refreshed, RpcError> {
        let state = self.state.read();
        let Some(control) = state.control() else {
            return Err(RpcError {
                code: RpcError::WRONG_STATE,
                message: "browser sources run while the live engine does (while recording, \
                          streaming or showing frames); it loads their pages fresh when it starts"
                    .into(),
            });
        };
        control
            .refresh_browser(&source)
            .map_err(|message| RpcError {
                code: RpcError::FAILED,
                message,
            })?;
        Ok(Refreshed { source })
    }

    fn marker(&self, label: String) -> Result<Marked, RpcError> {
        let state = self.state.read();
        let (Some(session), Some(control)) = (&state.session, state.control()) else {
            return Err(RpcError {
                code: RpcError::WRONG_STATE,
                message: "markers need a recording in progress".into(),
            });
        };
        let project = session.project.as_ref().map(|p| p.id.clone());
        let (take, time_seconds) = match control.marker(now(), label.clone()) {
            Ok(placed) => placed.unwrap_or((1, 0.0)),
            Err((take, time_seconds)) => {
                return Err(RpcError {
                    code: RpcError::WRONG_STATE,
                    message: format!(
                        "the recording ended at {time_seconds:.2} s (take {take}) and is \
                         finishing its files; markers need a recording in progress"
                    ),
                });
            }
        };
        drop(state);
        self.publish(Event::Marker {
            label: label.clone(),
            take,
            time_seconds,
            project,
        });
        Ok(Marked {
            label,
            take,
            time_seconds,
        })
    }

    fn switch_scene(
        &self,
        scene: String,
        transition: Option<Transition>,
        canvas: CanvasId,
    ) -> Result<Switched, RpcError> {
        let state = self.state.lock();
        check_scene(&state, &scene)?;
        if canvas.is_main() {
            self.go_on_air(state, scene, transition, false)
        } else {
            self.canvas_on_air(state, canvas, scene, transition, false)
        }
    }

    /// Put `scene` on air on the vertical canvas, which must not follow
    /// the main one; with `swap` (a take), the scene that was on air moves
    /// to its preview. Nothing changes if the engine refuses it.
    fn canvas_on_air(
        &self,
        mut state: Transaction<'_, State>,
        canvas: CanvasId,
        scene: String,
        transition: Option<Transition>,
        swap: bool,
    ) -> Result<Switched, RpcError> {
        let setup = independent_canvas(&state, canvas, "switch scenes on")?;
        // The switch's own transition, else the scene's, else the setup's.
        let transition = transition.unwrap_or_else(|| {
            setup
                .scenes
                .iter()
                .find(|s| s.id == scene)
                .and_then(|scene| scene.transition.clone())
                .unwrap_or_else(|| setup.transition.clone())
        });
        let issues = transition.issues("transition");
        if !issues.is_empty() {
            let issues: Vec<String> = issues
                .iter()
                .map(|issue| format!("{}: {}", issue.path, issue.message))
                .collect();
            return Err(failed(anyhow::anyhow!("{}", issues.join("; "))));
        }
        // What was on air, for a take's swap.
        let mut transition = transition;
        if let Some(control) = state.control() {
            control
                .prepare(&mut transition)
                .map_err(|err| failed(anyhow::anyhow!("{err}")))?;
        }
        let was = canvas_scene(&state, canvas);
        let placed = match &state.engine {
            Some(engine) => engine
                .switch_canvas_scene(canvas, now(), scene.clone(), transition.clone())
                .map_err(|err| failed(anyhow::anyhow!("{err}")))?,
            None => None,
        };
        state.canvas_scenes.insert(canvas, scene.clone());
        let swapped = swap && was.is_some();
        if swapped {
            match &was {
                Some(was) => state.canvas_previews.insert(canvas, was.clone()),
                None => state.canvas_previews.remove(&canvas),
            };
            if let Some(engine) = &state.engine
                && let Err(message) = engine.set_canvas_preview(canvas, was.clone())
            {
                isocast_diagnostics::diagnostic!("warn"; "isocastd: {canvas} preview: {message}");
            }
        }
        // Placed on the recording's timeline only while recording.
        let placed = state
            .session
            .as_ref()
            .map(|_| placed.unwrap_or((1, 0.0)))
            .filter(|_| state.engine.is_some());
        let preview = state.canvas_previews.get(&canvas).cloned();
        drop(state);
        let (take, time_seconds) = placed.unzip();
        let transition = (!transition.is_cut()).then_some(transition);
        self.publish(Event::CanvasSceneChanged {
            canvas,
            scene: scene.clone(),
            take,
            time_seconds,
            transition: transition.clone(),
        });
        if swapped {
            self.publish(Event::CanvasPreviewChanged {
                canvas,
                scene: preview.clone(),
            });
        }
        Ok(Switched {
            scene,
            preview,
            take,
            time_seconds,
            transition,
            canvas,
        })
    }

    /// Stage `scene` in `canvas`'s preview, for `take`.
    pub(crate) fn preview(&self, scene: String, canvas: CanvasId) -> Result<Switched, RpcError> {
        let mut state = self.state.lock();
        check_scene(&state, &scene)?;
        if !canvas.is_main() {
            independent_canvas(&state, canvas, "stage a preview on")?;
            if let Some(engine) = &state.engine {
                engine
                    .set_canvas_preview(canvas, Some(scene.clone()))
                    .map_err(|err| failed(anyhow::anyhow!("{err}")))?;
            }
            state.canvas_previews.insert(canvas, scene.clone());
            let on_air = canvas_scene(&state, canvas).unwrap_or_default();
            drop(state);
            self.publish(Event::CanvasPreviewChanged {
                canvas,
                scene: Some(scene.clone()),
            });
            return Ok(Switched {
                scene: on_air,
                preview: Some(scene),
                take: None,
                time_seconds: None,
                transition: None,
                canvas,
            });
        }
        state.preview = Some(scene.clone());
        if let Some(engine) = &state.engine {
            engine.set_preview(Some(scene.clone()));
        }
        let on_air = state.scene.clone().unwrap_or_default();
        drop(state);
        self.publish(Event::PreviewChanged {
            scene: scene.clone(),
        });
        Ok(Switched {
            scene: on_air,
            preview: Some(scene),
            take: None,
            time_seconds: None,
            transition: None,
            canvas: CanvasId::Main,
        })
    }

    fn clear_preview(&self) -> Switched {
        let mut state = self.state.lock();
        state.preview = None;
        if let Some(engine) = &state.engine {
            engine.set_preview(None);
        }
        let scene = state.scene.clone().unwrap_or_default();
        drop(state);
        self.publish(Event::PreviewCleared);
        Switched {
            scene,
            preview: None,
            take: None,
            time_seconds: None,
            transition: None,
            canvas: CanvasId::Main,
        }
    }

    /// Clear `canvas`'s preview (`clear_preview` for the main one).
    fn clear_canvas_preview(&self, canvas: CanvasId) -> Result<Switched, RpcError> {
        if canvas.is_main() {
            return Ok(self.clear_preview());
        }
        let mut state = self.state.lock();
        independent_canvas(&state, canvas, "clear the preview of")?;
        if let Some(engine) = &state.engine {
            engine
                .set_canvas_preview(canvas, None)
                .map_err(|err| failed(anyhow::anyhow!("{err}")))?;
        }
        state.canvas_previews.remove(&canvas);
        let scene = canvas_scene(&state, canvas).unwrap_or_default();
        drop(state);
        self.publish(Event::CanvasPreviewChanged {
            canvas,
            scene: None,
        });
        Ok(Switched {
            scene,
            preview: None,
            take: None,
            time_seconds: None,
            transition: None,
            canvas,
        })
    }

    /// Put `canvas`'s preview scene on air; the scene that was on air moves
    /// to preview.
    fn take(&self, transition: Option<Transition>, canvas: CanvasId) -> Result<Switched, RpcError> {
        let state = self.state.lock();
        if !canvas.is_main() {
            independent_canvas(&state, canvas, "take a preview on")?;
            let Some(scene) = chosen_canvas_preview(&state, canvas) else {
                return Err(RpcError {
                    code: RpcError::WRONG_STATE,
                    message: format!(
                        "nothing in the {canvas} canvas's preview; stage a scene first \
                         (`isocast preview SCENE --canvas {canvas}`, or preview with canvas \
                         \"{canvas}\")"
                    ),
                });
            };
            return self.canvas_on_air(state, canvas, scene, transition, true);
        }
        let Some(scene) = state.preview.clone() else {
            return Err(RpcError {
                code: RpcError::WRONG_STATE,
                message: "nothing in preview; stage a scene first with `preview`".into(),
            });
        };
        self.go_on_air(state, scene, transition, true)
    }

    /// Put `scene` on air; with `swap` (a take), the scene that was on air
    /// moves to preview. Nothing changes if the transition is refused.
    fn go_on_air(
        &self,
        mut state: Transaction<'_, State>,
        scene: String,
        transition: Option<Transition>,
        swap: bool,
    ) -> Result<Switched, RpcError> {
        // The switch's own transition, else the scene's, else the setup's.
        let setup = state.setup.as_ref();
        let transition = transition.unwrap_or_else(|| {
            setup.map_or_else(Transition::default, |setup| {
                let scene = setup.scenes.iter().find(|s| s.id == scene);
                scene
                    .and_then(|scene| scene.transition.clone())
                    .unwrap_or_else(|| setup.transition.clone())
            })
        });
        let issues = transition.issues("transition");
        if !issues.is_empty() {
            let issues: Vec<String> = issues
                .iter()
                .map(|issue| format!("{}: {}", issue.path, issue.message))
                .collect();
            return Err(failed(anyhow::anyhow!("{}", issues.join("; "))));
        }
        let mut transition = transition;
        let control = state.control();
        if let Some(control) = &control {
            control
                .prepare(&mut transition)
                .map_err(|err| failed(anyhow::anyhow!("{err}")))?;
        }
        if swap {
            state.preview = state.scene.clone();
            if let Some(engine) = &state.engine {
                engine.set_preview(state.preview.clone());
            }
        }
        state.scene = Some(scene.clone());
        let placed = control.and_then(|control| {
            let placed = control.switch_scene(now(), scene.clone(), transition.clone());
            // Placed on the recording's timeline only while recording, and
            // not once it has ended (while its files finish).
            let placed = placed.ok()?.unwrap_or((1, 0.0));
            state.session.as_ref().map(|_| placed)
        });
        let preview = state.preview.clone();
        drop(state);
        let (take, time_seconds) = placed.unzip();
        let transition = (!transition.is_cut()).then_some(transition);
        self.publish(Event::SceneChanged {
            scene: scene.clone(),
            take,
            time_seconds,
            transition: transition.clone(),
        });
        if swap && let Some(preview) = &preview {
            self.publish(Event::PreviewChanged {
                scene: preview.clone(),
            });
        }
        Ok(Switched {
            scene,
            preview,
            take,
            time_seconds,
            transition,
            canvas: CanvasId::Main,
        })
    }

    fn status(&self) -> Status {
        let state = self.state.read();
        let control = state.control();
        let metadata_pending = control
            .as_ref()
            .map(|control| control.metadata_pending())
            .unwrap_or_default();
        let finalizing = state
            .session
            .as_ref()
            .is_some_and(|session| session.stop.load(Ordering::SeqCst))
            || control
                .as_ref()
                .is_some_and(|control| control.metadata_finalizing());
        let (engine, mut live, files) = match &state.session {
            None => (EngineState::Idle, None, Vec::new()),
            Some(session) => match session.files.lock().clone() {
                None => (EngineState::Starting, None, Vec::new()),
                Some(files) => (EngineState::Recording, session.live.lock().clone(), files),
            },
        };
        if let (Some(live), Some(control)) = (&mut live, state.control())
            && let Some(recording) = control.pause_status(now())
        {
            live.paused = recording.paused;
            live.elapsed_seconds = recording.elapsed_seconds;
            live.paused_seconds = recording.paused_seconds;
            live.handover = control.handover(now());
        }
        let info = engine_info(&state);
        let (collection, profile) = state.file.current_names(state.setup.as_ref());
        // With no setup yet, the folder a new one records to (`setup
        // init`'s), so a client can say how much room there is from the
        // start.
        let folder = Some(isocast_engine::record::expand_home(
            state.setup.as_ref().map_or(init::DEFAULT_FOLDER, |setup| {
                setup.recording.folder.as_str()
            }),
        ));
        // Each read under one lock, taken and let go here: a guard kept to
        // the end of the `Status` below would deadlock the next.
        let mut status = Status {
            state: engine,
            finalizing,
            governor: Some(isocast_engine::governor::status()),
            machine_profile: Some(self.machine_status(false)),
            #[cfg(target_os = "macos")]
            load_warning: isocast_platform_macos::pressure::warning(),
            #[cfg(not(target_os = "macos"))]
            load_warning: None,
            scene: state.scene.clone(),
            preview: state.preview.clone(),
            session: state.session.as_ref().map(|session| session.token.clone()),
            live,
            files,
            last: state.last.clone(),
            last_error: state.last_error.clone(),
            engine: info,
            setup_file: Some(state.file.info()),
            collection,
            profile,
            stream: stream_status(&state),
            streams: streams::stream_statuses(&state),
            vertical_scene: canvas_scene(&state, CanvasId::Vertical),
            vertical_preview: canvas_preview(&state, CanvasId::Vertical),
            vertical_streams: streams::canvas_statuses(&state, CanvasId::Vertical),
            cpu_percent: self.cpu.lock().read(),
            data: Some(self.data.status()),
            diagnostics: Some({
                let mut health = isocast_diagnostics::health();
                health["frame_budget"] = native().frame_budget().unwrap_or(serde_json::Value::Null);
                health["metadata_pending"] = serde_json::json!(metadata_pending);
                health
            }),
            interrupted: self.interrupted.lock().clone(),
            virtual_camera: Some(state.camera.status_for(state.setup.as_ref())),
            replay: Some(state.replay.status_for(state.setup.as_ref())),
            recording_free_bytes: None,
            program_bytes_per_second: state
                .setup
                .as_ref()
                .map(|setup| setup.recording.program.bytes_per_second(&setup.canvas))
                .filter(|rate| *rate > 0.0),
            daemon: Some(daemon_build()),
            vertical_replay: state.replay.vertical_status(state.setup.as_ref()),
            canvases: canvas_statuses(&state),
        };
        // Measured without the lock (a slow disk can't hold up the rest).
        drop(state);
        if let Some(camera) = &mut status.virtual_camera {
            virtual_camera::availability(camera);
        }
        status.recording_free_bytes = folder.as_deref().and_then(free_bytes);
        status
    }

    fn stream_key_set(&self, params: StreamKeyParams) -> Result<StreamKey, RpcError> {
        let key = params.key.trim();
        if key.is_empty() {
            return Err(failed(anyhow::anyhow!(
                "the stream key is empty; paste the key from the service's dashboard"
            )));
        }
        if key.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(failed(anyhow::anyhow!(
                "the stream key has spaces or line breaks in it; give the key alone (the server \
                 goes in the setup's stream.server)"
            )));
        }
        // Resolved and written under the state lock, which every
        // destination change (and its key clearing) also holds: the key goes
        // to the account of the destination as it is when it's stored, and a
        // network caller's concurrent move can't slip between (decision 26).
        let state = self.state.lock();
        if let Some(revision) = params.target.revision
            && revision != state.revision
        {
            return Err(RpcError {
                code: RpcError::WRONG_STATE,
                message: format!(
                    "revision: the live setup is at revision {}, not {revision} (a destination \
                     may have changed); look at it again before setting the key",
                    state.revision
                ),
            });
        }
        let live = streams::canvas_stream_settings(&state, params.target.canvas);
        let (service, account) = self.key_target_with(live, params.target)?;
        set_secret(key_service(), &account, key).map_err(failed)?;
        drop(state);
        retire_legacy_key(&account);
        forget_key_problem();
        isocast_diagnostics::redact::remember_secret(key_service(), &account, key);
        Ok(StreamKey {
            service,
            account,
            key_set: true,
        })
    }

    fn stream_key_clear(&self, target: StreamKeyTarget) -> Result<StreamKey, RpcError> {
        let (service, account) = self.key_target(target)?;
        delete_secret(key_service(), &account).map_err(failed)?;
        retire_legacy_key(&account);
        forget_key_problem();
        Ok(StreamKey {
            service,
            account,
            key_set: false,
        })
    }

    /// The service and Keychain account a key command means: as given, or
    /// the live setup's `[stream]`.
    fn key_target(&self, target: StreamKeyTarget) -> Result<(StreamService, String), RpcError> {
        let live = streams::canvas_stream_settings(&self.state.read(), target.canvas);
        self.key_target_with(live, target)
    }

    /// [`Self::key_target`] against `live`, the canvas's stream settings.
    fn key_target_with(
        &self,
        live: Option<StreamSettings>,
        target: StreamKeyTarget,
    ) -> Result<(StreamService, String), RpcError> {
        // Each canvas keeps its own keys (`canvas_key_account`).
        let canvas = target.canvas;
        if let Some(id) = &target.destination {
            // A destination not applied yet (Settings' draft) names its
            // service (and custom server) itself.
            if let Some(service) = target.service {
                if id != MAIN_DESTINATION
                    && (id.is_empty()
                        || !id
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
                {
                    return Err(failed(anyhow::anyhow!(
                        "destination: {id:?} must use lowercase letters, digits and dashes"
                    )));
                }
                if service == StreamService::Custom {
                    let Some(server) = &target.server else {
                        return Err(failed(anyhow::anyhow!(
                            "a custom service's key belongs to its server; give the server \
                             (like rtmp://host/app)"
                        )));
                    };
                    isocast_protocol::stream::ServerUrl::parse(server)
                        .map_err(|err| failed(anyhow::anyhow!("server: {err}")))?;
                }
                let server = target
                    .server
                    .as_deref()
                    .filter(|_| service == StreamService::Custom);
                return Ok((service, canvas_key_account(canvas, id, service, server)));
            }
            if target.server.is_some() {
                return Err(failed(anyhow::anyhow!(
                    "server: give it with the destination's service, or leave both out"
                )));
            }
            let Some(settings) = &live else {
                return Err(failed(anyhow::anyhow!(
                    "destination: the live setup has no {}stream destinations",
                    if canvas.is_main() {
                        String::new()
                    } else {
                        format!("{canvas} ")
                    }
                )));
            };
            let Some(destination) = settings
                .resolve_canvas(canvas)
                .into_iter()
                .find(|d| d.id == *id)
            else {
                return Err(failed(anyhow::anyhow!(
                    "destination: no destination {id:?}; destinations: {}",
                    settings.destination_ids().join(", ")
                )));
            };
            return Ok((destination.settings.service, destination.key_account));
        }
        let service = match (target.service, &live) {
            (Some(service), _) => service,
            (None, Some(settings)) => settings.service,
            (None, None) => {
                let services: Vec<&str> = STREAM_SERVICES.iter().map(|s| s.id()).collect();
                return Err(failed(anyhow::anyhow!(
                    "which service is the key for? Give one ({}), or add a [stream] section to \
                     the live setup",
                    services.join(", ")
                )));
            }
        };
        // Only a custom server keeps a key per server; a preset has one.
        if service != StreamService::Custom && target.server.is_some() {
            return Err(failed(anyhow::anyhow!(
                "server: only a custom service's key belongs to a server; {0} has one key \
                 (Keychain account \"{0}\") whatever its server, so leave the server out (or \
                 give the service custom)",
                service.id()
            )));
        }
        let server = target.server.or_else(|| {
            live.filter(|settings| settings.service == service)
                .and_then(|settings| settings.server)
        });
        if service == StreamService::Custom {
            let Some(server) = &server else {
                return Err(failed(anyhow::anyhow!(
                    "a custom service's key belongs to its server; give the server (like \
                     rtmp://host/app) or set stream.server in the live setup"
                )));
            };
            isocast_protocol::stream::ServerUrl::parse(server)
                .map_err(|err| failed(anyhow::anyhow!("server: {err}")))?;
        }
        Ok((
            service,
            canvas_key_account(canvas, MAIN_DESTINATION, service, server.as_deref()),
        ))
    }

    /// Start recording.
    fn start(self: &Arc<Self>, params: StartParams) -> Result<Started, RpcError> {
        // As asked, for a repeated `start` (decision 17), and who asked.
        let asked = params.clone();
        let by = callers::by();
        // One start at a time from here until its session is reserved: a
        // second `start` (maybe with another setup) waits, then finds this
        // session (decision 17; review M2).
        let admission = self.start_admission.lock();
        check_folder(params.folder.as_deref())?;
        if let Some(folder) = &params.folder {
            plugin_folder_refusal(
                "params.folder",
                &projects::root_of(folder),
                &self.recording_roots(),
            )?;
        }
        let duration = match params.seconds {
            None => None,
            Some(s) if s > 0.0 && s <= 30.0 * 24.0 * 3600.0 => Some(Duration::from_secs_f64(s)),
            Some(s) => {
                return Err(failed(anyhow::anyhow!(
                    "seconds must be more than 0 and at most 30 days, not {s}"
                )));
            }
        };
        {
            let state = self.state.read();
            if params
                .setup
                .as_ref()
                .or(state.setup.as_ref())
                .is_some_and(|s| s.records_a_program())
                && let Some(reason) = state.engine.as_ref().and_then(|e| e.program_failure())
            {
                return Err(RpcError {
                    code: RpcError::WRONG_STATE,
                    message: reason,
                });
            }
            if let Some(session) = &state.session {
                return already_recording(session, &asked);
            }
        }
        // A setup given with the request becomes the live setup; say so if
        // it replaces another, since that one is only kept in the .bak.
        let mut notes = Vec::new();
        if let Some(setup) = params.setup {
            let (revision, replaced) = {
                let state = self.state.read();
                let replaced = state.setup.as_ref().filter(|live| **live != setup);
                (state.revision, replaced.map(|live| live.name.clone()))
            };
            let applied = self.apply(ApplyParams {
                revision,
                change: Change::Setup(Box::new(setup)),
                live: false,
            })?;
            notes.extend(applied.notes);
            if let Some(name) = replaced {
                let file = self.state.lock().file.info();
                notes.push(format!(
                    "the setup given became the live setup, replacing {name:?} (revision \
                     {revision}) in the current scene collection and profile; the previous \
                     versions are kept in {}.bak{} until the next change",
                    file.path.display(),
                    file.profile_path
                        .map(|path| format!(" and {}.bak", path.display()))
                        .unwrap_or_default()
                ));
            }
        }
        let folder = {
            let state = self.state.read();
            let Some(setup) = &state.setup else {
                return Err(state.no_setup("no setup to record (or give one)"));
            };
            params
                .folder
                .clone()
                .unwrap_or_else(|| expand_home(&setup.recording.folder))
        };
        // For a network caller, where it records is inside the folder roots
        // however it was chosen: given or the setup's (decision 26), and
        // used as checked.
        let folder =
            self.checked_path("folder", &folder.to_string_lossy(), roots::Access::Write)?;
        // It records into a project folder in the recordings folder
        // (`folder` itself, if it is a project's).
        let root = projects::root_of(&folder);
        // Checked again as it is remembered (the folder's project file is
        // read again): a plugin's folder must be a recordings folder (#103).
        if params.folder.is_some() {
            plugin_folder_refusal("params.folder", &root, &self.recording_roots())?;
        }
        // Recordings there that never finished (isocastd was killed or
        // crashed): the engine marks them as failed; say so now.
        self.remember_recording_folder(&root);
        // Startup recovery may already have marked them (status.interrupted);
        // the first start into their folder still says so, once.
        let mut interrupted: Vec<PathBuf> =
            isocast_protocol::project::recording_folders(&root, START_SCAN_PROJECTS)
                .iter()
                .flat_map(|folder| interrupted_sessions(folder, true))
                .collect();
        for take in self.interrupted_unnoted.lock().extract_if(.., |m| {
            m.parent()
                .is_some_and(|p| p == root || p.parent() == Some(&root))
        }) {
            if !interrupted.contains(&take) {
                interrupted.push(take);
            }
        }
        notes.extend(interrupted_note(&interrupted, true));
        // The session is in the state before the engine warms up, so
        // `status` says it is starting and `stop` can cancel it.
        let mut state = self.state.lock();
        if let Some(session) = &state.session {
            return already_recording(session, &asked);
        }
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(RpcError {
                code: RpcError::WRONG_STATE,
                message: "the daemon is shutting down; recording cannot start".into(),
            });
        }
        if state.tuning {
            return Err(RpcError {
                code: RpcError::WRONG_STATE,
                message: "`tune` is benchmarking the encoders; start once it has finished".into(),
            });
        }
        state.next_session += 1;
        let token = callers::session_token(state.next_session);
        let session = Session {
            id: state.next_session,
            token: token.clone(),
            asked,
            stopped_by: Arc::new(Mutex::new(None)),
            done: Arc::default(),
            stop: Arc::new(AtomicBool::new(false)),
            files: Arc::new(Mutex::new(None)),
            live: Arc::new(Mutex::new(None)),
            take: Arc::new(AtomicU32::new(0)),
            project: None,
        };
        let id = session.id;
        let stopped_by = session.stopped_by.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let options = (
            session.stop.clone(),
            session.files.clone(),
            session.live.clone(),
            session.take.clone(),
        );
        state.session = Some(session);
        drop(state);
        drop(admission);

        // Its project folder, picked holding the projects lock so a rename
        // can't move it meanwhile, and kept with the session (renames are
        // refused while it records there).
        let picked = {
            let _projects = self.projects.lock();
            let picked = projects::project_for(&folder);
            if let Ok(project) = &picked {
                let mut state = self.state.lock();
                if let Some(session) = state.session.as_mut().filter(|s| s.id == id) {
                    session.project = Some(project.clone());
                }
            }
            picked
        };
        let project = match picked {
            Ok(project) => project,
            Err(err) => {
                self.ended(
                    id,
                    Err(err.message.clone()),
                    Event::Stopped {
                        report: None,
                        error: Some(err.message.clone()),
                        project: None,
                        folder: None,
                        session: Some(token.clone()),
                        by: stopped_by.lock().clone().flatten(),
                    },
                );
                return Err(err);
            }
        };
        // A plugin's sandbox lists the recordings folders as they were when
        // it started: one used for the first time (made just now, maybe),
        // or a folder named like a project made since, restarts it with
        // them (#56).
        self.plugins.refresh_sandboxes();
        // A folder made for a recording that wrote nothing goes again.
        let discard = |project: &projects::ProjectRef| {
            if project.created {
                isocast_protocol::project::discard_if_empty(&project.folder);
            }
        };

        let engine = match self.ensure_engine() {
            Ok(_) if options.0.load(Ordering::SeqCst) => Err(RpcError {
                code: RpcError::FAILED,
                message: STOPPED_BEFORE_START.into(),
            }),
            engine => engine,
        };
        let engine = match engine {
            Ok(engine) => engine,
            Err(err) => {
                discard(&project);
                self.ended(
                    id,
                    Err(err.message.clone()),
                    Event::Stopped {
                        report: None,
                        error: Some(err.message.clone()),
                        project: None,
                        folder: None,
                        session: Some(token.clone()),
                        by: stopped_by.lock().clone().flatten(),
                    },
                );
                return Err(err);
            }
        };

        // On a software graphics adapter the warning is said before the take
        // begins (#128), with the adapter classified now if no preview did.
        // Hardware estimates still wait for the sources' recorded formats.
        let early = software_adapter();
        if early && let Some(message) = self.capacity_warning(&*engine) {
            self.publish(Event::Warning {
                message: message.clone(),
            });
            notes.push(message);
        }

        let daemon = self.clone();
        let recording = engine.clone();
        let session = token.clone();
        thread::spawn(move || {
            let (stop, files, live, take) = options;
            let on_start = |paths: &[PathBuf], manifest: &Path, opened: &[String]| {
                take.store(1, Ordering::SeqCst);
                *files.lock() = Some(paths.to_vec());
                let _ = started_tx.send(Ok((paths.to_vec(), opened.to_vec())));
                if project.created
                    && let Some(file) = isocast_protocol::project::read(&project.folder)
                {
                    daemon.publish(Event::ProjectCreated {
                        project: isocast_protocol::project::describe(&project.folder, &file, true),
                    });
                }
                daemon.publish(Event::Started {
                    files: paths.to_vec(),
                    begins_at: None,
                    take: Some(1),
                    manifest: Some(manifest.to_path_buf()),
                    folder: Some(project.folder.clone()),
                    project: Some(project.id.clone()),
                    cause: Some(TakeCause::Start),
                    session: Some(token.clone()),
                    by: by.clone(),
                });
            };
            let reported = Mutex::new(Vec::<(u64, u64)>::new());
            let overload = Mutex::new(OverloadWatch::default());
            let on_status = |status: &LiveStatus| {
                *live.lock() = Some(status.clone());
                for message in status.new_drops(&mut reported.lock()) {
                    daemon.publish(Event::Warning { message });
                }
                daemon.note_overload(&overload, take.load(Ordering::SeqCst), status);
                daemon.publish(Event::Status {
                    live: status.clone(),
                });
            };
            let on_take = |number: u32, path: &Path, manifest: &SessionManifest| {
                let folder = path.parent().unwrap_or(Path::new("")).to_path_buf();
                daemon.publish(Event::TakeFinished {
                    take: number,
                    manifest: path.to_path_buf(),
                    files: manifest
                        .files
                        .iter()
                        .map(|file| folder.join(&file.path))
                        .collect(),
                    folder,
                    project: Some(project.id.clone()),
                    status: manifest.status,
                    duration_seconds: manifest.duration_seconds,
                    errors: manifest.errors.clone(),
                });
            };
            let options = RecordOptions {
                folder: Some(project.folder.clone()),
                duration,
                stop,
                on_start: Some(&on_start),
                on_status: Some(&on_status),
                on_take: Some(&on_take),
                control: None,
            };
            let ended =
                isocast_diagnostics::contain("recording coordinator", || engine.record(&options))
                    .and_then(|r| r.map_err(|err| format!("{err:#}")));
            if ended.is_err() {
                discard(&project);
            }
            let stopped = Event::Stopped {
                report: ended.clone().ok().map(Box::new),
                error: ended.clone().err(),
                // Unless the folder went again (it held nothing).
                project: project.folder.exists().then(|| project.id.clone()),
                folder: project.folder.exists().then(|| project.folder.clone()),
                session: Some(token.clone()),
                by: stopped_by.lock().clone().flatten(),
            };
            let error = ended.as_ref().err().cloned();
            daemon.ended(id, ended, stopped);
            // A refused Start is final: a client may retry as soon as it
            // receives the error, so release the session before answering.
            if let Some(message) = error {
                let _ = started_tx.send(Err(message));
            }
        });

        match started_rx.recv_timeout(START_TIMEOUT) {
            Ok(Ok((files, opened))) => {
                // How the files' encoders were admitted (the platform's plan).
                notes.extend(opened);
                // Said up front, where OBS says "encoding overloaded" mid-take.
                if !early && let Some(message) = self.capacity_warning(&*recording) {
                    self.publish(Event::Warning {
                        message: message.clone(),
                    });
                    notes.push(message);
                }
                Ok(Started {
                    files,
                    begins_at: None,
                    notes,
                    session: Some(session),
                    already_recording: false,
                })
            }
            Ok(Err(message)) => Err(RpcError {
                code: RpcError::FAILED,
                message,
            }),
            // It may still start: say so rather than calling it a failure.
            Err(_) => Err(RpcError {
                code: RpcError::FAILED,
                message: format!(
                    "the recording is still starting after {} s (a device or the recording \
                     disk is slow to respond); `status` shows when it starts, and `stop` \
                     cancels it",
                    START_TIMEOUT.as_secs()
                ),
            }),
        }
    }

    /// A warning if the recording that just started asks more of the
    /// hardware encoders than `tune` measured they manage on this Mac, or,
    /// with nothing measured, than the chip's numbers say they manage.
    fn capacity_warning(&self, engine: &dyn LiveEngine) -> Option<String> {
        let (setup, memory) = {
            let state = self.state.read();
            let memory = capacity::memory_path(state.file.legacy());
            (state.setup.clone()?, memory)
        };
        // A software adapter's warning skips `tune`'s hardware
        // measurements, which don't describe CPU compositing (#128).
        let software = software_adapter();
        if !software && memory.exists() {
            // The ISO sources are running by now, at the formats they record.
            let info = engine.info();
            let measured = capacity::workload(&setup, |id| {
                let source = info.sources.iter().find(|s| s.source == id)?;
                Some((source.width?, source.height?, source.fps?))
            })
            .and_then(|workload| capacity::start_warning(&memory, machine_key()?, &workload));
            if measured.is_some() {
                return measured;
            }
        }
        // Else what the chip's numbers predict, with the stream if it runs
        // (#66): only when it can't keep up, so a fitting setup never nags.
        let (streaming, backends) = {
            let state = self.state.lock();
            (
                [state.streaming_on(CanvasId::Main), state.streaming_extra()],
                prores_backends(&state),
            )
        };
        let estimate = estimate_capacity(
            &setup,
            Some(engine),
            streaming,
            true,
            &memory,
            None,
            &backends,
        )
        .ok()?;
        if estimate.capacity != isocast_protocol::api::Capacity::CannotKeepUp && !software {
            return None;
        }
        // Within the reference nothing changes from take to take: say it
        // once a run, and where to read it again.
        let within = software && isocast_engine::estimate::software_within_reference(&estimate);
        if within && self.software_reference_warned.swap(true, Ordering::SeqCst) {
            return None;
        }
        let mut text = estimate.summary;
        if within {
            text.push_str("; said once while isocastd runs, `isocast capacity` shows it again");
        }
        if let Some(fix) = estimate.suggestions.first() {
            text.push_str(&format!(
                "; {}, {}",
                if software { "suggestion" } else { "to fit" },
                fix.description
            ));
            if let Some(change) = &fix.change {
                text.push_str(&format!(
                    " (isocast plan --patch '{change}', after this take)"
                ));
            }
        }
        Some(text)
    }

    /// `estimate_capacity`: whether a setup's files (the live setup's, at
    /// its sources' live formats) fit this Mac's encoders, by per-chip data.
    fn estimate_capacity(&self, params: CapacityParams) -> Result<CapacityEstimate, RpcError> {
        let (setup, streaming, recording, engine, memory, backends) = {
            let state = self.state.read();
            let live = params.setup.is_none();
            let Some(setup) = params.setup.or_else(|| state.setup.clone()) else {
                return Err(state.no_setup("no setup to judge (or give one)"));
            };
            // [main, every other canvas] (#84).
            let on = |canvas: CanvasId| {
                if live {
                    state.streaming_on(canvas)
                } else {
                    setup.stream_of(canvas).is_some()
                }
            };
            let streaming = [
                params.stream.unwrap_or_else(|| on(CanvasId::Main)),
                params
                    .stream
                    .unwrap_or_else(|| setup.extra_canvas_ids().into_iter().any(on)),
            ];
            let recording = live && state.session.is_some();
            let engine = state.engine.clone().filter(|_| live);
            let memory = capacity::memory_path(state.file.legacy());
            let backends = if live {
                prores_backends(&state)
            } else {
                Vec::new()
            };
            (setup, streaming, recording, engine, memory, backends)
        };
        estimate_capacity(
            &setup,
            engine.as_deref(),
            streaming,
            recording,
            &memory,
            params.add_iso.as_deref(),
            &backends,
        )
        .map_err(|message| RpcError {
            code: RpcError::INVALID_PARAMS,
            message,
        })
    }

    /// Once per take, when the take's files drop frames and the disk isn't
    /// behind: the encoders are at capacity. Says which files drop, why,
    /// and the live setup's fixes (#66).
    fn note_overload(
        self: &Arc<Self>,
        watch: &Mutex<OverloadWatch>,
        take: u32,
        status: &LiveStatus,
    ) {
        let mut watch = watch.lock();
        if watch.take != take {
            *watch = OverloadWatch {
                take,
                baseline: status
                    .files
                    .iter()
                    .map(|f| (f.source.clone(), f.frames, f.dropped_frames))
                    .collect(),
                stream_baseline: None,
                stream_now: watch.stream_now,
                sent: false,
            };
        }
        // The recording's own thread: never wait for the state lock (a
        // busy request keeps the last reading instead).
        let (stream_dropped, stream_sent) = match self.state.try_lock() {
            Some(state) => {
                // The running stream's own counts (no Keychain look-up
                // for the setup's key, as `stream_status` makes).
                // Every destination's, added up (#73).
                let now = state
                    .streams
                    .iter()
                    .map(|(_, output)| output.status())
                    .filter(|s| s.is_active())
                    .fold((0, 0), |(d, f), s| {
                        (d + s.dropped_frames, f + s.frames_sent)
                    });
                watch.stream_now = now;
                now
            }
            None => watch.stream_now,
        };
        let stream_baseline = *watch
            .stream_baseline
            .get_or_insert((stream_dropped, stream_sent));
        if watch.sent || status.paused {
            return;
        }
        // A slow disk drops frames too, and has its own warning.
        if status.disk.as_ref().is_some_and(|d| d.behind()) {
            return;
        }
        let mut dropping: Vec<OverloadedFile> = Vec::new();
        for file in &status.files {
            let (frames0, dropped0) = watch
                .baseline
                .iter()
                .find(|(s, ..)| *s == file.source)
                .map_or((0, 0), |(_, f, d)| (*f, *d));
            let dropped = file.dropped_frames.saturating_sub(dropped0);
            let due = file.frames.saturating_sub(frames0) + dropped;
            if overloaded(dropped, due) {
                dropping.push(OverloadedFile {
                    output: file.source.clone(),
                    dropped_frames: dropped,
                    percent: dropped as f64 * 100.0 / due.max(1) as f64,
                });
            }
        }
        // Only the take's files say the encoders are behind: the stream's
        // count also holds frames a congested or reconnecting connection
        // dropped, which have their own warnings. With a file dropping,
        // the stream's drops are a detail.
        if dropping.is_empty() {
            return;
        }
        let stream_dropped = stream_dropped.saturating_sub(stream_baseline.0);
        let stream_due = stream_sent.saturating_sub(stream_baseline.1) + stream_dropped;
        if overloaded(stream_dropped, stream_due) {
            dropping.push(OverloadedFile {
                output: "stream".into(),
                dropped_frames: stream_dropped,
                percent: stream_dropped as f64 * 100.0 / stream_due.max(1) as f64,
            });
        }
        watch.sent = true;
        drop(watch);
        // Off the recording's thread: probing sources can take a moment.
        let daemon = self.clone();
        let elapsed_seconds = status.elapsed_seconds;
        thread::spawn(move || daemon.publish_overload(take, elapsed_seconds, dropping));
    }

    fn publish_overload(&self, take: u32, elapsed_seconds: f64, mut dropping: Vec<OverloadedFile>) {
        dropping.sort_by(|a, b| b.percent.total_cmp(&a.percent));
        // Every canvas's program file (`program`, `vertical-program`,
        // `<id>-program`).
        let programs: Vec<String> = self
            .state
            .read()
            .setup
            .as_ref()
            .map(|setup| setup.canvas_ids())
            .unwrap_or_else(|| vec![CanvasId::Main])
            .into_iter()
            .map(CanvasId::program)
            .collect();
        let protected_dropping = dropping.iter().any(|f| programs.contains(&f.output));
        let estimate = self.estimate_capacity(CapacityParams::default()).ok();
        let names: Vec<String> = dropping
            .iter()
            .map(|f| format!("{} ({:.0}%)", f.output, f.percent))
            .collect();
        let verb = if names.len() == 1 { "is" } else { "are" };
        let at_capacity = estimate
            .as_ref()
            .is_none_or(|e| e.capacity != isocast_protocol::api::Capacity::Fits);
        let software = estimate.as_ref().is_some_and(|e| {
            e.resources
                .iter()
                .any(|r| r.resource == isocast_protocol::api::SOFTWARE_RENDERER_RESOURCE)
        });
        let mut reason = if software {
            format!(
                "software graphics and encoding share the CPU; {} {verb} dropping frames, and this machine's sustainable capacity is unmeasured",
                names.join(", ")
            )
        } else if at_capacity {
            format!(
                "this Mac's video encoders are at capacity, so {} {verb} dropping frames",
                names.join(", ")
            )
        } else {
            // By the chip's numbers these files fit: something else is
            // using the encoders, or they fell behind for a while.
            format!(
                "the video encoders fell behind, so {} {verb} dropping frames, though this \
                 Mac's encoders should keep up with these files (another app using them, or a \
                 busy Mac, can do this)",
                names.join(", ")
            )
        };
        if !software {
            reason.push_str("; ISO encodes yield first, then other canvases' outputs to protect main; main can still lose frames if its own workload is too high");
        }
        if let Some(fix) = estimate.as_ref().and_then(|e| e.suggestions.first()) {
            reason.push_str(&format!(
                "; {}, {}",
                if software { "suggestion" } else { "to fit" },
                fix.description
            ));
            if fix.after_take {
                reason.push_str(" (applies once this take ends)");
            }
        }
        let workload_key = estimate
            .as_ref()
            .map(|e| e.workload_key.clone())
            .unwrap_or_default();
        self.publish(Event::EncoderOverload {
            overload: Box::new(Overload {
                take,
                elapsed_seconds,
                dropping,
                protected_dropping,
                reason,
                estimate,
                workload_key,
            }),
        });
    }

    /// The live engine, started with the live setup if it isn't running.
    /// Starting it (devices warming up) happens outside the state lock, so
    /// other requests carry on; callers asking meanwhile wait for it, and
    /// changes made meanwhile apply once it runs.
    fn ensure_engine(self: &Arc<Self>) -> Result<Arc<dyn LiveEngine>, RpcError> {
        // A new preview must win over an automatic calibration too. Manual
        // calibration remains available from Settings while previewing.
        let _gate = self.starting.read_recursive();
        let mut state = self.state.lock();
        loop {
            if let Some(engine) = &state.engine {
                return Ok(engine.clone());
            }
            if !state.starting {
                break;
            }
            state.wait(&self.engine_ready);
        }
        let Some(setup) = state
            .setup
            .as_ref()
            .map(|s| state.with_hotkey_holds(s).into_owned())
        else {
            return Err(state.no_setup("nothing for the live engine to run"));
        };
        // The program starts on the scene on air, or the setup's first.
        let scenes = state.scene_ids();
        if !scenes.contains(&state.scene.clone().unwrap_or_default()) {
            state.scene = scenes.first().cloned();
        }
        let scene = state.scene.clone();
        let control = Arc::new(SessionControl::with_data(self.data.store.clone()));
        if let Some(scene) = &scene {
            let _ = control.switch_scene(now(), scene.clone(), Transition::default());
        }
        let daemon = Arc::downgrade(self);
        let on_event: OnEngineEvent = Arc::new(move |event| {
            let Some(daemon) = daemon.upgrade() else {
                return;
            };
            daemon.publish(match event {
                EngineEvent::Levels(levels) => Event::Levels { levels },
                EngineEvent::Source(status) => Event::SourceChanged { status },
                EngineEvent::Warning(message) => Event::Warning { message },
                EngineEvent::WorkerFailed {
                    name,
                    message,
                    broken,
                } => {
                    if broken {
                        daemon.emergency_stop(message.clone());
                    } else if name == "mixer" {
                        let daemon = daemon.clone();
                        let why = message.clone();
                        thread::spawn(move || {
                            daemon.contain("failed program output", || {
                                daemon.fail_streams(&why);
                            })
                        });
                    } else if let Some(canvas) = isocast_engine::record::compositor_canvas(&name) {
                        // A compositor owns only its canvas's picture; the
                        // other canvases still have healthy video and audio.
                        let daemon = daemon.clone();
                        let why = match canvas {
                            CanvasId::Main => message.clone(),
                            CanvasId::Vertical => {
                                format!("the vertical canvas stopped: {message}")
                            }
                            other => format!("canvas {other:?} stopped: {message}"),
                        };
                        thread::spawn(move || {
                            daemon.contain("failed canvas program output", || {
                                daemon.fail_canvas_streams(canvas, &why);
                                // The vertical replay buffer would keep a
                                // frozen picture; main's keeps going as before.
                                if !canvas.is_main() {
                                    daemon.fail_canvas_replay(canvas, &why);
                                }
                            })
                        });
                    }
                    Event::Warning { message }
                }
            });
        });
        state.starting = true;
        drop(state);
        // Callers wait until `starting` clears, so clear it even if
        // starting panics (a GPU error, say) rather than hang them all.
        let started = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.cancel_background_calibration()
                .map_err(|error| anyhow::anyhow!(error.message))?;
            start_engine(&setup, control, on_event)
        }))
        .unwrap_or_else(|_| {
            Err(anyhow::anyhow!(
                "the live engine panicked while starting; use `isocast logs --level error` or `isocast report`"
            ))
        });
        let mut state = self.state.lock();
        state.starting = false;
        self.engine_ready.notify_all();
        let engine = started.map_err(failed)?;
        // Its frames say which revision they show; and changes made while
        // it started.
        if let Some(setup) = &state.setup
            && let Err(message) = engine.update(&state.with_hotkey_holds(setup), state.revision)
        {
            isocast_diagnostics::diagnostic!("error"; "isocastd: {message}");
        }
        if state.scene != scene
            && let Some(scene) = &state.scene
        {
            let _ = engine
                .control()
                .switch_scene(now(), scene.clone(), Transition::default());
        }
        engine.set_preview(state.preview.clone());
        // Each other canvas starts on the scene it had on air.
        for (canvas, _) in canvas_links(&state) {
            if let Some(scene) = chosen_canvas_scene(&state, canvas)
                && let Err(message) =
                    engine.switch_canvas_scene(canvas, now(), scene, Transition::default())
            {
                isocast_diagnostics::diagnostic!("warn"; "isocastd: {canvas} canvas: {message}");
            }
            if let Some(scene) = chosen_canvas_preview(&state, canvas)
                && let Err(message) = engine.set_canvas_preview(canvas, Some(scene))
            {
                isocast_diagnostics::diagnostic!("warn"; "isocastd: {canvas} preview: {message}");
            }
        }
        state.engine = Some(engine.clone());
        drop(state);
        self.publish(Event::EngineChanged { running: true });
        Ok(engine)
    }

    /// The live engine's newest frame, as asked.
    fn frame(self: &Arc<Self>, params: FrameParams) -> Result<Frame, RpcError> {
        self.state.lock().wanted_at = Some(std::time::Instant::now());
        let engine = self.ensure_engine()?;
        let timeout =
            Duration::from_millis(params.timeout_ms.unwrap_or(1000).into()).min(MAX_FRAME_WAIT);
        let quality = params.quality.unwrap_or(0.75).clamp(0.0, 1.0);
        let frame = engine
            .frame_jpeg(
                &params.target,
                params.after,
                timeout,
                params.max_width,
                quality,
                params.fps,
            )
            .map_err(|message| RpcError {
                code: RpcError::FAILED,
                message,
            })?;
        let Some(frame) = frame else {
            return Err(RpcError {
                code: RpcError::FAILED,
                message: paused_preview(&params.target).unwrap_or_else(|| {
                    format!(
                        "no new frame of {} within {} ms; ask again",
                        params.target,
                        timeout.as_millis()
                    )
                }),
            });
        };
        Ok(Frame {
            target: params.target,
            seq: frame.seq,
            width: frame.width,
            height: frame.height,
            jpeg_base64: base64::engine::general_purpose::STANDARD.encode(frame.jpeg),
            revision: frame.revision,
            layout_generation: frame.layout_generation,
        })
    }

    /// Start streaming `params.target` to a monitor: the reply, and the
    /// stream's records.
    fn frames(
        self: &Arc<Self>,
        params: &FramesParams,
    ) -> Result<(FrameStream, FrameFeed), RpcError> {
        self.state.lock().wanted_at = Some(std::time::Instant::now());
        let engine = self.ensure_engine()?;
        let fps = params.fps.unwrap_or(30).clamp(1, 60);
        let feed = engine
            .frames(
                &params.target,
                params.max_width,
                fps,
                params.profile.unwrap_or_default(),
            )
            .map_err(|message| RpcError {
                code: RpcError::FAILED,
                message,
            })?;
        let reply = FrameStream {
            target: params.target.clone(),
            codec: "h264".into(),
            fps,
            host_time: now().as_nanos().max(0) as u64,
        };
        Ok((reply, feed))
    }

    /// Start showing `params.target` to a monitor in a client on this
    /// machine through shared surfaces: the reply, the viewer's feed, and
    /// where its rings are handed over.
    fn shared_frames(
        self: &Arc<Self>,
        params: &SharedFramesParams,
    ) -> Result<(SharedFrameStream, SharedFeed<Surface>, &'static Exporter), RpcError> {
        let invalid = |message: String| RpcError {
            code: RpcError::INVALID_REQUEST,
            message,
        };
        let slots = params.slots.unwrap_or(DEFAULT_SLOTS);
        if !(2..=isocast_surfaces::MAX_SLOTS as u32).contains(&slots) {
            return Err(invalid(format!(
                "slots: {slots} is out of range; give 2 to {}",
                isocast_surfaces::MAX_SLOTS
            )));
        }
        let fps = params.fps.unwrap_or(30);
        if !(1..=60).contains(&fps) {
            return Err(invalid(format!("fps: {fps} is out of range; give 1 to 60")));
        }
        for (field, value) in [
            ("max_width", params.max_width),
            ("max_height", params.max_height),
        ] {
            if value == Some(0) {
                return Err(invalid(format!(
                    "{field}: 0 is too small; give a size in pixels, or leave it out for full size"
                )));
            }
        }
        let exporter = Exporter::shared().map_err(|err| RpcError {
            // Not a passing failure: viewers stop asking and say so.
            code: RpcError::UNSUPPORTED,
            message: format!(
                "shared frames can't be handed over here ({err:#}); use `frames` (H.264) instead"
            ),
        })?;
        self.state.lock().wanted_at = Some(std::time::Instant::now());
        let engine = self.ensure_engine()?;
        let feed = engine
            .shared_frames(
                &params.target,
                (params.max_width, params.max_height),
                fps,
                slots,
                params.filtered,
            )
            .map_err(|message| RpcError {
                code: RpcError::FAILED,
                message,
            })?;
        let reply = SharedFrameStream {
            target: params.target.clone(),
            transport: isocast_surfaces::TRANSPORT.into(),
            service: exporter.service().to_string(),
            format: "rgba8unorm".into(),
            fps,
            slots,
            host_time: now().as_nanos().max(0) as u64,
        };
        Ok((reply, feed, exporter))
    }

    /// Keep the live engine running, or let it stop once nothing needs it.
    fn hold_engine(self: &Arc<Self>, running: bool) -> Result<Option<EngineInfo>, RpcError> {
        self.state.lock().hold = running;
        if running {
            self.ensure_engine()?;
        } else {
            self.stop_idle_engine_now();
        }
        Ok(engine_info(&self.state.lock()))
    }

    /// Stop the live engine (after the recording, if one runs).
    fn stop_engine(&self) {
        self.virtual_camera_stop();
        self.replay_stop_all();
        let engine = self.state.lock().engine.take();
        if let Some(engine) = engine {
            engine.stop();
            self.publish(Event::EngineChanged { running: false });
        }
    }

    /// Stop the live engine if nothing needs it: no recording, no hold, and
    /// no client asking for frames lately.
    fn stop_idle_engine_now(&self) {
        // Taking it out of state does not mean its native workers have
        // finished. Shutdown must wait for this stop before process exit
        // can run the GPU driver's finalizers on another thread.
        let _shutdown = self.shutdown_lock.lock();
        let engine = {
            let mut state = self.state.lock();
            let idle = state.session.is_none()
                && !state.camera.active()
                && !state.replay.active()
                && !state.streaming()
                && state.streams_starting == 0
                && !state.hold
                && state.wanted_at.is_none_or(|at| at.elapsed() >= ENGINE_IDLE);
            if !idle {
                return;
            }
            state.engine.take()
        };
        if let Some(engine) = engine {
            engine.stop();
            self.publish(Event::EngineChanged { running: false });
        }
    }

    /// Check every second for a live engine nothing needs. Never returns.
    fn stop_idle_engine(&self) {
        loop {
            thread::sleep(Duration::from_secs(1));
            self.warn_stream_drops();
            self.warn_replay_drops();
            self.stop_idle_engine_now();
        }
    }

    /// Ask the running session to stop (or cancel one still starting) and
    /// answer at once: the session's own coordinator finishes its files and
    /// ends it as `stop` waits for. Only flags and copies under the lock;
    /// asking again while it finishes answers the same way.
    fn request_stop(&self, only: Option<&str>) -> Result<StopAccepted, RpcError> {
        let state = self.state.read();
        let session = running_session(&state, only)?;
        let files = session.files.lock().clone().unwrap_or_default();
        session.stop.store(true, Ordering::SeqCst);
        note_stopped_by(session);
        let session_id = session.id;
        let token = session.token.clone();
        #[cfg(windows)]
        if let Some(control) = state.control() {
            control.wake_recording();
        }
        Ok(StopAccepted {
            session_id,
            session: Some(token),
            finalizing: true,
            files,
        })
    }

    /// Stop the running session (or cancel one still starting) and wait
    /// for its files to be finished; with `only`, only if that is the
    /// session running.
    fn stop(&self, only: Option<&str>) -> Result<Report, RpcError> {
        let state = self.state.lock();
        let session = running_session(&state, only)?;
        session.stop.store(true, Ordering::SeqCst);
        note_stopped_by(session);
        // This session's own completion, whatever starts and ends after it.
        let done = session.done.clone();
        #[cfg(windows)]
        if let Some(control) = state.control() {
            control.wake_recording();
        }
        drop(state);
        let (outcome, ended) = &*done;
        let mut outcome = outcome.lock();
        let deadline = std::time::Instant::now() + STOP_TIMEOUT;
        while outcome.is_none() {
            if ended.wait_until(&mut outcome, deadline).timed_out() {
                break;
            }
        }
        match outcome.clone() {
            Some(Ok(report)) => Ok(report),
            Some(Err(error)) => Err(RpcError {
                code: RpcError::FAILED,
                message: error,
            }),
            None => Err(RpcError {
                code: RpcError::FAILED,
                message: "the recording did not stop in time; inspect status.diagnostics.metadata_pending or run `isocast doctor` for blocked final metadata publication".into(),
            }),
        }
    }

    /// Session `id` has ended, with its report or why it has none.
    fn ended(&self, id: u64, ended: Result<Report, String>, stopped: Event) {
        // Logging can touch disk; it must not hold the control-state lock.
        diagnostics::event(&stopped);
        // Broken control state must not prevent a writer's completed report
        // from reaching the emergency finalizer (the lock never poisons).
        let mut state = self.state.lock();
        // Whoever waits for this session (a blocking `stop`) gets its own
        // outcome.
        if let Some(session) = state.session.as_ref().filter(|s| s.id == id) {
            let (outcome, finished) = &*session.done;
            *outcome.lock() = Some(ended.clone());
            finished.notify_all();
        }
        if state.session.as_ref().is_some_and(|s| s.id == id) {
            state.session = None;
        }
        state.ended_session = state.ended_session.max(id);
        match ended {
            Ok(report) => {
                state.last = Some(report);
                state.last_error = None;
            }
            Err(error) => state.last_error = Some(error),
        }
        // Queue the completion after updating state but before releasing the
        // session to Start/Shutdown or waking blocking Stop. Watchers refreshing
        // status see the new state; shutdown cannot exit before it was announced.
        // Unlike publish, send_event only queues bytes, with no state callbacks.
        self.send_event(stopped);
        self.ended.notify_all();
    }
}

/// When a split's take begins, as replies say it.
fn split_text(split: &SplitTake) -> String {
    split
        .begins_at
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, false)
}

/// `start` while `session` records: the same `start` again (a retry
/// after a lost answer) answers with the running session; anything else
/// is refused.
fn already_recording(session: &Session, asked: &StartParams) -> Result<Started, RpcError> {
    if session.stop.load(Ordering::SeqCst) {
        return Err(RpcError {
            code: RpcError::WRONG_STATE,
            message: format!(
                "session {:?} is stopping (finishing its files); start again once `status` \
                 says it has stopped",
                session.token
            ),
        });
    }
    // "The same params": the parsed StartParams compare equal (setup,
    // folder, seconds), whatever the JSON's spelling.
    if session.asked != *asked {
        return Err(RpcError {
            code: RpcError::WRONG_STATE,
            message: format!(
                "already recording (session {:?}); stop the current session first",
                session.token
            ),
        });
    }
    let files = session.files.lock().clone();
    let mut notes = vec![format!(
        "already recording with these params: session {:?} keeps going and nothing new started",
        session.token
    )];
    if files.is_none() {
        notes.push("it is still starting; `status` shows when it records".into());
    }
    Ok(Started {
        files: files.unwrap_or_default(),
        begins_at: None,
        notes,
        session: Some(session.token.clone()),
        already_recording: true,
    })
}

/// The running session, if it is `only` (any, without it): `stop` and
/// `request_stop` with a session id never end another (decision 17).
fn running_session<'a>(state: &'a State, only: Option<&str>) -> Result<&'a Session, RpcError> {
    let Some(session) = &state.session else {
        return Err(RpcError {
            code: RpcError::WRONG_STATE,
            message: match only {
                Some(only) => format!("not recording; session {only:?} has ended"),
                None => "not recording".into(),
            },
        });
    };
    if let Some(only) = only
        && only != session.token
    {
        return Err(RpcError {
            code: RpcError::WRONG_STATE,
            message: format!(
                "session {only:?} isn't the one recording (it has ended): session {:?} is \
                 recording now, and is left running",
                session.token
            ),
        });
    }
    Ok(session)
}

/// Remember who asked `session` to stop, for its `stopped` event (the
/// first to ask, if several do).
fn note_stopped_by(session: &Session) {
    let mut stopped_by = session.stopped_by.lock();
    if stopped_by.is_none() {
        *stopped_by = Some(callers::by());
    }
}

/// This isocastd's build, for `status` (an engine left running from
/// before an app update reports the old one).
fn daemon_build() -> isocast_protocol::api::DaemonBuild {
    static EXECUTABLE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    isocast_protocol::api::DaemonBuild {
        version: env!("CARGO_PKG_VERSION").into(),
        build: option_env!("ISOCAST_BUILD_NUMBER")
            .unwrap_or("unknown")
            .into(),
        commit: env!("ISOCAST_BUILD_COMMIT").into(),
        executable: EXECUTABLE
            .get_or_init(|| {
                std::env::current_exe()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default()
            })
            .clone(),
        pid: std::process::id(),
        parent_owned: PARENT_OWNED.load(Ordering::SeqCst),
    }
}

/// The live engine's sources and outputs, with what keeps it running
/// (held, a client showing frames, a stream), as status reports them.
fn engine_info(state: &State) -> Option<EngineInfo> {
    let mut info = state.engine.as_ref().map(|engine| engine.info())?;
    if state.hold {
        info.outputs.push("held".into());
    }
    if state.wanted_at.is_some_and(|at| at.elapsed() < ENGINE_IDLE) {
        info.outputs.push("frames".into());
    }
    if state.streaming() {
        info.outputs.push("stream".into());
    }
    if state.camera.active() {
        info.outputs.push("virtual_camera".into());
    }
    if state.replay.active() {
        info.outputs.push("replay".into());
    }
    Some(info)
}

/// Why `folder` can't be used, if it is relative: isocastd runs in its
/// own folder, not the client's, so a relative folder would land somewhere
/// unexpected.
fn relative_folder(folder: &str) -> Option<String> {
    (!Path::new(folder).is_absolute() && !folder.starts_with('~')).then(|| {
        format!(
            "{folder:?} is relative, but isocastd doesn't run in your folder; give an absolute \
             path (or one starting with ~/)"
        )
    })
}

/// Refuse a `folder` parameter that isn't absolute (see
/// [`relative_folder`]; `~` isn't expanded in it either).
fn check_folder(folder: Option<&Path>) -> Result<(), RpcError> {
    match folder.filter(|folder| !folder.is_absolute()) {
        Some(folder) => Err(RpcError {
            code: RpcError::FAILED,
            message: format!(
                "folder: {:?} isn't an absolute path, and isocastd doesn't run in your folder; \
                 give an absolute path",
                folder.display().to_string()
            ),
        }),
        None => Ok(()),
    }
}

thread_local! {
    /// The plugin whose request this thread is answering (`handle`), if
    /// any.
    static PLUGIN_CALLER: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// Marks the thread as answering a plugin's request until dropped. Each
/// connection is answered on its own thread, and the calls that take a
/// folder from the request run on it.
struct PluginCaller;

impl PluginCaller {
    fn set(id: &str) -> Self {
        PLUGIN_CALLER.with(|caller| *caller.borrow_mut() = Some(id.to_string()));
        Self
    }

    /// The plugin whose request this thread is answering, if any.
    fn current() -> Option<String> {
        PLUGIN_CALLER.with(|caller| caller.borrow().clone())
    }
}

impl Drop for PluginCaller {
    fn drop(&mut self) {
        PLUGIN_CALLER.with(|caller| *caller.borrow_mut() = None);
    }
}

/// Refuse a recordings folder a plugin gives (`field`) unless it is one
/// already (`roots`, or the default `~/Movies/Isocast`), compared as
/// written: a plugin's sandbox may read (or write) the recordings folders,
/// so one it named would widen its own sandbox and every other plugin's
/// (#103). Requests that aren't a plugin's are never refused here.
fn plugin_folder_refusal(field: &str, root: &Path, roots: &[PathBuf]) -> Result<(), RpcError> {
    let Some(plugin) = PluginCaller::current() else {
        return Ok(());
    };
    let default = expand_home(isocast_protocol::init::DEFAULT_FOLDER);
    let mut allowed: Vec<&Path> = roots.iter().map(PathBuf::as_path).collect();
    allowed.push(&default);
    allowed.sort();
    allowed.dedup();
    if allowed.contains(&root) {
        return Ok(());
    }
    Err(RpcError {
        code: RpcError::NOT_PERMITTED,
        message: format!(
            "{field}: {:?} isn't a recordings folder, and plugin {plugin} may only use the \
             recordings folders there are (or a project folder in one), because its sandbox \
             is given them: {}",
            root.display().to_string(),
            allowed
                .iter()
                .map(|root| format!("{:?}", root.display().to_string()))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    })
}

/// Hotkey actions that write into a folder their arguments name, the
/// argument, and whether it names a file (in the folder) rather than the
/// folder: `program.screenshot` writes its file there.
const PATH_ACTIONS: &[(&str, &str, bool)] = &[("program.screenshot", "path", true)];

/// Refuse, for a plugin's request, a hotkey binding in `new` (new, or
/// changed since `old`) whose [`PATH_ACTIONS`] path isn't in a recordings
/// folder (the folder itself or a project folder in one): it fires later
/// on the hotkeys thread, maybe when the user presses its keys, as no
/// plugin's request, so nothing checks it then (#103). The stored bindings
/// are read as they are, including ones that don't parse or conflict now.
fn plugin_hotkeys_refusal(
    field: &str,
    new: &isocast_protocol::hotkeys::Config,
    old: Option<&isocast_protocol::hotkeys::Config>,
    roots: &[PathBuf],
) -> Result<(), RpcError> {
    if PluginCaller::current().is_none() {
        return Ok(());
    }
    let path_of = |binding: &serde_json::Value| {
        let action = binding.get("action").and_then(serde_json::Value::as_str)?;
        let &(_, arg, file) = PATH_ACTIONS.iter().find(|(id, ..)| *id == action)?;
        let value = binding.get("args")?.get(arg)?;
        (!value.is_null()).then(|| (action.to_string(), arg, file, value.clone()))
    };
    let Some(new) = new.value.as_ref().and_then(serde_json::Value::as_object) else {
        return Ok(());
    };
    let old = old
        .and_then(|old| old.value.as_ref())
        .and_then(serde_json::Value::as_object);
    for (id, binding) in new {
        let Some((action, arg, file, path)) = path_of(binding) else {
            continue;
        };
        // Kept as it was (the user's, or checked when planted).
        if old
            .and_then(|old| old.get(id))
            .and_then(path_of)
            .is_some_and(|(a, _, _, p)| a == action && p == path)
        {
            continue;
        }
        let field = format!(
            "{field}.{}.args.{arg}",
            isocast_protocol::hotkeys::bounded(id, 64)
        );
        let Some(path) = path.as_str().map(Path::new) else {
            return Err(RpcError {
                code: RpcError::NOT_PERMITTED,
                message: format!("{field}: give a path in a recordings folder (a string)"),
            });
        };
        let folder = if file {
            path.parent().unwrap_or(path)
        } else {
            path
        };
        plugin_folder_refusal(&field, &projects::root_of(folder), roots)?;
    }
    Ok(())
}

/// The revision of a setup loaded at start: the time in milliseconds, so
/// it is above any revision an earlier run handed out (that would take over
/// a thousand changes a second since it started). A client planning with a
/// revision from before a restart is then told to plan again, rather than
/// overwriting changes made since.
fn first_revision() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |since| since.as_millis() as u64)
        .max(1)
}

/// The live setup's stream settings.
fn stream_settings(state: &State) -> Option<StreamSettings> {
    state.setup.as_ref().and_then(|setup| setup.stream.clone())
}

/// Every extra canvas's status (`Status::canvases`).
fn canvas_statuses(state: &State) -> Vec<isocast_protocol::api::CanvasStatus> {
    let Some(setup) = state.setup.as_ref() else {
        return Vec::new();
    };
    setup
        .extra_canvas_ids()
        .into_iter()
        .filter_map(|id| {
            let extra = setup.extra_canvas(id)?;
            Some(isocast_protocol::api::CanvasStatus {
                id,
                name: extra.label().to_string(),
                canvas: extra.canvas(&setup.canvas),
                follow_main: extra.follow_main,
                scene: canvas_scene(state, id),
                preview: canvas_preview(state, id),
                recorded: extra.recording.as_ref().is_some_and(|r| r.enabled),
                streams: streams::canvas_statuses(state, id),
                replay: state.replay.canvas_status(id, Some(setup)),
            })
        })
        .collect()
}

/// The live setup's extra canvases (`[vertical]`, `[[canvases]]`), each
/// with whether it follows the main canvas.
fn canvas_links(state: &State) -> Vec<(CanvasId, bool)> {
    let Some(setup) = state.setup.as_ref() else {
        return Vec::new();
    };
    setup
        .extra_canvas_ids()
        .into_iter()
        .filter_map(|canvas| Some((canvas, setup.extra_canvas(canvas)?.follow_main)))
        .collect()
}

/// Extra canvas `canvas` of the live setup: absent, or whether it follows
/// the main canvas.
fn canvas_link(state: &State, canvas: CanvasId) -> Option<bool> {
    state
        .setup
        .as_ref()?
        .extra_canvas(canvas)
        .map(|extra| extra.follow_main)
}

/// [`sync_canvas`] for every extra canvas the live setup has or had
/// (`before`, from [`canvas_links`]).
fn sync_canvases(state: &mut State, before: &[(CanvasId, bool)], reset: bool) -> Vec<Event> {
    let mut canvases: Vec<CanvasId> = before.iter().map(|(canvas, _)| *canvas).collect();
    for (canvas, _) in canvas_links(state) {
        if !canvases.contains(&canvas) {
            canvases.push(canvas);
        }
    }
    // Choices kept for canvases no longer there go too.
    let mut events = Vec::new();
    for canvas in canvases {
        let was = before
            .iter()
            .find(|(other, _)| *other == canvas)
            .map(|(_, follows)| *follows);
        events.extend(sync_canvas(state, canvas, was, reset));
    }
    events
}

/// Bring an extra canvas's own scene and preview in line with the live
/// setup after it changed, given whether it was there and linked before
/// (`canvas_link`), so status, the engine and a later engine start agree
/// (#86, #84):
/// - removed: no choices of its own are kept;
/// - linked (`follow_main`): it shows the main canvas's, and the engine's
///   preview of it is set to the main preview once it becomes linked
///   (dropping an unlinked one it had);
/// - becoming unlinked (added, unlinked, or `reset` for a new scene
///   collection): it starts from the main canvas's current scene and
///   preview, never from choices it had before;
/// - staying unlinked: its choices stay, unless the setup lost their scenes
///   (then the main scene, and no preview).
///
/// Called with the state held, as the main canvas's scene and preview are
/// updated; returns the events to publish once it is released.
fn sync_canvas(
    state: &mut State,
    canvas: CanvasId,
    before: Option<bool>,
    reset: bool,
) -> Vec<Event> {
    let mut events = Vec::new();
    let engine = state.engine.clone();
    let name = match canvas {
        CanvasId::Vertical => "vertical".to_string(),
        other => format!("canvas {other}"),
    };
    let warn = |what: &str, message: String| {
        isocast_diagnostics::diagnostic!("warn"; "isocastd: {name} {what}: {message}");
        Event::Warning {
            message: format!("{name} {what}: {message}"),
        }
    };
    match canvas_link(state, canvas) {
        None => {
            state.canvas_scenes.remove(&canvas);
            state.canvas_previews.remove(&canvas);
        }
        Some(true) => {
            state.canvas_scenes.remove(&canvas);
            state.canvas_previews.remove(&canvas);
            if (reset || before != Some(true))
                && let Some(engine) = &engine
                && let Err(message) = engine.set_canvas_preview(canvas, state.preview.clone())
            {
                events.push(warn("preview", message));
            }
        }
        Some(false) => {
            let ids = state.scene_ids();
            let fresh = reset || before != Some(false);
            let scene_gone = state
                .canvas_scenes
                .get(&canvas)
                .is_some_and(|scene| !ids.contains(scene));
            if fresh || scene_gone {
                match state.scene.clone() {
                    Some(scene) => {
                        state.canvas_scenes.insert(canvas, scene.clone());
                        if let Some(engine) = &engine
                            && let Err(message) = engine.switch_canvas_scene(
                                canvas,
                                now(),
                                scene.clone(),
                                Transition::default(),
                            )
                        {
                            events.push(warn("canvas", message));
                        }
                        events.push(Event::CanvasSceneChanged {
                            canvas,
                            scene,
                            take: None,
                            time_seconds: None,
                            transition: None,
                        });
                    }
                    None => {
                        state.canvas_scenes.remove(&canvas);
                    }
                }
            }
            let preview_gone = state
                .canvas_previews
                .get(&canvas)
                .is_some_and(|scene| !ids.contains(scene));
            if fresh || preview_gone {
                let preview = if fresh { state.preview.clone() } else { None };
                match &preview {
                    Some(scene) => state.canvas_previews.insert(canvas, scene.clone()),
                    None => state.canvas_previews.remove(&canvas),
                };
                if let Some(engine) = &engine
                    && let Err(message) = engine.set_canvas_preview(canvas, preview.clone())
                {
                    events.push(warn("preview", message));
                }
                events.push(Event::CanvasPreviewChanged {
                    canvas,
                    scene: preview,
                });
            }
        }
    }
    events
}

/// The scene extra canvas `canvas` was switched to that it should show:
/// while it doesn't follow the main canvas and the setup still has it.
fn chosen_canvas_scene(state: &State, canvas: CanvasId) -> Option<String> {
    let extra = state.setup.as_ref()?.extra_canvas(canvas)?;
    if extra.follow_main {
        return None;
    }
    let scene = state.canvas_scenes.get(&canvas).cloned()?;
    state.scene_ids().contains(&scene).then_some(scene)
}

/// The scene staged in extra canvas `canvas`'s own preview that it should
/// show: while it doesn't follow the main canvas and the setup still has
/// it.
fn chosen_canvas_preview(state: &State, canvas: CanvasId) -> Option<String> {
    let extra = state.setup.as_ref()?.extra_canvas(canvas)?;
    if extra.follow_main {
        return None;
    }
    let scene = state.canvas_previews.get(&canvas).cloned()?;
    state.scene_ids().contains(&scene).then_some(scene)
}

/// What status says is staged in extra canvas `canvas`'s preview: the
/// main preview while it follows the main one, else its own.
fn canvas_preview(state: &State, canvas: CanvasId) -> Option<String> {
    let extra = state.setup.as_ref()?.extra_canvas(canvas)?;
    if extra.follow_main {
        return state.preview.clone();
    }
    chosen_canvas_preview(state, canvas)
}

/// The live setup, if it has extra canvas `canvas` and it doesn't follow
/// the main one, so `what` ("switch scenes on") can happen on it; else why
/// not.
fn independent_canvas<'a>(
    state: &'a State,
    canvas: CanvasId,
    what: &str,
) -> Result<&'a Setup, RpcError> {
    let Some(setup) = state.setup.as_ref() else {
        return Err(state.no_setup(&format!("no {canvas} canvas to {what}")));
    };
    let Some(extra) = setup.extra_canvas(canvas) else {
        return Err(RpcError {
            code: RpcError::WRONG_STATE,
            message: streams::no_canvas(setup, canvas),
        });
    };
    if extra.follow_main {
        let field = setup.canvas_field(canvas);
        let patch = match canvas {
            CanvasId::Vertical => "the patch {\"vertical\": {\"follow_main\": false}}".to_string(),
            other => format!("`isocast canvas set {other} --follow-main false`"),
        };
        return Err(RpcError {
            code: RpcError::WRONG_STATE,
            message: format!(
                "canvas: the {canvas} canvas follows the main one ({field}.follow_main), so it \
                 shows the main canvas's scenes and preview; to {what} it, use the main canvas, or \
                 let it pick its own scenes with {patch}"
            ),
        });
    }
    Ok(setup)
}

/// What status says is on air on extra canvas `canvas`: the engine's,
/// while it runs; else the main scene while it follows it, or the one it
/// was switched to (the main scene if none).
fn canvas_scene(state: &State, canvas: CanvasId) -> Option<String> {
    let extra = state.setup.as_ref()?.extra_canvas(canvas)?;
    if let Some(engine) = &state.engine
        && let Some(scene) = engine.canvas_scene(canvas)
    {
        return Some(scene);
    }
    if extra.follow_main {
        return state.scene.clone();
    }
    chosen_canvas_scene(state, canvas).or_else(|| state.scene.clone())
}

use streams::stream_status;

/// When `key_problem` last asked about an account, and what it found.
type KeyCheck = (std::time::Instant, Option<String>);

/// What `key_problem` last found, per account.
static KEY_CHECKED: Mutex<Option<HashMap<String, KeyCheck>>> = Mutex::new(None);

/// Whether the stored key for `account` can be read without a prompt (a
/// key stored by another build can't): why not, if not. Asked again at
/// most every 10 s per account, and at once after a key is set or cleared.
fn key_problem(account: &str) -> Option<String> {
    let mut checked = KEY_CHECKED.lock();
    let checked = checked.get_or_insert_with(HashMap::new);
    if let Some((at, problem)) = checked.get(account)
        && at.elapsed() < Duration::from_secs(10)
    {
        return problem.clone();
    }
    let problem = match secret(key_service(), account) {
        Err(err) if format!("{err:#}").contains("another build") => Some(format!(
            "The saved key was stored by a differently signed copy of this Isocast, and macOS \
             lets only that copy use or replace it. Open Keychain Access, delete the “{}” item, \
             then paste the key and press Save Key.",
            key_service()
        )),
        _ => None,
    };
    checked.insert(
        account.to_string(),
        (std::time::Instant::now(), problem.clone()),
    );
    problem
}

/// Forget what `key_problem` found (a key was just set or cleared).
fn forget_key_problem() {
    *KEY_CHECKED.lock() = None;
}

/// "twitch", or "custom (rtmp://host/app)".
fn key_label(service: StreamService, account: &str) -> String {
    match service {
        StreamService::Custom => format!("custom ({account})"),
        service => service.id().to_string(),
    }
}

/// What a change would do to the live setup (with its uses of plugins'
/// kinds checked against the installed plugins).
fn plan(state: &State, change: Change, plugins: &plugins::Registry) -> Result<Plan, RpcError> {
    let new_items = matches!(&change, Change::Patch(_));
    let mut notes: Vec<String> = Vec::new();
    let current = match &state.setup {
        Some(setup) => to_json(setup)?,
        None => serde_json::Value::Object(Default::default()),
    };
    let (proposed, missing) = match change {
        Change::Setup(mut setup) => {
            if setup.hotkeys.is_absent()
                && let Some(current) = &state.setup
            {
                setup.hotkeys = current.hotkeys.clone();
                if !current.hotkeys.is_absent() {
                    notes.push("hotkeys omitted: kept the current profile's bindings; use hotkeys_set to change or clear them".into());
                }
            }
            (to_json(setup)?, Vec::new())
        }
        Change::Patch(_) if state.setup.is_none() => {
            return Err(state.no_setup("nothing to patch (or give a whole setup)"));
        }
        Change::Patch(patch) => {
            let mut proposed = merge_patch(&current, &patch);
            if patch.get("hotkeys").is_some_and(serde_json::Value::is_null) {
                // An explicit merge-patch deletion clears the sidecar too;
                // omission elsewhere means preserve the profile setting.
                proposed["hotkeys"] = serde_json::json!({});
            }
            (proposed, missing_removals(&current, &patch))
        }
    };
    let mut changes = diff(&current, &proposed);
    let (mut setup, mut problems) = match Setup::from_value(proposed) {
        Ok(mut setup) => {
            if let Some(previous) = &state.setup {
                let sizes = state
                    .engine
                    .as_ref()
                    .map(|e| {
                        e.info()
                            .sources
                            .into_iter()
                            .filter_map(|s| Some((s.source, (s.width?, s.height?))))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                isocast_engine::layout::follow_source_sizes(
                    previous, &mut setup, &sizes, new_items,
                );
                changes = diff(&current, &to_json(&setup)?);
            }
            if setup.hotkeys.is_absent()
                && let Some(current) = &state.setup
                && current.hotkeys.load_warning.is_some()
            {
                setup.hotkeys = current.hotkeys.clone();
            }
            if !state.camera.active()
                && let Some(previous) = &state.setup
            {
                use isocast_protocol::virtual_camera::Target;
                let before = &previous.virtual_camera;
                let after = &setup.virtual_camera;
                let removed = before.target == after.target
                    && match after.target {
                        Target::Scene => {
                            before.scene == after.scene
                                && before.scene.as_ref().is_some_and(|id| {
                                    previous.scenes.iter().any(|s| &s.id == id)
                                        && !setup.scenes.iter().any(|s| &s.id == id)
                                })
                        }
                        Target::Source => {
                            before.source == after.source
                                && before.source.as_ref().is_some_and(|id| {
                                    previous.source(id).is_some_and(|s| s.kind.has_video())
                                        && !setup.source(id).is_some_and(|s| s.kind.has_video())
                                })
                        }
                        Target::Vertical | Target::Canvas => {
                            after
                                .target_canvas()
                                .is_some_and(|c| previous.has_canvas(c) && !setup.has_canvas(c))
                                && before.canvas == after.canvas
                        }
                        _ => false,
                    };
                if removed {
                    notes.push("virtual_camera: its selected scene/video source (or extra canvas) was removed while stopped; falling back to program".into());
                    setup.virtual_camera.target = Target::Program;
                    setup.virtual_camera.scene = None;
                    setup.virtual_camera.source = None;
                    setup.virtual_camera.canvas = None;
                    changes = diff(&current, &to_json(&setup)?);
                }
            }
            let mut problems = setup.validate();
            if let Some(message) = relative_folder(&setup.recording.folder) {
                problems.push(Issue {
                    path: "recording.folder".into(),
                    message,
                });
            }
            // The camera's negotiated format stays fixed until Stop, and so
            // does the vertical canvas it shows (target = vertical).
            if state.camera.active()
                && state.setup.as_ref().is_some_and(|current| {
                    // The extra canvas it shows (target = vertical or canvas).
                    let shown = |s: &Setup| {
                        current
                            .virtual_camera
                            .target_canvas()
                            .and_then(|c| s.extra_canvas(c))
                            .map(|v| v.canvas(&s.canvas))
                    };
                    current.canvas != setup.canvas
                        || current.virtual_camera != setup.virtual_camera
                        || shown(current) != shown(&setup)
                })
            {
                problems.push(Issue { path: "virtual_camera".into(), message: "stop the virtual camera before changing its settings or canvas; scene contents can be edited live".into() });
            }
            // The live engine refuses a new canvas while the program is
            // recorded or streamed; say so here, before `apply`.
            if let Some(message) = state
                .engine
                .as_ref()
                .and_then(|engine| engine.canvas_refusal(&setup.canvas))
            {
                problems.push(Issue {
                    path: "canvas".into(),
                    message,
                });
            }
            // And whatever else it would refuse now (the vertical canvas
            // changing while it is recorded or streamed, say).
            if let Some(issue) = state
                .engine
                .as_ref()
                .and_then(|engine| engine.setup_refusal(&setup))
                && !problems
                    .iter()
                    .any(|p| p.path == issue.path && p.message == issue.message)
            {
                problems.push(issue);
            }
            (Some(setup), problems)
        }
        Err(err) => (
            None,
            vec![Issue {
                path: err.path().to_string(),
                message: format!("not a valid setup: {}", err.message()),
            }],
        ),
    };
    problems.splice(0..0, missing);
    if let Some(setup) = &setup {
        let (_, issues) = setup.hotkeys.usable();
        if !issues.is_empty() {
            notes.push(format!(
                "invalid hotkeys are disabled; recording is unaffected: {}",
                isocast_protocol::hotkeys::describe(&issues)
            ));
        }
        let (plugin_problems, plugin_notes) = plugins.check_kinds(setup);
        problems.extend(plugin_problems.into_iter().map(|mut issue| {
            setup.identify_source_issue(&mut issue);
            issue
        }));
        notes.extend(plugin_notes);
    }
    if state.session.is_some() {
        notes.push(
            "a recording is in progress: changes show in the program at once, but the sources \
             it records keep their devices until it stops, and the canvas can't change"
                .into(),
        );
    }
    if state.streaming_on(CanvasId::Vertical)
        && changes.iter().any(|change| {
            change.path == "vertical"
                || change.path == "vertical.stream"
                || change.path.starts_with("vertical.stream.")
        })
    {
        notes.push(
            "a vertical stream is running: it keeps its current [vertical.stream] settings until \
             it stops; the changes take effect at its next start (`isocast stream stop --canvas \
             vertical`, then `isocast stream start --canvas vertical`)"
                .into(),
        );
    }
    // `[[canvases]]` (#84): a change to a canvas's entry while it streams.
    let streaming_extras: Vec<CanvasId> = state
        .setup
        .as_ref()
        .map(|setup| setup.extra_canvas_ids())
        .unwrap_or_default()
        .into_iter()
        .filter(|canvas| !canvas.is_vertical() && state.streaming_on(*canvas))
        .collect();
    if !streaming_extras.is_empty()
        && changes
            .iter()
            .any(|change| change.path == "canvases" || change.path.starts_with("canvases["))
    {
        let names: Vec<String> = streaming_extras.iter().map(|c| c.to_string()).collect();
        notes.push(format!(
            "canvas streams are running ({}): each keeps its current stream settings until it \
             stops; the changes take effect at its next start (`isocast stream stop --canvas \
             ID`, then `isocast stream start --canvas ID`)",
            names.join(", ")
        ));
    }
    if state.streaming_on(CanvasId::Main)
        && changes
            .iter()
            .any(|change| change.path == "stream" || change.path.starts_with("stream."))
    {
        notes.push(
            "a stream is running: it keeps its current [stream] settings until it stops; the \
             changes take effect at the next start (`isocast stream stop`, then `isocast stream \
             start`, or MCP's stop_stream and start_stream)"
                .into(),
        );
    }
    if state.replay.active_on(CanvasId::Main)
        && changes.iter().any(|change| {
            change.path.starts_with("recording.replay")
                || change.path == "stream"
                || change.path.starts_with("stream.")
        })
    {
        notes.push(
            "the replay buffer is running: it keeps its current format and length until it \
             stops; the changes take effect at its next start (`isocast replay stop`, then \
             `isocast replay start`)"
                .into(),
        );
    }
    if state.replay.active_on(CanvasId::Vertical)
        && changes.iter().any(|change| {
            change.path.starts_with("recording.replay")
                || change.path == "vertical.stream"
                || change.path.starts_with("vertical.stream.")
        })
    {
        notes.push(
            "the vertical replay buffer is running: it keeps its current format and length \
             until it stops; the changes take effect at its next start (`isocast replay stop \
             --canvas vertical`, then `isocast replay start --canvas vertical`)"
                .into(),
        );
    }
    // A stream key pasted into stream.server is refused (a problem says
    // so), and the plan mustn't show it either.
    let pasted: Vec<String> = setup
        .as_ref()
        .and_then(|setup| setup.stream.as_ref())
        .map(|stream| {
            stream
                .resolve()
                .iter()
                .filter_map(|d| d.settings.pasted_key())
                .collect()
        })
        .unwrap_or_default();
    for key in pasted {
        for change in &mut changes {
            for value in [&mut change.from, &mut change.to].into_iter().flatten() {
                redact_value(value, &key);
            }
        }
        if let Some(stream) = setup.as_mut().and_then(|setup| setup.stream.as_mut()) {
            let servers = std::iter::once(&mut stream.server)
                .chain(stream.destinations.iter_mut().map(|d| &mut d.server));
            for server in servers.flatten() {
                *server = isocast_engine::stream::redact(server, &key);
            }
        }
    }
    Ok(Plan {
        revision: state.revision,
        changes,
        problems,
        notes,
        setup,
    })
}

/// `value` with every string's `key` replaced by a placeholder.
fn redact_value(value: &mut serde_json::Value, key: &str) {
    match value {
        serde_json::Value::String(text) => *text = isocast_engine::stream::redact(text, key),
        serde_json::Value::Array(items) => items.iter_mut().for_each(|v| redact_value(v, key)),
        serde_json::Value::Object(fields) => fields.values_mut().for_each(|v| redact_value(v, key)),
        _ => {}
    }
}

/// The host clock, which markers and scene switches are stamped with.
fn now() -> Time {
    native().now()
}

/// The Keychain service this build keeps stream keys under:
/// `<app bundle id>.stream` (`dev.isocast.release.stream` for Isocast.app),
/// so each build owns its own items. One shared service let a key saved by
/// the dev build block the release build, which may neither read nor
/// replace it (#63).
fn key_service() -> &'static str {
    static SERVICE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SERVICE.get_or_init(|| match app_identifier() {
        Some(id) => format!("{id}.stream"),
        None => "dev.isocast.unbundled.stream".to_string(),
    })
}

fn app_identifier() -> Option<String> {
    native().app_identifier()
}

/// The stream key for `account`: this build's own, or one an older Isocast
/// kept under the shared legacy service, moved here once if this build may
/// read it without asking.
///
/// `_held` is the state, borrowed from a guard the caller holds: resolving
/// (and migrating, which writes a key) happens only under the state lock,
/// as destination changes, their key clearing and key provisioning do, so
/// a key resolved for a destination is that destination's as checked, and
/// a migration can't bring back a key a destination change just cleared
/// (decision 26). Every resolver takes it: stream start
/// (`destination_key`), the speed test (and `optimize` through it), and
/// stream statuses (`has_stream_key`).
fn stream_key(_held: &State, account: &str) -> Result<Option<String>> {
    match secret(key_service(), account)? {
        Some(key) => Ok(Some(key)),
        None => Ok(migrate_legacy_key(account)),
    }
}

/// Whether a key for `account` is stored for this build (moving a readable
/// legacy one here first); under the state lock, as [`stream_key`].
fn has_stream_key(_held: &State, account: &str) -> Result<bool> {
    Ok(has_secret(key_service(), account)? || migrate_legacy_key(account).is_some())
}

/// Accounts whose legacy item this build can't read: ignored, not asked
/// about again until the daemon restarts.
static LEGACY_IGNORED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Move the legacy item for `account` to this build's service when it can
/// be read without a prompt, and remove it, so a key cleared later doesn't
/// come back. One another build saved is ignored: the user saves the key
/// again, under this build's own service.
fn migrate_legacy_key(account: &str) -> Option<String> {
    let mut ignored = LEGACY_IGNORED.lock();
    if ignored.iter().any(|a| a == account)
        || key_service() == LEGACY_KEY_SERVICE
        || !has_secret(LEGACY_KEY_SERVICE, account).unwrap_or(false)
    {
        return None;
    }
    let key = match secret(LEGACY_KEY_SERVICE, account) {
        Ok(Some(key)) => key,
        Ok(None) => return None,
        Err(err) => {
            isocast_diagnostics::diagnostic!(
                "warn";
                "isocastd: ignoring the stream key for {account} under the old Keychain service \
                 {LEGACY_KEY_SERVICE}: {err:#}"
            );
            ignored.push(account.to_string());
            return None;
        }
    };
    if let Err(err) = set_secret(key_service(), account, &key) {
        isocast_diagnostics::diagnostic!(
            "warn";
            "isocastd: could not move the stream key for {account} to the Keychain service {}: \
             {err:#}",
            key_service()
        );
        // Still usable this time; moving is tried again next time.
        return Some(key);
    }
    isocast_diagnostics::diagnostic!(
        "isocastd: moved the stream key for {account} from the Keychain service \
         {LEGACY_KEY_SERVICE} to {}",
        key_service()
    );
    drop(ignored);
    retire_legacy_key(account);
    Some(key)
}

/// Remove the legacy item for `account` if this build may (it saved it);
/// one another build saved stays, ignored.
fn retire_legacy_key(account: &str) {
    if key_service() == LEGACY_KEY_SERVICE {
        return;
    }
    if let Err(err) = delete_secret(LEGACY_KEY_SERVICE, account) {
        isocast_diagnostics::diagnostic!(
            "warn";
            "isocastd: left the old Keychain item {LEGACY_KEY_SERVICE} for {account} alone: \
             {err:#}"
        );
        let mut ignored = LEGACY_IGNORED.lock();
        if !ignored.iter().any(|a| a == account) {
            ignored.push(account.to_string());
        }
    }
}

fn has_secret(service: &str, account: &str) -> Result<bool> {
    native().has_secret(service, account)
}

fn secret(service: &str, account: &str) -> Result<Option<String>> {
    let key = native().secret(service, account)?;
    if let Some(key) = &key {
        isocast_diagnostics::redact::remember_secret(service, account, key);
    }
    Ok(key)
}

fn set_secret(service: &str, account: &str, value: &str) -> Result<()> {
    native().set_secret(service, account, value)
}

fn delete_secret(service: &str, account: &str) -> Result<bool> {
    native().delete_secret(service, account)
}

fn permissions() -> Result<PermissionsReport> {
    native().permissions()
}

fn request_permissions(only: Option<PermissionKind>) -> Result<PermissionsReport> {
    native().request_permissions(only)
}

fn open_permission_settings(permission: PermissionKind) -> Result<String> {
    native().open_permission_settings(permission)
}

/// A take's drop counts when it began, and whether its overload was said.
#[derive(Default)]
struct OverloadWatch {
    take: u32,
    /// (source, frames, dropped frames) as the take began.
    baseline: Vec<(String, u64, u64)>,
    /// The stream's (dropped, sent) as the take began, and at the last
    /// reading.
    stream_baseline: Option<(u64, u64)>,
    stream_now: (u64, u64),
    sent: bool,
}

/// Whether `dropped` of `due` pictures is an overload rather than a hiccup:
/// at least a second's worth at 30 fps and 2% of them.
fn overloaded(dropped: u64, due: u64) -> bool {
    dropped >= 30 && dropped * 50 >= due
}

/// This machine's chip and encoders.
fn this_machine() -> Option<&'static isocast_protocol::api::Machine> {
    static MACHINE: std::sync::OnceLock<isocast_protocol::api::Machine> =
        std::sync::OnceLock::new();
    Some(MACHINE.get_or_init(|| native().machine()))
}

/// Backend observations from the actual take, not editable next-take settings.
fn prores_backends(state: &State) -> Vec<(isocast_protocol::Codec, Option<bool>)> {
    state
        .session
        .as_ref()
        .and_then(|s| s.live.lock().clone())
        .map(|live| {
            live.files
                .into_iter()
                .filter_map(|f| {
                    f.codec
                        .filter(|c| {
                            !matches!(
                                c,
                                isocast_protocol::Codec::H264 | isocast_protocol::Codec::Hevc
                            )
                        })
                        .map(|c| (c, f.hardware_accelerated))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Estimate `setup`'s capacity: sources at their live formats in `engine`
/// (if it runs this setup), else probed.
fn estimate_capacity(
    setup: &Setup,
    engine: Option<&dyn LiveEngine>,
    streaming: [bool; 2],
    recording: bool,
    memory: &Path,
    add_iso: Option<&str>,
    backends: &[(isocast_protocol::Codec, Option<bool>)],
) -> Result<CapacityEstimate, String> {
    // A software graphics adapter gets its own advisory estimate, never the
    // hardware tables (#128).
    let software = if cfg!(windows) {
        isocast_engine::estimate::software_adapter(native())?
    } else {
        false
    };
    if !cfg!(target_os = "macos") && !software {
        // The table is measured for Apple's media engines, not a CPU-named
        // Windows GPU. Native encoder budgeting has a separate qualification.
        return Err("capacity estimates are only supported on macOS so far".into());
    }
    let Some(machine) = this_machine() else {
        return Err("capacity estimates are only supported on macOS so far".into());
    };
    let live: HashMap<String, (u32, u32, u32)> = engine
        .map(|engine| {
            engine
                .info()
                .sources
                .into_iter()
                .filter_map(|s| Some((s.source, (s.width?, s.height?, s.fps?))))
                .collect()
        })
        .unwrap_or_default();
    let probed = isocast_engine::estimate::probed_formats(native(), setup, add_iso, &|id| {
        live.contains_key(id)
    });
    let formats = |id: &str| live.get(id).or_else(|| probed.get(id)).copied();
    if software {
        return isocast_engine::estimate::software(
            &isocast_engine::estimate::Inputs {
                machine,
                setup,
                formats: &formats,
                streaming,
                memory: None,
                recording,
                profile: None,
            },
            add_iso,
        );
    }
    let kept = capacity::Memory::load(memory);
    let mut profile = capacity::load_profile(
        &capacity::profile_path(memory),
        machine,
        &daemon_build().build,
    );
    if let Some(profile) = &mut profile {
        // A running native fallback must not inherit earlier hardware costs.
        // Unknown/different selection invalidates only that codec's resource
        // observations here; the saved profile is never rewritten in a take.
        // During startup the first live-file snapshot may not exist yet:
        // an empty observation list is unknown, not proof that HW was chosen.
        profile.encoders.retain(|m| {
            m.evidence != isocast_protocol::machine::Evidence::ProresResource
                || (!recording || backends.iter().any(|(codec, _)| *codec == m.codec))
                    && backends.iter().filter(|(codec, _)| *codec == m.codec).all(
                        |(_, hardware)| hardware.is_some() && *hardware == m.hardware_accelerated,
                    )
        });
    }
    let key = machine_key();
    isocast_engine::estimate::estimate(
        &isocast_engine::estimate::Inputs {
            machine,
            setup,
            formats: &formats,
            streaming,
            memory: key.map(|key| (&kept, key)),
            recording,
            profile: profile.as_ref(),
        },
        add_iso,
    )
}

/// Why no frame of a Studio Mode preview `target` comes: automatic quality
/// paused previews, on a software graphics adapter while it records or
/// streams (#128), or on any adapter until the program keeps up after
/// missing frames while an output runs. Only a preview target waits on
/// that; the canvas's program is still drawn for its outputs.
fn paused_preview(target: &str) -> Option<String> {
    let (canvas, true) = CanvasId::from_target(target)? else {
        return None;
    };
    let status = isocast_engine::governor::status();
    let why = status.previews_paused_why()?;
    // "previews paused <why>" as the governor's reason says it (clients
    // and e2e match that phrase).
    let (paused, resumes) = if why.starts_with("to protect ") {
        (
            format!("previews paused {why} on this software graphics adapter"),
            "when it stops",
        )
    } else {
        (
            format!(
                "previews paused {why} (it missed frames while a recording, stream or replay \
                 buffer runs)"
            ),
            "a step per 30 s once the program keeps up",
        )
    };
    Some(format!(
        "no frame of {target}: automatic quality has {paused}; they come back {resumes}. Ask \
         for `{}` (drawn while it records, streams or keeps a replay), or pin preview.fps in \
         automatic quality to keep previews running",
        canvas.program_target()
    ))
}

/// Whether this process's GPU is a software adapter (WARP, wgpu
/// `DeviceType::Cpu`). Windows only (#128): the shared GPU opens here if no
/// preview opened it yet, on the engine's own thread class.
fn software_adapter() -> bool {
    cfg!(windows) && isocast_engine::estimate::software_adapter(native()).unwrap_or(false)
}

/// Which machine and OS `tune`'s measurements here hold for.
fn machine_key() -> Option<&'static str> {
    static KEY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    Some(KEY.get_or_init(|| capacity::machine_key(&native().machine())))
}

fn tune(setup: &Setup, folder: Option<&Path>, memory: &Path) -> Result<Tuning> {
    isocast_engine::tune::tune(native(), setup, folder, Some(memory))
}

/// Refuse a scene id the setup (the recording's, while recording) lacks.
fn check_scene(state: &State, scene: &str) -> Result<(), RpcError> {
    let scenes = state.scene_ids();
    if scenes.iter().any(|s| s == scene) {
        return Ok(());
    }
    Err(RpcError {
        code: RpcError::FAILED,
        message: if scenes.is_empty() {
            "the setup has no scenes".into()
        } else {
            format!("no scene {scene:?}; scenes: {}", scenes.join(", "))
        },
    })
}

/// While the live engine runs, refuse cameras among `ids` that it doesn't
/// run: opening one again could change its format under the engine.
fn check_cameras(
    setup: &Setup,
    ids: &[String],
    recording: Option<&SessionControl>,
) -> Result<(), RpcError> {
    let Some(control) = recording else {
        return Ok(());
    };
    let captured = control.captured();
    for id in ids {
        let camera = setup
            .source(id)
            .is_some_and(|source| matches!(source.kind, SourceKind::Camera { .. }));
        if camera && !captured.contains(id) {
            return Err(RpcError {
                code: RpcError::WRONG_STATE,
                message: format!(
                    "while the live engine runs (for a recording, a stream or frames), cameras \
                     can only be seen through it, and it doesn't run {id:?}; give one of its \
                     sources: {}",
                    captured.join(", ")
                ),
            });
        }
    }
    Ok(())
}

fn ids_or_none<'a>(ids: impl Iterator<Item = &'a String>) -> String {
    let ids: Vec<&str> = ids.map(String::as_str).collect();
    if ids.is_empty() {
        "none".into()
    } else {
        ids.join(", ")
    }
}

type Graphics = isocast_engine::snapshot::Graphics<NativePlatform>;

/// The GPU, set up on first use and kept until its device is lost.
fn with_graphics<T>(draw: impl FnOnce(&mut Graphics) -> Result<T>) -> Result<T> {
    static GRAPHICS: Mutex<Option<Graphics>> = Mutex::new(None);
    let mut cached = GRAPHICS.lock();
    if cached.as_ref().is_some_and(Graphics::is_lost) {
        *cached = None;
    }
    let graphics = match &mut *cached {
        Some(graphics) => graphics,
        none => none.insert(Graphics::new(native())?),
    };
    let result = draw(graphics);
    // Lost while drawing: this answer may fail, the next opens a new one.
    if graphics.is_lost() {
        *cached = None;
    }
    result
}

/// Frames of the setup's sources `ids`: the newest the recording captured
/// where it captures them, else taken by opening them briefly.
fn frames(
    setup: &Setup,
    ids: &[String],
    recording: Option<&SessionControl>,
    data: &Arc<DataStore>,
) -> Result<Vec<(String, platform::Frame)>> {
    let mut frames = Vec::new();
    let mut missing = Vec::new();
    for id in ids {
        match recording.and_then(|control| control.newest_frame::<platform::Frame>(id)) {
            Some(frame) => {
                let frame =
                    NativePlatform::retain_frame(&frame, isocast_engine::FrameUse::Auxiliary)
                        .context("snapshot: auxiliary frame budget exhausted")?;
                frames.push((id.clone(), frame));
            }
            None => missing.push(id.clone()),
        }
    }
    frames.extend(isocast_engine::snapshot::take_frames(
        native(),
        setup,
        &missing,
        data,
    )?);
    Ok(frames)
}

/// One frame of the setup's source `id` as a PNG.
fn source_png(
    setup: &Setup,
    id: &str,
    recording: Option<&SessionControl>,
    width: Option<u32>,
    data: &Arc<DataStore>,
) -> Result<(Vec<u8>, u32, u32)> {
    let mut frames = frames(setup, &[id.to_string()], recording, data)?;
    let (_, frame) = frames.pop().context("no frame")?;
    let filters = setup.source(id).map_or(&[][..], |s| &s.filters[..]);
    with_graphics(|graphics| graphics.png(id, &frame, filters, width))
}

/// The setup's scene `id`, drawn from frames of its visible `sources` (and
/// its content, with `data`'s values), as a PNG.
fn scene_png(
    setup: &Setup,
    id: &str,
    sources: &[String],
    recording: Option<&SessionControl>,
    width: Option<u32>,
    data: &Arc<DataStore>,
) -> Result<(Vec<u8>, u32, u32)> {
    let scene = setup
        .scenes
        .iter()
        .find(|s| s.id == id)
        .context("no scene")?;
    let frames = frames(setup, sources, recording, data)?;
    with_graphics(|graphics| graphics.scene_png(setup, scene, &frames, width, data))
}

/// Free space on the disk holding `folder` (or the nearest folder above
/// it that exists).
fn free_bytes(folder: &Path) -> Option<u64> {
    let existing = folder.ancestors().find(|path| path.exists())?;
    native()
        .volume(existing)
        .ok()
        .map(|volume| volume.free_bytes)
}

fn doctor(
    setup: Option<&Setup>,
    folder: Option<&Path>,
    may_write: &dyn Fn(&Path) -> bool,
) -> isocast_protocol::api::Doctor {
    isocast_engine::doctor::doctor_writing(native(), setup, folder, may_write)
}

fn devices() -> Result<isocast_protocol::devices::Devices> {
    native().devices()
}

/// Read an OBS scene collection, matching its devices and sizes against
/// this Mac.
fn import_obs(params: ImportObsParams) -> Result<Imported, RpcError> {
    let invalid = |message: &str| RpcError {
        code: RpcError::FAILED,
        message: message.into(),
    };
    let canvas = match (params.width, params.height) {
        (Some(width), Some(height)) if width > 0 && height > 0 => Some((width, height)),
        (None, None) => None,
        _ => {
            return Err(invalid(
                "give the canvas width and height together, above 0",
            ));
        }
    };
    if params.fps.is_some_and(|fps| !(1..=240).contains(&fps)) {
        return Err(invalid("fps must be between 1 and 240"));
    }
    let devices = devices();
    let home = library::home_text();
    let options = obs::Options {
        canvas,
        fps: params.fps,
        profile: None,
        devices: devices.as_ref().ok(),
        content_size: &content_size,
        home: home.as_deref(),
    };
    #[cfg(target_os = "macos")]
    SLOW_CONTENT.store(0, std::sync::atomic::Ordering::SeqCst);
    let mut imported = obs::import_file(&params.collection, params.profile.as_deref(), options)
        .map_err(|message| RpcError {
            code: RpcError::FAILED,
            message,
        })?;
    #[cfg(target_os = "macos")]
    match SLOW_CONTENT.load(std::sync::atomic::Ordering::SeqCst) {
        0 => {}
        n => imported.notes.push(format!(
            "{n} file(s) took too long to read, so their sizes were guessed; macOS may be asking \
             for permission to read their folder (like Downloads): allow it and import again"
        )),
    }
    if let Err(err) = devices {
        imported.notes.push(format!(
            "devices couldn't be listed ({err:#}), so device names and sizes weren't checked"
        ));
    }
    Ok(imported)
}

/// How long reading one file's picture size may take during an import.
#[cfg(target_os = "macos")]
const CONTENT_SIZE_TIMEOUT: Duration = Duration::from_secs(3);

/// Files whose size took too long to read in the current import.
#[cfg(target_os = "macos")]
static SLOW_CONTENT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The picture size of an image, slide show, text or media source. Reading
/// a file in a protected folder (Downloads, Documents, Desktop) waits for
/// macOS to ask for permission, so it gives up after a few seconds rather
/// than hang the import.
#[cfg(target_os = "macos")]
fn content_size(kind: &SourceKind) -> Option<(u32, u32)> {
    let (tx, rx) = std::sync::mpsc::channel();
    let kind = kind.clone();
    thread::spawn(move || {
        let _ = tx.send(isocast_engine::content_size(native(), &kind));
    });
    match rx.recv_timeout(CONTENT_SIZE_TIMEOUT) {
        Ok(size) => size,
        Err(_) => {
            SLOW_CONTENT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            None
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn content_size(_kind: &SourceKind) -> Option<(u32, u32)> {
    None
}

fn failed(err: anyhow::Error) -> RpcError {
    RpcError {
        code: RpcError::FAILED,
        message: format!("{err:#}"),
    }
}

/// A data call the store refused (a bad key, a value over a limit),
/// naming the param: `params.values["Title"]: ...`.
fn refused(err: isocast_engine::data::Refused) -> RpcError {
    RpcError {
        code: RpcError::INVALID_PARAMS,
        message: format!("params.{err}"),
    }
}

fn to_json(value: impl serde::Serialize) -> Result<serde_json::Value, RpcError> {
    serde_json::to_value(value).map_err(|err| failed(err.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// A live engine whose `stop` holds its native teardown open until the
    /// test lets it finish, as a GPU encoder's destruction can.
    struct SlowStop {
        entered: mpsc::SyncSender<()>,
        release: Mutex<mpsc::Receiver<()>>,
        finished: AtomicBool,
    }

    impl LiveEngine for SlowStop {
        fn monitor_h264_constrained_high(&self) -> bool {
            unreachable!()
        }
        fn update(&self, _setup: &Setup, _revision: u64) -> Result<(), String> {
            unreachable!()
        }
        fn info(&self) -> EngineInfo {
            unreachable!()
        }
        fn program_failure(&self) -> Option<String> {
            unreachable!()
        }
        fn canvas_stopped(&self, _canvas: CanvasId) -> Option<String> {
            unreachable!()
        }
        fn control(&self) -> Arc<SessionControl> {
            unreachable!()
        }
        fn set_preview(&self, _scene: Option<String>) {
            unreachable!()
        }
        fn frame_jpeg(
            &self,
            _target: &str,
            _after: Option<u64>,
            _timeout: Duration,
            _max_width: Option<u32>,
            _quality: f32,
            _fps: Option<u32>,
        ) -> Result<Option<JpegFrame>, String> {
            unreachable!()
        }
        fn frames(
            &self,
            _target: &str,
            _max_width: Option<u32>,
            _fps: u32,
            _profile: H264Profile,
        ) -> Result<FrameFeed, String> {
            unreachable!()
        }
        fn opus(&self) -> Result<isocast_engine::record::OpusFeed, String> {
            unreachable!()
        }
        fn shared_frames(
            &self,
            _target: &str,
            _max_size: (Option<u32>, Option<u32>),
            _fps: u32,
            _slots: u32,
            _filtered: bool,
        ) -> Result<SharedFeed<Surface>, String> {
            unreachable!()
        }
        fn record(&self, _options: &RecordOptions) -> Result<Report> {
            unreachable!()
        }
        fn resolved(&self) -> Option<Resolved> {
            unreachable!()
        }
        fn canvas_refusal(&self, _canvas: &Canvas) -> Option<String> {
            unreachable!()
        }
        fn stream(
            &self,
            _id: &str,
            _settings: StreamSettings,
            _key: String,
            _on_change: OnChange,
        ) -> Result<Stream> {
            unreachable!()
        }
        fn stream_canvas(
            &self,
            _canvas: CanvasId,
            _id: &str,
            _settings: StreamSettings,
            _key: String,
            _on_change: OnChange,
        ) -> Result<Stream> {
            unreachable!()
        }
        fn switch_canvas_scene(
            &self,
            _canvas: CanvasId,
            _at: Time,
            _scene: String,
            _transition: Transition,
        ) -> Result<Option<(u32, f64)>, String> {
            unreachable!()
        }
        fn canvas_scene(&self, _canvas: CanvasId) -> Option<String> {
            unreachable!()
        }
        fn set_canvas_preview(
            &self,
            _canvas: CanvasId,
            _scene: Option<String>,
        ) -> Result<(), String> {
            unreachable!()
        }
        fn resolved_canvas(&self, _canvas: CanvasId) -> Option<Resolved> {
            unreachable!()
        }
        fn setup_refusal(&self, _setup: &Setup) -> Option<Issue> {
            unreachable!()
        }
        fn virtual_camera(
            &self,
            _on_change: isocast_engine::record::OnCameraChange,
        ) -> Result<isocast_engine::record::VirtualCamera> {
            unreachable!()
        }
        fn replay(
            &self,
            _canvas: CanvasId,
            _settings: &isocast_protocol::replay::ReplaySettings,
            _stream: Option<&StreamSettings>,
            _on_change: isocast_engine::replay::OnChange,
        ) -> Result<isocast_engine::replay::ReplayBuffer> {
            unreachable!()
        }
        fn stop(&self) {
            let _ = self.entered.send(());
            // A failed assertion drops the sender: finish rather than hang.
            let _ = self.release.lock().recv();
            self.finished.store(true, Ordering::SeqCst);
        }
    }

    /// The idle stop takes the engine out of `state` before its native
    /// workers finish. Process shutdown must not get past its start (and on
    /// to exit, where driver finalizers run) until that stop has returned.
    #[test]
    fn shutdown_waits_for_an_idle_engine_teardown_in_progress() {
        let daemon = Arc::new(Daemon::default());
        let (entered, stopping) = mpsc::sync_channel(1);
        let (release, released) = mpsc::sync_channel(1);
        let engine = Arc::new(SlowStop {
            entered,
            release: Mutex::new(released),
            finished: AtomicBool::new(false),
        });
        daemon.state.lock().engine = Some(engine.clone());

        // Nothing records, holds or asked for frames: the engine is idle.
        let idle = thread::spawn({
            let daemon = daemon.clone();
            move || daemon.stop_idle_engine_now()
        });
        stopping
            .recv_timeout(Duration::from_secs(10))
            .expect("the idle check stops an unused engine");
        assert!(
            daemon.state.read().engine.is_none(),
            "the engine has left state while its stop is still running"
        );
        assert!(
            daemon.shutdown_lock.try_lock().is_none(),
            "idle teardown must exclude process shutdown until stop returns"
        );

        let (began, shutdown_began) = mpsc::channel();
        let shutdown = thread::spawn({
            let daemon = daemon.clone();
            let engine = engine.clone();
            move || {
                let _shutdown = daemon.begin_shutdown();
                let _ = began.send(engine.finished.load(Ordering::SeqCst));
            }
        });
        // Wait for actual shutdown admission, not an assumed scheduling
        // delay. The timeout is only a watchdog for a broken test.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !daemon.shutting_down.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "shutdown never started");
            thread::yield_now();
        }
        assert_eq!(shutdown_began.try_recv(), Err(mpsc::TryRecvError::Empty));
        release.send(()).unwrap();
        assert_eq!(
            shutdown_began.recv_timeout(Duration::from_secs(10)),
            Ok(true),
            "shutdown goes ahead once the engine's stop has finished"
        );
        idle.join().unwrap();
        shutdown.join().unwrap();
        assert!(daemon.shutting_down.load(Ordering::SeqCst));
    }
}
