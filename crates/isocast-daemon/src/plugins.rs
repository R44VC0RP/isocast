//! The plugin registry: installed plugins, what they were granted, and
//! their processes (docs/scenes-as-code.md, "7. Plugins").
//!
//! The plugins folder holds a folder per plugin:
//!
//! ```text
//! plugins/<id>/install.toml   where it came from, what it was granted, on or off
//! plugins/<id>/plugin/        its files, copied at install (absent when linked)
//! plugins/<id>/data/          the only place its process may write (its HOME)
//! plugins/<id>/socket         where its process reaches isocastd
//! plugins/<id>/log            its process's output, and its components' log lines
//! plugins/<id>/pid            its running process, to stop one a crashed isocastd left
//! plugins/<id>/compiled/      its WebAssembly components' machine code (only isocastd writes it)
//! ```
//!
//! WebAssembly audio filters are compiled and checked against their world
//! when the plugin is added (a component that can't run is refused), and
//! the engine is told which are installed (`isocast_engine::wasm`)
//! whenever plugins change.
//!
//! A process plugin runs sandboxed, in its own process group, without
//! isocastd's privacy grants, and talks to isocastd only through its own
//! socket, where every call is checked against its grant. A connection on
//! the main socket from one of its processes (when the sandbox isn't
//! available) is scoped the same way, by its peer process and parents. If
//! it exits on its own it is restarted (1 s, doubling to 60 s, counted from
//! its last start), and turned off after 5 crashes in 5 minutes.

use isocast_ipc::{Listener as UnixListener, Stream as UnixStream};
use parking_lot::{Condvar, Mutex, RwLock};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use isocast_engine::wasm::{self, AudioFilterPlugin};
use isocast_protocol::api::{Check, CheckStatus, Event, Method, RpcError, plugins_path};
use isocast_protocol::plugin::{
    AudioFilterSpec, Installed, KindClass, MANIFEST, Manifest, Permissions, PluginAction,
    PluginAddParams, PluginChange, PluginEnableParams, PluginInfo, PluginState, Plugins,
    ProcessInfo, ProcessState, RecordingsAccess, Version,
};
use isocast_protocol::{Issue, Setup};
use serde::{Deserialize, Serialize};

/// How long reading a plugin's manifest may take: a folder macOS protects
/// (Documents, Downloads, Desktop) waits for a permission prompt.
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a process has to exit after SIGTERM before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(3);
/// Crashes within this window that turn a plugin off.
const CRASH_WINDOW: Duration = Duration::from_secs(5 * 60);
const MAX_CRASHES: usize = 5;
/// The longest pause before a restart.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A log over this is moved to `log.1` when the process starts.
const MAX_LOG: u64 = 10 << 20;
/// A plugin connection that stops reading is let go after this.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// A folder bigger than this isn't copied (use `--link`).
const MAX_COPY_FILES: usize = 20_000;
const MAX_COPY_BYTES: u64 = 1 << 30;
/// How often the recordings folders are looked at again for the sandboxes
/// of running plugins, for changes no request makes: a standalone
/// `isocast record` into a new folder, or a folder named like a project
/// made while idle (#56).
const SANDBOX_POLL: Duration = Duration::from_secs(5);

/// What isocastd gives the registry: a way to tell watchers, and to serve
/// a plugin's connection with its scope.
pub struct Hooks {
    pub publish: Box<dyn Fn(Event) + Send + Sync>,
    pub serve: Box<dyn Fn(UnixStream, Arc<Scope>) + Send + Sync>,
    /// The recordings folders, for `permissions.recordings`.
    pub recording_roots: Box<dyn Fn() -> Vec<PathBuf> + Send + Sync>,
}

/// The installed plugins.
pub struct Registry(Arc<Shared>);

impl Default for Registry {
    fn default() -> Self {
        Self(Arc::new(Shared {
            folder: plugins_path(),
            entries: Mutex::default(),
            pids: Mutex::default(),
            hooks: OnceLock::new(),
            admin: Mutex::new(()),
            polling: std::sync::Once::new(),
        }))
    }
}

struct Shared {
    folder: PathBuf,
    entries: Mutex<BTreeMap<String, Entry>>,
    /// Running plugin processes by process id, to scope connections on the
    /// main socket.
    pids: Mutex<HashMap<u32, Arc<Scope>>>,
    hooks: OnceLock<Hooks>,
    /// Held through each add, remove, enable and disable.
    admin: Mutex<()>,
    /// The sandbox poll (`SANDBOX_POLL`) runs, from the first process on.
    polling: std::sync::Once,
}

/// One installed plugin.
struct Entry {
    id: String,
    /// `plugins/<id>`.
    dir: PathBuf,
    record: Record,
    manifest: Result<Manifest, String>,
    scope: Arc<Scope>,
    /// Its process's supervisor, while it runs or restarts.
    process: Option<Arc<Process>>,
    /// Its socket, while its process may run.
    listener: Option<Listener>,
}

/// `install.toml`: what isocastd keeps about an installed plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    /// The plugin's name and version when it was added (shown if its
    /// manifest can't be read any more).
    name: String,
    version: String,
    /// The folder it was added from.
    source: PathBuf,
    /// It runs from `source` rather than a copy.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    linked: bool,
    #[serde(default = "yes")]
    enabled: bool,
    /// Why it was turned off, if not by hand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    disabled_reason: Option<String>,
    /// What it was granted.
    #[serde(default)]
    granted: Permissions,
}

fn yes() -> bool {
    true
}

/// Who a plugin connection belongs to, and what it may call.
pub struct Scope {
    pub id: String,
    methods: RwLock<Vec<String>>,
    /// Data key prefixes it may read and write.
    data_read: Vec<String>,
    data_write: Vec<String>,
    revoked: AtomicBool,
    /// Its open connections, to cut them when it is removed or turned off.
    connections: Mutex<HashMap<u64, UnixStream>>,
    next: AtomicU64,
}

impl Scope {
    fn new(id: &str, granted: &Permissions) -> Self {
        Self {
            id: id.to_string(),
            methods: RwLock::new(granted.methods.clone()),
            data_read: granted.data_read.clone(),
            data_write: granted.data_write.clone(),
            revoked: AtomicBool::new(false),
            connections: Mutex::default(),
            next: AtomicU64::new(0),
        }
    }

    /// Why a data call is refused: a key outside the prefixes the plugin
    /// may read (`data_read`) or write (`data_write`).
    fn data_refusal(&self, method: &Method) -> Option<String> {
        let allowed = |granted: &[String], key: &str| {
            granted
                .iter()
                .any(|prefix| isocast_protocol::data::under(key, prefix))
        };
        let refuse = |field: &str, granted: &[String], what: &str| {
            Some(format!(
                "{field}: {} may {what} data keys under: {}",
                self.id,
                if granted.is_empty() {
                    "nothing (it wasn't granted any)".to_string()
                } else {
                    granted.join(", ")
                }
            ))
        };
        match method {
            Method::DataSet(params) => params
                .values
                .keys()
                .find(|key| !allowed(&self.data_write, key))
                .and_then(|key| {
                    refuse(
                        &format!("data_write; params.values[{key:?}]"),
                        &self.data_write,
                        "write",
                    )
                }),
            Method::DataDelete(params) => params
                .keys
                .iter()
                .position(|key| !allowed(&self.data_write, key))
                .and_then(|i| {
                    refuse(
                        &format!("data_write; params.keys[{i}]"),
                        &self.data_write,
                        "write",
                    )
                }),
            Method::DataGet(params) => {
                let params = params.clone().unwrap_or_default();
                if let Some(i) = params
                    .keys
                    .iter()
                    .position(|key| !allowed(&self.data_read, key))
                {
                    return refuse(
                        &format!("data_read; params.keys[{i}]"),
                        &self.data_read,
                        "read",
                    );
                }
                match &params.prefix {
                    Some(prefix) if !allowed(&self.data_read, prefix.trim_end_matches('.')) => {
                        refuse("data_read; params.prefix", &self.data_read, "read")
                    }
                    None if params.keys.is_empty() => {
                        refuse("data_read; give keys or a prefix", &self.data_read, "read")
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Why a call of `method` is refused, if it is: a method the plugin
    /// wasn't granted (`methods.shutdown: deck may call: get, watch`), or
    /// the plugin was removed or turned off. Checked on the request as
    /// parsed, so the method checked is the one that runs.
    pub fn refusal(&self, method: &Method) -> Option<String> {
        if self.revoked.load(Ordering::SeqCst) {
            return Some(format!(
                "the plugin {} was removed or turned off, so it can't call isocastd",
                self.id
            ));
        }
        let value = serde_json::to_value(method).ok();
        let name = value
            .as_ref()
            .and_then(|value| value.get("method"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?");
        // Never, even if granted before it became ungrantable (an
        // install.toml from an earlier version).
        if let Some(reason) = isocast_protocol::plugin::ungrantable_reason(name) {
            return Some(format!(
                "methods.{name}: {} can't call {name}: {reason}",
                self.id
            ));
        }
        let methods = self.methods.read();
        if methods.iter().any(|m| *m == name) {
            return self.data_refusal(method);
        }
        let method = name;
        Some(format!(
            "methods.{method}: {} may call: {}",
            self.id,
            if methods.is_empty() {
                "nothing".to_string()
            } else {
                methods.join(", ")
            }
        ))
    }

    /// Keep `stream` to cut it off later; dropping the guard forgets it.
    pub fn track(self: &Arc<Self>, stream: &UnixStream) -> Option<Tracked> {
        let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
        let clone = stream.try_clone().ok()?;
        let key = self.next.fetch_add(1, Ordering::SeqCst);
        self.connections.lock().insert(key, clone);
        Some(Tracked {
            scope: self.clone(),
            key,
        })
    }

    fn revoke(&self) {
        self.revoked.store(true, Ordering::SeqCst);
        for (_, stream) in self.connections.lock().drain() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }
}

/// A plugin connection being served; forgotten when dropped.
pub struct Tracked {
    scope: Arc<Scope>,
    key: u64,
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.scope.connections.lock().remove(&self.key);
    }
}

/// A plugin's socket, served while its process may run.
struct Listener {
    path: PathBuf,
    stop: Arc<AtomicBool>,
}

impl Listener {
    fn start(path: &Path, scope: Arc<Scope>, shared: &Arc<Shared>) -> io::Result<Self> {
        // Another isocastd sharing this plugins folder runs it already.
        if UnixStream::connect(path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "another isocastd is serving it (they share this plugins folder)",
            ));
        }
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)?;
        let stop = Arc::new(AtomicBool::new(false));
        let weak = Arc::downgrade(shared);
        thread::spawn({
            let stop = stop.clone();
            move || {
                for connection in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let (Ok(connection), Some(shared)) = (connection, weak.upgrade()) else {
                        continue;
                    };
                    if let Some(hooks) = shared.hooks.get() {
                        (hooks.serve)(connection, scope.clone());
                    }
                }
            }
        });
        Ok(Self {
            path: path.to_path_buf(),
            stop,
        })
    }

    fn stop(self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept so the thread sees it should stop.
        let _ = UnixStream::connect(&self.path);
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A plugin's process and its supervisor.
struct Process {
    id: String,
    command: Vec<String>,
    state: Mutex<Run>,
    changed: Condvar,
    stop: AtomicBool,
    /// Its process was stopped to start again with new sandbox rules
    /// (`Registry::refresh_sandboxes`): not a crash.
    refresh: AtomicBool,
}

struct Run {
    phase: ProcessState,
    pid: Option<u32>,
    restarts: u32,
    crashes: VecDeque<Instant>,
    last_exit: Option<(String, Instant)>,
    sandboxed: bool,
    /// The recordings folders its sandbox was made with, while it runs
    /// sandboxed.
    recordings: Option<RecordingFolders>,
    /// The supervisor has finished.
    done: bool,
}

/// The recordings folders a sandbox is made with: the recordings folders
/// (resolved), and, for a `write` grant, the folders in them named like
/// projects that aren't (`isocast_protocol::project::named_like_but_not`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RecordingFolders {
    roots: Vec<PathBuf>,
    not_projects: Vec<PathBuf>,
}

/// A recordings folder as Seatbelt will match it: resolved, or for one not
/// made yet, its nearest existing parent resolved plus the rest. Its rules
/// are then in place before it is made, so making it (the first recording
/// into a new folder) restarts no plugin; made as a link somewhere else,
/// it resolves differently and `refresh_sandboxes` still restarts them.
fn resolved(root: &Path) -> Option<PathBuf> {
    if let Ok(real) = root.canonicalize() {
        return Some(real);
    }
    if !root.is_absolute() {
        return None;
    }
    let existing = root.ancestors().skip(1).find(|parent| parent.exists())?;
    let rest = root.strip_prefix(existing).ok()?;
    rest.components()
        .all(|part| matches!(part, std::path::Component::Normal(_)))
        .then(|| Some(existing.canonicalize().ok()?.join(rest)))?
}

impl RecordingFolders {
    fn new(roots: &[PathBuf], granted: &Permissions) -> Self {
        let mut roots: Vec<PathBuf> = roots.iter().filter_map(|root| resolved(root)).collect();
        roots.sort();
        roots.dedup();
        let not_projects = match granted.recordings {
            Some(RecordingsAccess::Write) => roots
                .iter()
                .flat_map(|root| isocast_protocol::project::named_like_but_not(root))
                .collect(),
            _ => Vec::new(),
        };
        Self {
            roots,
            not_projects,
        }
    }

    /// A folder here that `self`'s sandbox has no rule for.
    fn adds_to(&self, earlier: &Self) -> bool {
        self.roots.iter().any(|r| !earlier.roots.contains(r))
            || self
                .not_projects
                .iter()
                .any(|p| !earlier.not_projects.contains(p))
    }

    /// Whether a sandbox made with `earlier` misses something there is
    /// now: a folder (`adds_to`), or a project in a folder it denies
    /// because it wasn't one yet (a recording's project folder seen
    /// between being made and getting its project file).
    fn outdates(&self, earlier: &Self) -> bool {
        self.adds_to(earlier)
            || earlier.not_projects.iter().any(|folder| {
                !self.not_projects.contains(folder)
                    && isocast_protocol::project::read(folder).is_some()
            })
    }
}

impl Process {
    fn info(&self, dir: &Path) -> ProcessInfo {
        let run = self.state.lock();
        ProcessInfo {
            state: run.phase,
            command: self.command.clone(),
            pid: run.pid,
            restarts: run.restarts,
            last_exit: run
                .last_exit
                .as_ref()
                .map(|(how, when)| format!("{how} ({} s ago)", when.elapsed().as_secs())),
            sandboxed: run.sandboxed,
            socket: dir.join("socket"),
            log: dir.join("log"),
        }
    }

    /// Stop it: SIGTERM to its process group, then SIGKILL if it hasn't
    /// gone after a grace period. Waits for the supervisor to finish.
    fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let mut run = self.state.lock();
        self.changed.notify_all();
        if let Some(pid) = run.pid {
            signal_group(pid, Signal::Terminate);
        }
        let deadline = Instant::now() + STOP_GRACE;
        while !run.done {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            self.changed.wait_for(&mut run, left);
        }
        if !run.done {
            if let Some(pid) = run.pid {
                signal_group(pid, Signal::Kill);
            }
            self.changed
                .wait_while_for(&mut run, |run| !run.done, Duration::from_secs(2));
        }
        drop(run);
    }
}

/// How [`signal_group`] stops a plugin process group.
#[derive(Clone, Copy)]
enum Signal {
    /// SIGTERM: asks it to exit.
    Terminate,
    /// SIGKILL: ends it at once.
    Kill,
}

#[cfg(unix)]
fn signal_group(pid: u32, signal: Signal) {
    let signal = match signal {
        Signal::Terminate => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    // SAFETY: kill has no memory effects; a negative pid names the process
    // group the plugin leads (it was started in a group of its own).
    unsafe {
        libc::kill(-(pid as libc::pid_t), signal);
    }
}

/// On Windows no plugin process can be started ([`find_program`] and
/// [`spawn`] refuse before a child exists, and [`process_info`] knows no
/// leftover), so there is never a pid to signal.
#[cfg(windows)]
fn signal_group(pid: u32, _signal: Signal) {
    unreachable!(
        "plugin process {pid}: plugin processes can't be started on Windows, so there is none \
         to stop"
    );
}

fn failed(message: impl Into<String>) -> RpcError {
    RpcError {
        code: RpcError::FAILED,
        message: message.into(),
    }
}

fn not_permitted(message: impl Into<String>) -> RpcError {
    RpcError {
        code: RpcError::NOT_PERMITTED,
        message: message.into(),
    }
}

impl Registry {
    /// Load the installed plugins and start the processes that start on
    /// their own. Nothing happens (no folder, no threads) without plugins.
    pub fn start(&self, hooks: Hooks) {
        let shared = &self.0;
        let _ = shared.hooks.set(hooks);
        let Ok(dirs) = std::fs::read_dir(&shared.folder) else {
            return;
        };
        let _admin = shared.admin.lock();
        for dir in dirs.flatten() {
            let id = dir.file_name().to_string_lossy().into_owned();
            let dir = dir.path();
            if !dir.join("install.toml").exists() || !isocast_protocol::library::valid_id(&id) {
                continue;
            }
            stop_leftover(&dir);
            clean_staging(&dir);
            let record = match read_record(&dir) {
                Ok(record) => record,
                Err(err) => {
                    isocast_diagnostics::diagnostic!("error"; "isocastd: plugin {id}: {err}");
                    continue;
                }
            };
            let manifest = load_manifest(&id, &dir, &record);
            let scope = Arc::new(Scope::new(&id, &record.granted));
            let entry = Entry {
                id: id.clone(),
                dir,
                record,
                manifest,
                scope,
                process: None,
                listener: None,
            };
            shared.entries.lock().insert(id.clone(), entry);
            if let Err(err) = shared.run(&id) {
                isocast_diagnostics::diagnostic!("error"; "isocastd: plugin {id}: {}", err.message);
            }
        }
        shared.publish_kinds();
    }

    /// Problems and notes for a setup's uses of plugins' kinds (for
    /// `plan`): an installed plugin's kind used wrongly is a problem, one
    /// that is bypassed (not installed, turned off) a note.
    pub fn check_kinds(&self, setup: &Setup) -> (Vec<Issue>, Vec<String>) {
        let entries = self.0.entries.lock();
        let installed: BTreeMap<String, Installed> = entries
            .values()
            .filter_map(|entry| {
                let manifest = entry.manifest.as_ref().ok()?;
                Some((
                    entry.id.clone(),
                    Installed {
                        manifest,
                        bypassed: entry.bypassed(),
                    },
                ))
            })
            .collect();
        isocast_protocol::plugin::check_kinds(setup, &installed)
    }

    /// Stop every plugin process (isocastd is exiting).
    pub fn stop_all(&self) {
        let processes: Vec<Arc<Process>> = {
            let mut entries = self.0.entries.lock();
            entries
                .values_mut()
                .filter_map(|entry| {
                    if let Some(listener) = entry.listener.take() {
                        listener.stop();
                    }
                    entry.process.take()
                })
                .collect()
        };
        for process in processes {
            process.stop();
        }
    }

    /// Restart, in the background, each sandboxed plugin process whose
    /// sandbox has no rule for a recordings folder (or, with a `write`
    /// grant, a folder named like a project that isn't one) that exists
    /// now: Seatbelt rules can't change in a running process (#56).
    pub fn refresh_sandboxes(&self) {
        let shared = self.0.clone();
        let _ = thread::Builder::new()
            .name("plugin sandboxes".into())
            .spawn(move || shared.refresh_sandboxes());
    }

    /// The plugin a connection on the main socket belongs to, if its peer
    /// process or one of its parents is a plugin process isocastd started.
    pub fn scope_of_peer(&self, stream: &UnixStream) -> Option<Arc<Scope>> {
        let pids = self.0.pids.lock();
        if pids.is_empty() {
            return None;
        }
        let mut pid = peer_pid(stream)?;
        for _ in 0..32 {
            if let Some(scope) = pids.get(&pid) {
                return Some(scope.clone());
            }
            pid = match parent_pid(pid) {
                Some(parent) if parent > 1 && parent != pid => parent,
                _ => return None,
            };
        }
        None
    }

    pub fn list(&self) -> Plugins {
        let entries = self.0.entries.lock();
        Plugins {
            folder: self.0.folder.clone(),
            plugins: entries.values().map(Entry::info).collect(),
        }
    }

    /// A `doctor` check on how plugin processes are isolated, if any are
    /// installed.
    pub fn doctor_check(&self) -> Option<Check> {
        let entries = self.0.entries.lock();
        let processes = entries
            .values()
            .filter(
                |entry| matches!(&entry.manifest, Ok(m) if m.process.is_some() && m.compatible()),
            )
            .count();
        if processes == 0 {
            return None;
        }
        Some(if sandbox_available() {
            Check {
                name: "plugins: sandbox".into(),
                status: CheckStatus::Ok,
                detail: format!(
                    "{processes} plugin process(es) run sandboxed (sandbox-exec), without \
                     isocastd's camera, microphone and screen grants"
                ),
                fix: None,
            }
        } else {
            Check {
                name: "plugins: sandbox".into(),
                status: CheckStatus::Warning,
                detail: format!(
                    "sandbox-exec isn't on this Mac, so {processes} plugin process(es) run \
                     unsandboxed: they can read your files and use the network (their calls to \
                     isocastd are still limited to their grants)"
                ),
                fix: Some(
                    "remove plugins you don't trust (`isocast plugin remove ID`)".to_string(),
                ),
            }
        })
    }

    pub fn add(&self, params: PluginAddParams) -> Result<PluginChange, RpcError> {
        let shared = &self.0;
        let _admin = shared.admin.lock();
        let source = params.path.clone();
        if !source.is_absolute() {
            return Err(failed(format!(
                "path: {:?} isn't an absolute path, and isocastd doesn't run in your folder; \
                 give the plugin's folder as an absolute path",
                source.display().to_string()
            )));
        }
        let manifest = read_in_time(&source)?;
        let id = manifest.id.clone();
        let old = shared
            .entries
            .lock()
            .get(&id)
            .map(|entry| (entry.record.clone(), entry.version()));
        let action = match &old {
            None => PluginAction::Added,
            Some((_, installed)) => {
                let new = Version::parse(&manifest.version);
                match (new, Version::parse(installed)) {
                    (Some(new), Some(installed)) if new > installed => PluginAction::Upgraded,
                    _ if params.force => PluginAction::Reinstalled,
                    _ => {
                        return Err(RpcError {
                            code: RpcError::WRONG_STATE,
                            message: format!(
                                "{id} {} is already installed (this is {}); give --force (MCP: \
                                 force) to install this one over it, or `isocast plugin remove \
                                 {id}` first",
                                installed, manifest.version
                            ),
                        });
                    }
                }
            }
        };
        let granted = check_grant(&manifest, params.grant.as_ref(), old.as_ref().map(|o| &o.0))?;
        if let Some(process) = &manifest.process {
            find_program(&process.command[0], &source)
                .map_err(|message| failed(format!("{MANIFEST}: process.command[0]: {message}")))?;
        }
        let dir = shared.folder.join(&id);
        // Its WebAssembly components are compiled and checked against their
        // world first: one that can't run refuses the plugin, leaving any
        // installed version as it was.
        if manifest.compatible() {
            for (i, spec) in manifest.audio_filters.iter().enumerate() {
                if let Err(err) = wasm::prepare(&audio_filter(&id, &dir, &source, spec, None)) {
                    // A plugin that wasn't installed leaves nothing behind.
                    if old.is_none() {
                        remove_tree(&dir, "compiled");
                        let _ = std::fs::remove_dir(&dir);
                    }
                    return Err(failed(format!(
                        "{MANIFEST}: audio_filters[{i}].component: {err}"
                    )));
                }
            }
        }
        let mut notes = Vec::new();
        std::fs::create_dir_all(dir.join("data"))
            .map_err(|err| failed(format!("couldn't make {}: {err}", dir.display())))?;
        // Copied first, so a copy that fails leaves the installed version
        // running.
        let staged = if params.link {
            None
        } else {
            Some(stage_copy(&source, &dir).map_err(|err| {
                failed(format!(
                    "couldn't copy {} into {}: {err}",
                    source.display(),
                    dir.display()
                ))
            })?)
        };
        // Its process stops while its files change.
        let running = shared.entries.lock().get_mut(&id).map(|entry| {
            entry.scope.revoke();
            (entry.listener.take(), entry.process.take())
        });
        if let Some((listener, process)) = running {
            if let Some(listener) = listener {
                listener.stop();
            }
            if let Some(process) = process {
                process.stop();
            }
        }
        let source = match staged {
            Some(staged) => {
                put_in_place(&dir, &staged).map_err(|err| {
                    failed(format!("couldn't install into {}: {err}", dir.display()))
                })?;
                source
            }
            None => {
                remove_tree(&dir, "plugin");
                source.canonicalize().unwrap_or(source)
            }
        };
        let record = Record {
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            source,
            linked: params.link,
            enabled: true,
            disabled_reason: None,
            granted: granted.clone(),
        };
        write_record(&dir, &record)
            .map_err(|err| failed(format!("couldn't save {}: {err}", dir.display())))?;
        if !manifest.compatible() {
            notes.push(format!(
                "{id} was made for plugin API {}, but this Isocast runs {}: it is installed as \
                 incompatible, and isn't run",
                manifest.api,
                api_versions()
            ));
        }
        let waiting: Vec<&str> = manifest
            .kinds()
            .iter()
            .filter(|kind| kind.what != KindClass::AudioFilter)
            .map(|kind| kind.what.section())
            .fold(Vec::new(), |mut list, section| {
                if !list.contains(&section) {
                    list.push(section);
                }
                list
            });
        if manifest.compatible() && !manifest.audio_filters.is_empty() {
            // Machine code for the installed copy (the same as the one just
            // checked unless copying changed its time).
            let folder = match params.link {
                true => record.source.clone(),
                false => dir.join("plugin"),
            };
            for spec in &manifest.audio_filters {
                let _ = wasm::prepare(&audio_filter(&id, &dir, &folder, spec, None));
            }
            let kinds: Vec<String> = manifest
                .audio_filters
                .iter()
                .map(|spec| {
                    let mut params: Vec<&str> = spec.params.keys().map(String::as_str).collect();
                    if spec.sidechain {
                        params.push(isocast_protocol::plugin::SIDECHAIN_PARAM);
                    }
                    format!("{id}/{} ({})", spec.id, params.join(", "))
                })
                .collect();
            notes.push(format!(
                "its audio filters: {}; use one in a source's audio_filters, like {{ kind = \
                 \"{id}/{}\" }}; its log lines go to {}",
                kinds.join(", "),
                manifest.audio_filters[0].id,
                dir.join("log").display()
            ));
        }
        if !waiting.is_empty() {
            notes.push(format!(
                "this Isocast doesn't run plugin {} yet, so setups can't use its kinds until it \
                 does",
                waiting.join(", ")
            ));
        }
        if manifest.process.is_some() && manifest.compatible() {
            if !sandbox_available() {
                notes.push(
                    "sandbox-exec isn't on this Mac, so its process runs unsandboxed".to_string(),
                );
            }
            notes.push(format!(
                "its process may call: {}; its output goes to {}",
                granted.methods_text(),
                dir.join("log").display()
            ));
        }
        let entry = Entry {
            id: id.clone(),
            dir: dir.clone(),
            scope: Arc::new(Scope::new(&id, &record.granted)),
            record,
            manifest: Ok(manifest),
            process: None,
            listener: None,
        };
        shared.entries.lock().insert(id.clone(), entry);
        if let Err(err) = shared.run(&id) {
            notes.push(err.message);
        }
        shared.changed(&id, action);
        let plugin = shared.info(&id).expect("just added");
        Ok(PluginChange {
            action,
            plugin,
            notes,
        })
    }

    pub fn remove(&self, id: &str) -> Result<PluginChange, RpcError> {
        let shared = &self.0;
        let _admin = shared.admin.lock();
        let entry = shared.entries.lock().remove(id);
        let Some(mut entry) = entry else {
            return Err(shared.unknown(id));
        };
        entry.scope.revoke();
        if let Some(listener) = entry.listener.take() {
            listener.stop();
        }
        if let Some(process) = entry.process.take() {
            process.stop();
        }
        let mut info = entry.info();
        if let Some(process) = &mut info.process {
            process.state = ProcessState::Stopped;
            process.pid = None;
        }
        let mut notes = Vec::new();
        // Its data is kept if it wrote any; the rest goes.
        let data = entry.dir.join("data");
        if holds_files(&data) {
            let to = shared
                .folder
                .join(".removed")
                .join(format!("{id}-{}", unix_seconds()));
            if std::fs::create_dir_all(&to).is_ok()
                && std::fs::rename(&data, to.join("data")).is_ok()
            {
                notes.push(format!("its data folder was kept in {}", to.display()));
            }
        }
        if let Err(err) = remove_plugin_dir(&shared.folder, id) {
            notes.push(format!(
                "couldn't delete {}: {err}",
                shared.folder.join(id).display()
            ));
        }
        shared.publish_kinds();
        if let Some(hooks) = shared.hooks.get() {
            (hooks.publish)(Event::PluginChanged {
                id: id.to_string(),
                action: PluginAction::Removed,
                state: info.state,
                process: info.process.as_ref().map(|p| p.state),
            });
        }
        Ok(PluginChange {
            action: PluginAction::Removed,
            plugin: info,
            notes,
        })
    }

    pub fn set_enabled(&self, params: PluginEnableParams) -> Result<PluginChange, RpcError> {
        let shared = &self.0;
        let _admin = shared.admin.lock();
        let id = params.id.as_str();
        let stopping = {
            let mut entries = shared.entries.lock();
            let Some(entry) = entries.get_mut(id) else {
                drop(entries);
                return Err(shared.unknown(id));
            };
            // Already so (and, if on, its process running): nothing to do.
            let running = entry
                .process
                .as_ref()
                .is_some_and(|process| !process.state.lock().done);
            if entry.record.enabled == params.enabled && (!params.enabled || running) {
                let info = entry.info();
                return Ok(PluginChange {
                    action: if params.enabled {
                        PluginAction::Enabled
                    } else {
                        PluginAction::Disabled
                    },
                    plugin: info,
                    notes: vec![format!(
                        "{id} was already {}",
                        if params.enabled { "on" } else { "off" }
                    )],
                });
            }
            entry.record.enabled = params.enabled;
            entry.record.disabled_reason = None;
            write_record(&entry.dir, &entry.record)
                .map_err(|err| failed(format!("couldn't save {}: {err}", entry.dir.display())))?;
            if params.enabled {
                None
            } else {
                entry.scope.revoke();
                Some((entry.listener.take(), entry.process.take()))
            }
        };
        let mut notes = Vec::new();
        match stopping {
            Some((listener, process)) => {
                if let Some(listener) = listener {
                    listener.stop();
                }
                if let Some(process) = process {
                    process.stop();
                }
            }
            None => {
                // A fresh start: a manifest that changed (linked), a new
                // crash count.
                {
                    let mut entries = shared.entries.lock();
                    let entry = entries.get_mut(id).expect("checked");
                    entry.manifest = load_manifest(id, &entry.dir, &entry.record);
                    entry.scope = Arc::new(Scope::new(id, &entry.record.granted));
                    let old = (entry.listener.take(), entry.process.take());
                    drop(entries);
                    if let Some(listener) = old.0 {
                        listener.stop();
                    }
                    if let Some(process) = old.1 {
                        process.stop();
                    }
                }
                if let Err(err) = shared.run(id) {
                    notes.push(err.message);
                }
            }
        }
        let action = if params.enabled {
            PluginAction::Enabled
        } else {
            PluginAction::Disabled
        };
        shared.changed(id, action);
        Ok(PluginChange {
            action,
            plugin: shared.info(id).expect("checked"),
            notes,
        })
    }
}

impl Shared {
    fn unknown(&self, id: &str) -> RpcError {
        let entries = self.entries.lock();
        let ids: Vec<&str> = entries.keys().map(String::as_str).collect();
        failed(format!(
            "no plugin {id:?} is installed; installed: {}",
            if ids.is_empty() {
                "none".to_string()
            } else {
                ids.join(", ")
            }
        ))
    }

    fn info(&self, id: &str) -> Option<PluginInfo> {
        self.entries.lock().get(id).map(Entry::info)
    }

    /// Tell the engine which plugins' audio filters are installed, and
    /// which of them are bypassed.
    fn publish_kinds(&self) {
        let entries = self.entries.lock();
        let filters: Vec<AudioFilterPlugin> = entries
            .values()
            .flat_map(|entry| {
                let folder = entry.plugin_folder();
                let bypassed = entry.bypassed();
                let specs = match &entry.manifest {
                    Ok(manifest) => &manifest.audio_filters[..],
                    Err(_) => &[],
                };
                specs
                    .iter()
                    .map(|spec| {
                        audio_filter(&entry.id, &entry.dir, &folder, spec, bypassed.clone())
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        drop(entries);
        wasm::set_installed(filters);
    }

    fn changed(&self, id: &str, action: PluginAction) {
        self.publish_kinds();
        let Some(info) = self.info(id) else { return };
        if let Some(hooks) = self.hooks.get() {
            (hooks.publish)(Event::PluginChanged {
                id: id.to_string(),
                action,
                state: info.state,
                process: info.process.map(|p| p.state),
            });
        }
    }

    fn warn(&self, message: String) {
        isocast_diagnostics::diagnostic!("warn"; "isocastd: {message}");
        if let Some(hooks) = self.hooks.get() {
            (hooks.publish)(Event::Warning { message });
        }
    }

    /// Start the plugin's process (and its socket) if it should run: it
    /// has one, starts on its own, and is enabled and compatible.
    fn run(self: &Arc<Self>, id: &str) -> Result<(), RpcError> {
        let mut entries = self.entries.lock();
        let Some(entry) = entries.get_mut(id) else {
            return Ok(());
        };
        let Ok(manifest) = &entry.manifest else {
            return Ok(());
        };
        let Some(spec) = manifest.process.clone() else {
            return Ok(());
        };
        if !spec.autostart || !entry.record.enabled || !manifest.compatible() {
            return Ok(());
        }
        let socket = entry.dir.join("socket");
        let listener = Listener::start(&socket, entry.scope.clone(), self).map_err(|err| {
            failed(format!(
                "{id}'s process couldn't start: its socket {} couldn't be made ({err}); a \
                 shorter plugins folder (ISOCAST_PLUGINS) may help",
                socket.display()
            ))
        })?;
        let process = Arc::new(Process {
            id: id.to_string(),
            command: spec.command.clone(),
            state: Mutex::new(Run {
                phase: ProcessState::Running,
                pid: None,
                restarts: 0,
                crashes: VecDeque::new(),
                last_exit: None,
                sandboxed: false,
                recordings: None,
                done: false,
            }),
            changed: Condvar::new(),
            stop: AtomicBool::new(false),
            refresh: AtomicBool::new(false),
        });
        entry.listener = Some(listener);
        entry.process = Some(process.clone());
        let launch = Launch {
            dir: entry.dir.clone(),
            folder: entry.plugin_folder(),
            granted: entry.record.granted.clone(),
            scope: entry.scope.clone(),
        };
        drop(entries);
        let shared = self.clone();
        let supervised = process.clone();
        thread::Builder::new()
            .name(format!("plugin {id}"))
            .spawn(move || shared.supervise(&supervised, &launch))
            .map_err(|err| failed(format!("{id}'s process couldn't start: {err}")))?;
        self.polling.call_once(|| {
            let shared = self.clone();
            let _ = thread::Builder::new()
                .name("plugin sandbox poll".into())
                .spawn(move || {
                    loop {
                        thread::sleep(SANDBOX_POLL);
                        shared.refresh_sandboxes();
                    }
                });
        });
        // Replies say how the first start went.
        let mut run = process.state.lock();
        let _ = process.changed.wait_while_for(
            &mut run,
            |run| run.pid.is_none() && run.last_exit.is_none() && !run.done,
            Duration::from_secs(2),
        );
        Ok(())
    }

    /// Run a plugin's process until it is stopped: restart it when it
    /// exits on its own, with a growing pause, and turn the plugin off
    /// after too many crashes.
    fn supervise(self: &Arc<Self>, process: &Process, launch: &Launch) {
        let id = process.id.clone();
        let mut first = true;
        loop {
            if process.stop.load(Ordering::SeqCst) {
                break;
            }
            let started = Instant::now();
            // The recordings folders as they are now (one used later, or a
            // folder named like a project made later, restarts it:
            // `refresh_sandboxes`).
            let roots = self
                .hooks
                .get()
                .map(|hooks| (hooks.recording_roots)())
                .unwrap_or_default();
            let recordings = RecordingFolders::new(&roots, &launch.granted);
            let outcome = launch.start(&process.command, &recordings);
            let exit = match outcome {
                Ok((pid, sandboxed)) => {
                    self.pids.lock().insert(pid, launch.scope.clone());
                    {
                        let mut run = process.state.lock();
                        run.pid = Some(pid);
                        run.phase = ProcessState::Running;
                        run.sandboxed = sandboxed;
                        run.recordings = sandboxed.then_some(recordings);
                        process.changed.notify_all();
                    }
                    if !first {
                        self.changed(&id, PluginAction::Restarted);
                    }
                    let exit = wait(pid);
                    self.pids.lock().remove(&pid);
                    let _ = std::fs::remove_file(launch.dir.join("pid"));
                    let mut run = process.state.lock();
                    run.pid = None;
                    run.recordings = None;
                    exit
                }
                Err(err) => format!("couldn't start: {err}"),
            };
            first = false;
            if process.stop.load(Ordering::SeqCst) {
                break;
            }
            // Stopped for new sandbox rules: start it again at once.
            if process.refresh.swap(false, Ordering::SeqCst) {
                process.state.lock().restarts += 1;
                continue;
            }
            // It ended on its own: a crash.
            let now = Instant::now();
            let mut run = process.state.lock();
            run.crashes.push_back(now);
            while run
                .crashes
                .front()
                .is_some_and(|at| now.duration_since(*at) > CRASH_WINDOW)
            {
                run.crashes.pop_front();
            }
            run.last_exit = Some((exit.clone(), now));
            process.changed.notify_all();
            let crashes = run.crashes.len();
            if crashes >= MAX_CRASHES {
                run.phase = ProcessState::Stopped;
                drop(run);
                let reason = format!(
                    "its process ended {crashes} times in {} minutes (last: {exit}), so it was \
                     turned off; see {} and turn it on again with `isocast plugin enable {id}`",
                    CRASH_WINDOW.as_secs() / 60,
                    launch.dir.join("log").display()
                );
                self.turn_off(&id, &reason);
                self.warn(format!("plugin {id}: {reason}"));
                self.changed(&id, PluginAction::Disabled);
                break;
            }
            let pause = Duration::from_secs(1 << (crashes - 1).min(6)).min(MAX_BACKOFF);
            let at = started + pause;
            run.phase = ProcessState::Restarting;
            drop(run);
            self.warn(format!(
                "plugin {id}: its process ended ({exit}); starting it again in {:.0} s",
                at.saturating_duration_since(Instant::now()).as_secs_f64()
            ));
            self.changed(&id, PluginAction::Crashed);
            let mut run = process.state.lock();
            while !process.stop.load(Ordering::SeqCst) {
                let left = at.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                process.changed.wait_for(&mut run, left);
            }
            run.restarts += 1;
        }
        let mut run = process.state.lock();
        run.phase = ProcessState::Stopped;
        run.done = true;
        process.changed.notify_all();
    }

    fn refresh_sandboxes(&self) {
        let running: Vec<(Arc<Process>, Permissions)> = {
            let entries = self.entries.lock();
            entries
                .values()
                .filter_map(|e| Some((e.process.clone()?, e.record.granted.clone())))
                .collect()
        };
        if running.is_empty() {
            return;
        }
        let roots = self
            .hooks
            .get()
            .map(|hooks| (hooks.recording_roots)())
            .unwrap_or_default();
        for (process, granted) in running {
            let now = RecordingFolders::new(&roots, &granted);
            let pid = {
                let run = process.state.lock();
                match (&run.recordings, run.pid) {
                    (Some(earlier), Some(pid)) if now.outdates(earlier) => pid,
                    _ => continue,
                }
            };
            isocast_diagnostics::diagnostic!(
                "isocastd: plugin {}: restarting it so its sandbox has the recordings folders \
                 as they are now",
                process.id
            );
            process.refresh.store(true, Ordering::SeqCst);
            signal_group(pid, Signal::Terminate);
            let process = process.clone();
            thread::spawn(move || {
                thread::sleep(STOP_GRACE);
                if process.state.lock().pid == Some(pid) {
                    signal_group(pid, Signal::Kill);
                }
            });
        }
    }

    /// Turn a plugin off after it crashed too often (kept across restarts
    /// of isocastd until it is turned on again).
    fn turn_off(&self, id: &str, reason: &str) {
        let mut entries = self.entries.lock();
        let Some(entry) = entries.get_mut(id) else {
            return;
        };
        entry.record.enabled = false;
        entry.record.disabled_reason = Some(reason.to_string());
        entry.scope.revoke();
        if let Some(listener) = entry.listener.take() {
            listener.stop();
        }
        if let Err(err) = write_record(&entry.dir, &entry.record) {
            isocast_diagnostics::diagnostic!("error";
                "isocastd: plugin {id}: couldn't save its state: {err}"
            );
        }
    }
}

impl Entry {
    /// Where its files are: its copy, or the folder it links to.
    fn plugin_folder(&self) -> PathBuf {
        if self.record.linked {
            self.record.source.clone()
        } else {
            self.dir.join("plugin")
        }
    }

    fn version(&self) -> String {
        match &self.manifest {
            Ok(manifest) => manifest.version.clone(),
            Err(_) => self.record.version.clone(),
        }
    }

    /// Why its kinds are bypassed (after "the plugin <id>"), if they are.
    fn bypassed(&self) -> Option<String> {
        match &self.manifest {
            Err(err) => Some(format!("can't be read ({err})")),
            Ok(manifest) if !manifest.compatible() => Some(format!(
                "was made for plugin API {}, and this Isocast runs {}",
                manifest.api,
                api_versions()
            )),
            Ok(_) if !self.record.enabled => Some(format!(
                "is turned off (`isocast plugin enable {}` turns it on)",
                self.id
            )),
            Ok(_) => None,
        }
    }

    fn info(&self) -> PluginInfo {
        let (state, reason) =
            match &self.manifest {
                Err(err) => (PluginState::Failed, Some(err.clone())),
                Ok(manifest) if !manifest.compatible() => (
                    PluginState::Incompatible,
                    Some(format!(
                        "made for plugin API {}; this Isocast runs {}",
                        manifest.api,
                        api_versions()
                    )),
                ),
                Ok(_) if !self.record.enabled => (
                    PluginState::Disabled,
                    Some(self.record.disabled_reason.clone().unwrap_or_else(|| {
                        "turned off (`isocast plugin enable` turns it on)".into()
                    })),
                ),
                Ok(_) => (PluginState::Ok, None),
            };
        let manifest = self.manifest.as_ref().ok();
        let process = manifest
            .and_then(|m| m.process.as_ref())
            .map(|spec| match &self.process {
                Some(process) => process.info(&self.dir),
                None => ProcessInfo {
                    state: ProcessState::Stopped,
                    command: spec.command.clone(),
                    pid: None,
                    restarts: 0,
                    last_exit: None,
                    sandboxed: sandbox_available(),
                    socket: self.dir.join("socket"),
                    log: self.dir.join("log"),
                },
            });
        PluginInfo {
            id: self.id.clone(),
            name: manifest.map_or_else(|| self.record.name.clone(), |m| m.name.clone()),
            version: manifest.map_or_else(|| self.record.version.clone(), |m| m.version.clone()),
            api: manifest.map_or(0, |m| m.api),
            description: manifest.map(|m| m.description.clone()).unwrap_or_default(),
            path: self.plugin_folder(),
            linked: self.record.linked,
            kinds: manifest.map(Manifest::kinds).unwrap_or_default(),
            state,
            reason,
            log: manifest
                .is_some_and(|m| m.process.is_some() || !m.audio_filters.is_empty())
                .then(|| self.dir.join("log")),
            process,
            granted: self.record.granted.clone(),
        }
    }
}

/// A plugin's audio filter as the engine runs it: its component in the
/// plugin's files (`folder`), its machine code and log in its folder in
/// the plugins folder (`dir`).
fn audio_filter(
    id: &str,
    dir: &Path,
    folder: &Path,
    spec: &AudioFilterSpec,
    bypassed: Option<String>,
) -> AudioFilterPlugin {
    AudioFilterPlugin {
        kind: format!("{id}/{}", spec.id),
        component: folder.join(&spec.component),
        cache: dir.join("compiled").join(format!("{}.cwasm", spec.id)),
        sidechain: spec.sidechain,
        params: spec.params.clone(),
        log: dir.join("log"),
        bypassed,
    }
}

/// What a plugin process is started with.
struct Launch {
    /// `plugins/<id>`.
    dir: PathBuf,
    /// Its files.
    folder: PathBuf,
    granted: Permissions,
    scope: Arc<Scope>,
}

impl Launch {
    /// Start the process; its id and whether it is sandboxed.
    fn start(
        &self,
        command: &[String],
        recordings: &RecordingFolders,
    ) -> Result<(u32, bool), String> {
        let program = find_program(&command[0], &self.folder)?;
        let data = self.dir.join("data");
        let tmp = data.join("tmp");
        std::fs::create_dir_all(&tmp).map_err(|err| format!("{}: {err}", tmp.display()))?;
        let log = self.dir.join("log");
        if std::fs::metadata(&log).is_ok_and(|m| m.len() > MAX_LOG) {
            let _ = std::fs::rename(&log, self.dir.join("log.1"));
        }
        let socket = self.dir.join("socket");
        let mut env: Vec<(String, String)> = Vec::new();
        for key in ["USER", "LOGNAME", "LANG", "LC_ALL", "LC_CTYPE"] {
            if let Ok(value) = std::env::var(key) {
                env.push((key.into(), value));
            }
        }
        let text = |path: &Path| path.to_string_lossy().into_owned();
        env.extend([
            ("PATH".into(), search_path()),
            ("HOME".into(), text(&data)),
            ("TMPDIR".into(), format!("{}/", text(&tmp))),
            ("ISOCAST_SOCKET".into(), text(&socket)),
            ("ISOCAST_PLUGIN_ID".into(), self.scope.id.clone()),
            ("ISOCAST_PLUGIN_DIR".into(), text(&self.folder)),
            ("ISOCAST_PLUGIN_DATA".into(), text(&data)),
            // Python can't write bytecode into its read-only folder, and
            // its output should reach the log as it happens.
            ("PYTHONDONTWRITEBYTECODE".into(), "1".into()),
            ("PYTHONUNBUFFERED".into(), "1".into()),
        ]);
        let profile =
            sandbox_available().then(|| self.profile(&program, &socket, &data, recordings));
        let sandboxed = profile.is_some();
        let pid = spawn(
            &program,
            &command[1..],
            &env,
            &self.folder,
            &log,
            profile.as_deref(),
        )
        .map_err(|err| format!("{}: {err}", program.display()))?;
        if let Some((_, started)) = process_info(pid) {
            let _ = std::fs::write(
                self.dir.join("pid"),
                format!("{pid} {} {}\n", started.0, started.1),
            );
        }
        Ok((pid, sandboxed))
    }

    /// Its sandbox profile.
    #[cfg(target_os = "macos")]
    fn profile(
        &self,
        program: &Path,
        socket: &Path,
        data: &Path,
        recordings: &RecordingFolders,
    ) -> String {
        let real = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let home = real(Path::new(&std::env::var_os("HOME").unwrap_or_default()));
        let mut readable = vec![real(&self.dir), real(&self.folder)];
        for file in &self.granted.files {
            let path = match file.strip_prefix("~/") {
                Some(rest) => home.join(rest),
                None => PathBuf::from(file),
            };
            readable.push(real(&path));
        }
        let program_real = real(program);
        let mut program_folders = vec![real(&self.folder)];
        program_folders.extend(installation(&program_real));
        let socket = match (socket.parent(), socket.file_name()) {
            (Some(parent), Some(name)) => real(parent).join(name),
            _ => socket.to_path_buf(),
        };
        // Canonical, as Seatbelt matches resolved paths (a recordings folder
        // not made yet as it will resolve: `resolved`); left out is a recordings
        // folder holding the home folder, the plugin's own folders or its
        // program (`/`, say): denying or granting all of that isn't what
        // the grant means, and the home folder's own rule still applies.
        let mut own = vec![
            home.clone(),
            real(&self.dir),
            real(&self.folder),
            program_real.clone(),
        ];
        own.extend(program_folders.iter().cloned());
        let not_projects = recordings
            .not_projects
            .iter()
            .filter(|folder| {
                folder
                    .parent()
                    .is_some_and(|root| !own.iter().any(|path| path.starts_with(root)))
            })
            .cloned()
            .collect();
        let recordings: Vec<PathBuf> = recordings
            .roots
            .iter()
            .filter(|root| !own.iter().any(|path| path.starts_with(root)))
            .cloned()
            .collect();
        isocast_platform_macos::plugin::Sandbox {
            home,
            readable,
            writable: vec![real(data)],
            programs: vec![program.to_path_buf(), program_real],
            program_folders,
            socket,
            network: !self.granted.network.is_empty(),
            recordings,
            not_projects,
            recordings_access: match self.granted.recordings {
                None => isocast_platform_macos::plugin::Recordings::None,
                Some(RecordingsAccess::Read) => isocast_platform_macos::plugin::Recordings::Read,
                Some(RecordingsAccess::Write) => isocast_platform_macos::plugin::Recordings::Write,
            },
        }
        .profile()
    }

    #[cfg(not(target_os = "macos"))]
    fn profile(
        &self,
        _program: &Path,
        _socket: &Path,
        _data: &Path,
        _recordings: &RecordingFolders,
    ) -> String {
        String::new()
    }
}

/// The folders a program's own installation may run programs from: a
/// framework version (Python re-runs itself from its app bundle), a
/// Homebrew keg, or for macOS's /usr/bin tool stubs, the developer tools.
#[cfg(target_os = "macos")]
fn installation(program: &Path) -> Vec<PathBuf> {
    let mut folders = Vec::new();
    let parts: Vec<_> = program.components().collect();
    for (i, part) in parts.iter().enumerate() {
        let name = part.as_os_str().to_string_lossy();
        if name.ends_with(".framework")
            && parts
                .get(i + 1)
                .is_some_and(|p| p.as_os_str() == "Versions")
            && i + 2 < parts.len()
        {
            folders.push(parts[..=i + 2].iter().collect());
            break;
        }
        if name == "Cellar" && i + 3 < parts.len() {
            folders.push(parts[..=i + 2].iter().collect());
            break;
        }
    }
    if program.starts_with("/usr/bin") || program.starts_with("/bin") {
        folders.push("/Library/Developer/CommandLineTools".into());
        folders.push("/Applications/Xcode.app/Contents/Developer".into());
    }
    folders
}

/// Where plugin processes look for programs: isocastd's PATH, then the
/// usual places (Homebrew's too, which a daemon started by an app lacks).
fn search_path() -> String {
    let mut dirs: Vec<String> = std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(String::from)
        .collect();
    for dir in [
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
        "/usr/sbin",
        "/sbin",
    ] {
        if !dirs.iter().any(|d| d == dir) {
            dirs.push(dir.into());
        }
    }
    dirs.join(":")
}

/// Plugin processes on Windows: refused until they can run sandboxed with
/// their output passed through the redacting log.
#[cfg(windows)]
const WINDOWS_PROCESS_UNSUPPORTED: &str = "plugin processes aren't supported on Windows yet \
     (they need a sandbox and a redacted log, which aren't built); use a plugin without a process";

/// On Windows: refused (see [`WINDOWS_PROCESS_UNSUPPORTED`]), so no plugin
/// with a process installs or starts.
#[cfg(windows)]
fn find_program(_name: &str, _folder: &Path) -> Result<PathBuf, String> {
    Err(WINDOWS_PROCESS_UNSUPPORTED.into())
}

/// The program a command names: a path inside the plugin's folder, or a
/// program on the plugins' PATH.
#[cfg(unix)]
fn find_program(name: &str, folder: &Path) -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;
    let runnable = |path: &Path| {
        std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if name.contains('/') {
        let path = folder.join(name);
        return if runnable(&path) {
            Ok(path)
        } else {
            Err(format!(
                "{name:?} isn't a program in {} (it must exist and be executable)",
                folder.display()
            ))
        };
    }
    let path = search_path();
    path.split(':')
        .map(|dir| Path::new(dir).join(name))
        .find(|path| runnable(path))
        .ok_or_else(|| {
            format!("{name:?} isn't on PATH ({path}); install it, or name its full path")
        })
}

/// Read and check a plugin's manifest, giving up after a few seconds (see
/// [`READ_TIMEOUT`]).
fn read_in_time(folder: &Path) -> Result<Manifest, RpcError> {
    let (tx, rx) = std::sync::mpsc::channel();
    let path = folder.to_path_buf();
    thread::spawn(move || {
        let _ = tx.send(Manifest::read(&path));
    });
    match rx.recv_timeout(READ_TIMEOUT) {
        Ok(Ok(manifest)) => Ok(manifest),
        Ok(Err(err)) => Err(failed(
            err.issues
                .iter()
                .map(|issue| match issue.path.as_str() {
                    "" => format!("{MANIFEST}: {}", issue.message),
                    path => format!("{MANIFEST}: {path}: {}", issue.message),
                })
                .collect::<Vec<_>>()
                .join("\n"),
        )),
        Err(_) => Err(failed(format!(
            "reading {} took more than {} s: macOS may be asking whether isocastd may read that \
             folder; allow it, or move the plugin out of Documents, Downloads or Desktop, and add \
             it again",
            folder.join(MANIFEST).display(),
            READ_TIMEOUT.as_secs()
        ))),
    }
}

/// The permissions to store for a plugin being added: what it asks for,
/// once granted (or, for a new version, already granted).
fn check_grant(
    manifest: &Manifest,
    grant: Option<&Permissions>,
    old: Option<&Record>,
) -> Result<Permissions, RpcError> {
    let asked = manifest.permissions.normalized();
    let id = &manifest.id;
    let listed = |permissions: &Permissions| {
        permissions
            .describe()
            .iter()
            .map(|line| format!("\n  {line}"))
            .collect::<String>()
    };
    match (grant, old) {
        (Some(grant), _) if grant.same_as(&asked) => Ok(asked),
        (Some(grant), _) => {
            let missing = asked.beyond(grant);
            let extra = grant.beyond(&asked);
            let mut message = format!(
                "grant: it must be exactly what {id} asks for, so nothing was installed; it asks \
                 for:{}",
                listed(&asked)
            );
            if !missing.is_empty() {
                message.push_str(&format!("\nnot granted:{}", listed(&missing)));
            }
            if !extra.is_empty() {
                message.push_str(&format!("\ngranted but not asked for:{}", listed(&extra)));
            }
            Err(not_permitted(message))
        }
        (None, Some(old)) if asked.beyond(&old.granted).is_empty() => Ok(asked),
        (None, Some(old)) => Err(not_permitted(format!(
            "{id} {} asks for more than {} was granted, so nothing was changed:{}\nGrant them with \
             `isocast plugin add PATH --allow` (MCP add_plugin: pass `grant` with everything it \
             asks for:{})",
            manifest.version,
            old.version,
            listed(&asked.beyond(&old.granted)),
            listed(&asked)
        ))),
        (None, None) => Err(not_permitted(format!(
            "{id} {} asks for these permissions, so nothing was installed until they are \
             granted:{}\nGrant them with `isocast plugin add PATH --allow` (MCP add_plugin: pass \
             `grant` with exactly these)",
            manifest.version,
            listed(&asked)
        ))),
    }
}

/// The manifest of an installed plugin, checked against what it was
/// granted (a linked folder may have changed since).
fn load_manifest(id: &str, dir: &Path, record: &Record) -> Result<Manifest, String> {
    let folder = if record.linked {
        record.source.clone()
    } else {
        dir.join("plugin")
    };
    let manifest = Manifest::read(&folder).map_err(|err| {
        format!(
            "its manifest can't be read: {}",
            err.to_string().replace('\n', "; ")
        )
    })?;
    if manifest.id != id {
        return Err(format!(
            "its manifest in {} now says id {:?}; add it again",
            folder.display(),
            manifest.id
        ));
    }
    let more = manifest.permissions.beyond(&record.granted);
    if !more.is_empty() {
        return Err(format!(
            "its manifest now asks for more than it was granted ({}); add it again to grant it",
            more.describe().join("; ")
        ));
    }
    Ok(manifest)
}

fn read_record(dir: &Path) -> Result<Record, String> {
    let path = dir.join("install.toml");
    let text =
        std::fs::read_to_string(&path).map_err(|err| format!("{}: {err}", path.display()))?;
    isocast_protocol::library::parse(&text, false)
        .map_err(|err| format!("{}: {err}", path.display()))
}

/// Save `install.toml` atomically.
fn write_record(dir: &Path, record: &Record) -> io::Result<()> {
    let text = isocast_protocol::library::to_toml(record).map_err(io::Error::other)?;
    let path = dir.join("install.toml");
    let temporary = dir.join("install.toml.new");
    std::fs::write(&temporary, text)?;
    isocast_protocol::fs::replace(&temporary, &path)
}

/// Copy a plugin's folder into `dir/plugin`, replacing what was there
/// only once the copy is complete.
/// Copy a plugin's folder into `dir/.staging-<time>`; its name.
fn stage_copy(source: &Path, dir: &Path) -> io::Result<String> {
    let name = format!(".staging-{}", unix_nanos());
    let mut budget = (MAX_COPY_FILES, MAX_COPY_BYTES);
    if let Err(err) = copy_tree(source, &dir.join(&name), &mut budget) {
        remove_tree(dir, &name);
        return Err(err);
    }
    Ok(name)
}

/// Make the staged copy `dir/plugin`, replacing the one there.
fn put_in_place(dir: &Path, staged: &str) -> io::Result<()> {
    let installed = dir.join("plugin");
    let old = format!(".old-{}", unix_nanos());
    if installed.exists() {
        std::fs::rename(&installed, dir.join(&old))?;
    }
    std::fs::rename(dir.join(staged), &installed)?;
    remove_tree(dir, &old);
    Ok(())
}

fn copy_tree(from: &Path, to: &Path, budget: &mut (usize, u64)) -> io::Result<()> {
    std::fs::create_dir_all(to)?;
    for item in std::fs::read_dir(from)? {
        let item = item?;
        let name = item.file_name();
        if name == ".git" {
            continue;
        }
        if budget.0 == 0 {
            return Err(io::Error::other(format!(
                "it has more than {MAX_COPY_FILES} files; add it with --link instead"
            )));
        }
        budget.0 -= 1;
        let kind = item.file_type()?;
        let (source, target) = (item.path(), to.join(&name));
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
            // Refuse every reparse point, including junctions/cloud placeholders,
            // rather than following something outside the requested source tree.
            if std::fs::symlink_metadata(&source)?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT
                != 0
            {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!(
                        "copying a Windows reparse point is unsupported: {}",
                        source.display()
                    ),
                ));
            }
        }
        if kind.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(std::fs::read_link(&source)?, &target)?;
            // Windows links (and junctions) aren't recreated or followed: a
            // copy must not pull in what they point at.
            #[cfg(windows)]
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "{} is a link, which isn't copied on Windows; add it with --link instead",
                    source.display()
                ),
            ));
        } else if kind.is_dir() {
            copy_tree(&source, &target, budget)?;
        } else {
            let size = item.metadata()?.len();
            if size > budget.1 {
                return Err(io::Error::other(format!(
                    "it is more than {} MB; add it with --link instead",
                    MAX_COPY_BYTES >> 20
                )));
            }
            budget.1 -= size;
            std::fs::copy(&source, &target)?;
        }
    }
    Ok(())
}

/// Delete `dir/name`, a folder isocastd made inside a plugin's folder.
fn remove_tree(dir: &Path, name: &str) {
    if name.is_empty() || name.contains('/') || name == "." || name == ".." {
        return;
    }
    let path = dir.join(name);
    if path.exists() {
        let _ = std::fs::remove_dir_all(&path);
    }
}

/// Delete an installed plugin's folder, `<plugins>/<id>`.
fn remove_plugin_dir(folder: &Path, id: &str) -> io::Result<()> {
    if !isocast_protocol::library::valid_id(id) {
        return Err(io::Error::other("not a plugin id"));
    }
    let dir = folder.join(id);
    if dir.join("install.toml").exists() || dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    Ok(())
}

/// Whether a folder has any file in it, at any depth (an empty `tmp` it
/// was given doesn't count).
fn holds_files(folder: &Path) -> bool {
    std::fs::read_dir(folder).is_ok_and(|items| {
        items.flatten().any(|item| match item.file_type() {
            Ok(kind) if kind.is_dir() => holds_files(&item.path()),
            _ => true,
        })
    })
}

/// Remove copies a crashed or interrupted add left behind.
fn clean_staging(dir: &Path) {
    let Ok(items) = std::fs::read_dir(dir) else {
        return;
    };
    for item in items.flatten() {
        let name = item.file_name().to_string_lossy().into_owned();
        if name.starts_with(".staging-") || name.starts_with(".old-") {
            remove_tree(dir, &name);
        }
    }
}

/// Stop a plugin process an earlier isocastd started and left running
/// (it crashed or was killed), if it is still the same process.
fn stop_leftover(dir: &Path) {
    let path = dir.join("pid");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let fields: Vec<u64> = text
        .split_whitespace()
        .filter_map(|n| n.parse().ok())
        .collect();
    if let [pid, seconds, micros] = fields[..]
        && let Some((_, started)) = process_info(pid as u32)
        && started == (seconds, micros)
    {
        signal_group(pid as u32, Signal::Kill);
    }
    let _ = std::fs::remove_file(path);
}

/// Wait for a child to end; how it ended, in words.
#[cfg(unix)]
fn wait(pid: u32) -> String {
    let mut status: libc::c_int = 0;
    loop {
        // SAFETY: waits for our own child; writes only `status`.
        let code = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, 0) };
        if code == pid as libc::pid_t {
            break;
        }
        if code == -1 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return "ended (its status couldn't be read)".into();
    }
    if libc::WIFEXITED(status) {
        format!("exited with status {}", libc::WEXITSTATUS(status))
    } else if libc::WIFSIGNALED(status) {
        format!("killed by signal {}", libc::WTERMSIG(status))
    } else {
        format!("ended (status {status})")
    }
}

/// On Windows no plugin process can be started (see [`signal_group`]), so
/// there is never a child to wait for.
#[cfg(windows)]
fn wait(pid: u32) -> String {
    unreachable!(
        "plugin process {pid}: plugin processes can't be started on Windows, so there is none \
         to wait for"
    );
}

/// The plugin API versions this Isocast runs, for messages: "1".
fn api_versions() -> String {
    let versions: Vec<String> = isocast_protocol::plugin::API_VERSIONS
        .iter()
        .map(u32::to_string)
        .collect();
    versions.join(", ")
}

fn unix_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

fn unix_seconds() -> u64 {
    (unix_nanos() / 1_000_000_000) as u64
}

#[cfg(target_os = "macos")]
fn sandbox_available() -> bool {
    isocast_platform_macos::plugin::sandbox_available()
}

#[cfg(not(target_os = "macos"))]
fn sandbox_available() -> bool {
    false
}

#[cfg(target_os = "macos")]
fn spawn(
    program: &Path,
    args: &[String],
    env: &[(String, String)],
    cwd: &Path,
    log: &Path,
    profile: Option<&str>,
) -> io::Result<u32> {
    isocast_platform_macos::plugin::spawn(&isocast_platform_macos::plugin::Spawn {
        program,
        args,
        env,
        cwd,
        log,
        profile,
    })
}

/// Other Unix systems: unsandboxed, in its own process group.
#[cfg(all(unix, not(target_os = "macos")))]
fn spawn(
    program: &Path,
    args: &[String],
    env: &[(String, String)],
    cwd: &Path,
    log: &Path,
    _profile: Option<&str>,
) -> io::Result<u32> {
    use std::os::unix::process::CommandExt;
    let log = isocast_diagnostics::output::plugin_pipe(log)?;
    let child = std::process::Command::new(program)
        .args(args)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0)
        .spawn()?;
    Ok(child.id())
}

/// On Windows: refused before any child exists (see
/// [`WINDOWS_PROCESS_UNSUPPORTED`]); there's no sandbox or redacted log.
#[cfg(windows)]
fn spawn(
    _program: &Path,
    _args: &[String],
    _env: &[(String, String)],
    _cwd: &Path,
    _log: &Path,
    _profile: Option<&str>,
) -> io::Result<u32> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        WINDOWS_PROCESS_UNSUPPORTED,
    ))
}

#[cfg(target_os = "macos")]
fn peer_pid(stream: &UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    isocast_platform_macos::plugin::peer_pid(stream.local()?.as_raw_fd())
}

/// The pipe's client process, as the kernel reports it.
#[cfg(windows)]
fn peer_pid(stream: &UnixStream) -> Option<u32> {
    isocast_ipc::peer_pid(stream).ok()
}

#[cfg(all(unix, not(target_os = "macos")))]
fn peer_pid(_stream: &UnixStream) -> Option<u32> {
    None
}

fn parent_pid(pid: u32) -> Option<u32> {
    process_info(pid).map(|(parent, _)| parent)
}

#[cfg(target_os = "macos")]
fn process_info(pid: u32) -> Option<(u32, (u64, u64))> {
    isocast_platform_macos::plugin::process_info(pid)
}

#[cfg(not(target_os = "macos"))]
fn process_info(_pid: u32) -> Option<(u32, (u64, u64))> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "macos")]
    fn interpreters_may_run_their_own_installation() {
        assert_eq!(
            installation(Path::new(
                "/opt/homebrew/Cellar/python@3.14/3.14.7/Frameworks/Python.framework/Versions/3.14/bin/python3.14"
            )),
            [PathBuf::from("/opt/homebrew/Cellar/python@3.14/3.14.7")]
        );
        assert_eq!(
            installation(Path::new(
                "/Library/Frameworks/Python.framework/Versions/3.13/bin/python3.13"
            )),
            [PathBuf::from(
                "/Library/Frameworks/Python.framework/Versions/3.13"
            )]
        );
        assert_eq!(
            installation(Path::new("/opt/homebrew/Cellar/node/24.1.0/bin/node")).len(),
            1
        );
        assert_eq!(installation(Path::new("/usr/bin/python3")).len(), 2);
        assert!(installation(Path::new("/Users/me/bin/tool")).is_empty());
    }

    #[test]
    fn a_scope_refuses_what_it_wasnt_granted() {
        let scope = Scope::new(
            "deck",
            &Permissions {
                methods: vec!["get".into(), "watch".into()],
                ..Default::default()
            },
        );
        assert_eq!(scope.refusal(&Method::Get(None)), None);
        assert_eq!(
            scope.refusal(&Method::Shutdown).as_deref(),
            Some("methods.shutdown: deck may call: get, watch")
        );
        let marker = Method::Marker { label: "x".into() };
        assert_eq!(
            scope.refusal(&marker).as_deref(),
            Some("methods.marker: deck may call: get, watch")
        );
        scope.revoke();
        assert!(scope.refusal(&Method::Get(None)).is_some());
    }

    #[test]
    fn data_calls_stay_under_the_granted_prefixes() {
        use isocast_protocol::data::{DataDeleteParams, DataGetParams, DataSetParams};
        let scope = Scope::new(
            "chat",
            &Permissions {
                methods: vec!["data_get".into(), "data_set".into(), "data_delete".into()],
                data_read: vec!["twitch".into()],
                data_write: vec!["twitch".into()],
                ..Default::default()
            },
        );
        let set = |key: &str| {
            Method::DataSet(Box::new(DataSetParams {
                values: [(key.to_string(), serde_json::json!(1))].into(),
                persist: None,
            }))
        };
        assert_eq!(scope.refusal(&set("twitch.chat")), None);
        assert_eq!(
            scope.refusal(&set("score")).as_deref(),
            Some(r#"data_write; params.values["score"]: chat may write data keys under: twitch"#)
        );
        let delete = Method::DataDelete(DataDeleteParams {
            keys: vec!["twitch".into(), "twitchy".into()],
        });
        assert!(scope.refusal(&delete).unwrap().contains("params.keys[1]"));
        let get = |keys: &[&str], prefix: Option<&str>| {
            Method::DataGet(Some(DataGetParams {
                keys: keys.iter().map(|key| key.to_string()).collect(),
                prefix: prefix.map(String::from),
            }))
        };
        assert_eq!(scope.refusal(&get(&["twitch.chat"], None)), None);
        assert_eq!(scope.refusal(&get(&[], Some("twitch."))), None);
        assert!(scope.refusal(&get(&[], Some("score"))).is_some());
        assert!(scope.refusal(&Method::DataGet(None)).is_some());
    }
}
