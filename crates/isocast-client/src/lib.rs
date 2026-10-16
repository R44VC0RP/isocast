//! Talking to the daemon, `isocastd`: connecting (launching it if needed),
//! calling methods and watching events. The CLI, MCP and the app all use
//! this one client.

pub mod library;
#[cfg(feature = "shared")]
pub mod shared;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use isocast_protocol::api::{
    Event, FrameStream, FramesParams, Live, Method, Notification, Request, Response, RpcError,
    WatchParams, lock_path, socket_path,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// How long a freshly launched daemon may take to start listening.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a running daemon may refuse connections before giving up.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
}

/// An error the daemon returned, with its JSON-RPC code.
#[derive(Debug)]
pub struct DaemonError(pub RpcError);

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.message)
    }
}

impl std::error::Error for DaemonError {}

impl DaemonError {
    /// The JSON-RPC error code, like [`RpcError::WRONG_STATE`].
    pub fn code(&self) -> i64 {
        self.0.code
    }
}

impl Client {
    pub fn clear_preview(&mut self) -> Result<isocast_protocol::api::Switched> {
        self.call(Method::ClearPreview)
    }

    pub fn encoder_options(&mut self) -> Result<Vec<isocast_protocol::EncoderOption>> {
        self.call(Method::EncoderOptions)
    }

    /// Forward a focused fallback key edge. Reuse this connection for both
    /// edges; disconnecting releases its push keys. The daemon accepts even
    /// globally registered chords and deduplicates matching native press edges.
    pub fn hotkey_input(
        &mut self,
        keys: String,
        phase: isocast_protocol::hotkeys::Phase,
    ) -> Result<Value> {
        self.call(Method::HotkeysInput(
            isocast_protocol::hotkeys::InputParams { keys, phase },
        ))
    }

    /// Connect to a running daemon, or `None` if none is running. A daemon
    /// that runs (holds its lock) but refuses connections, because it is
    /// starting, exiting or too busy to accept (its queue is full), is
    /// retried for up to [`BUSY_TIMEOUT`]: launching another would take
    /// over its socket and orphan whatever it records.
    pub fn connect() -> Result<Option<Self>> {
        let deadline = Instant::now() + BUSY_TIMEOUT;
        loop {
            let error = match UnixStream::connect(socket_path()) {
                Ok(stream) => return Ok(Some(Self::new(stream)?)),
                Err(error) => error,
            };
            let Some(pid) = running_daemon() else {
                return Ok(None);
            };
            if Instant::now() > deadline {
                bail!(
                    "isocastd (pid {pid}) is running but has not accepted a connection on {} \
                     for {} s ({error}); it may be stopped or overloaded. Try again, or end it \
                     with `kill {pid}` (it finishes its files first)",
                    socket_path().display(),
                    BUSY_TIMEOUT.as_secs()
                );
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Connect, launching the daemon in the background if it isn't running.
    pub fn connect_or_launch() -> Result<Self> {
        if let Some(client) = Self::connect()? {
            return Ok(client);
        }
        launch()?;
        let deadline = Instant::now() + SPAWN_TIMEOUT;
        loop {
            if let Some(client) = Self::connect()? {
                return Ok(client);
            }
            if Instant::now() > deadline {
                bail!(
                    "isocastd did not start listening on {}; inspect `isocast logs` (folder {}) and bootstrap stderr {}",
                    socket_path().display(),
                    isocast_diagnostics::logs_folder().display(),
                    log_path().display()
                );
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn new(stream: UnixStream) -> Result<Self> {
        Ok(Self {
            writer: stream.try_clone()?,
            reader: BufReader::new(stream),
            next_id: 1,
        })
    }

    /// Call a method and decode its result. Errors the daemon reports come
    /// back as [`DaemonError`].
    pub fn call<T: DeserializeOwned>(&mut self, method: Method) -> Result<T> {
        let id = self.next_id();
        let result = self.request(&Request::new(id, method))?;
        Ok(serde_json::from_value(result)?)
    }

    /// Call a method by name with JSON params and return its result as
    /// JSON, for clients that relay requests they don't interpret (the
    /// app). Errors the daemon reports come back as [`DaemonError`].
    pub fn call_value(&mut self, method: &str, params: Option<Value>) -> Result<Value> {
        let id = self.next_id();
        let mut request = serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method});
        if let Some(params) = params {
            request["params"] = params;
        }
        self.request(&request)
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Send one request and read its response's result.
    fn request(&mut self, request: &impl Serialize) -> Result<Value> {
        let started = Instant::now();
        let audit = serde_json::to_value(request)?;
        // Learn credentials without persisting arbitrary client params/prose.
        let _ = isocast_diagnostics::redact::Redactor::default().value(&audit);
        let request = serde_json::to_string(request)?;
        writeln!(self.writer, "{request}").context("lost the connection to isocastd")?;
        let mut line = String::new();
        self.reader
            .read_line(&mut line)
            .context("lost the connection to isocastd")?;
        if line.is_empty() {
            bail!("isocastd closed the connection");
        }
        isocast_diagnostics::daemon_answered();
        let response: Response = serde_json::from_str(&line)
            .with_context(|| format!("isocastd sent an unreadable response: {line}"))?;
        if let Some(error) = response.error {
            isocast_diagnostics::emit(
                "error",
                "client",
                "API request failed",
                serde_json::json!({"method":audit["method"],"error":error,"duration_ms":started.elapsed().as_secs_f64()*1000.0}),
            );
            return Err(DaemonError(error).into());
        }
        isocast_diagnostics::poll::record(
            audit["method"].as_str().unwrap_or("unknown"),
            &serde_json::json!({"pid":std::process::id()}),
            started.elapsed().as_secs_f64() * 1000.0,
            line.len(),
        );
        // A `null` result (like shutdown's) arrives as no result at all.
        Ok(response.result.unwrap_or_default())
    }
}

impl Client {
    /// Watch a target's picture as H.264 (`frames`): returns the reply and
    /// the connection, positioned at the first binary record (see
    /// [`isocast_protocol::api::frames`]). Closing it stops watching.
    pub fn frames(mut self, params: FramesParams) -> Result<(FrameStream, BufReader<UnixStream>)> {
        let reply: FrameStream = self.call(Method::Frames(params))?;
        Ok((reply, self.reader))
    }

    /// Watch events: returns the live setup, then the stream of events
    /// until the daemon goes away.
    pub fn watch(self) -> Result<(Live, impl Iterator<Item = Result<Event>>)> {
        self.watch_some(None)
    }

    /// Watch the kinds of events `params` asks for (every kind if `None`):
    /// returns the live setup, then the stream of events until the daemon
    /// goes away.
    pub fn watch_some(
        mut self,
        params: Option<WatchParams>,
    ) -> Result<(Live, impl Iterator<Item = Result<Event>>)> {
        let live: Live = self.call(Method::Watch(params))?;
        let events = self.reader.lines().map(|line| {
            let line = line.context("lost the connection to isocastd")?;
            let notification: Notification = serde_json::from_str(&line)
                .with_context(|| format!("isocastd sent an unreadable event: {line}"))?;
            Ok(notification.params)
        });
        Ok((live, events))
    }

    /// Watch events as JSON, including kinds this build doesn't know:
    /// returns the live setup, then each event's `params` until the daemon
    /// goes away.
    pub fn watch_values(self) -> Result<(Value, impl Iterator<Item = Result<Value>>)> {
        self.watch_values_some(None)
    }

    /// [`Client::watch_values`] of the kinds `params` asks for.
    pub fn watch_values_some(
        mut self,
        params: Option<WatchParams>,
    ) -> Result<(Value, impl Iterator<Item = Result<Value>>)> {
        let params = params.map(serde_json::to_value).transpose()?;
        let live = self.call_value("watch", params)?;
        let events = self.reader.lines().map(|line| {
            let line = line.context("lost the connection to isocastd")?;
            let mut notification: Value = serde_json::from_str(&line)
                .with_context(|| format!("isocastd sent an unreadable event: {line}"))?;
            Ok(notification
                .get_mut("params")
                .map(Value::take)
                .unwrap_or_default())
        });
        Ok((live, events))
    }
}

/// Start `isocastd` (next to this binary) in the background, detached from
/// the terminal so Ctrl-C there doesn't stop a recording. Its output goes
/// to a log next to the socket.
fn launch() -> Result<()> {
    let daemon = daemon_path()?;
    let log = log_path();
    if let Some(folder) = log.parent() {
        std::fs::create_dir_all(folder)?;
    }
    spawn_daemon(&daemon, &log).with_context(|| format!("could not launch {}", daemon.display()))
}

/// Start the daemon in its own process group, detached from the terminal,
/// and on macOS as its own "responsible process": privacy permissions
/// (camera, microphone, screen recording) then belong to isocastd, not to
/// whichever terminal or agent app started it, so they are granted once.
#[cfg(target_os = "macos")]
fn spawn_daemon(daemon: &Path, log: &Path) -> std::io::Result<()> {
    use std::ffi::{CString, c_char, c_int};
    use std::os::unix::ffi::OsStrExt;
    use std::ptr;

    unsafe extern "C" {
        // libSystem; not in public headers, but stable since macOS 10.14
        // and what browsers use for their helpers.
        fn responsibility_spawnattrs_setdisclaim(
            attrs: *mut libc::posix_spawnattr_t,
            disclaim: c_int,
        ) -> c_int;
        fn _NSGetEnviron() -> *mut *const *const c_char;
    }

    let check = |status: c_int| {
        if status == 0 {
            Ok(())
        } else {
            Err(std::io::Error::from_raw_os_error(status))
        }
    };
    let program = CString::new(daemon.as_os_str().as_bytes())?;
    let log = CString::new(log.as_os_str().as_bytes())?;
    let null = c"/dev/null";
    let argv = [program.as_ptr(), ptr::null()];
    unsafe {
        let mut attrs: libc::posix_spawnattr_t = ptr::null_mut();
        check(libc::posix_spawnattr_init(&mut attrs))?;
        let mut actions: libc::posix_spawn_file_actions_t = ptr::null_mut();
        check(libc::posix_spawn_file_actions_init(&mut actions))?;
        let result = (|| {
            check(libc::posix_spawnattr_setflags(
                &mut attrs,
                (libc::POSIX_SPAWN_SETPGROUP | libc::POSIX_SPAWN_CLOEXEC_DEFAULT) as i16,
            ))?;
            check(libc::posix_spawnattr_setpgroup(&mut attrs, 0))?;
            check(responsibility_spawnattrs_setdisclaim(&mut attrs, 1))?;
            check(libc::posix_spawn_file_actions_addopen(
                &mut actions,
                0,
                null.as_ptr(),
                libc::O_RDONLY,
                0,
            ))?;
            check(libc::posix_spawn_file_actions_addopen(
                &mut actions,
                1,
                log.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND,
                0o600,
            ))?;
            check(libc::posix_spawn_file_actions_adddup2(&mut actions, 1, 2))?;
            let mut pid = 0;
            check(libc::posix_spawn(
                &mut pid,
                program.as_ptr(),
                &actions,
                &attrs,
                argv.as_ptr().cast(),
                (*_NSGetEnviron()).cast(),
            ))?;
            // Reap it if it exits while this process is still running.
            std::thread::spawn(move || {
                let mut status = 0;
                libc::waitpid(pid, &mut status, 0);
            });
            Ok(())
        })();
        libc::posix_spawn_file_actions_destroy(&mut actions);
        libc::posix_spawnattr_destroy(&mut attrs);
        result
    }
}

#[cfg(not(target_os = "macos"))]
fn spawn_daemon(daemon: &Path, log: &Path) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)?;
    std::process::Command::new(daemon)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0)
        .spawn()
        .map(|_| ())
}

/// The daemon to launch: `$ISOCAST_DAEMON`; else this distribution's bundled
/// helper; else the signed development app from `scripts/sign-dev.sh`, which
/// keeps its privacy permissions across rebuilds; else the adjacent build.
fn daemon_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("ISOCAST_DAEMON") {
        return Ok(PathBuf::from(path));
    }
    let exe = std::env::current_exe().context("could not find the isocast binary")?;
    // Resolve a CLI symlink installed on PATH. A distribution must use its own
    // engine, even on a developer's Mac with an older Isocast Dev.app present.
    let exe = exe.canonicalize().unwrap_or(exe);
    if let Some(helper) = bundled_helper(&exe) {
        return Ok(helper);
    }
    let built = exe.with_file_name("isocastd");
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let app = std::env::var_os("ISOCAST_DEV_APP").map_or_else(
        || home.join("Library/Application Support/Isocast/Isocast Dev.app"),
        PathBuf::from,
    );
    let signed = app.join("Contents/MacOS/isocastd");
    if signed.exists() {
        let modified = |path: &Path| std::fs::metadata(path).and_then(|m| m.modified()).ok();
        if modified(&built) > modified(&signed) {
            isocast_diagnostics::diagnostic!("warn";
                "warning: {} is newer than the signed daemon in {}; run scripts/sign-dev.sh",
                built.display(),
                app.display()
            );
        }
        return Ok(signed);
    }
    if !built.exists() {
        bail!(
            "{} is missing; build it with `cargo build -p isocast-daemon`",
            built.display()
        );
    }
    Ok(built)
}

/// The engine a distribution bundles next to `exe` (its
/// `Contents/MacOS/…`): `Contents/Helpers/Isocast Engine.app`'s
/// "Isocast Engine" (#83), or its `isocastd` in releases made before that.
fn bundled_helper(exe: &Path) -> Option<PathBuf> {
    use isocast_protocol::api::{ENGINE_EXECUTABLE, OLD_ENGINE_EXECUTABLE};
    let contents = exe.parent().and_then(Path::parent)?;
    if contents.file_name().is_none_or(|name| name != "Contents") {
        return None;
    }
    let macos = contents.join("Helpers/Isocast Engine.app/Contents/MacOS");
    [ENGINE_EXECUTABLE, OLD_ENGINE_EXECUTABLE]
        .into_iter()
        .map(|name| macos.join(name))
        .find(|helper| helper.is_file())
}

/// An isocastd older than the one this client would launch: left running
/// from before an update (Sparkle replaced the app while it ran), or
/// another, older copy's engine on the same socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleEngine {
    /// The running engine's version and build, as people read it
    /// (`0.0.4 (build 1760400000)`), or that it is too old to say.
    pub running: String,
    /// Its pid, when it reports one.
    pub pid: Option<u32>,
    /// The build this client bundles (`CFBundleVersion`).
    pub expected: String,
}

/// Whether the engine that answered `status` (its `daemon`; `None` from an
/// engine older than that field) is older than the one this distribution
/// bundles. Only a distribution knows: a development client (no bundled
/// engine, or `$ISOCAST_DAEMON`) never calls an engine stale. Builds are
/// compared as numbers (release and development builds both number them
/// by time), so a newer engine is never called stale and two copies of
/// Isocast don't restart each other's engines in turn.
pub fn stale_engine(daemon: Option<&isocast_protocol::api::DaemonBuild>) -> Option<StaleEngine> {
    older_engine(daemon, &bundled_build()?)
}

/// The build (`CFBundleVersion`) of the engine this distribution bundles
/// and launches; `None` for a development client or `$ISOCAST_DAEMON`.
pub fn bundled_build() -> Option<String> {
    if std::env::var_os("ISOCAST_DAEMON").is_some() {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let helper = bundled_helper(&exe)?;
    let plist = std::fs::read_to_string(helper.parent()?.parent()?.join("Info.plist")).ok()?;
    plist_string(&plist, "CFBundleVersion")
}

/// [`stale_engine`]'s comparison, against the bundled build `expected`.
fn older_engine(
    daemon: Option<&isocast_protocol::api::DaemonBuild>,
    expected: &str,
) -> Option<StaleEngine> {
    let own: u64 = expected.parse().ok()?;
    let Some(daemon) = daemon else {
        return Some(StaleEngine {
            running: "an engine too old to report its version".into(),
            pid: None,
            expected: expected.into(),
        });
    };
    // A build that isn't a number ("unknown") predates numbered builds.
    let older =
        daemon.build.parse::<u64>().is_ok_and(|b| b < own) || daemon.build.parse::<u64>().is_err();
    older.then(|| StaleEngine {
        running: format!("{} (build {})", daemon.version, daemon.build),
        pid: Some(daemon.pid),
        expected: expected.into(),
    })
}

/// A `<string>` value from an XML property list (as `plistlib` and the
/// build scripts write them), by key.
fn plist_string(plist: &str, key: &str) -> Option<String> {
    let after = &plist[plist.find(&format!("<key>{key}</key>"))? + key.len() + 11..];
    let value = after.trim_start().strip_prefix("<string>")?;
    Some(value[..value.find("</string>")?].trim().to_string())
}

/// The pid of the daemon holding the socket's lock ([`lock_path`]), if one
/// is running; the pid is empty if it hasn't written it yet.
fn running_daemon() -> Option<String> {
    let mut file = std::fs::File::open(lock_path()).ok()?;
    match file.try_lock_shared() {
        // Nobody holds it; dropping the file lets go at once.
        Ok(()) => None,
        Err(std::fs::TryLockError::WouldBlock) => {
            let mut pid = String::new();
            let _ = std::io::Read::read_to_string(&mut file, &mut pid);
            Some(pid.trim().to_string())
        }
        Err(std::fs::TryLockError::Error(_)) => None,
    }
}

/// The daemon's log, next to its socket and named after it
/// (`isocastd.sock` → `isocastd.log`), so daemons on sockets of their own
/// in one folder (`$ISOCAST_SOCKET`) don't share a log.
pub fn log_path() -> PathBuf {
    socket_path().with_extension("log")
}
