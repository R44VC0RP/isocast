//! isocastd: the headless Isocast engine.
//!
//! It owns the recording session and answers JSON-RPC 2.0 requests from the
//! CLI, MCP and the app on a local socket, one message per line (see
//! `isocast_protocol::api`). `isocast start` launches it when it isn't
//! running.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use isocast_engine::Platform;
use isocast_engine::Time;
use isocast_engine::capacity;
use isocast_engine::data::DataStore;
use isocast_engine::record::{
    DEFAULT_SLOTS, Engine as LiveEngineOf, EngineEvent, FrameFeed, JpegFrame, OnEngineEvent,
    RecordOptions, SessionControl, SharedEvent, SharedFeed, SplitTake, SplitTiming, expand_home,
    interrupted_note, interrupted_sessions,
};
use isocast_engine::stream::{KEY_ENV, KEY_SERVICE, OnChange, Stream};
use isocast_protocol::api::InitParams;
use isocast_protocol::api::{
    Applied, ApplyParams, EngineInfo, EngineState, Event, EventKind, EventKinds, Frame,
    FrameParams, FrameRate, FrameStream, FramesParams, GetParams, ImportObsParams, Live,
    LiveStatus, Marked, Method, Notification, Plan, Refreshed, Report, Request, Resolved, Response,
    RpcError, SharedFrameStream, SharedFramesParams, Snapshot, SnapshotParams, StartParams,
    Started, Status, StreamKey, StreamKeyParams, StreamKeyTarget, Switched, TuneParams, Tuning,
    WatchParams, lock_path, shared, socket_path,
};
use isocast_protocol::change::{Change, diff, merge_patch, missing_removals};
use isocast_protocol::devices::{PermissionKind, PermissionsReport};
use isocast_protocol::init::{self, Initialized};
use isocast_protocol::manifest::SessionManifest;
use isocast_protocol::obs::{self, Imported};
use isocast_protocol::stream::{
    STREAM_SERVICES, StreamService, StreamSettings, StreamState, StreamStatus, key_account,
};
use isocast_protocol::{Canvas, Issue, PROGRAM, Setup, SourceKind, Transition};
use isocast_surfaces::{Exporter, Surface, Token};

/// How long `start` waits for devices to open and files to be created.
const START_TIMEOUT: Duration = Duration::from_secs(30);
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

#[cfg(target_os = "macos")]
use isocast_platform_macos::MacPlatform;

mod data;
mod export;
mod library;
mod plugins;
mod store;
mod timeline;

fn main() -> ExitCode {
    let socket = socket_path();
    match hold_instance_lock().and_then(|lock| Ok((lock, listen(&socket)?))) {
        // The lock is held until the process exits.
        Ok((_lock, listener)) => {
            eprintln!("isocastd listening on {}", socket.display());
            let daemon = Arc::new(Daemon::default());
            daemon.load_setup();
            daemon.load_data();
            daemon.load_timelines();
            start_plugins(&daemon);
            let saver = daemon.clone();
            thread::spawn(move || saver.save_live_changes());
            // On Ctrl-C, SIGTERM or SIGHUP, finish the files before exiting.
            let on_signal = daemon.clone();
            let handler = ctrlc::set_handler(move || {
                let _ = on_signal.stop();
                on_signal.stop_engine();
                on_signal.flush_live_save();
                on_signal.data.flush();
                on_signal.plugins.stop_all();
                let _ = std::fs::remove_file(socket_path());
                std::process::exit(0);
            });
            let idle = daemon.clone();
            thread::spawn(move || idle.stop_idle_engine());
            if let Err(err) = handler {
                eprintln!("isocastd: could not handle signals: {err}");
            }
            #[cfg(target_os = "macos")]
            isocast_platform_macos::will_run_main_queue();
            thread::spawn(move || serve(listener, daemon));
            run_main_queue()
        }
        Err(err) => {
            eprintln!("isocastd: {err:#}");
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
    UnixListener::bind(socket).with_context(|| format!("could not listen on {}", socket.display()))
}

/// Keep the main thread serving the main queue, which macOS needs for
/// AppKit calls (display names). Never returns.
#[cfg(target_os = "macos")]
fn run_main_queue() -> ExitCode {
    isocast_platform_macos::run_main_queue()
}

#[cfg(not(target_os = "macos"))]
fn run_main_queue() -> ExitCode {
    loop {
        thread::park();
    }
}

fn serve(listener: UnixListener, daemon: Arc<Daemon>) {
    for connection in listener.incoming().flatten() {
        let daemon = daemon.clone();
        // A plugin's process (unsandboxed, so it could reach this socket)
        // is held to its grant here too.
        let scope = daemon.plugins.scope_of_peer(&connection);
        thread::spawn(move || {
            let _ = handle(connection, &daemon, scope);
        });
    }
}

/// Load the installed plugins and start their processes; each plugin's
/// connections are served like any client's, but held to its grant.
fn start_plugins(daemon: &Arc<Daemon>) {
    let publish = Arc::downgrade(daemon);
    let serve = Arc::downgrade(daemon);
    daemon.plugins.start(plugins::Hooks {
        publish: Box::new(move |event| {
            if let Some(daemon) = publish.upgrade() {
                daemon.publish(event);
            }
        }),
        serve: Box::new(move |connection, scope| {
            if let Some(daemon) = serve.upgrade() {
                thread::spawn(move || {
                    let _ = handle(connection, &daemon, Some(scope));
                });
            }
        }),
    });
}

/// Answer requests on one connection until the client hangs up. A
/// plugin's connection (`scope`) may only call what it was granted.
fn handle(
    connection: UnixStream,
    daemon: &Arc<Daemon>,
    scope: Option<Arc<plugins::Scope>>,
) -> std::io::Result<()> {
    let _tracked = scope.as_ref().and_then(|scope| scope.track(&connection));
    let mut writer = connection.try_clone()?;
    let mut reader = BufReader::new(connection);
    let mut line = String::new();
    loop {
        line.clear();
        let read = (&mut reader)
            .take(MAX_REQUEST as u64 + 1)
            .read_line(&mut line)?;
        if read == 0 {
            return Ok(());
        }
        if read > MAX_REQUEST {
            // The rest of the line can't be skipped without reading it, so
            // the connection ends here.
            let response = Response::err(
                None,
                RpcError {
                    code: RpcError::INVALID_REQUEST,
                    message: format!(
                        "the request is longer than {} MB; requests are one JSON object per line",
                        MAX_REQUEST >> 20
                    ),
                },
            );
            writeln!(writer, "{}", serde_json::to_string(&response)?)?;
            return Ok(());
        }
        if line.trim().is_empty() {
            continue;
        }
        let request = serde_json::from_str::<Request>(&line);
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
            continue;
        }
        let response = match request {
            Ok(Request {
                id,
                method: Method::Watch(params),
                ..
            }) => {
                let params = params.unwrap_or_default();
                match params.problem() {
                    None => return watch(writer, daemon, id, &params),
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
                method: Method::Frames(params),
                ..
            }) => match daemon.frames(&params) {
                Ok((reply, feed)) => return stream_frames(reader, writer, daemon, id, reply, feed),
                Err(error) => Response::err(Some(id), error),
            },
            Ok(Request {
                id,
                method: Method::SharedFrames(params),
                ..
            }) => match daemon.shared_frames(&params) {
                Ok((reply, feed, exporter)) => {
                    return stream_shared_frames(reader, writer, daemon, id, reply, feed, exporter);
                }
                Err(error) => Response::err(Some(id), error),
            },
            Ok(request) => {
                let shutdown = request.method == Method::Shutdown;
                let response = daemon.call(request);
                if shutdown {
                    daemon
                        .stop_export("isocastd was stopped (restarted or quit) during the export");
                    daemon.stop_engine();
                    daemon.flush_live_save();
                    daemon.data.flush();
                    daemon.plugins.stop_all();
                    writeln!(writer, "{}", serde_json::to_string(&response)?)?;
                    let _ = std::fs::remove_file(socket_path());
                    std::process::exit(0);
                }
                response
            }
            Err(err) => Response::err(
                id_of(&line),
                RpcError {
                    code: request_error_code(&line),
                    message: Request::located_error(&line)
                        .map_or_else(|| err.to_string(), |located| located.to_string()),
                },
            ),
        };
        writeln!(writer, "{}", serde_json::to_string(&response)?)?;
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
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if let Ok(change) = serde_json::from_str::<FrameRate>(&line) {
                    rate.set(change.fps);
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
            daemon.state.lock().unwrap().wanted_at = Some(kept);
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
            for line in reader.lines() {
                let Ok(line) = line else { break };
                match serde_json::from_str::<shared::Control>(&line) {
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
            daemon.state.lock().unwrap().wanted_at = Some(kept);
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
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let key = NEXT.fetch_add(1, Ordering::Relaxed);
    daemon
        .watchers
        .lock()
        .unwrap()
        .push(Watcher { key, wanted, tx });
    // However this ends, the watcher goes (else one asking for rare kinds
    // would stay listed until the next event of its kind).
    struct Leave<'a>(&'a Daemon, u64);
    impl Drop for Leave<'_> {
        fn drop(&mut self) {
            let mut watchers = self.0.watchers.lock().unwrap();
            watchers.retain(|watcher| watcher.key != self.1);
        }
    }
    let _leave = Leave(daemon, key);
    let live = to_json(daemon.get(GetParams::default())).unwrap_or_default();
    writeln!(
        writer,
        "{}",
        serde_json::to_string(&Response::ok(id, live))?
    )?;
    // A setup file that didn't load stays a warning until a change fixes it.
    if wanted.contains(EventKind::Warning)
        && let Some(message) = daemon.state.lock().unwrap().file.load_warning()
    {
        let warning = Notification::event(Event::Warning { message });
        writeln!(writer, "{}", serde_json::to_string(&warning)?)?;
    }
    loop {
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
    use std::os::fd::AsRawFd;
    let mut byte = 0u8;
    // SAFETY: a one-byte peek into a local buffer on an open socket.
    let read = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            (&raw mut byte).cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    read == 0
        || (read < 0
            && !matches!(
                std::io::Error::last_os_error().kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ))
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
    state: Mutex<State>,
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
    /// Timelines (`timeline_*`, `retake`), and which is current.
    timelines: Mutex<timeline::Timelines>,
    /// Held while a `retake` runs: one at a time.
    retaking: Mutex<()>,
    /// The export running, if one is.
    export: Mutex<Option<export::Running>>,
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
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: getrusage fills in the struct it is given, which is zeroed
    // and the right size; on failure it is left alone and not read.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: filled in by getrusage above.
    let usage = unsafe { usage.assume_init() };
    let seconds = |t: libc::timeval| t.tv_sec as f64 + f64::from(t.tv_usec) / 1e6;
    Some(seconds(usage.ru_utime) + seconds(usage.ru_stime))
}

#[derive(Default)]
struct State {
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
    /// A `tune` benchmark is running; it and a recording would compete for
    /// the encoders.
    tuning: bool,
    /// Where the live setup is kept between runs: the current scene
    /// collection and profile.
    file: store::Store,
    /// The stream, running or ended (kept for its final status).
    stream: Option<Stream>,
    /// `stream_start` calls getting the engine ready for a stream that
    /// isn't in `stream` yet; the engine mustn't idle out under them.
    streams_starting: u32,
    /// Stream frames dropped already warned about.
    stream_drops_reported: u64,
    stream_audio_drops_reported: u64,
    stream_bitrate_warned: bool,
}

impl State {
    /// A stream is running (or reconnecting), so the engine is needed.
    fn streaming(&self) -> bool {
        self.stream
            .as_ref()
            .is_some_and(|stream| stream.status().is_active())
    }

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

/// Why a split didn't happen, and whether because the recording is ending
/// (`retake` then waits for it and retakes its last take).
struct SplitFailed {
    ending: bool,
    error: RpcError,
}

struct Session {
    /// Counts up from 1, so `stop` knows when this session has ended.
    id: u64,
    stop: Arc<AtomicBool>,
    files: Arc<Mutex<Option<Vec<PathBuf>>>>,
    live: Arc<Mutex<Option<LiveStatus>>>,
    /// Its takes, for the current timeline.
    takes: Arc<timeline::SessionTakes>,
}

/// The live engine, whatever the platform.
trait LiveEngine: Send + Sync {
    fn update(&self, setup: &Setup, revision: u64) -> Result<(), String>;
    fn info(&self) -> EngineInfo;
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
    /// `target`'s picture as low-latency H.264 for a monitor.
    fn frames(&self, target: &str, max_width: Option<u32>, fps: u32) -> Result<FrameFeed, String>;
    /// `target`'s picture in surfaces shared with a client on this machine.
    fn shared_frames(
        &self,
        target: &str,
        max_size: (Option<u32>, Option<u32>),
        fps: u32,
        slots: u32,
    ) -> Result<SharedFeed<Surface>, String>;
    fn record(&self, options: &RecordOptions) -> Result<Report>;
    /// Where layout put every item, once the scenes are resolved.
    fn resolved(&self) -> Option<Resolved>;
    /// Why the canvas can't change to `canvas` now, if it can't.
    fn canvas_refusal(&self, canvas: &Canvas) -> Option<String>;
    /// Stream the program with `settings` under `key`.
    fn stream(&self, settings: StreamSettings, key: String, on_change: OnChange) -> Result<Stream>;
    fn stop(&self);
}

impl<P: Platform<SharedSurface = Surface>> LiveEngine for LiveEngineOf<P> {
    fn update(&self, setup: &Setup, revision: u64) -> Result<(), String> {
        LiveEngineOf::update(self, setup, revision)
    }

    fn info(&self) -> EngineInfo {
        LiveEngineOf::info(self)
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

    fn frames(&self, target: &str, max_width: Option<u32>, fps: u32) -> Result<FrameFeed, String> {
        LiveEngineOf::frames(self, target, max_width, fps)
    }

    fn shared_frames(
        &self,
        target: &str,
        max_size: (Option<u32>, Option<u32>),
        fps: u32,
        slots: u32,
    ) -> Result<SharedFeed<Surface>, String> {
        LiveEngineOf::shared_frames(self, target, max_size, fps, slots)
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

    fn stream(&self, settings: StreamSettings, key: String, on_change: OnChange) -> Result<Stream> {
        isocast_engine::stream::start(self, settings, key, on_change)
    }

    fn stop(&self) {
        LiveEngineOf::stop(self);
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

#[cfg(target_os = "macos")]
fn start_engine(
    setup: &Setup,
    control: Arc<SessionControl>,
    on_event: OnEngineEvent,
) -> Result<Arc<dyn LiveEngine>> {
    Ok(Arc::new(LiveEngineOf::start(
        &MacPlatform,
        setup,
        Some(control),
        Some(on_event),
    )?))
}

#[cfg(not(target_os = "macos"))]
fn start_engine(
    _setup: &Setup,
    _control: Arc<SessionControl>,
    _on_event: OnEngineEvent,
) -> Result<Arc<dyn LiveEngine>> {
    bail!("the live engine only runs on macOS so far")
}

impl Daemon {
    fn call(self: &Arc<Self>, request: Request) -> Response {
        let id = request.id;
        let shutdown = request.method == Method::Shutdown;
        let result = match request.method {
            Method::Status => to_json(self.status()),
            Method::Devices => devices().map_err(failed).and_then(to_json),
            Method::Get(params) => to_json(self.get(params.unwrap_or_default())),
            Method::Plan(change) => {
                let state = self.state.lock().unwrap();
                plan(&state, *change, &self.plugins).and_then(to_json)
            }
            Method::Apply(params) => self.apply(*params).and_then(to_json),
            Method::Split => self
                .split_take(SplitTiming::WholeSecond)
                .map(|split| Started {
                    begins_at: Some(timeline::split_text(&split)),
                    files: split.files,
                    notes: Vec::new(),
                })
                .map_err(|failed| failed.error)
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
            Method::RequestPermissions => request_permissions().map_err(failed).and_then(to_json),
            Method::OpenPermissionSettings { permission } => open_permission_settings(permission)
                .map(|url| isocast_protocol::api::Opened { url })
                .map_err(failed)
                .and_then(to_json),
            Method::ImportObs(params) => import_obs(*params).and_then(to_json),
            Method::InitSetup(params) => self.init_setup(*params).and_then(to_json),
            Method::Doctor(params) => check_folder(params.folder.as_deref()).and_then(|()| {
                let setup = params
                    .setup
                    .or_else(|| self.state.lock().unwrap().setup.clone());
                let mut report = doctor(setup.as_ref(), params.folder.as_deref());
                report.checks.extend(self.plugins.doctor_check());
                to_json(report)
            }),
            Method::Marker { label } => self.marker(label).and_then(to_json),
            Method::RefreshBrowser { source } => self.refresh_browser(source).and_then(to_json),
            Method::SwitchScene { scene, transition } => {
                self.switch_scene(scene, transition).and_then(to_json)
            }
            Method::Preview { scene } => self.preview(scene).and_then(to_json),
            Method::Take { transition } => self.take(transition).and_then(to_json),
            // Handled by the connection, which turns into an event stream.
            Method::Watch(_) => Err(RpcError {
                code: RpcError::INVALID_REQUEST,
                message: "watch must be the first request on its connection".into(),
            }),
            Method::Start(params) => self.start(*params).and_then(to_json),
            Method::StreamStart => self.stream_start().and_then(to_json),
            Method::StreamStop => self.stream_stop().and_then(to_json),
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
            Method::TimelineNew(params) => self
                .timeline_new(params.unwrap_or_default())
                .and_then(to_json),
            Method::TimelineGet(target) => self
                .timeline_get(target.unwrap_or_default())
                .and_then(to_json),
            Method::TimelineList => to_json(self.timeline_list()),
            Method::TimelineOpen(params) => self.timeline_open(params).and_then(to_json),
            Method::TimelineClose => self.timeline_close().and_then(to_json),
            Method::TimelineRemove(params) => self.timeline_remove(params).and_then(to_json),
            Method::TimelineMove(params) => self.timeline_move(params).and_then(to_json),
            Method::Retake(params) => self.retake(params.unwrap_or_default()).and_then(to_json),
            Method::TimelineExport(params) => self
                .timeline_export(params.unwrap_or_default())
                .and_then(to_json),
            Method::Stop | Method::Shutdown => match self.stop() {
                // Shutting down while idle is fine.
                Err(err) if shutdown && err.code == RpcError::WRONG_STATE => {
                    to_json(serde_json::Value::Null)
                }
                result => result.and_then(to_json),
            },
        };
        match result {
            Ok(value) => Response::ok(id, value),
            Err(error) => Response::err(Some(id), error),
        }
    }

    fn publish(&self, event: Event) {
        let mut watchers = self.watchers.lock().unwrap();
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
                eprintln!("isocastd: could not serialize an event: {error}");
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
        let used = match &self.state.lock().unwrap().setup {
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

    fn get(&self, params: GetParams) -> Live {
        let state = self.state.lock().unwrap();
        let (collection, profile) = state.file.current_names(state.setup.as_ref());
        let mut live = Live {
            revision: state.revision,
            setup: state.setup.clone(),
            resolved: None,
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
            live.resolved = Some(resolved(setup, live.revision, engine, &self.data.store));
        }
        live
    }

    /// Load the setup saved by the last run, at [`first_revision`]. A file
    /// that can't be loaded is left alone and reported (in `get`, `status`
    /// and to watchers), and the daemon starts without a setup.
    fn load_setup(&self) {
        let mut state = self.state.lock().unwrap();
        let loaded = state.file.load();
        for note in state.file.notes() {
            eprintln!("isocastd: {note}");
        }
        let path = state.file.info().path;
        match loaded {
            Some(setup) => {
                eprintln!("isocastd: loaded the setup from {}", path.display());
                state.scene = setup.scenes.first().map(|scene| scene.id.clone());
                state.setup = Some(setup);
                state.revision = first_revision();
            }
            None => match state.file.load_warning() {
                Some(warning) => eprintln!("isocastd: {warning}"),
                None => eprintln!("isocastd: no saved setup yet at {}", path.display()),
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
                    eprintln!("isocastd: {note}");
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
        let state = self.state.lock().unwrap();
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
        let mut state = self.state.lock().unwrap();
        let plan = plan(&state, params.change, &self.plugins)?;
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
        // The live engine runs it from the next frame (or refuses, like a
        // canvas change while recording).
        if let (Some(engine), Some(setup)) = (&state.engine, &plan.setup) {
            engine
                .update(setup, state.revision + 1)
                .map_err(|message| RpcError {
                    code: RpcError::WRONG_STATE,
                    message,
                })?;
        }
        state.setup = plan.setup;
        state.revision += 1;
        let mut notes = plan.notes;
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
        let revision = state.revision;
        drop(state);
        self.publish(Event::SetupChanged { revision });
        if let Some((setup, target)) = to_save {
            if params.live {
                // Saved by the saver thread once the gesture pauses.
                *self.live_save.lock().unwrap() = Some((revision, setup, target));
                self.live_saved.notify_all();
                return Ok(Applied {
                    revision,
                    changes: plan.changes,
                    notes,
                });
            }
            notes.extend(self.save(revision, &setup, &target));
        }
        if let Some(message) = self.state.lock().unwrap().file.info().save_error {
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
        let mut saved = self.saved.lock().unwrap();
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
        self.state.lock().unwrap().file.saved(result)
    }

    /// The saver thread: writes the newest live change once changes pause.
    fn save_live_changes(&self) {
        loop {
            let mut pending = self.live_save.lock().unwrap();
            while pending.is_none() {
                pending = self.live_saved.wait(pending).unwrap();
            }
            let first = std::time::Instant::now();
            loop {
                let (guard, wait) = self.live_saved.wait_timeout(pending, SAVE_QUIET).unwrap();
                pending = guard;
                if wait.timed_out() || first.elapsed() >= SAVE_AT_MOST {
                    break;
                }
            }
            let Some((revision, setup, target)) = pending.take() else {
                continue;
            };
            drop(pending);
            if !self.save(revision, &setup, &target).is_empty()
                && let Some(message) = self.state.lock().unwrap().file.info().save_error
            {
                self.publish(Event::Warning { message });
            }
        }
    }

    /// Save a live change still waiting for the saver (before exiting).
    fn flush_live_save(&self) {
        let pending = self.live_save.lock().unwrap().take();
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
        if !params.apply {
            return Ok(made);
        }
        let revision = self.state.lock().unwrap().revision;
        let mut applied = self.apply(ApplyParams {
            revision,
            change: Change::Setup(Box::new(made.setup.clone())),
            live: false,
        })?;
        if revision > 0 {
            let file = self.state.lock().unwrap().file.info();
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
            let state = self.state.lock().unwrap();
            if state.scene.as_ref() != Some(&first.id) {
                self.go_on_air(state, first.id.clone(), Some(Transition::default()), false)?;
            }
        }
        made.applied = Some(applied);
        Ok(made)
    }

    fn tune(&self, params: TuneParams) -> Result<Tuning, RpcError> {
        check_folder(params.folder.as_deref())?;
        let mut state = self.state.lock().unwrap();
        if state.session.is_some() || state.tuning {
            return Err(RpcError {
                code: RpcError::WRONG_STATE,
                message: "not while recording or tuning: the benchmark would compete for the \
                          encoders"
                    .into(),
            });
        }
        let Some(setup) = params.setup.or_else(|| state.setup.clone()) else {
            return Err(state.no_setup("no setup to tune (or give one)"));
        };
        // Kept beside the setup file, for `start` to check against.
        let memory = capacity::memory_path(state.file.legacy());
        state.tuning = true;
        drop(state);
        // Recording stays blocked until `tuning` clears, so clear it even if
        // the benchmark panics.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tune(&setup, params.folder.as_deref(), &memory)
        }))
        .unwrap_or_else(|_| Err(anyhow::anyhow!("the benchmark crashed; see isocastd.log")))
        .map_err(failed);
        self.state.lock().unwrap().tuning = false;
        result
    }

    fn snapshot(&self, params: SnapshotParams) -> Result<Snapshot, RpcError> {
        let state = self.state.lock().unwrap();
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
                let sources: Vec<String> = found
                    .video_sources(&setup.scenes)
                    .into_iter()
                    .map(String::from)
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

    /// Split the running recording into a new take (at `timing`).
    fn split_take(&self, timing: SplitTiming) -> Result<SplitTake, SplitFailed> {
        let refused = |ending: bool, code: i64, message: &str| SplitFailed {
            ending,
            error: RpcError {
                code,
                message: message.into(),
            },
        };
        let state = self.state.lock().unwrap();
        let (Some(session), Some(control)) = (&state.session, state.control()) else {
            return Err(refused(true, RpcError::WRONG_STATE, "not recording"));
        };
        if session.stop.load(Ordering::SeqCst) {
            return Err(refused(
                true,
                RpcError::WRONG_STATE,
                "the recording is stopping, so no new take can begin",
            ));
        }
        if session.files.lock().unwrap().is_none() {
            return Err(refused(
                false,
                RpcError::WRONG_STATE,
                "the recording is still starting; split once it has started (`isocast status` \
                 says when)",
            ));
        }
        let reply = control.split(timing);
        let current = session.files.clone();
        let takes = session.takes.clone();
        drop(state);
        match reply.recv_timeout(START_TIMEOUT) {
            Ok(Ok(split)) => {
                *current.lock().unwrap() = Some(split.files.clone());
                let begins_at = timeline::split_text(&split);
                *takes.current.lock().unwrap() = timeline::TakeNow {
                    number: split.take,
                    files: split.files.clone(),
                    begins: Some(std::time::Instant::now() + split.begins_in),
                    begins_at: Some(begins_at.clone()),
                };
                self.take_began(&takes, split.take, &split.manifest);
                self.publish(Event::Started {
                    files: split.files.clone(),
                    begins_at: Some(begins_at),
                });
                Ok(split)
            }
            Ok(Err(why)) => Err(refused(
                why.ending,
                match why.ending {
                    true => RpcError::WRONG_STATE,
                    false => RpcError::FAILED,
                },
                &why.message,
            )),
            // Every split is answered, even once the recording has ended;
            // this is only isocastd's engine having gone.
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(refused(
                true,
                RpcError::WRONG_STATE,
                "the recording stopped before it could split",
            )),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(refused(
                false,
                RpcError::FAILED,
                "the recording did not split in time",
            )),
        }
    }

    fn refresh_browser(&self, source: String) -> Result<Refreshed, RpcError> {
        let state = self.state.lock().unwrap();
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
        let state = self.state.lock().unwrap();
        let (Some(_), Some(control)) = (&state.session, state.control()) else {
            return Err(RpcError {
                code: RpcError::WRONG_STATE,
                message: "markers need a recording in progress".into(),
            });
        };
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
    ) -> Result<Switched, RpcError> {
        let state = self.state.lock().unwrap();
        check_scene(&state, &scene)?;
        self.go_on_air(state, scene, transition, false)
    }

    /// Stage `scene` in preview, for `take`.
    fn preview(&self, scene: String) -> Result<Switched, RpcError> {
        let mut state = self.state.lock().unwrap();
        check_scene(&state, &scene)?;
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
        })
    }

    /// Put the preview scene on air; the scene that was on air moves to
    /// preview.
    fn take(&self, transition: Option<Transition>) -> Result<Switched, RpcError> {
        let state = self.state.lock().unwrap();
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
        mut state: std::sync::MutexGuard<'_, State>,
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
        })
    }

    fn status(&self) -> Status {
        let state = self.state.lock().unwrap();
        let (engine, live, files) = match &state.session {
            None => (EngineState::Idle, None, Vec::new()),
            Some(session) => match session.files.lock().unwrap().clone() {
                None => (EngineState::Starting, None, Vec::new()),
                Some(files) => (
                    EngineState::Recording,
                    session.live.lock().unwrap().clone(),
                    files,
                ),
            },
        };
        let info = engine_info(&state);
        let (collection, profile) = state.file.current_names(state.setup.as_ref());
        Status {
            state: engine,
            scene: state.scene.clone(),
            preview: state.preview.clone(),
            live,
            files,
            last: state.last.clone(),
            last_error: state.last_error.clone(),
            engine: info,
            setup_file: Some(state.file.info()),
            collection,
            profile,
            stream: stream_status(&state),
            cpu_percent: self.cpu.lock().unwrap().read(),
            data: Some(self.data.status()),
            timeline: {
                let timelines = self.timelines.lock().unwrap();
                timelines.current().and_then(|id| timelines.summary(id))
            },
            timeline_problem: self
                .timelines
                .lock()
                .unwrap()
                .current_problem()
                .map(str::to_string),
            export: self.export_running(),
        }
    }

    /// Start streaming the live engine's program to the live setup's
    /// `[stream]` service, starting the engine if needed.
    fn stream_start(self: &Arc<Self>) -> Result<StreamStatus, RpcError> {
        let wrong = |message: String| RpcError {
            code: RpcError::WRONG_STATE,
            message,
        };
        let mut state = self.state.lock().unwrap();
        if state.streaming() {
            return Err(wrong(
                "already streaming; stop it first (`isocast stream stop`, or MCP's stop_stream)"
                    .to_string(),
            ));
        }
        let settings = stream_settings(&state).ok_or_else(|| {
            wrong(
                "the live setup has no stream; add one (for example the patch \
                 {\"stream\": {\"service\": \"twitch\"}} with `isocast apply --patch` or MCP's \
                 apply_change), then set the key (`isocast stream key set`, or MCP's \
                 set_stream_key)"
                    .to_string(),
            )
        })?;
        settings
            .server()
            .map_err(|err| failed(anyhow::anyhow!("{err}")))?;
        let account = settings.key_account();
        let key = match std::env::var(KEY_ENV) {
            Ok(key) if !key.trim().is_empty() => key.trim().to_string(),
            _ => match secret(KEY_SERVICE, &account) {
                Ok(Some(key)) => key,
                Ok(None) => {
                    return Err(failed(anyhow::anyhow!(
                        "no stream key is set for {}; run `isocast stream key set` (it reads the \
                         key from stdin), or MCP's set_stream_key",
                        key_label(settings.service, &account)
                    )));
                }
                Err(err) => return Err(failed(err)),
            },
        };
        // Starting the engine happens outside the lock; until the stream is
        // in the state, this keeps the engine from idling out.
        state.streams_starting += 1;
        drop(state);
        let engine = self.ensure_engine();
        let mut state = self.state.lock().unwrap();
        state.streams_starting -= 1;
        let engine = engine?;
        if state.streaming() {
            return Err(wrong(
                "already streaming; stop it first (`isocast stream stop`, or MCP's stop_stream)"
                    .to_string(),
            ));
        }
        // The previous stream, finished, makes way.
        state.stream = None;
        state.stream_drops_reported = 0;
        state.stream_audio_drops_reported = 0;
        state.stream_bitrate_warned = false;
        let daemon = Arc::downgrade(self);
        let on_change: OnChange = Box::new(move |status| {
            if let Some(daemon) = daemon.upgrade() {
                daemon.publish(Event::Stream {
                    stream: status.clone(),
                });
            }
        });
        let stream = engine.stream(settings, key, on_change).map_err(failed)?;
        // In the state while it connects, so the engine stays up for it,
        // a second start is refused and `stream stop` can cancel it.
        state.stream = Some(stream);
        drop(state);
        let deadline = std::time::Instant::now() + STREAM_START_TIMEOUT;
        let status = loop {
            let Some(status) = self
                .state
                .lock()
                .unwrap()
                .stream
                .as_ref()
                .map(Stream::status)
            else {
                return Err(failed(anyhow::anyhow!(
                    "the stream was stopped before it went live"
                )));
            };
            if status.state != StreamState::Connecting || std::time::Instant::now() >= deadline {
                break status;
            }
            thread::sleep(Duration::from_millis(20));
        };
        match status.state {
            StreamState::Failed | StreamState::Offline => Err(failed(anyhow::anyhow!(
                "{}",
                status
                    .last_error
                    .unwrap_or_else(|| "the stream stopped".into())
            ))),
            _ => Ok(status),
        }
    }
    fn stream_stop(&self) -> Result<StreamStatus, RpcError> {
        let mut state = self.state.lock().unwrap();
        let status = state.stream.as_ref().map(Stream::status);
        if !status.as_ref().is_some_and(StreamStatus::is_active) {
            // A stream that failed stays in the status until the next
            // start; say so rather than only "not streaming".
            let message = match status {
                Some(StreamStatus {
                    state: StreamState::Failed,
                    last_error,
                    ..
                }) => format!(
                    "not streaming: the stream already failed ({})",
                    last_error.as_deref().unwrap_or("no reason given")
                ),
                _ => "not streaming".into(),
            };
            return Err(RpcError {
                code: RpcError::WRONG_STATE,
                message,
            });
        }
        let stream = state.stream.take().expect("checked");
        drop(state);
        Ok(stream.stop())
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
        let (service, account) = self.key_target(params.target)?;
        set_secret(KEY_SERVICE, &account, key).map_err(failed)?;
        Ok(StreamKey {
            service,
            account,
            key_set: true,
        })
    }

    fn stream_key_clear(&self, target: StreamKeyTarget) -> Result<StreamKey, RpcError> {
        let (service, account) = self.key_target(target)?;
        delete_secret(KEY_SERVICE, &account).map_err(failed)?;
        Ok(StreamKey {
            service,
            account,
            key_set: false,
        })
    }

    /// The service and Keychain account a key command means: as given, or
    /// the live setup's `[stream]`.
    fn key_target(&self, target: StreamKeyTarget) -> Result<(StreamService, String), RpcError> {
        let live = stream_settings(&self.state.lock().unwrap());
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
        Ok((service, key_account(service, server.as_deref())))
    }

    /// Warn about stream-only losses and sustained bitrate overshoot (once
    /// per episode), checked every second even when encoding/sending stalls.
    fn warn_stream_drops(&self) {
        let mut state = self.state.lock().unwrap();
        let Some(status) = state.stream.as_ref().map(Stream::status) else {
            return;
        };
        // While reconnecting, the stream's state events say why frames go.
        if status.state != StreamState::Live {
            state.stream_drops_reported = status.dropped_frames;
            state.stream_audio_drops_reported = status.audio_dropouts;
            state.stream_bitrate_warned = false;
            return;
        }
        let mut warnings = Vec::new();
        if status.dropped_frames > state.stream_drops_reported {
            let new = status.dropped_frames - state.stream_drops_reported;
            state.stream_drops_reported = status.dropped_frames;
            warnings.push(format!(
                "the stream dropped {new} frame(s) ({} so far, congestion {:.0}%); the \
                     program and recordings are unaffected",
                status.dropped_frames,
                status.congestion * 100.0
            ));
        }
        if status.audio_dropouts > state.stream_audio_drops_reported {
            let new = status.audio_dropouts - state.stream_audio_drops_reported;
            state.stream_audio_drops_reported = status.audio_dropouts;
            warnings.push(format!(
                "the stream dropped {new} audio chunk(s) ({} so far); \
                 the stream encoder or connection could not keep up",
                status.audio_dropouts,
            ));
        }
        if let Some(warning) = &status.bitrate_warning
            && !state.stream_bitrate_warned
        {
            warnings.push(warning.clone());
        }
        state.stream_bitrate_warned = status.bitrate_warning.is_some();
        drop(state);
        for message in warnings {
            self.publish(Event::Warning { message });
        }
    }

    fn start(self: &Arc<Self>, params: StartParams) -> Result<Started, RpcError> {
        check_folder(params.folder.as_deref())?;
        let duration = match params.seconds {
            None => None,
            Some(s) if s > 0.0 && s <= 30.0 * 24.0 * 3600.0 => Some(Duration::from_secs_f64(s)),
            Some(s) => {
                return Err(failed(anyhow::anyhow!(
                    "seconds must be more than 0 and at most 30 days, not {s}"
                )));
            }
        };
        if self.state.lock().unwrap().session.is_some() {
            return Err(RpcError {
                code: RpcError::WRONG_STATE,
                message: "already recording; stop the current session first".into(),
            });
        }
        // A setup given with the request becomes the live setup; say so if
        // it replaces another, since that one is only kept in the .bak.
        let mut notes = Vec::new();
        if let Some(setup) = params.setup {
            let (revision, replaced) = {
                let state = self.state.lock().unwrap();
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
                let file = self.state.lock().unwrap().file.info();
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
            let state = self.state.lock().unwrap();
            let Some(setup) = &state.setup else {
                return Err(state.no_setup("no setup to record (or give one)"));
            };
            params
                .folder
                .clone()
                .unwrap_or_else(|| expand_home(&setup.recording.folder))
        };
        // Recordings there that never finished (isocastd was killed or
        // crashed): the engine marks them as failed; say so now.
        notes.extend(interrupted_note(&interrupted_sessions(&folder, true), true));
        // A timeline that should be current but couldn't be read.
        notes.extend(
            self.timelines
                .lock()
                .unwrap()
                .current_problem()
                .map(str::to_string),
        );
        // The session is in the state before the engine warms up, so
        // `status` says it is starting and `stop` can cancel it.
        let mut state = self.state.lock().unwrap();
        if state.session.is_some() {
            return Err(RpcError {
                code: RpcError::WRONG_STATE,
                message: "already recording; stop the current session first".into(),
            });
        }
        if state.tuning {
            return Err(RpcError {
                code: RpcError::WRONG_STATE,
                message: "`tune` is benchmarking the encoders; start once it has finished".into(),
            });
        }
        state.next_session += 1;
        let session = Session {
            id: state.next_session,
            stop: Arc::new(AtomicBool::new(false)),
            files: Arc::new(Mutex::new(None)),
            live: Arc::new(Mutex::new(None)),
            takes: Arc::new(timeline::SessionTakes::default()),
        };
        let id = session.id;
        let (started_tx, started_rx) = mpsc::channel();
        let options = (
            session.stop.clone(),
            session.files.clone(),
            session.live.clone(),
            session.takes.clone(),
        );
        state.session = Some(session);
        drop(state);

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
                self.publish(Event::Stopped {
                    report: None,
                    error: Some(err.message.clone()),
                });
                self.ended(id, Err(err.message.clone()));
                return Err(err);
            }
        };

        let daemon = self.clone();
        let recording = engine.clone();
        thread::spawn(move || {
            let (stop, files, live, takes) = options;
            let on_start = |paths: &[PathBuf], manifest: &Path| {
                *takes.current.lock().unwrap() = timeline::TakeNow {
                    number: 1,
                    files: paths.to_vec(),
                    begins: None,
                    begins_at: None,
                };
                daemon.take_began(&takes, 1, manifest);
                *files.lock().unwrap() = Some(paths.to_vec());
                let _ = started_tx.send(Ok(paths.to_vec()));
                daemon.publish(Event::Started {
                    files: paths.to_vec(),
                    begins_at: None,
                });
            };
            let reported = Mutex::new(Vec::<(u64, u64)>::new());
            let on_status = |status: &LiveStatus| {
                *live.lock().unwrap() = Some(status.clone());
                for message in status.new_drops(&mut reported.lock().unwrap()) {
                    daemon.publish(Event::Warning { message });
                }
                daemon.publish(Event::Status {
                    live: status.clone(),
                });
            };
            let on_take = |number: u32, path: &Path, manifest: &SessionManifest| {
                daemon.take_finished(&takes, number, path, manifest);
            };
            let options = RecordOptions {
                folder: params.folder,
                duration,
                stop,
                on_start: Some(&on_start),
                on_status: Some(&on_status),
                on_take: Some(&on_take),
                control: None,
            };
            let ended = engine.record(&options).map_err(|err| format!("{err:#}"));
            daemon.session_over(&takes);
            if let Err(message) = &ended {
                let _ = started_tx.send(Err(message.clone()));
            }
            daemon.publish(Event::Stopped {
                report: ended.clone().ok().map(Box::new),
                error: ended.clone().err(),
            });
            daemon.ended(id, ended);
        });

        match started_rx.recv_timeout(START_TIMEOUT) {
            Ok(Ok(files)) => {
                // Said up front, where OBS says "encoding overloaded" mid-take.
                if let Some(message) = self.capacity_warning(&*recording) {
                    self.publish(Event::Warning {
                        message: message.clone(),
                    });
                    notes.push(message);
                }
                Ok(Started {
                    files,
                    begins_at: None,
                    notes,
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
    /// hardware encoders than `tune` measured they manage on this Mac.
    fn capacity_warning(&self, engine: &dyn LiveEngine) -> Option<String> {
        let (setup, memory) = {
            let state = self.state.lock().unwrap();
            let memory = capacity::memory_path(state.file.legacy());
            (state.setup.clone()?, memory)
        };
        if !memory.exists() {
            return None;
        }
        // The ISO sources are running by now, at the formats they record.
        let info = engine.info();
        let workload = capacity::workload(&setup, |id| {
            let source = info.sources.iter().find(|s| s.source == id)?;
            Some((source.width?, source.height?, source.fps?))
        })?;
        capacity::start_warning(&memory, machine_key()?, &workload)
    }

    /// The live engine, started with the live setup if it isn't running.
    /// Starting it (devices warming up) happens outside the state lock, so
    /// other requests carry on; callers asking meanwhile wait for it, and
    /// changes made meanwhile apply once it runs.
    fn ensure_engine(self: &Arc<Self>) -> Result<Arc<dyn LiveEngine>, RpcError> {
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(engine) = &state.engine {
                return Ok(engine.clone());
            }
            if !state.starting {
                break;
            }
            state = self.engine_ready.wait(state).unwrap();
        }
        let Some(setup) = state.setup.clone() else {
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
            });
        });
        state.starting = true;
        drop(state);
        // Callers wait until `starting` clears, so clear it even if
        // starting panics (a GPU error, say) rather than hang them all.
        let started = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            start_engine(&setup, control, on_event)
        }))
        .unwrap_or_else(|_| {
            Err(anyhow::anyhow!(
                "the live engine crashed while starting; see isocastd.log"
            ))
        });
        let mut state = self.state.lock().unwrap();
        state.starting = false;
        self.engine_ready.notify_all();
        let engine = started.map_err(failed)?;
        // Its frames say which revision they show; and changes made while
        // it started.
        if let Some(setup) = &state.setup
            && let Err(message) = engine.update(setup, state.revision)
        {
            eprintln!("isocastd: {message}");
        }
        if state.scene != scene
            && let Some(scene) = &state.scene
        {
            let _ = engine
                .control()
                .switch_scene(now(), scene.clone(), Transition::default());
        }
        engine.set_preview(state.preview.clone());
        state.engine = Some(engine.clone());
        drop(state);
        self.publish(Event::EngineChanged { running: true });
        Ok(engine)
    }

    /// The live engine's newest frame, as asked.
    fn frame(self: &Arc<Self>, params: FrameParams) -> Result<Frame, RpcError> {
        self.state.lock().unwrap().wanted_at = Some(std::time::Instant::now());
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
                message: format!(
                    "no new frame of {} within {} ms; ask again",
                    params.target,
                    timeout.as_millis()
                ),
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
        self.state.lock().unwrap().wanted_at = Some(std::time::Instant::now());
        let engine = self.ensure_engine()?;
        let fps = params.fps.unwrap_or(30).clamp(1, 60);
        let feed = engine
            .frames(&params.target, params.max_width, fps)
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
            code: RpcError::FAILED,
            message: format!(
                "shared frames can't be handed over here ({err:#}); use `frames` (H.264) instead"
            ),
        })?;
        self.state.lock().unwrap().wanted_at = Some(std::time::Instant::now());
        let engine = self.ensure_engine()?;
        let feed = engine
            .shared_frames(
                &params.target,
                (params.max_width, params.max_height),
                fps,
                slots,
            )
            .map_err(|message| RpcError {
                code: RpcError::FAILED,
                message,
            })?;
        let reply = SharedFrameStream {
            target: params.target.clone(),
            transport: "iosurface-mach".into(),
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
        self.state.lock().unwrap().hold = running;
        if running {
            self.ensure_engine()?;
        } else {
            self.stop_idle_engine_now();
        }
        Ok(engine_info(&self.state.lock().unwrap()))
    }

    /// Stop the live engine (after the recording, if one runs).
    fn stop_engine(&self) {
        let engine = self.state.lock().unwrap().engine.take();
        if let Some(engine) = engine {
            engine.stop();
            self.publish(Event::EngineChanged { running: false });
        }
    }

    /// Stop the live engine if nothing needs it: no recording, no hold, and
    /// no client asking for frames lately.
    fn stop_idle_engine_now(&self) {
        let engine = {
            let mut state = self.state.lock().unwrap();
            let idle = state.session.is_none()
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
            self.stop_idle_engine_now();
        }
    }

    /// Stop the running session (or cancel one still starting) and wait
    /// for its files to be finished.
    fn stop(&self) -> Result<Report, RpcError> {
        let state = self.state.lock().unwrap();
        let Some(session) = &state.session else {
            return Err(RpcError {
                code: RpcError::WRONG_STATE,
                message: "not recording".into(),
            });
        };
        session.stop.store(true, Ordering::SeqCst);
        let id = session.id;
        let (state, timeout) = self
            .ended
            .wait_timeout_while(state, STOP_TIMEOUT, |s| s.ended_session < id)
            .unwrap();
        if timeout.timed_out() {
            return Err(RpcError {
                code: RpcError::FAILED,
                message: "the recording did not stop in time".into(),
            });
        }
        // This session's outcome: a later one can't have ended yet.
        match (&state.last_error, &state.last) {
            (Some(error), _) => Err(RpcError {
                code: RpcError::FAILED,
                message: error.clone(),
            }),
            (None, Some(report)) => Ok(report.clone()),
            (None, None) => Err(RpcError {
                code: RpcError::FAILED,
                message: "the recording ended without a report".into(),
            }),
        }
    }

    /// Session `id` has ended, with its report or why it has none.
    fn ended(&self, id: u64, ended: Result<Report, String>) {
        let mut state = self.state.lock().unwrap();
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
        self.ended.notify_all();
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
    Some(info)
}

/// Why `folder` can't be used, if it is relative: isocastd runs in its
/// own folder, not the client's, so a relative folder would land somewhere
/// unexpected.
fn relative_folder(folder: &str) -> Option<String> {
    (!folder.starts_with('/') && !folder.starts_with('~')).then(|| {
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

/// What `status` says about streaming: the stream's own status if one ran,
/// else the live setup's stream, offline.
fn stream_status(state: &State) -> Option<StreamStatus> {
    if let Some(stream) = &state.stream {
        return Some(stream.status());
    }
    let settings = stream_settings(state)?;
    let key_set = std::env::var(KEY_ENV).is_ok_and(|key| !key.trim().is_empty())
        || has_secret(KEY_SERVICE, &settings.key_account()).unwrap_or(false);
    Some(StreamStatus::offline(&settings, key_set))
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
    let current = match &state.setup {
        Some(setup) => to_json(setup)?,
        None => serde_json::Value::Object(Default::default()),
    };
    let (proposed, missing) = match change {
        Change::Setup(setup) => (to_json(setup)?, Vec::new()),
        Change::Patch(_) if state.setup.is_none() => {
            return Err(state.no_setup("nothing to patch (or give a whole setup)"));
        }
        Change::Patch(patch) => (
            merge_patch(&current, &patch),
            missing_removals(&current, &patch),
        ),
    };
    let mut changes = diff(&current, &proposed);
    let (mut setup, mut problems) = match Setup::from_value(proposed) {
        Ok(setup) => {
            let mut problems = setup.validate();
            if let Some(message) = relative_folder(&setup.recording.folder) {
                problems.push(Issue {
                    path: "recording.folder".into(),
                    message,
                });
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
    let mut notes = Vec::new();
    if let Some(setup) = &setup {
        let (plugin_problems, plugin_notes) = plugins.check_kinds(setup);
        problems.extend(plugin_problems);
        notes.extend(plugin_notes);
    }
    if state.session.is_some() {
        notes.push(
            "a recording is in progress: changes show in the program at once, but the sources \
             it records keep their devices until it stops, and the canvas can't change"
                .into(),
        );
    }
    if state.streaming()
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
    // A stream key pasted into stream.server is refused (a problem says
    // so), and the plan mustn't show it either.
    if let Some(key) = setup
        .as_ref()
        .and_then(|setup| setup.stream.as_ref())
        .and_then(StreamSettings::pasted_key)
    {
        for change in &mut changes {
            for value in [&mut change.from, &mut change.to].into_iter().flatten() {
                redact_value(value, &key);
            }
        }
        if let Some(stream) = setup.as_mut().and_then(|setup| setup.stream.as_mut())
            && let Some(server) = &mut stream.server
        {
            *server = isocast_engine::stream::redact(server, &key);
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
#[cfg(target_os = "macos")]
fn now() -> Time {
    MacPlatform.now()
}

#[cfg(not(target_os = "macos"))]
fn now() -> Time {
    Time::from_nanos(0)
}

#[cfg(target_os = "macos")]
fn has_secret(service: &str, account: &str) -> Result<bool> {
    MacPlatform.has_secret(service, account)
}

#[cfg(target_os = "macos")]
fn secret(service: &str, account: &str) -> Result<Option<String>> {
    MacPlatform.secret(service, account)
}

#[cfg(target_os = "macos")]
fn set_secret(service: &str, account: &str, value: &str) -> Result<()> {
    MacPlatform.set_secret(service, account, value)
}

#[cfg(target_os = "macos")]
fn delete_secret(service: &str, account: &str) -> Result<bool> {
    MacPlatform.delete_secret(service, account)
}

#[cfg(not(target_os = "macos"))]
fn has_secret(_service: &str, _account: &str) -> Result<bool> {
    Ok(false)
}

#[cfg(not(target_os = "macos"))]
fn secret(_service: &str, _account: &str) -> Result<Option<String>> {
    bail!("stream keys are only kept on macOS so far")
}

#[cfg(not(target_os = "macos"))]
fn set_secret(_service: &str, _account: &str, _value: &str) -> Result<()> {
    bail!("stream keys are only kept on macOS so far")
}

#[cfg(not(target_os = "macos"))]
fn delete_secret(_service: &str, _account: &str) -> Result<bool> {
    bail!("stream keys are only kept on macOS so far")
}

#[cfg(target_os = "macos")]
fn permissions() -> Result<PermissionsReport> {
    MacPlatform.permissions()
}

#[cfg(not(target_os = "macos"))]
fn permissions() -> Result<PermissionsReport> {
    bail!("permissions are only supported on macOS so far")
}

#[cfg(target_os = "macos")]
fn request_permissions() -> Result<PermissionsReport> {
    MacPlatform.request_permissions()
}

#[cfg(not(target_os = "macos"))]
fn request_permissions() -> Result<PermissionsReport> {
    bail!("permissions are only supported on macOS so far")
}

#[cfg(target_os = "macos")]
fn open_permission_settings(permission: PermissionKind) -> Result<String> {
    MacPlatform.open_permission_settings(permission)
}

#[cfg(not(target_os = "macos"))]
fn open_permission_settings(_permission: PermissionKind) -> Result<String> {
    bail!("permissions are only supported on macOS so far")
}

/// Which Mac (and macOS) `tune`'s measurements here hold for.
#[cfg(target_os = "macos")]
fn machine_key() -> Option<&'static str> {
    static KEY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    Some(KEY.get_or_init(|| capacity::machine_key(&MacPlatform.machine())))
}

#[cfg(not(target_os = "macos"))]
fn machine_key() -> Option<&'static str> {
    None
}

#[cfg(target_os = "macos")]
fn tune(setup: &Setup, folder: Option<&Path>, memory: &Path) -> Result<Tuning> {
    isocast_engine::tune::tune(&MacPlatform, setup, folder, Some(memory))
}

#[cfg(not(target_os = "macos"))]
fn tune(_setup: &Setup, _folder: Option<&Path>, _memory: &Path) -> Result<Tuning> {
    bail!("tuning is only supported on macOS so far")
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

#[cfg(target_os = "macos")]
type Graphics = isocast_engine::snapshot::Graphics<MacPlatform>;

/// The GPU, set up on first use and kept.
#[cfg(target_os = "macos")]
fn with_graphics<T>(draw: impl FnOnce(&mut Graphics) -> Result<T>) -> Result<T> {
    static GRAPHICS: Mutex<Option<Graphics>> = Mutex::new(None);
    let mut graphics = GRAPHICS.lock().unwrap();
    let graphics = match &mut *graphics {
        Some(graphics) => graphics,
        none => none.insert(Graphics::new(&MacPlatform)?),
    };
    draw(graphics)
}

/// Frames of the setup's sources `ids`: the newest the recording captured
/// where it captures them, else taken by opening them briefly.
#[cfg(target_os = "macos")]
fn frames(
    setup: &Setup,
    ids: &[String],
    recording: Option<&SessionControl>,
    data: &Arc<DataStore>,
) -> Result<Vec<(String, isocast_platform_macos::PixelBuffer)>> {
    use isocast_platform_macos::PixelBuffer;
    let mut frames = Vec::new();
    let mut missing = Vec::new();
    for id in ids {
        match recording.and_then(|control| control.newest_frame::<PixelBuffer>(id)) {
            Some(frame) => frames.push((id.clone(), frame)),
            None => missing.push(id.clone()),
        }
    }
    frames.extend(isocast_engine::snapshot::take_frames(
        &MacPlatform,
        setup,
        &missing,
        data,
    )?);
    Ok(frames)
}

/// One frame of the setup's source `id` as a PNG.
#[cfg(target_os = "macos")]
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
#[cfg(target_os = "macos")]
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

#[cfg(not(target_os = "macos"))]
fn source_png(
    _setup: &Setup,
    _id: &str,
    _recording: Option<&SessionControl>,
    _width: Option<u32>,
    _data: &Arc<DataStore>,
) -> Result<(Vec<u8>, u32, u32)> {
    bail!("snapshots are only supported on macOS so far")
}

#[cfg(not(target_os = "macos"))]
fn scene_png(
    _setup: &Setup,
    _id: &str,
    _sources: &[String],
    _recording: Option<&SessionControl>,
    _width: Option<u32>,
    _data: &Arc<DataStore>,
) -> Result<(Vec<u8>, u32, u32)> {
    bail!("snapshots are only supported on macOS so far")
}

#[cfg(target_os = "macos")]
fn doctor(setup: Option<&Setup>, folder: Option<&Path>) -> isocast_protocol::api::Doctor {
    isocast_engine::doctor::doctor(&MacPlatform, setup, folder)
}

#[cfg(target_os = "macos")]
fn devices() -> Result<isocast_protocol::devices::Devices> {
    MacPlatform.devices()
}

#[cfg(not(target_os = "macos"))]
fn devices() -> Result<isocast_protocol::devices::Devices> {
    bail!("listing devices is only supported on macOS so far")
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
    let home = std::env::var("HOME").ok();
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
        let _ = tx.send(isocast_engine::content_size(&MacPlatform, &kind));
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
