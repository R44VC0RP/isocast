//! WebAssembly audio filters (docs/scenes-as-code.md, "7.4 WebAssembly
//! tier"): plugins' components built for the `audio-filter` world of
//! `wit/plugin.wit`, run with wasmtime in a source's chain of audio
//! filters, exactly where built-in filters run.
//!
//! Containment: each filter has an instance (a store) of its own, with no
//! WASI (nothing but `isocast:plugin/host`), at most 64 MB of memory, and a
//! budget of work per call (fuel, about one wasm instruction each). A call
//! that traps, runs out of fuel or goes over its memory leaves the block as
//! it was, so the dry sound is mixed and there is never a gap; the
//! instance is dropped and a fresh one made off the audio thread, and the
//! third failure in a minute turns the filter off. Each failure is one
//! warning naming the filter's place in the setup.
//!
//! Fuel rather than wall-clock deadlines (the design's epoch ticker): a
//! budget of work is the same however loaded the Mac is, so an overloaded
//! machine (the base M2's 4K flagship) never bypasses a healthy filter
//! because the audio thread was descheduled mid-call, and no ticker thread
//! wakes 1,000 times a second. Each call's wall time is still measured and
//! reported (`engine.audio_filters`).
//!
//! Nothing here runs (no wasmtime engine, no threads) unless a setup uses
//! a plugin's audio filter.

use parking_lot::{Mutex, RwLock};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime};

use isocast_protocol::api::{Percentiles, PluginFilterInfo, PluginFilterState};
use isocast_protocol::plugin::{Param, ParamType, PluginKind};
use wasmtime::component::{
    Component, ComponentNamedList, ComponentType, Func, InstancePre, Lift, Linker, Lower, TypedFunc,
};
use wasmtime::{Config, Engine, Store, StoreContextMut, StoreLimits, StoreLimitsBuilder, Trap};

use crate::SAMPLE_RATE;

/// The work one call may do, in fuel (about one wasm instruction each),
/// against the design's 3 ms deadline per 10 ms block: an endless loop
/// on a base M2 is stopped after about 2 ms (20,000,000 took 7-10 ms
/// there, 1.2-1.6 ms on an M5). The example gain uses 9,301 a block, its
/// ducker 165,877.
pub const FUEL_PER_CALL: u64 = 5_000_000;
/// The most memory a filter's instance may have.
pub const MEMORY_LIMIT: usize = 64 << 20;
/// This many failures within [`FAILURE_WINDOW`] turn a filter off.
const MAX_FAILURES: usize = 3;
const FAILURE_WINDOW: Duration = Duration::from_secs(60);
/// Lines a filter may log a second; more are counted and dropped.
const LOG_LINES_PER_SECOND: u32 = 20;
/// Calls the reported percentiles are taken over (10 s of 10 ms blocks).
const WINDOW: usize = 1000;
/// The one interface a component may import, and its types.
const HOST: &str = "isocast:plugin/host@1.0.0";
const TYPES: &str = "isocast:plugin/types@1.0.0";

/// An installed plugin's audio filter, as isocastd tells the engine.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioFilterPlugin {
    /// `<plugin id>/<kind id>`.
    pub kind: String,
    /// The component (`.wasm`) in the plugin's folder.
    pub component: PathBuf,
    /// Where its compiled machine code is kept (`.cwasm`), in a folder only
    /// isocastd writes.
    pub cache: PathBuf,
    /// It hears another source's sound (`sidechain_source`).
    pub sidechain: bool,
    /// Its params, from the manifest.
    pub params: BTreeMap<String, Param>,
    /// The plugin's log.
    pub log: PathBuf,
    /// Why its filters are bypassed (the plugin is turned off or made for
    /// another plugin API), if they are.
    pub bypassed: Option<String>,
}

/// The installed plugins' audio filters, by kind.
#[derive(Default)]
struct Installed {
    kinds: RwLock<HashMap<String, Arc<Entry>>>,
    /// Bumped by every change, so chains look their filters up again.
    generation: AtomicU64,
}

/// An installed filter and the file it was installed as (size and
/// modification time), so a reinstall that changes it makes new instances.
#[derive(Debug, PartialEq)]
struct Entry {
    plugin: AudioFilterPlugin,
    stamp: u64,
}

fn installed() -> &'static Installed {
    static INSTALLED: OnceLock<Installed> = OnceLock::new();
    INSTALLED.get_or_init(Installed::default)
}

/// Take the installed plugins' audio filters (isocastd, whenever a plugin
/// is added, removed, turned on or off). Running filters whose plugin
/// changed are made again; those of a plugin that went are bypassed.
pub fn set_installed(plugins: Vec<AudioFilterPlugin>) {
    let installed = installed();
    let mut kinds = installed.kinds.write();
    let mut next = HashMap::new();
    for plugin in plugins {
        let entry = Entry {
            stamp: stamp(&plugin.component),
            plugin,
        };
        let entry = match kinds.get(&entry.plugin.kind) {
            Some(old) if **old == entry => old.clone(),
            _ => Arc::new(entry),
        };
        next.insert(entry.plugin.kind.clone(), entry);
    }
    let same = next.len() == kinds.len()
        && next
            .iter()
            .all(|(kind, entry)| kinds.get(kind).is_some_and(|old| Arc::ptr_eq(old, entry)));
    if !same {
        *kinds = next;
        installed.generation.fetch_add(1, Ordering::SeqCst);
    }
}

/// Bumped whenever the installed audio filters change.
pub(crate) fn generation() -> u64 {
    installed().generation.load(Ordering::SeqCst)
}

fn lookup(kind: &str) -> Option<Arc<Entry>> {
    installed().kinds.read().get(kind).cloned()
}

/// A file's size and modification time, as one number (0 if it can't be
/// read).
fn stamp(path: &Path) -> u64 {
    let Ok(meta) = std::fs::metadata(path) else {
        return 0;
    };
    let modified = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |time| time.as_nanos() as u64);
    modified ^ meta.len().rotate_left(48)
}

/// Compile a filter's component (or take its cached machine code) and
/// check it against the `audio-filter` world: what isocastd does at
/// `plugin add`, so a component that can't run is refused there.
pub fn prepare(plugin: &AudioFilterPlugin) -> Result<(), String> {
    let runtime = runtime()?;
    let loaded = runtime.load(plugin, stamp(&plugin.component))?;
    // A trial instance checks the exports' types and that it starts.
    let mut store = runtime.store(plugin);
    loaded.exports(&mut store).map(|_| ())
}

/// A param's value as a component gets it (wit: `value`).
#[derive(Debug, Clone, PartialEq, ComponentType, Lower)]
#[component(variant)]
enum WitValue {
    #[component(name = "number")]
    Number(f64),
    #[component(name = "integer")]
    Integer(i64),
    #[component(name = "flag")]
    Flag(bool),
    #[component(name = "text")]
    Text(String),
    #[component(name = "color")]
    Color((f32, f32, f32, f32)),
}

/// wit: `param`.
#[derive(Debug, Clone, PartialEq, ComponentType, Lower)]
#[component(record)]
struct WitParam {
    #[component(name = "name")]
    name: String,
    #[component(name = "value")]
    value: WitValue,
}

/// Every param of the manifest with the setup's value, or its default. A
/// value of the wrong type (a setup isocastd didn't check) is left out, so
/// the component uses its own default.
fn wit_params(
    plugin: &AudioFilterPlugin,
    given: &serde_json::Map<String, serde_json::Value>,
) -> Vec<WitParam> {
    plugin
        .params
        .iter()
        .filter_map(|(name, param)| {
            let value = given.get(name).or(param.default.as_ref())?;
            let value = match param.ty {
                ParamType::F32 => WitValue::Number(value.as_f64()?),
                ParamType::I32 => WitValue::Integer(value.as_i64()?),
                ParamType::Bool => WitValue::Flag(value.as_bool()?),
                ParamType::String | ParamType::Enum | ParamType::File => {
                    WitValue::Text(value.as_str()?.to_string())
                }
                ParamType::Color => WitValue::Color(color(value.as_str()?)?),
                ParamType::Vec2 | ParamType::Vec4 => return None,
            };
            Some(WitParam {
                name: name.clone(),
                value,
            })
        })
        .collect()
}

/// `#rrggbb` or `#rrggbbaa` as straight RGBA from 0 to 1.
fn color(text: &str) -> Option<(f32, f32, f32, f32)> {
    let hex = text.strip_prefix('#')?;
    let byte = |i: usize| -> Option<f32> {
        let part = hex.get(i..i + 2)?;
        Some(f32::from(u8::from_str_radix(part, 16).ok()?) / 255.0)
    };
    let alpha = match hex.len() {
        6 => 1.0,
        8 => byte(6)?,
        _ => return None,
    };
    Some((byte(0)?, byte(2)?, byte(4)?, alpha))
}

/// The wasmtime engine and what every filter shares, made the first time
/// a setup uses a plugin's audio filter.
struct Runtime {
    engine: Engine,
    linker: Linker<Host>,
    /// Components loaded, by kind, with the stamp of the file they came
    /// from.
    loaded: Mutex<HashMap<String, (u64, Arc<Loaded>)>>,
    /// Makes instances off the audio thread (after a failure).
    jobs: Mutex<Option<Sender<Job>>>,
    /// Plugins' log lines, written on a thread of their own.
    logs: Mutex<Option<mpsc::SyncSender<(PathBuf, String)>>>,
}

fn runtime() -> Result<&'static Runtime, String> {
    static RUNTIME: OnceLock<Result<Runtime, String>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| Runtime::new().map_err(|err| format!("WebAssembly can't run here: {err}")))
        .as_ref()
        .map_err(Clone::clone)
}

/// What a filter's store holds.
struct Host {
    limits: StoreLimits,
    log: Log,
}

/// A filter's log lines, at most [`LOG_LINES_PER_SECOND`].
struct Log {
    file: PathBuf,
    kind: String,
    second: Instant,
    lines: u32,
    dropped: u32,
}

impl Log {
    fn line(&mut self, level: u8, message: &str) {
        if self.second.elapsed() >= Duration::from_secs(1) {
            if self.dropped > 0 {
                let dropped = std::mem::take(&mut self.dropped);
                self.send(format!(
                    "{dropped} more lines were dropped (at most {LOG_LINES_PER_SECOND} a second)"
                ));
            }
            self.second = Instant::now();
            self.lines = 0;
        }
        if self.lines >= LOG_LINES_PER_SECOND {
            self.dropped += 1;
            return;
        }
        self.lines += 1;
        let level = match level {
            0 => "error",
            1 => "warning",
            2 => "info",
            _ => "debug",
        };
        let message: String = message.chars().take(1000).collect();
        self.send(format!("{level}: {message}"));
    }

    fn send(&self, text: String) {
        let Ok(runtime) = runtime() else { return };
        let line = format!(
            "{} {}: {text}\n",
            chrono::Local::now().format("%H:%M:%S%.3f"),
            self.kind
        );
        if let Some(logs) = runtime.logs.lock().as_ref() {
            // Never waits: a full queue drops the line.
            let _ = logs.try_send((self.file.clone(), line));
        }
    }
}

impl Runtime {
    fn new() -> wasmtime::Result<Self> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.consume_fuel(true);
        config.cranelift_opt_level(wasmtime::OptLevel::Speed);
        let engine = Engine::new(&config)?;
        let mut linker = Linker::new(&engine);
        let mut host = linker.instance(HOST)?;
        host.func_wrap(
            "log",
            |mut store: StoreContextMut<'_, Host>, (level, message): (u8, String)| {
                store.data_mut().log.line(level, &message);
                Ok(())
            },
        )?;
        // No data store yet (5.5a): nothing to read.
        host.func_wrap(
            "data-get",
            |_store: StoreContextMut<'_, Host>, (_key,): (String,)| Ok((None::<String>,)),
        )?;
        let (logs, lines) = mpsc::sync_channel::<(PathBuf, String)>(256);
        isocast_diagnostics::spawn(
            "plugin logs",
            move || {
                for (file, line) in lines {
                    let open = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&file);
                    if let Ok(mut file) = open {
                        let _ = file.write_all(line.as_bytes());
                    }
                }
            },
            |_| {},
        )
        .map_err(|err| wasmtime::format_err!("{err}"))?;
        Ok(Self {
            engine,
            linker,
            loaded: Mutex::default(),
            jobs: Mutex::default(),
            logs: Mutex::new(Some(logs)),
        })
    }

    fn store(&self, plugin: &AudioFilterPlugin) -> Store<Host> {
        let host = Host {
            limits: StoreLimitsBuilder::new()
                .memory_size(MEMORY_LIMIT)
                .trap_on_grow_failure(true)
                .build(),
            log: Log {
                file: plugin.log.clone(),
                kind: plugin.kind.clone(),
                second: Instant::now(),
                lines: 0,
                dropped: 0,
            },
        };
        let mut store = Store::new(&self.engine, host);
        store.limiter(|host| &mut host.limits);
        store
    }

    /// The filter's component, ready to instantiate: loaded before (and its
    /// file unchanged), else from its cached machine code, else compiled
    /// (and cached).
    fn load(&self, plugin: &AudioFilterPlugin, stamp: u64) -> Result<Arc<Loaded>, String> {
        if let Some((loaded_stamp, loaded)) = self.loaded.lock().get(&plugin.kind)
            && *loaded_stamp == stamp
        {
            return Ok(loaded.clone());
        }
        let component = self.component(plugin, stamp)?;
        check_imports(&self.engine, &component, &plugin.component)?;
        let pre = self
            .linker
            .instantiate_pre(&component)
            .map_err(|err| format!("{}: {err}", plugin.component.display()))?;
        let loaded = Arc::new(Loaded {
            pre,
            file: plugin.component.clone(),
        });
        self.loaded
            .lock()
            .insert(plugin.kind.clone(), (stamp, loaded.clone()));
        Ok(loaded)
    }

    fn component(&self, plugin: &AudioFilterPlugin, stamp: u64) -> Result<Component, String> {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.engine
            .precompile_compatibility_hash()
            .hash(&mut hasher);
        let key = format!("{stamp} {:x}\n", hasher.finish());
        let key_file = plugin.cache.with_extension("key");
        if std::fs::read_to_string(&key_file).is_ok_and(|saved| saved == key) {
            // SAFETY: the cache is machine code isocastd compiled itself, in
            // a folder no plugin may write (`plugins/<id>/compiled`), for
            // this wasmtime (the key says so).
            if let Ok(component) =
                unsafe { Component::deserialize_file(&self.engine, &plugin.cache) }
            {
                return Ok(component);
            }
        }
        let bytes = std::fs::read(&plugin.component)
            .map_err(|err| format!("can't read {}: {err}", plugin.component.display()))?;
        let compiled = self.engine.precompile_component(&bytes).map_err(|err| {
            format!(
                "{} isn't a WebAssembly component Isocast can run: {err}",
                plugin.component.display()
            )
        })?;
        // Kept for next time; without it, it is compiled again then.
        if let Some(folder) = plugin.cache.parent() {
            let _ = std::fs::create_dir_all(folder);
        }
        let staged = plugin.cache.with_extension("cwasm.new");
        if std::fs::write(&staged, &compiled).is_ok()
            && std::fs::rename(&staged, &plugin.cache).is_ok()
        {
            let _ = std::fs::write(&key_file, &key);
        }
        // SAFETY: just compiled by this engine.
        unsafe { Component::deserialize(&self.engine, &compiled) }
            .map_err(|err| format!("{}: {err}", plugin.component.display()))
    }

    /// Run `job` on the thread that makes instances.
    fn spawn(&self, job: Job) {
        let mut jobs = self.jobs.lock();
        let sender = jobs.get_or_insert_with(|| {
            let (tx, rx) = mpsc::channel::<Job>();
            let _ = isocast_diagnostics::spawn(
                "plugin instances",
                move || rx.into_iter().for_each(|job| job()),
                |_| {},
            );
            tx
        });
        if let Err(mpsc::SendError(job)) = sender.send(job) {
            // The thread is gone: run it here.
            *jobs = None;
            drop(jobs);
            job();
        }
    }
}

/// A component may import only `isocast:plugin/host` (and its types): no
/// WASI, so no files, network, clocks or randomness.
fn check_imports(engine: &Engine, component: &Component, file: &Path) -> Result<(), String> {
    let others: Vec<String> = component
        .component_type()
        .imports(engine)
        .map(|(name, _)| name.to_string())
        .filter(|name| name != HOST && name != TYPES)
        .collect();
    if others.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{} imports {}, which Isocast doesn't give plugins: a component gets only {HOST} (no \
         WASI: no files, network or clocks); build it without the standard library, as \
         examples/plugins/gain does",
        file.display(),
        others.join(", ")
    ))
}

/// A filter's component, compiled and linked.
struct Loaded {
    pre: InstancePre<Host>,
    file: PathBuf,
}

/// The audio-filter world's exports, checked against their types.
struct Exports {
    configure: Func,
    latency: Func,
    process: Func,
    reset: Func,
}

/// Work for the thread that makes instances.
type Job = Box<dyn FnOnce() + Send>;

/// Where warnings go.
type Warn = Arc<dyn Fn(String) + Send + Sync>;

impl Loaded {
    /// Instantiate it in `store` (its start runs with a call's fuel) and
    /// find its exports.
    fn exports(&self, store: &mut Store<Host>) -> Result<Exports, String> {
        let _ = store.set_fuel(FUEL_PER_CALL);
        let instance = self
            .pre
            .instantiate(&mut *store)
            .map_err(|err| describe(&err))?;
        let mut export =
            |name: &str, signature: &str, check: &dyn Fn(&Func, &Store<Host>) -> bool| {
                let func = instance.get_func(&mut *store, name);
                match func {
                    Some(func) if check(&func, store) => Ok(func),
                    _ => Err(format!(
                        "{} doesn't export `{name}` as `{signature}`: it isn't built for the \
                     audio-filter world of isocast:plugin@1.0.0 (wit/plugin.wit)",
                        self.file.display()
                    )),
                }
            };
        Ok(Exports {
            configure: export(
                "configure",
                "func(sample-rate: u32, channels: u32, params: list<param>) -> result<_, string>",
                &|f, s| {
                    f.typed::<(u32, u32, &[WitParam]), (Result<(), String>,)>(s)
                        .is_ok()
                },
            )?,
            latency: export("latency", "func() -> u32", &|f, s| {
                f.typed::<(), (u32,)>(s).is_ok()
            })?,
            process: export(
                "process",
                "func(samples: list<f32>, sidechain: option<list<f32>>) -> list<f32>",
                &|f, s| f.typed::<(&[f32], Option<&[f32]>), (Vec<f32>,)>(s).is_ok(),
            )?,
            reset: export("reset", "func()", &|f, s| f.typed::<(), ()>(s).is_ok())?,
        })
    }
}

/// What went wrong in a call, for a warning (after the filter's place).
fn describe(err: &wasmtime::Error) -> String {
    match err.downcast_ref::<Trap>() {
        Some(Trap::OutOfFuel) => format!(
            "ran out of fuel: it did more than {FUEL_PER_CALL} units of work in one call (an \
             endless loop, or far too slow)"
        ),
        Some(Trap::UnreachableCodeReached) => {
            "crashed (a WebAssembly trap: unreachable code, as a panic ends)".into()
        }
        Some(Trap::StackOverflow) => "crashed (its stack overflowed)".into(),
        Some(trap) => format!("crashed (a WebAssembly trap: {trap})"),
        None if err
            .chain()
            .any(|cause| cause.to_string().contains("growing memory")) =>
        {
            format!("went over its {} MB of memory", MEMORY_LIMIT >> 20)
        }
        None => format!("failed: {}", err.root_cause()),
    }
}

/// A filter's instance.
struct Instance {
    store: Store<Host>,
    exports: Exports,
}

/// Why making or calling an instance didn't work.
enum Failure {
    /// The component said no to its settings (`configure`'s error): not
    /// worth trying again until they change.
    Refused(String),
    /// It trapped, ran out of fuel or memory, or broke the rules.
    Failed(String),
}

impl Instance {
    /// A new instance of `loaded`, configured.
    fn new(
        loaded: &Loaded,
        plugin: &AudioFilterPlugin,
        channels: usize,
        params: &[WitParam],
    ) -> Result<(Self, u32), Failure> {
        let runtime = runtime().map_err(Failure::Failed)?;
        let mut store = runtime.store(plugin);
        let exports = loaded.exports(&mut store).map_err(Failure::Failed)?;
        let mut instance = Self { store, exports };
        instance.configure(channels, params)?;
        let latency = instance.latency()?;
        Ok((instance, latency))
    }

    /// Call `func` (whose type was checked when the instance was made)
    /// with a full budget of fuel; also returns the fuel it used.
    fn call<P, R>(&mut self, func: Func, params: P) -> Result<(R, u64), String>
    where
        P: ComponentNamedList + Lower,
        R: ComponentNamedList + Lift,
    {
        let _ = self.store.set_fuel(FUEL_PER_CALL);
        // SAFETY: `Loaded::exports` checked each export against these
        // types.
        let typed = unsafe { TypedFunc::<P, R>::new_unchecked(func) };
        let result = typed
            .call(&mut self.store, params)
            .map_err(|err| describe(&err))?;
        let left = self.store.get_fuel().unwrap_or(0);
        Ok((result, FUEL_PER_CALL - left))
    }

    fn configure(&mut self, channels: usize, params: &[WitParam]) -> Result<(), Failure> {
        let func = self.exports.configure;
        let ((result,), _) = self
            .call::<(u32, u32, &[WitParam]), (Result<(), String>,)>(
                func,
                (SAMPLE_RATE, channels as u32, params),
            )
            .map_err(Failure::Failed)?;
        result.map_err(Failure::Refused)
    }

    fn latency(&mut self) -> Result<u32, Failure> {
        let func = self.exports.latency;
        let ((latency,), _) = self.call::<(), (u32,)>(func, ()).map_err(Failure::Failed)?;
        Ok(latency.min(SAMPLE_RATE))
    }

    fn process(
        &mut self,
        samples: &[f32],
        sidechain: Option<&[f32]>,
    ) -> Result<(Vec<f32>, u64), String> {
        let func = self.exports.process;
        let ((out,), fuel) =
            self.call::<(&[f32], Option<&[f32]>), (Vec<f32>,)>(func, (samples, sidechain))?;
        Ok((out, fuel))
    }

    fn reset(&mut self) -> Result<(), String> {
        let func = self.exports.reset;
        self.call::<(), ()>(func, ()).map(|_| ())
    }
}

/// Where a chain's plugin filters report: warnings (the engine's events)
/// and their counters (the engine's status).
#[derive(Default)]
pub(crate) struct Reports {
    warn: Mutex<Option<Warn>>,
    filters: Mutex<Vec<Weak<FilterStats>>>,
    /// Warnings given lately, so the program mix and filtered ISO files
    /// running the same filter don't say the same thing twice.
    recent: Mutex<VecDeque<(Instant, String)>>,
}

impl Reports {
    /// Send warnings to `warn` from now on (else they are printed).
    pub(crate) fn set_warn(&self, warn: Warn) {
        *self.warn.lock() = Some(warn);
    }

    fn warn(&self, message: String) {
        {
            let mut recent = self.recent.lock();
            let now = Instant::now();
            recent.retain(|(at, _)| now.duration_since(*at) < Duration::from_secs(10));
            if recent.iter().any(|(_, said)| *said == message) {
                return;
            }
            recent.push_back((now, message.clone()));
        }
        let warn = self.warn.lock().clone();
        match warn {
            Some(warn) => warn(message),
            None => isocast_diagnostics::diagnostic!("warn"; "isocast: {message}"),
        }
    }

    fn register(&self, stats: &Arc<FilterStats>) {
        let mut filters = self.filters.lock();
        filters.retain(|filter| filter.strong_count() > 0);
        filters.push(Arc::downgrade(stats));
    }

    /// Every running plugin filter's state and counters.
    pub(crate) fn info(&self) -> Vec<PluginFilterInfo> {
        let filters = self.filters.lock();
        let mut info: Vec<PluginFilterInfo> = filters
            .iter()
            .filter_map(Weak::upgrade)
            .map(|stats| stats.info())
            .collect();
        info.sort_by(|a, b| (&a.path, &a.runs_in).cmp(&(&b.path, &b.runs_in)));
        info
    }
}

/// Where a chain is: its source's place in the setup, what it filters for
/// (`mix` or `iso`), and where its plugin filters report.
#[derive(Clone)]
pub(crate) struct Place {
    pub(crate) source: Option<usize>,
    pub(crate) runs_in: &'static str,
    pub(crate) reports: Arc<Reports>,
}

impl Default for Place {
    fn default() -> Self {
        Self {
            source: None,
            runs_in: "mix",
            reports: Arc::default(),
        }
    }
}

impl Place {
    /// The setup path of the chain's filter `index`.
    pub(crate) fn path(&self, index: usize) -> String {
        match self.source {
            Some(source) => format!("sources[{source}].audio_filters[{index}]"),
            None => format!("audio_filters[{index}]"),
        }
    }
}

/// A plugin filter's counters.
struct FilterStats {
    path: String,
    kind: String,
    runs_in: &'static str,
    inner: Mutex<StatsInner>,
}

struct StatsInner {
    state: PluginFilterState,
    reason: Option<String>,
    calls: u64,
    /// Wall time and fuel of the last [`WINDOW`] calls (a ring).
    ns: Vec<u32>,
    fuel: Vec<u32>,
    next: usize,
    failures: u64,
    last_failure: Option<String>,
}

impl FilterStats {
    fn new(path: String, kind: String, runs_in: &'static str) -> Self {
        Self {
            path,
            kind,
            runs_in,
            inner: Mutex::new(StatsInner {
                state: PluginFilterState::Running,
                reason: None,
                calls: 0,
                ns: Vec::new(),
                fuel: Vec::new(),
                next: 0,
                failures: 0,
                last_failure: None,
            }),
        }
    }

    fn call(&self, took: Duration, fuel: u64) {
        let mut inner = self.inner.lock();
        let ns = took.as_nanos().min(u128::from(u32::MAX)) as u32;
        let fuel = fuel.min(u64::from(u32::MAX)) as u32;
        if inner.ns.len() < WINDOW {
            inner.ns.push(ns);
            inner.fuel.push(fuel);
        } else {
            let next = inner.next;
            inner.ns[next] = ns;
            inner.fuel[next] = fuel;
        }
        inner.next = (inner.next + 1) % WINDOW;
        inner.calls += 1;
    }

    fn state(&self, state: PluginFilterState, reason: Option<String>) {
        let mut inner = self.inner.lock();
        inner.state = state;
        inner.reason = reason;
    }

    fn failure(&self, what: &str) {
        let mut inner = self.inner.lock();
        inner.failures += 1;
        inner.last_failure = Some(what.to_string());
    }

    fn info(&self) -> PluginFilterInfo {
        let inner = self.inner.lock();
        let percentiles = |values: &[u32], scale: f64| {
            let mut sorted = values.to_vec();
            sorted.sort_unstable();
            let at = |p: f64| {
                if sorted.is_empty() {
                    return 0.0;
                }
                let rank = (p * sorted.len() as f64).ceil() as usize;
                f64::from(sorted[rank.clamp(1, sorted.len()) - 1]) * scale
            };
            Percentiles {
                p50: (at(0.5) * 10.0).round() / 10.0,
                p99: (at(0.99) * 10.0).round() / 10.0,
            }
        };
        PluginFilterInfo {
            path: self.path.clone(),
            kind: self.kind.clone(),
            runs_in: self.runs_in.to_string(),
            state: inner.state,
            reason: inner.reason.clone(),
            calls: inner.calls,
            call_us: percentiles(&inner.ns, 0.001),
            fuel: percentiles(&inner.fuel, 1.0),
            fuel_budget: FUEL_PER_CALL,
            failures: inner.failures,
            last_failure: inner.last_failure.clone(),
        }
    }
}

/// A plugin's audio filter in a chain: an instance of its component, or
/// the sound passed through while there is none.
pub(crate) struct PluginFilter {
    entry: Option<Arc<Entry>>,
    kind: String,
    given: serde_json::Map<String, serde_json::Value>,
    params: Vec<WitParam>,
    channels: usize,
    state: State,
    latency: usize,
    failures: VecDeque<Instant>,
    stats: Arc<FilterStats>,
    reports: Arc<Reports>,
    /// The sidechain's sound for the block, sized to it.
    key: Vec<f32>,
}

enum State {
    Ready(Box<Instance>),
    /// A fresh instance is being made after a failure.
    Making(Receiver<Result<(Instance, u32), Failure>>),
    Off,
}

impl PluginFilter {
    pub(crate) fn new(kind: &PluginKind, channels: usize, place: &Place, index: usize) -> Self {
        let stats = Arc::new(FilterStats::new(
            place.path(index),
            kind.kind.clone(),
            place.runs_in,
        ));
        place.reports.register(&stats);
        let mut filter = Self {
            entry: None,
            kind: kind.kind.clone(),
            given: kind.params.clone(),
            params: Vec::new(),
            channels,
            state: State::Off,
            latency: 0,
            failures: VecDeque::new(),
            stats,
            reports: place.reports.clone(),
            key: Vec::new(),
        };
        filter.start();
        filter
    }

    /// Look the kind up and make an instance (on this thread: the chain is
    /// being set up), or say why it is bypassed.
    fn start(&mut self) {
        self.entry = lookup(&self.kind);
        self.state = State::Off;
        let Some(entry) = self.entry.clone() else {
            self.off(format!(
                "the plugin {:?} isn't installed",
                self.kind.split('/').next().unwrap_or_default()
            ));
            return;
        };
        if let Some(reason) = &entry.plugin.bypassed {
            self.off(format!("its plugin {reason}"));
            return;
        }
        self.params = wit_params(&entry.plugin, &self.given);
        let loaded = match runtime().and_then(|runtime| runtime.load(&entry.plugin, entry.stamp)) {
            Ok(loaded) => loaded,
            Err(err) => {
                self.off(format!("it can't be loaded: {err}"));
                return;
            }
        };
        match Instance::new(&loaded, &entry.plugin, self.channels, &self.params) {
            Ok((instance, latency)) => {
                self.latency = latency as usize;
                self.ready(instance);
            }
            Err(Failure::Refused(why)) => self.off(format!("it refused its settings: {why}")),
            Err(Failure::Failed(what)) => self.fail(what),
        }
    }

    fn ready(&mut self, instance: Instance) {
        self.state = State::Ready(Box::new(instance));
        self.stats.state(PluginFilterState::Running, None);
    }

    /// Bypass it until its settings change, with one warning saying why.
    fn off(&mut self, why: String) {
        self.state = State::Off;
        self.stats.state(PluginFilterState::Off, Some(why.clone()));
        self.reports.warn(format!(
            "{} ({}): {why}, so it is bypassed (the sound passes through)",
            self.stats.path, self.kind
        ));
    }

    /// A call failed: the block passes through; a fresh instance is made
    /// off this thread, unless this is the third failure in a minute.
    fn fail(&mut self, what: String) {
        let now = Instant::now();
        self.failures.push_back(now);
        while self
            .failures
            .front()
            .is_some_and(|at| now.duration_since(*at) > FAILURE_WINDOW)
        {
            self.failures.pop_front();
        }
        self.stats.failure(&what);
        // The copy filtering a filtered ISO file's sound fails on its own.
        let place = match self.stats.runs_in {
            "iso" => format!(
                "{} ({}, for the filtered ISO file)",
                self.stats.path, self.kind
            ),
            _ => format!("{} ({})", self.stats.path, self.kind),
        };
        if self.failures.len() >= MAX_FAILURES {
            self.state = State::Off;
            let why = format!(
                "{MAX_FAILURES} failures in {} s (last: it {what})",
                FAILURE_WINDOW.as_secs()
            );
            self.stats.state(PluginFilterState::Off, Some(why.clone()));
            self.reports.warn(format!(
                "{place} {what}; that is {MAX_FAILURES} failures in a minute, so it is turned off \
                 and its sound passes through until its settings change or its plugin is added \
                 again"
            ));
            return;
        }
        self.reports.warn(format!(
            "{place} {what}; that block passed through unfiltered (no gap) and a fresh copy is \
             being started (failure {} in a minute; the {MAX_FAILURES}rd turns it off)",
            self.failures.len()
        ));
        self.stats.state(PluginFilterState::Restarting, Some(what));
        self.remake();
    }

    /// Make a fresh instance on the instance thread.
    fn remake(&mut self) {
        let (Some(entry), Ok(runtime)) = (self.entry.clone(), runtime()) else {
            self.state = State::Off;
            return;
        };
        let loaded = match runtime.load(&entry.plugin, entry.stamp) {
            Ok(loaded) => loaded,
            Err(err) => {
                self.off(format!("it can't be loaded: {err}"));
                return;
            }
        };
        let (tx, rx) = mpsc::channel();
        let (channels, params) = (self.channels, self.params.clone());
        runtime.spawn(Box::new(move || {
            let _ = tx.send(Instance::new(&loaded, &entry.plugin, channels, &params));
        }));
        self.state = State::Making(rx);
    }

    /// New settings (the same kind): a reinstalled plugin is started
    /// afresh; new params go to `configure`.
    pub(crate) fn set(&mut self, kind: &PluginKind) {
        let entry = lookup(&self.kind);
        let reinstalled = match (&entry, &self.entry) {
            (Some(new), Some(old)) => !Arc::ptr_eq(new, old),
            (None, None) => false,
            _ => true,
        };
        if reinstalled || (kind.params != self.given && matches!(self.state, State::Off)) {
            self.given = kind.params.clone();
            self.failures.clear();
            self.start();
            return;
        }
        if kind.params == self.given {
            return;
        }
        self.given = kind.params.clone();
        let Some(entry) = self.entry.clone() else {
            return;
        };
        self.params = wit_params(&entry.plugin, &self.given);
        let State::Ready(instance) = &mut self.state else {
            // A fresh instance comes with the old params: make another.
            self.remake();
            return;
        };
        match instance.configure(self.channels, &self.params) {
            Ok(()) => {}
            Err(Failure::Refused(why)) => self.off(format!("it refused its settings: {why}")),
            Err(Failure::Failed(what)) => self.fail(what),
        }
    }

    /// Frames of delay it adds.
    pub(crate) fn latency(&self) -> usize {
        self.latency
    }

    /// Whether it hears a sidechain source.
    fn takes_sidechain(&self) -> bool {
        self.entry
            .as_ref()
            .is_some_and(|entry| entry.plugin.sidechain)
    }

    /// The source whose sound it hears, if it takes one and has one.
    pub(crate) fn sidechain(&self) -> Option<&str> {
        if !self.takes_sidechain() {
            return None;
        }
        self.given
            .get(isocast_protocol::plugin::SIDECHAIN_PARAM)
            .and_then(serde_json::Value::as_str)
    }

    /// Filter interleaved `samples` in place; `key` is the sidechain's
    /// sound for the same frames as stereo samples, if it has one. A block
    /// it fails on, or while it has no instance, passes through as it is.
    pub(crate) fn process(&mut self, samples: &mut [f32], key: Option<&[f32]>) {
        if let State::Making(rx) = &self.state {
            match rx.try_recv() {
                Ok(Ok((instance, _))) => self.ready(instance),
                Ok(Err(Failure::Refused(why))) => {
                    self.off(format!("it refused its settings: {why}"))
                }
                Ok(Err(Failure::Failed(what))) => self.fail(what),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.state = State::Off,
            }
        }
        let sidechain = self.takes_sidechain();
        let State::Ready(instance) = &mut self.state else {
            return;
        };
        let key = match (sidechain, key) {
            (true, Some(key)) => {
                // Exactly the block's frames, as stereo.
                let frames = samples.len() / self.channels.max(1);
                self.key.clear();
                self.key.extend(key.iter().take(frames * 2));
                self.key.resize(frames * 2, 0.0);
                Some(self.key.as_slice())
            }
            _ => None,
        };
        let started = Instant::now();
        match instance.process(samples, key) {
            Ok((out, fuel)) if out.len() == samples.len() => {
                samples.copy_from_slice(&out);
                self.stats.call(started.elapsed(), fuel);
            }
            Ok((out, _)) => {
                let what = format!(
                    "returned {} samples for a block of {}",
                    out.len(),
                    samples.len()
                );
                self.fail(what);
            }
            Err(what) => {
                let took = started.elapsed().as_secs_f64() * 1000.0;
                self.fail(format!("{what}, stopped {took:.1} ms into the call"));
            }
        }
    }

    /// Forget the sound so far (the source starts afresh after a gap).
    pub(crate) fn reset(&mut self) {
        if let State::Ready(instance) = &mut self.state
            && let Err(what) = instance.reset()
        {
            self.fail(what);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_and_params_become_component_values() {
        assert_eq!(color("#ff0000"), Some((1.0, 0.0, 0.0, 1.0)));
        assert_eq!(
            color("#00000080").map(|c| (c.3 * 255.0).round()),
            Some(128.0)
        );
        assert_eq!(color("red"), None);
        let param = |ty, default: serde_json::Value| Param {
            ty,
            default: Some(default),
            min: None,
            max: None,
            values: Vec::new(),
            doc: None,
        };
        let plugin = AudioFilterPlugin {
            kind: "p/k".into(),
            component: PathBuf::new(),
            cache: PathBuf::new(),
            sidechain: false,
            params: BTreeMap::from([
                (
                    "gain_db".into(),
                    param(ParamType::F32, serde_json::json!(0)),
                ),
                (
                    "mode".into(),
                    param(ParamType::Enum, serde_json::json!("a")),
                ),
                ("on".into(), param(ParamType::Bool, serde_json::json!(true))),
            ]),
            log: PathBuf::new(),
            bypassed: None,
        };
        let given = serde_json::json!({"gain_db": -6, "on": "yes"});
        let params = wit_params(&plugin, given.as_object().unwrap());
        // Given, default, and left out when of the wrong type.
        assert_eq!(
            params,
            [
                WitParam {
                    name: "gain_db".into(),
                    value: WitValue::Number(-6.0)
                },
                WitParam {
                    name: "mode".into(),
                    value: WitValue::Text("a".into())
                },
            ]
        );
    }

    use crate::audio_filter::AudioChain;
    use isocast_protocol::plugin::Manifest;
    use isocast_protocol::{AudioFilter, AudioFilterKind, Compressor, Gain};

    /// The example plugin (examples/plugins/gain, its built components),
    /// installed for these tests; its machine code goes to a temporary
    /// folder.
    fn install_example() {
        static ONCE: OnceLock<()> = OnceLock::new();
        ONCE.get_or_init(|| {
            let folder = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins/gain");
            let manifest = Manifest::read(&folder).expect("the example's manifest");
            let dir =
                std::env::temp_dir().join(format!("isocast-wasm-test-{}", std::process::id()));
            set_installed(
                manifest
                    .audio_filters
                    .iter()
                    .map(|spec| AudioFilterPlugin {
                        kind: format!("gain/{}", spec.id),
                        component: folder.join(&spec.component),
                        cache: dir.join(format!("{}.cwasm", spec.id)),
                        sidechain: spec.sidechain,
                        params: spec.params.clone(),
                        log: dir.join("log"),
                        bypassed: None,
                    })
                    .collect(),
            );
        });
    }

    fn filter(kind: AudioFilterKind) -> AudioFilter {
        AudioFilter {
            id: None,
            enabled: true,
            kind,
            unknown: Default::default(),
        }
    }

    fn plugin(kind: &str, params: serde_json::Value) -> AudioFilter {
        let mut value = params;
        value["kind"] = kind.into();
        filter(AudioFilterKind::Plugin(
            PluginKind::from_value(&value).unwrap(),
        ))
    }

    /// A chain at `sources[0]` of the mix whose warnings are kept.
    fn chain(channels: usize, filters: &[AudioFilter]) -> (AudioChain, Arc<Mutex<Vec<String>>>) {
        let warnings = Arc::new(Mutex::new(Vec::new()));
        let reports = Arc::new(Reports::default());
        let kept = warnings.clone();
        reports.set_warn(Arc::new(move |message| kept.lock().push(message)));
        let mut chain = AudioChain::new(channels);
        chain.set_place(Place {
            source: Some(0),
            runs_in: "mix",
            reports,
        });
        chain.update(filters);
        (chain, warnings)
    }

    /// `seconds` of a stereo 1 kHz sine with peak `level`.
    fn tone(level: f32, seconds: f64) -> Vec<f32> {
        let frames = (seconds * f64::from(SAMPLE_RATE)) as usize;
        (0..frames)
            .flat_map(|i| {
                let t = i as f64 / f64::from(SAMPLE_RATE);
                let s = level * (std::f64::consts::TAU * 1000.0 * t).sin() as f32;
                [s, s]
            })
            .collect()
    }

    #[test]
    fn the_example_gain_is_exactly_the_built_in_gain() {
        install_example();
        let input = tone(0.5, 1.0);
        let run = |filters: &[AudioFilter]| {
            let (mut chain, warnings) = chain(2, filters);
            let mut out = input.clone();
            for block in out.chunks_mut(960) {
                chain.process(block, &|_| None);
            }
            assert!(warnings.lock().is_empty(), "{warnings:?}");
            out
        };
        let built_in = run(&[filter(AudioFilterKind::Gain(Gain { gain_db: -6.0 }))]);
        let wasm = run(&[plugin("gain/gain", serde_json::json!({"gain_db": -6}))]);
        assert!(built_in == wasm, "not bit for bit the same");
        // What it costs (`cargo test -- --nocapture` shows it): making the
        // instance (from cached machine code), and a 10 ms block.
        let started = Instant::now();
        let (mut timed, _) = chain(
            2,
            &[plugin("gain/gain", serde_json::json!({"gain_db": -6}))],
        );
        let made = started.elapsed();
        let mut out = input.clone();
        let started = Instant::now();
        for block in out.chunks_mut(960) {
            timed.process(block, &|_| None);
        }
        isocast_diagnostics::diagnostic!(
            "wasm gain: instance made in {:.0} µs, {:.1} µs a 10 ms block",
            made.as_secs_f64() * 1e6,
            started.elapsed().as_secs_f64() * 1e6 / 100.0
        );
        let peak = wasm.iter().fold(0f32, |peak, s| peak.max(s.abs()));
        assert!((peak - 0.2506).abs() < 0.0001, "{peak}");
    }

    #[test]
    fn the_example_ducker_follows_the_built_in_compressor() {
        install_example();
        // Music at 0.5 throughout; a voice in the second quarter only.
        let music = tone(0.5, 2.0);
        let voice: Vec<f32> = (0..music.len() / 2)
            .flat_map(|i| {
                let speaking = (24_000..48_000).contains(&i);
                let s = if speaking {
                    0.25 * ((i as f32) * 0.07).sin()
                } else {
                    0.0
                };
                [s, s]
            })
            .collect();
        let run = |filters: &[AudioFilter]| {
            let (mut chain, _) = chain(2, filters);
            let mut out = music.clone();
            for (n, block) in out.chunks_mut(960).enumerate() {
                let key = &voice[n * 960..(n * 960 + 960).min(voice.len())];
                chain.process(block, &|id| (id == "voice").then_some(key));
            }
            out
        };
        let settings = Compressor {
            ratio: 10.0,
            threshold_db: -30.0,
            attack_ms: 5.0,
            release_ms: 100.0,
            output_gain_db: 0.0,
            sidechain_source: Some("voice".into()),
        };
        let built_in = run(&[filter(AudioFilterKind::Compressor(settings))]);
        let wasm = run(&[plugin(
            "gain/ducker",
            serde_json::json!({"sidechain_source": "voice", "ratio": 10, "threshold_db": -30,
                               "attack_ms": 5, "release_ms": 100}),
        )]);
        // Compare 10 ms peaks in dB: within 0.5 dB everywhere (the formula
        // is the same; only the logarithms are worked out differently).
        let db = |s: &[f32]| 20.0 * s.iter().fold(1e-9f32, |p, x| p.max(x.abs())).log10();
        let mut worst = 0f32;
        for (a, b) in built_in.chunks(960).zip(wasm.chunks(960)) {
            worst = worst.max((db(a) - db(b)).abs());
        }
        isocast_diagnostics::diagnostic!(
            "wasm ducker vs built-in compressor: at most {worst} dB apart"
        );
        assert!(worst < 0.5, "{worst} dB apart");
        // And it did duck: 0.25 is 18 dB over -30, reduced by 18 × 0.9.
        let middle = db(&built_in[60_000..61_000]);
        assert!(middle < db(&music[..960]) - 10.0, "{middle}");
    }

    #[test]
    fn a_failing_filter_passes_the_sound_through_and_is_turned_off_after_three() {
        install_example();
        for fault in ["trap", "loop", "memory"] {
            let params = serde_json::json!({"gain_db": -6, "fault": fault, "fault_after_ms": 20});
            let (mut chain, warnings) = chain(2, &[plugin("gain/gain", params)]);
            let input = tone(0.5, 0.2);
            let mut blocks = 0;
            let mut passed = 0;
            // Until it is off: each fresh copy filters 20 ms, then fails.
            let deadline = Instant::now() + Duration::from_secs(30);
            while warnings.lock().len() < 3 && Instant::now() < deadline {
                for block in input.chunks(960) {
                    let mut out = block.to_vec();
                    chain.process(&mut out, &|_| None);
                    blocks += 1;
                    // Every block is either filtered or the dry sound: never
                    // a gap or anything else.
                    let filtered = out
                        .iter()
                        .zip(block)
                        .all(|(o, i)| *o == i * amplitude_6db());
                    let dry = out == block;
                    assert!(filtered || dry, "{fault}: block {blocks} is neither");
                    passed += usize::from(dry && !filtered);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            let warnings = warnings.lock().clone();
            assert_eq!(warnings.len(), 3, "{fault}: {warnings:#?}");
            assert!(warnings[0].starts_with("sources[0].audio_filters[0] (gain/gain) "));
            assert!(warnings[0].contains("fresh copy"), "{}", warnings[0]);
            assert!(warnings[2].contains("turned off"), "{}", warnings[2]);
            let what = match fault {
                "trap" => "unreachable",
                "loop" => "out of fuel",
                _ => "memory",
            };
            assert!(warnings[0].contains(what), "{fault}: {}", warnings[0]);
            assert!(passed > 0);
            // Off: the sound passes through from now on.
            let mut out = input[..960].to_vec();
            chain.process(&mut out, &|_| None);
            assert_eq!(out, input[..960]);
        }
    }

    fn amplitude_6db() -> f32 {
        10f32.powf(-6.0 / 20.0)
    }

    #[test]
    fn a_missing_plugin_is_bypassed_with_one_warning() {
        install_example();
        let (mut chain, warnings) = chain(1, &[plugin("nope/gain", serde_json::json!({}))]);
        let mut out = vec![0.5; 480];
        chain.process(&mut out, &|_| None);
        assert_eq!(out, vec![0.5; 480]);
        let warnings = warnings.lock();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("isn't installed"), "{}", warnings[0]);
    }
}
