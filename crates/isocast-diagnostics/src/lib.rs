//! Synchronous JSONL at control-plane rates: no queue can lose the last error.
//! No capture/render/audio callback should emit per-frame logs.
pub mod bundle;
pub mod fds;
pub mod native;
pub mod output;
pub mod poll;
pub mod preview;
pub mod redact;
pub mod scan;
pub mod submit;

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};

const SEGMENT_BYTES: u64 = 4 << 20;
/// Developer mode's own log budget per process family: its verbose records
/// and traces never evict the ordinary 40 MB (daemon) history.
pub const DEVELOPER_BUDGET: u64 = 200 << 20;
/// Developer mode (verbose logs and per-second traces). Off by default; one
/// relaxed load is everything a hot path pays while it is off.
static DEVELOPER: AtomicBool = AtomicBool::new(false);
static DEVELOPER_ERRORS: AtomicU64 = AtomicU64::new(0);
const MAX_AGE: Duration = Duration::from_secs(7 * 86400);
static LOGGER: OnceLock<Logger> = OnceLock::new();
static ERRORS: AtomicU64 = AtomicU64::new(0);
static LOG_CPU_NS: AtomicU64 = AtomicU64::new(0);
static PANICS: AtomicU64 = AtomicU64::new(0);
static LAST_PANIC: Mutex<Option<Value>> = Mutex::new(None);
static MAINTENANCE: OnceLock<std::sync::mpsc::SyncSender<()>> = OnceLock::new();
/// A console process's start-up prune has finished (see `init`).
static FIRST_PRUNE: (Mutex<bool>, std::sync::Condvar) =
    (Mutex::new(false), std::sync::Condvar::new());
/// This process got an answer from isocastd (see [`daemon_answered`]).
static DAEMON_ANSWERED: AtomicBool = AtomicBool::new(false);
static WORKER_WARNING: OnceLock<Box<dyn Fn(String) + Send + Sync>> = OnceLock::new();

struct Logger {
    process: String,
    launch: String,
    folder: PathBuf,
    console: bool,
    previous: Vec<Value>,
    writer: Mutex<Option<Writer>>,
    /// Developer mode's segments (`<launch>-developer-NNNN.jsonl`), opened
    /// when the first developer record is written.
    developer: Mutex<DeveloperWriter>,
    init_error: Option<String>,
    previous_failure: Option<Value>,
}
#[derive(Default)]
struct DeveloperWriter {
    file: Option<File>,
    bytes: u64,
    /// The next segment's number; the open one is `segment - 1`.
    segment: u32,
}
struct Writer {
    file: File,
    marker: File,
    bytes: u64,
    segment: u32,
    last_prune: Instant,
    partial_line: bool,
}

/// The same scope in every process; an isolated socket never uses default logs.
pub fn logs_folder() -> PathBuf {
    if let Some(socket) = std::env::var_os("ISOCAST_SOCKET") {
        return PathBuf::from(socket).with_extension("logs");
    }
    let home = isocast_protocol::api::home_folder();
    if cfg!(target_os = "macos") {
        home.join("Library/Logs/Isocast")
    } else if cfg!(any(windows, target_os = "linux")) {
        isocast_protocol::api::support_folder().join("logs")
    } else {
        home.join(".isocast/logs")
    }
}

pub struct Guard;
impl Drop for Guard {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            clean_exit();
        }
    }
}

pub fn init(process: &str, console: bool) -> Guard {
    if LOGGER.get().is_some() {
        return Guard;
    }
    let folder = logs_folder();
    let launch = format!(
        "{}-{}-{process}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S%.9fZ"),
        std::process::id()
    );
    let previous = abnormal_runs(&folder, process);
    let previous_failure = std::fs::read(health_path(process))
        .ok()
        .and_then(|s| serde_json::from_slice(&s).ok());
    let opened = (|| -> std::io::Result<Writer> {
        std::fs::create_dir_all(&folder)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700))?;
        }
        let mut marker = private_file(&folder.join(format!("{launch}.run")))?;
        marker.lock()?;
        writeln!(
            marker,
            "{}",
            json!({"version":2,"launch":launch,"process":process,"pid":std::process::id(),"clean":false})
        )?;
        marker.flush()?;
        let file = private_file(&folder.join(format!("{launch}-0000.jsonl")))?;
        Ok(Writer {
            file,
            marker,
            bytes: 0,
            segment: 0,
            last_prune: Instant::now(),
            partial_line: false,
        })
    })();
    let (writer, init_error) = match opened {
        Ok(w) => (Some(w), None),
        Err(err) => (None, Some(err.to_string())),
    };
    let _ = LOGGER.set(Logger {
        process: process.into(),
        launch,
        folder: folder.clone(),
        console,
        previous,
        writer: Mutex::new(writer),
        developer: Mutex::default(),
        init_error,
        previous_failure,
    });
    if let Ok(key) = std::env::var("ISOCAST_STREAM_KEY") {
        redact::remember_secret("environment", "ISOCAST_STREAM_KEY", key.trim());
    }
    std::panic::set_hook(Box::new(|panic| {
        let thread = std::thread::current();
        let sequence = PANICS.fetch_add(1, Ordering::Relaxed) + 1;
        let crash = json!({"panic":panic.to_string().chars().take(8000).collect::<String>(),"thread_id":format!("{:?}",thread.id()),"thread":format!("{:?} {}",thread.id(),thread.name().unwrap_or("unnamed").chars().take(256).collect::<String>()),
            "sequence":sequence,"time":chrono::Utc::now().to_rfc3339(),
            "launch":LOGGER.get().map(|l|&l.launch),
            "backtrace":std::backtrace::Backtrace::force_capture().to_string().chars().take(32000).collect::<String>()});
        // A hook runs BEFORE unwind releases guards. Never wait for a mutex
        // here, and never call emit (which takes both redaction/writer locks).
        let safe = redact::panic_value(&crash);
        if let Ok(mut last) = LAST_PANIC.try_lock() {
            *last = Some(safe.clone());
        }
        if let Some(log) = LOGGER.get() {
            if let Ok(mut writer) = log.writer.try_lock()
                && let Some(writer) = writer.as_mut()
            {
                let line = json!({"time":crash["time"],"level":"error","process":log.process,"pid":std::process::id(),
                    "launch":log.launch,"thread":crash["thread"],"component":"panic","message":"panic captured; unwinding to recovery boundary","fields":safe});
                let _ = writeln!(writer.file, "{line}");
                let _ = writer.file.sync_all();
            }
            if let Ok(mut file) = private_file(
                &log.folder
                    .join(format!("{}-{sequence:08}.crash.json", log.launch)),
            ) {
                let _ = writeln!(file, "{safe}");
                let _ = file.sync_all();
            }
        } else {
            let _ = writeln!(std::io::stderr(), "Isocast panic: {safe}");
        }
        // Let catch_unwind, thread joins, and the async runtime contain it.
    }));
    emit(
        "info",
        "lifecycle",
        "process started",
        json!({"version":env!("CARGO_PKG_VERSION"),"previous_abnormal":previous_abnormal()}),
    );
    if let Some(log) = LOGGER.get()
        && let Some(error) = &log.init_error
    {
        persist_failure(log, error, 0, false);
        let _ = if log.console {
            writeln!(
                std::io::stderr(),
                "warning: Isocast diagnostic logs are unavailable ({error}); continuing without them"
            )
        } else {
            writeln!(
                std::io::stderr(),
                "isocast diagnostics unavailable; falling back to stderr: {error}"
            )
        };
    }
    #[cfg(unix)]
    if !console
        && LOGGER.get().is_some_and(|l| l.init_error.is_none())
        && let Some(log) = LOGGER.get()
    {
        let _ = native::start(&log.folder, &log.launch, &log.process);
    }
    // A console process (the CLI above all) prunes on the maintenance
    // thread while it does its work (see `clean_exit`): on the command's
    // path it cost ~1 ms of every `isocast` run (GitHub #4).
    if !console {
        prune(&folder);
    }
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let _ = MAINTENANCE.set(tx);
    let maintained = folder.clone();
    let spawned = std::thread::Builder::new()
        .name("diagnostic maintenance".into())
        .spawn(move || {
            let folder = maintained;
            if console {
                prune(&folder);
                let (done, pruned) = &FIRST_PRUNE;
                *done.lock().unwrap_or_else(|p| p.into_inner()) = true;
                pruned.notify_all();
            }
            loop {
                let _ = rx.recv_timeout(Duration::from_secs(30));
                poll::flush(false);
                prune(&folder);
            }
        });
    if console && spawned.is_err() {
        prune(&folder);
        *FIRST_PRUNE.0.lock().unwrap_or_else(|p| p.into_inner()) = true;
    }
    Guard
}

/// isocastd answered this process (the client calls this): a console
/// process then exits without waiting for its start-up prune, which the
/// daemon's own pruning of the same folder covers (GitHub #4).
pub fn daemon_answered() {
    DAEMON_ANSWERED.store(true, Ordering::Relaxed);
}

/// This process's resident memory (`VmRSS` in `/proc/self/status`).
#[cfg(target_os = "linux")]
pub fn resident_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}

pub fn private_file(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).read(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

pub fn previous_abnormal() -> Vec<Value> {
    LOGGER
        .get()
        .map_or_else(Vec::new, |log| log.previous.clone())
}

/// Only a daemon that acquired its instance lock and control socket represents
/// a daemon run. Failed concurrent startup attempts must not acknowledge one.
pub fn ready() {
    if let Some(log) = LOGGER.get()
        && let Ok(mut writer) = log.writer.lock()
        && let Some(writer) = writer.as_mut()
    {
        let _ = writeln!(writer.marker, "{{\"ready\":true}}");
    }
}
pub fn health() -> Value {
    json!({"native_relay_pid":native::pid(),"native_relay":native::health(),"callback_records_dropped":output::dropped(),"credential_pinned_count":redact::pinned_count(),"credential_cache_evictions":redact::cache_evictions(),"write_errors":ERRORS.load(Ordering::Relaxed),"logging_cpu_ns":cfg!(unix).then(||LOG_CPU_NS.load(Ordering::Relaxed)),"launch":LOGGER.get().map(|l| &l.launch),"mode":if LOGGER.get().is_some_and(|l|l.init_error.is_none()) {"jsonl"} else {"stderr"},"init_error":LOGGER.get().and_then(|l|l.init_error.as_ref()),"previous_logging_failure":LOGGER.get().and_then(|l|l.previous_failure.as_ref()),"previous_abnormal":previous_abnormal(),"open_files":fds::health(),"panics":PANICS.load(Ordering::Relaxed),"developer":developer(),"developer_write_errors":DEVELOPER_ERRORS.load(Ordering::Relaxed),"last_panic":LAST_PANIC.try_lock().ok().and_then(|v|v.clone())})
}

pub fn clean_exit() {
    poll::flush(true);
    let Some(log) = LOGGER.get() else { return };
    if log.console && !DAEMON_ANSWERED.load(Ordering::Relaxed) {
        // The start-up prune runs beside the process's work; finish it, so
        // every run still prunes. Bounded, so a stuck disk can't hold the
        // exit. Not when isocastd answered: it prunes the same folder every
        // 30 s, and a prune cut short leaves nothing half-done (it deletes
        // whole files, a launch's `.run` marker last, and the next prune
        // takes up the rest).
        let (done, pruned) = &FIRST_PRUNE;
        let done = done.lock().unwrap_or_else(|p| p.into_inner());
        let _ = pruned.wait_timeout_while(done, Duration::from_secs(5), |done| !*done);
    }
    emit("info", "lifecycle", "clean exit", Value::Null);
    let failed = ERRORS.load(Ordering::Relaxed);
    if failed > 0 {
        let recovered = log
            .writer
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .is_some_and(|w| !w.partial_line);
        persist_failure(log, "diagnostic writes failed", failed, recovered);
    }
    let mut writer = log.writer.lock().unwrap_or_else(|p| p.into_inner());
    let Some(writer) = writer.as_mut() else {
        return;
    };
    let _ = writer.file.flush();
    // Append, rather than overwrite: an interrupted write cannot erase evidence.
    let _ = writeln!(writer.marker, "{{\"clean\":true}}");
    sync_marker(&writer.marker, log.console);
}

/// Put the clean-exit mark on disk. The daemon flushes the drive's cache
/// too (`sync_all` is F_FULLFSYNC on macOS); a console process (the CLI, the
/// app) uses a plain fsync, which reaches the drive without waiting for its
/// cache: F_FULLFSYNC took 3.6–14 ms on every `isocast` command, more than
/// the rest of the command (GitHub #4). The line is in the kernel either
/// way, so only a power loss in the next moment could cost the mark, and a
/// missing mark only reports that one run as unclean.
fn sync_marker(marker: &File, console: bool) {
    #[cfg(unix)]
    if console {
        use std::os::fd::AsRawFd;
        // SAFETY: fsync on a descriptor this File owns and keeps open.
        unsafe { libc::fsync(marker.as_raw_fd()) };
        return;
    }
    let _ = console;
    let _ = marker.sync_all();
}

/// Contain a worker failure after its stack guards have unwound. The panic
/// hook owns crash capture; the caller owns state repair and failure events.
pub fn contain<T>(name: &str, run: impl FnOnce() -> T) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).map_err(|payload| {
        let detail = payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_else(|| "non-string panic".into());
        format!("{name} panicked: {detail}")
    })
}

/// Run detached work with owner-specific cleanup after unwinding, then notify
/// the daemon (or the console when embedded). Never invoke this from a hook.
pub fn spawn(
    name: impl Into<String>,
    run: impl FnOnce() + Send + 'static,
    failed: impl FnOnce(String) + Send + 'static,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let name = name.into();
    std::thread::Builder::new()
        .name(name.clone())
        .spawn(move || {
            if let Err(mut message) = contain(&name, run) {
                if let Err(cleanup) = contain("worker cleanup", || failed(message.clone())) {
                    message.push_str(&format!("; {cleanup}"));
                }
                diagnostic("error", "worker", message.clone());
                if let Some(notify) = WORKER_WARNING.get() {
                    notify(message);
                }
            }
        })
}

pub fn on_worker_warning(notify: impl Fn(String) + Send + Sync + 'static) {
    let _ = WORKER_WARNING.set(Box::new(notify));
}

/// Developer mode is on: verbose records and traces are written. A single
/// relaxed atomic load, so callers check it before building anything.
#[inline]
pub fn developer() -> bool {
    DEVELOPER.load(Ordering::Relaxed)
}

/// Turn developer mode on or off for this process. Returns whether it was
/// on. Turning it off closes the developer segment.
pub fn set_developer(on: bool) -> bool {
    let was = DEVELOPER.swap(on, Ordering::Relaxed);
    if !on
        && was
        && let Some(log) = LOGGER.get()
    {
        log.developer.lock().unwrap_or_else(|p| p.into_inner()).file = None;
    }
    was
}

/// A developer-mode record: written to this launch's developer segments
/// (their own 200 MB budget), never to the ordinary log, and only while
/// developer mode is on. Redacted like every record.
pub fn emit_developer(level: &str, component: &str, message: &str, fields: Value) {
    if !developer() {
        return;
    }
    let Some(log) = LOGGER.get() else { return };
    if log.init_error.is_some() {
        return;
    }
    let cpu = thread_cpu_ns();
    let Some(bytes) = encode(log, level, component, message, fields) else {
        return;
    };
    let mut dev = log.developer.lock().unwrap_or_else(|p| p.into_inner());
    let mut rotated = false;
    let result = (|| -> std::io::Result<()> {
        if dev.file.is_none() || dev.bytes + bytes.len() as u64 > SEGMENT_BYTES {
            let path = log
                .folder
                .join(format!("{}-developer-{:04}.jsonl", log.launch, dev.segment));
            dev.file = Some(private_file(&path)?);
            dev.segment += 1;
            dev.bytes = 0;
            rotated = true;
        }
        dev.file.as_mut().expect("opened").write_all(&bytes)?;
        dev.bytes += bytes.len() as u64;
        Ok(())
    })();
    if result.is_err() {
        // A failed or partial line: start a fresh segment next time.
        dev.file = None;
        DEVELOPER_ERRORS.fetch_add(1, Ordering::Relaxed);
    }
    drop(dev);
    LOG_CPU_NS.fetch_add(thread_cpu_ns().saturating_sub(cpu), Ordering::Relaxed);
    if rotated && let Some(tx) = MAINTENANCE.get() {
        let _ = tx.try_send(());
    }
}

/// A debug-level message for developer mode (see the `debug!` macro, which
/// checks [`developer`] before formatting anything).
pub fn debug_message(component: &str, message: String) {
    emit_developer("debug", component, &message, Value::Null);
}

/// Debug-level messages: nothing is formatted unless developer mode is on.
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {
        if $crate::developer() {
            $crate::debug_message(module_path!(), format!($($arg)*));
        }
    };
}

/// One redacted, size-bounded JSONL line (with its newline).
fn encode(
    log: &Logger,
    level: &str,
    component: &str,
    message: &str,
    fields: Value,
) -> Option<Vec<u8>> {
    let mut redactor = redact::Redactor::default();
    // Learn credentials before sanitizing the message as it may echo a field.
    let fields = redactor.value(&fields);
    let mut message = redactor.text(message);
    if message.len() > 32 * 1024 {
        message = message.chars().take(8000).collect::<String>()
            + " [message truncated at 8000 characters]";
    }
    let thread = std::thread::current();
    let record = json!({"time":chrono::Utc::now().to_rfc3339(),"level":level,"process":log.process,
        "pid":std::process::id(),"launch":log.launch,"thread":format!("{:?}",thread.id()),
        "thread_name":thread.name(),"component":component,"message":message,"fields":fields});
    let Ok(mut bytes) = serde_json::to_vec(&record) else {
        return None;
    };
    // Malicious calls cannot allocate an unbounded log line or exhaust the disk.
    if bytes.len() > 256 * 1024 {
        let fields = &record["fields"];
        let method = fields["request"]["method"]
            .as_str()
            .or_else(|| fields["method"].as_str())
            .map(|s| s.chars().take(256).collect::<String>());
        let compact = json!({"time":record["time"],"level":level,"process":log.process,"pid":std::process::id(),
            "launch":log.launch,"thread":record["thread"],"component":component,"message":message,
            "fields":{"omitted":"payload exceeds 256 KiB","bytes":bytes.len(),"method":method,
                "request_id":fields["request"]["id"].as_u64().or_else(||fields["id"].as_u64()),
                "client":fields["client"],"duration_ms":fields["duration_ms"],
                "error_code":fields["response"]["error"]["code"],
                "error_message":fields["response"]["error"]["message"].as_str().map(|s|s.chars().take(2000).collect::<String>())}});
        bytes = serde_json::to_vec(&compact).unwrap_or_default();
    }
    bytes.push(b'\n');
    Some(bytes)
}

pub fn emit(level: &str, component: &str, message: &str, fields: Value) {
    // The one gate: debug records exist only in developer mode, in its own
    // segments.
    if level == "debug" {
        return emit_developer(level, component, message, fields);
    }
    let Some(log) = LOGGER.get() else { return };
    let cpu = thread_cpu_ns();
    let Some(bytes) = encode(log, level, component, message, fields) else {
        return;
    };
    let mut writer = log.writer.lock().unwrap_or_else(|p| p.into_inner());
    let Some(writer) = writer.as_mut() else {
        // Only the daemon's stderr is a log file. Console processes already
        // printed the one human warning; never put JSONL in a terminal.
        if !log.console {
            let _ = std::io::stderr().write_all(&bytes);
        }
        return;
    };
    let mut rotated = false;
    let recovering = writer.partial_line;
    let result = (|| -> std::io::Result<()> {
        if writer.bytes + bytes.len() as u64 > SEGMENT_BYTES {
            let next = writer.segment + 1;
            let file = private_file(&log.folder.join(format!("{}-{next:04}.jsonl", log.launch)))?;
            writer.file = file;
            writer.segment = next;
            writer.bytes = 0;
            writer.partial_line = false;
            rotated = true;
        }
        if writer.partial_line {
            writer.file.write_all(b"\n")?;
            writer.bytes += 1;
            writer.partial_line = false;
        }
        budget_write_delay(component, message);
        writer.file.write_all(&bytes)?;
        writer.bytes += bytes.len() as u64;
        // Unbuffered File: every complete line reaches the OS immediately.
        if level == "error" {
            writer.file.flush()?;
        }
        Ok(())
    })();
    if let Err(error) = &result {
        writer.partial_line = true;
        let count = ERRORS.fetch_add(1, Ordering::Relaxed) + 1;
        if count == 1 || count.is_multiple_of(100) {
            persist_failure(log, &error.to_string(), count, false);
        }
    } else if recovering {
        persist_failure(log, "logging resumed", ERRORS.load(Ordering::Relaxed), true);
    }
    LOG_CPU_NS.fetch_add(thread_cpu_ns().saturating_sub(cpu), Ordering::Relaxed);
    if rotated || writer.last_prune.elapsed() > Duration::from_secs(30) {
        writer.last_prune = Instant::now();
        if let Some(tx) = MAINTENANCE.get() {
            let _ = tx.try_send(());
        }
    }
}

fn health_path(process: &str) -> PathBuf {
    isocast_protocol::api::setup_path().with_extension(format!("{process}.diagnostics-health.json"))
}

fn persist_failure(log: &Logger, error: &str, count: u64, recovered: bool) {
    let path = health_path(&log.process);
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    if let Ok(mut file) = options.open(&temp) {
        let prior = std::fs::read(&path)
            .ok()
            .and_then(|s| serde_json::from_slice::<Value>(&s).ok())
            .unwrap_or(Value::Null);
        let mut events = prior["events"].as_array().cloned().unwrap_or_else(|| {
            if prior["launch"].is_string() {
                vec![prior.clone()]
            } else {
                Vec::new()
            }
        });
        let previous = events.iter().find(|v| v["launch"] == log.launch).cloned();
        events.retain(|v| v["launch"] != log.launch);
        let error = previous
            .as_ref()
            .and_then(|v| v["error"].as_str())
            .unwrap_or(error);
        events.push(json!({"launch":log.launch,"time":chrono::Utc::now().to_rfc3339(),"error":error,"write_errors":count,"recovered":recovered}));
        if events.len() > 5 {
            events.drain(..events.len() - 5);
        }
        let value = json!({"events":events});
        if writeln!(file, "{value}").is_ok() {
            let _ = std::fs::rename(temp, path);
        }
    }
}

#[cfg(unix)]
fn thread_cpu_ns() -> u64 {
    let mut time = std::mem::MaybeUninit::<libc::timespec>::zeroed();
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, time.as_mut_ptr()) } != 0 {
        return 0;
    }
    let time = unsafe { time.assume_init() };
    time.tv_sec.max(0) as u64 * 1_000_000_000 + time.tv_nsec.max(0) as u64
}

#[cfg(not(unix))]
fn thread_cpu_ns() -> u64 {
    0
}

/// Preserve the CLI's human stderr while also recording the same, redacted text.
pub fn diagnostic(level: &str, component: &str, message: String) {
    emit(level, component, &message, Value::Null);
    if LOGGER.get().is_none_or(|l| l.console) {
        let _ = writeln!(
            std::io::stderr(),
            "{}",
            redact::Redactor::default().text(&message)
        );
    }
}

/// Isolated developer exercise of slow diagnostic storage, not slow media
/// storage: AFTER_MS:DURATION_MS, once, while the real writer lock is held.
fn budget_write_delay(component: &str, message: &str) {
    static BEGAN: OnceLock<Instant> = OnceLock::new();
    static FIRED: AtomicBool = AtomicBool::new(false);
    if !developer()
        || std::env::var_os("ISOCAST_SOCKET").is_none()
        || component != "isocast_platform_macos::frames"
        || !message.starts_with("frame budget")
    {
        return;
    }
    let Some((after, duration)) =
        std::env::var("ISOCAST_BUDGET_LOG_STALL_MS")
            .ok()
            .and_then(|value| {
                let (after, duration) = value.split_once(':')?;
                Some((
                    after.parse::<u64>().ok()?,
                    duration.parse::<u64>().ok()?.min(10_000),
                ))
            })
    else {
        return;
    };
    if BEGAN.get_or_init(Instant::now).elapsed() < Duration::from_millis(after)
        || FIRED.swap(true, Ordering::AcqRel)
    {
        return;
    }
    let _ = writeln!(
        std::io::stderr(),
        "injected diagnostic writer delay {duration} ms begins"
    );
    std::thread::sleep(Duration::from_millis(duration));
    let _ = writeln!(std::io::stderr(), "injected diagnostic writer delay ends");
}

#[macro_export]
macro_rules! diagnostic {
    ($level:literal; $($arg:tt)*) => { $crate::diagnostic($level,module_path!(),format!($($arg)*)) };
    () => { $crate::diagnostic("info",module_path!(), String::new()) };
    ($($arg:tt)*) => { $crate::diagnostic("info",module_path!(), format!($($arg)*)) };
}

pub fn files(folder: &Path) -> Vec<PathBuf> {
    let mut files: Vec<_> = std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.symlink_metadata().is_ok_and(|m| m.is_file()))
        .collect();
    files.sort();
    files
}

fn abnormal_runs(folder: &Path, process: &str) -> Vec<Value> {
    let mut found = Vec::new();
    for path in files(folder)
        .iter()
        .rev()
        .filter(|p| p.extension().is_some_and(|e| e == "run"))
    {
        let Ok(mut file) = OpenOptions::new().read(true).write(true).open(path) else {
            continue;
        };
        if file.try_lock().is_err() {
            continue;
        }
        let mut text = String::new();
        if file.read_to_string(&mut text).is_err() {
            continue;
        }
        let Some(value) = text
            .lines()
            .next()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
        else {
            continue;
        };
        if value["process"] != process {
            continue;
        }
        if process == "isocastd"
            && value["version"] == 2
            && !text.lines().any(|l| l == "{\"ready\":true}")
        {
            continue;
        }
        if text.lines().any(|l| l == "{\"clean\":true}") {
            break;
        }
        found.push(value);
        if found.len() == 5 {
            break;
        }
    }
    found
}

fn prune(folder: &Path) {
    let files = files(folder)
        .into_iter()
        .filter(|p| {
            let name = p.file_name().unwrap().to_string_lossy();
            name.len() > 16
                && name.as_bytes()[..8].iter().all(u8::is_ascii_digit)
                && name.as_bytes()[8] == b'T'
                && (name.ends_with(".run")
                    || name.ends_with(".jsonl")
                    || name.ends_with(".crash.json"))
        })
        .collect::<Vec<_>>();
    let family = |path: &Path| -> &'static str {
        let name = path.file_name().unwrap().to_string_lossy();
        if name.contains("-developer-") {
            "developer"
        } else if name.contains("-isocastd") {
            "daemon"
        } else if name.contains("-isocast-ui") {
            "ui"
        } else if name.contains("-isocast-app") {
            "app"
        } else {
            "cli"
        }
    };
    let mut protected = std::collections::HashSet::new();
    let mut retired = Vec::new();
    let mut counts = std::collections::HashMap::<String, usize>::new();
    let mut faults = std::collections::HashMap::<String, usize>::new();
    for path in files
        .iter()
        .rev()
        .filter(|p| p.extension().is_some_and(|e| e == "run"))
    {
        let Ok(mut marker) = OpenOptions::new().read(true).write(true).open(path) else {
            continue;
        };
        let launch = path.file_stem().unwrap().to_string_lossy().into_owned();
        let active = marker.try_lock().is_err();
        let mut text = String::new();
        let _ = marker.read_to_string(&mut text);
        let crash_files = files
            .iter()
            .filter(|p| {
                p.file_name().is_some_and(|n| {
                    n.to_string_lossy().starts_with(&launch)
                        && n.to_string_lossy().ends_with(".crash.json")
                })
            })
            .collect::<Vec<_>>();
        let abnormal =
            !active && !text.lines().any(|l| l == "{\"clean\":true}") || !crash_files.is_empty();
        let recent = path
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_some_and(|age| age < MAX_AGE);
        let count = faults.entry(family(path).into()).or_default();
        let protect_fault =
            abnormal && recent && *count < if family(path) == "daemon" { 3 } else { 1 };
        if protect_fault {
            *count += 1;
        }
        if active || protect_fault {
            protected.insert(path.clone());
            for kind in ["native", "developer", "structured"] {
                if let Some(current) = files.iter().rev().find(|p| {
                    p.extension().is_some_and(|e| e == "jsonl")
                        && p.file_name().is_some_and(|n| {
                            let n = n.to_string_lossy();
                            let of = if n.contains("-native-") {
                                "native"
                            } else if n.contains("-developer-") {
                                "developer"
                            } else {
                                "structured"
                            };
                            n.starts_with(&launch) && of == kind
                        })
                }) {
                    protected.insert(current.clone());
                }
            }
            for crash in crash_files.into_iter().rev().take(3) {
                protected.insert(crash.clone());
            }
        }
        if !active
            && let Some(process) = text
                .lines()
                .next()
                .and_then(|l| serde_json::from_str::<Value>(l).ok())
                .and_then(|v| v["process"].as_str().map(String::from))
        {
            let count = counts.entry(process).or_default();
            *count += 1;
            if *count > 20 {
                retired.push(launch);
            }
        }
    }
    let now = SystemTime::now();
    let mut totals = std::collections::HashMap::<&str, u64>::new();
    for path in &files {
        *totals.entry(family(path)).or_default() += path.metadata().map_or(0, |m| m.len());
    }
    for path in files {
        if protected.contains(&path) {
            continue;
        }
        let Ok(meta) = path.metadata() else { continue };
        let old = meta
            .modified()
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > MAX_AGE);
        let retired = path
            .file_name()
            .is_some_and(|n| retired.iter().any(|l| n.to_string_lossy().starts_with(l)));
        let total = totals.entry(family(&path)).or_default();
        let budget = match family(&path) {
            "daemon" => 40 << 20,
            "developer" => DEVELOPER_BUDGET,
            _ => 8 << 20,
        };
        if !old && !retired && *total <= budget {
            continue;
        }
        // Do not remove a live process's marker or current segment. Earlier
        // segments can be retired even during an exceptionally long launch.
        if let Some(log) = LOGGER.get()
            && path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(&log.launch))
        {
            let guard = log.writer.lock().unwrap_or_else(|p| p.into_inner());
            let Some(writer) = guard.as_ref() else {
                continue;
            };
            if path.extension().is_some_and(|e| e == "run")
                || path.ends_with(format!("{}-{:04}.jsonl", log.launch, writer.segment))
            {
                continue;
            }
            drop(guard);
            let dev = log.developer.lock().unwrap_or_else(|p| p.into_inner());
            if dev.file.is_some()
                && path.ends_with(format!(
                    "{}-developer-{:04}.jsonl",
                    log.launch,
                    dev.segment.saturating_sub(1)
                ))
            {
                continue;
            }
        }
        if path.extension().is_some_and(|e| e == "run") {
            let Ok(file) = OpenOptions::new().read(true).write(true).open(&path) else {
                continue;
            };
            if file.try_lock().is_err() {
                continue;
            }
        }
        if std::fs::remove_file(&path).is_ok() {
            *total = total.saturating_sub(meta.len());
        }
    }
}
