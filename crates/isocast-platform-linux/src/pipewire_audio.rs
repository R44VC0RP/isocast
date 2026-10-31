//! Native PipeWire capture. All PipeWire objects stay on their owning worker.
//!
//! Requires PipeWire headers/runtime >= 1.0.5 (per-buffer graph timestamps),
//! and pipewire-rs's `v0_3_77` feature (resampler-delay queries). Linux
//! Application selection excludes this process's playback. Sink-monitor desktop
//! capture still includes all sound sent to the selected output.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use isocast_engine::{AudioSink, Time};
use isocast_protocol as protocol;
use pipewire as pw;
use protocol::devices::{AudioApp, AudioOutput, Microphone};
use pw::properties::properties;
use pw::proxy::ProxyT;
use pw::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pw::spa::pod::Pod;
use pw::{spa, stream::StreamState};

use crate::pw_loop::WorkerLoop;

const RATE: u32 = 48_000;
const TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(20);
const MAX_CHANNELS: u32 = 64;
// A malformed server buffer must not trigger an unbounded allocation.
const MAX_BUFFER_BYTES: usize = 16 * 1024 * 1024;

pub struct Capture {
    cancel: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            // A sink may remove its own source from its stopped callback.
            // Cancellation still ends that worker when the callback returns.
            if worker.thread().id() != thread::current().id() {
                let _ = worker.join();
            }
        }
    }
}

pub fn microphone(
    device: &str,
    channels: Option<&[u32]>,
    sink: AudioSink,
) -> Result<(Capture, u32)> {
    ensure!(!device.contains('\0'), "microphone.device contains NUL");
    if let Some(channels) = channels {
        selection(MAX_CHANNELS, Some(channels))?;
    }
    start(
        Target::Microphone(device.to_owned()),
        channels.map(<[u32]>::to_vec),
        sink,
    )
}

pub fn desktop(exclude: &[String], sink: AudioSink, _exclude_own: bool) -> Result<(Capture, u32)> {
    ensure!(
        exclude.is_empty(),
        "desktop_audio.exclude: per-application exclusions are not supported by Linux sink-monitor capture; use an empty list"
    );
    // Sink-monitor capture currently includes our own playback too.
    start(Target::Desktop, None, sink)
}

pub fn application(app: &str, sink: AudioSink) -> Result<(Capture, u32)> {
    validate_application(app)?;
    start(Target::Application(app.to_owned()), None, sink)
}

pub fn probe_application(app: &str) -> Result<String> {
    validate_application(app)?;
    let app = app.to_owned();
    thread::Builder::new()
        .name("pipewire-application-probe".into())
        .spawn(move || enumerate(&AtomicBool::new(false), true)?.apps.resolve(&app))?
        .join()
        .map_err(|_| anyhow!("PipeWire application probe panicked"))?
}

fn validate_application(app: &str) -> Result<()> {
    ensure!(
        !app.is_empty() && !app.contains('\0'),
        "application_audio.app: expected a nonempty application ID or unambiguous name/binary without NUL"
    );
    Ok(())
}

enum Target {
    Microphone(String),
    Desktop,
    Application(String),
}

fn start(target: Target, channels: Option<Vec<u32>>, sink: AudioSink) -> Result<(Capture, u32)> {
    let sink = Arc::new(sink);
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = cancel.clone();
    let (tx, rx) = mpsc::sync_channel(1);
    let worker = thread::Builder::new()
        .name("pipewire-audio".into())
        .spawn(move || {
            let mut started = false;
            let result = capture_worker(
                &target,
                channels.as_deref(),
                &sink,
                &worker_cancel,
                &tx,
                &mut started,
            );
            if let Err(error) = result {
                if started {
                    if !worker_cancel.load(Ordering::Acquire) {
                        (sink.stopped)(format!("PipeWire capture: {error:#}"));
                    }
                } else {
                    let _ = tx.send(Err(error));
                }
            }
        })
        .context("starting PipeWire capture worker")?;
    let capture = Capture {
        cancel,
        worker: Some(worker),
    };
    // Enumeration and stream startup each have an internal deadline. Dropping
    // on timeout cancels and joins rather than leaving an orphaned capture.
    let count = rx
        .recv_timeout(TIMEOUT * 2 + POLL)
        .context("PipeWire capture startup timed out or worker stopped")??;
    Ok((capture, count))
}

#[derive(Default)]
struct Inventory {
    nodes: BTreeMap<u32, Device>,
    apps: Applications,
    input: Option<String>,
    output: Option<String>,
}

#[derive(Default)]
struct Device {
    id: String,
    name: String,
    model: String,
    manufacturer: String,
    input: bool,
    channels: u32,
    rate: u32,
    position: Option<[u32; spa::param::audio::MAX_CHANNELS]>,
}

impl Device {
    fn update(&mut self, props: &spa::utils::dict::DictRef) {
        for (key, dest) in [
            ("node.name", &mut self.id),
            ("node.description", &mut self.name),
            ("device.product.name", &mut self.model),
            ("device.vendor.name", &mut self.manufacturer),
        ] {
            if let Some(value) = props.get(key) {
                *dest = value.to_owned();
            }
        }
        if self.name.is_empty() {
            self.name = props.get("node.nick").unwrap_or(&self.id).to_owned();
        }
        if let Some(count) = props.get("audio.channels").and_then(|v| v.parse().ok()) {
            self.channels = count;
        }
        if let Some(rate) = props.get("audio.rate").and_then(|v| v.parse().ok()) {
            self.rate = rate;
        }
    }
}

pub fn devices() -> Result<(Vec<Microphone>, Vec<AudioOutput>, Vec<AudioApp>)> {
    // Registry proxies/listeners are !Send, and are created and destroyed here.
    thread::Builder::new()
        .name("pipewire-devices".into())
        .spawn(|| {
            let inventory = enumerate(&AtomicBool::new(false), true)?;
            let mut microphones = Vec::new();
            let mut outputs = Vec::new();
            for device in inventory.nodes.into_values() {
                if device.id.is_empty() {
                    continue;
                }
                if device.input {
                    microphones.push(Microphone {
                        default: inventory.input.as_deref() == Some(&device.id),
                        id: device.id,
                        name: device.name,
                        model: device.model,
                        manufacturer: device.manufacturer,
                        // PipeWire converts the graph to our fixed stream rate.
                        sample_rates: vec![f64::from(RATE)],
                        channels: device.channels,
                        channel_names: Vec::new(),
                    });
                } else {
                    outputs.push(AudioOutput {
                        default: inventory.output.as_deref() == Some(&device.id),
                        id: device.id,
                        name: device.name,
                        sample_rate: f64::from(device.rate),
                        channels: device.channels,
                    });
                }
            }
            Ok((microphones, outputs, inventory.apps.list()))
        })
        .context("starting PipeWire device enumeration")?
        .join()
        .map_err(|_| anyhow!("PipeWire device worker panicked"))?
}

fn enumerate(cancel: &AtomicBool, include_apps: bool) -> Result<Inventory> {
    pw::init();
    let deadline = Instant::now() + TIMEOUT;
    let mainloop = WorkerLoop::new()?;
    let context = pw::context::ContextRc::new(mainloop.main_loop(), None)?;
    let core = context.connect_rc(None).context("connecting to PipeWire")?;
    let apps = include_apps
        .then(|| ApplicationWatch::new(&core, false, Rc::default()))
        .transpose()?;
    let registry = core.get_registry_rc()?;
    let inventory = Rc::new(RefCell::new(Inventory::default()));
    let failure = Rc::new(RefCell::new(None));
    let error = failure.clone();
    let _core_listener = core
        .add_listener_local()
        .error(move |id, _, _, message| {
            if id == pw::core::PW_ID_CORE {
                *error.borrow_mut() = Some(message.to_owned());
            }
        })
        .register();
    let nodes = Rc::new(RefCell::new(Vec::new()));
    let metadata = Rc::new(RefCell::new(Vec::new()));
    let state = inventory.clone();
    let weak_registry = registry.downgrade();
    let keep_nodes = nodes.clone();
    let keep_metadata = metadata.clone();
    let error = failure.clone();
    let removed = inventory.clone();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            let Some(props) = global.props else { return };
            let Some(registry) = weak_registry.upgrade() else {
                return;
            };
            if global.type_ == pw::types::ObjectType::Node {
                let input = match props.get("media.class") {
                    Some("Audio/Source") => true,
                    Some("Audio/Sink") => false,
                    _ => return,
                };
                let mut device = Device {
                    input,
                    ..Device::default()
                };
                device.update(props);
                let id = global.id;
                state.borrow_mut().nodes.insert(id, device);
                let node: pw::node::Node = match registry.bind(global) {
                    Ok(node) => node,
                    Err(err) => {
                        *error.borrow_mut() = Some(err.to_string());
                        return;
                    }
                };
                let info_state = state.clone();
                let param_state = state.clone();
                let listener = node
                    .add_listener_local()
                    .info(move |info| {
                        if let (Some(device), Some(props)) =
                            (info_state.borrow_mut().nodes.get_mut(&id), info.props())
                        {
                            device.update(props);
                        }
                    })
                    .param(move |_, _, _, _, param| {
                        let Some(param) = param else { return };
                        let mut format = AudioInfoRaw::new();
                        if format.parse(param).is_ok()
                            && let Some(device) = param_state.borrow_mut().nodes.get_mut(&id)
                        {
                            if format.channels() > 0 {
                                device.channels = format.channels();
                                device.position = Some(format.position());
                            }
                            if format.rate() > 0 {
                                device.rate = format.rate();
                            }
                        }
                    })
                    .register();
                node.enum_params(0, Some(spa::param::ParamType::EnumFormat), 0, 1);
                node.enum_params(0, Some(spa::param::ParamType::Format), 0, 1);
                keep_nodes.borrow_mut().push((listener, node));
            } else if global.type_ == pw::types::ObjectType::Metadata
                && props.get("metadata.name") == Some("default")
            {
                let object: pw::metadata::Metadata = match registry.bind(global) {
                    Ok(object) => object,
                    Err(err) => {
                        *error.borrow_mut() = Some(err.to_string());
                        return;
                    }
                };
                let state = state.clone();
                let listener = object
                    .add_listener_local()
                    .property(move |subject, key, _, value| {
                        if subject == 0 {
                            let mut state = state.borrow_mut();
                            match key {
                                Some("default.audio.source") => {
                                    state.input = value.and_then(default_name)
                                }
                                Some("default.audio.sink") => {
                                    state.output = value.and_then(default_name)
                                }
                                None => {
                                    state.input = None;
                                    state.output = None;
                                }
                                _ => {}
                            }
                        }
                        0
                    })
                    .register();
                keep_metadata.borrow_mut().push((listener, object));
            }
        })
        .global_remove(move |id| {
            removed.borrow_mut().nodes.remove(&id);
        })
        .register();
    // The first barrier discovers globals. The second covers the bindings,
    // node format queries and default metadata subscribed from those callbacks.
    for _ in 0..2 {
        let done = Rc::new(Cell::new(false));
        let completed = done.clone();
        let pending = core.sync(0)?;
        let _sync = core
            .add_listener_local()
            .done(move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq == pending {
                    completed.set(true);
                }
            })
            .register();
        while !done.get() {
            ensure!(
                !cancel.load(Ordering::Acquire),
                "PipeWire enumeration cancelled"
            );
            ensure!(
                Instant::now() < deadline,
                "PipeWire device enumeration timed out"
            );
            if let Some(error) = failure.borrow_mut().take() {
                bail!("PipeWire enumeration: {error}");
            }
            iterate(&mainloop)?;
        }
    }
    if let Some(error) = failure.borrow_mut().take() {
        bail!("PipeWire enumeration: {error}");
    }
    let mut result = std::mem::take(&mut *inventory.borrow_mut());
    if let Some(apps) = apps {
        result.apps = std::mem::take(&mut *apps.state.borrow_mut());
    }
    Ok(result)
}

#[derive(Default)]
struct AppProperties {
    name: Option<String>,
    id: Option<String>,
    binary: Option<String>,
    pid: Option<String>,
}

impl AppProperties {
    fn update(&mut self, props: &spa::utils::dict::DictRef) {
        for (key, dest) in [
            ("application.name", &mut self.name),
            ("application.id", &mut self.id),
            ("application.process.binary", &mut self.binary),
            ("application.process.id", &mut self.pid),
        ] {
            if let Some(value) = props.get(key) {
                *dest = (!value.is_empty()).then(|| value.to_owned());
            }
        }
    }

    fn application(&self, client: Option<&Self>, playing: bool) -> Option<AudioApp> {
        let id = self
            .id
            .as_ref()
            .or_else(|| client.and_then(|p| p.id.as_ref()));
        let binary = self
            .binary
            .as_ref()
            .or_else(|| client.and_then(|p| p.binary.as_ref()));
        let name = self
            .name
            .as_ref()
            .or_else(|| client.and_then(|p| p.name.as_ref()));
        let canonical = if let Some(id) = id {
            format!("pipewire:application:{id}")
        } else if let Some(binary) = binary {
            format!("pipewire:binary:{binary}")
        } else {
            format!("pipewire:name:{}", name?)
        };
        Some(AudioApp {
            id: canonical,
            name: name.or(binary).or(id)?.clone(),
            bundle_id: id.cloned(),
            binary: binary.cloned(),
            pid: self
                .pid
                .as_ref()
                .or_else(|| client.and_then(|p| p.pid.as_ref()))
                .and_then(|pid| pid.parse().ok())
                .unwrap_or(0),
            playing,
        })
    }
}

#[derive(Default)]
struct AppNode {
    generation: u64,
    props: AppProperties,
    client: Option<u32>,
    informed: bool,
    playing: bool,
    output_count: u32,
}

#[derive(Default)]
struct AppClient {
    props: AppProperties,
    informed: bool,
}

#[derive(Default)]
struct AudioPort {
    node: u32,
    input: bool,
    channel: String,
    monitor: bool,
}

impl AudioPort {
    fn update(&mut self, props: &spa::utils::dict::DictRef) {
        if let Some(node) = props.get("node.id").and_then(|v| v.parse().ok()) {
            self.node = node;
        }
        if let Some(direction) = props.get("port.direction") {
            self.input = direction == "in";
        }
        if let Some(channel) = props.get("audio.channel") {
            self.channel = channel.to_owned();
        }
        if let Some(monitor) = props.get("port.monitor") {
            self.monitor = monitor == "true";
        }
    }
}

#[derive(Default)]
struct Applications {
    nodes: BTreeMap<u32, AppNode>,
    clients: BTreeMap<u32, AppClient>,
    ports: BTreeMap<u32, AudioPort>,
}

impl Applications {
    fn ready(&self, identity: &str, capture: u32) -> bool {
        if self
            .ports
            .values()
            .filter(|port| port.node == capture && port.input)
            .count()
            != 2
        {
            return false;
        }
        self.nodes.iter().all(|(&id, node)| {
            node.informed
                && node.client.is_none_or(|client| {
                    self.clients
                        .get(&client)
                        .is_some_and(|client| client.informed)
                })
                && (self.app(node).is_none_or(|app| app.id != identity)
                    || self
                        .ports
                        .values()
                        .filter(|port| port.node == id && !port.input)
                        .count()
                        == node.output_count as usize)
        })
    }

    fn app(&self, node: &AppNode) -> Option<AudioApp> {
        if !node.informed {
            return None;
        }
        // Do not briefly link under a weaker identity before Client.info arrives.
        let client = if let Some(id) = node.client {
            let client = self.clients.get(&id)?;
            if !client.informed {
                return None;
            }
            Some(&client.props)
        } else {
            None
        };
        let app = node.props.application(client, node.playing)?;
        // Monitoring is a playback stream too: never list or capture ourselves.
        (app.pid != std::process::id()).then_some(app)
    }

    fn list(&self) -> Vec<AudioApp> {
        let mut apps: BTreeMap<String, AudioApp> = BTreeMap::new();
        for app in self.nodes.values().filter_map(|node| self.app(node)) {
            apps.entry(app.id.clone())
                .and_modify(|other| other.playing |= app.playing)
                .or_insert(app);
        }
        let mut apps: Vec<_> = apps.into_values().collect();
        apps.sort_by(|a, b| b.playing.cmp(&a.playing).then_with(|| a.id.cmp(&b.id)));
        apps
    }

    fn resolve(&self, selector: &str) -> Result<String> {
        let mut matches = BTreeSet::new();
        for app in self.nodes.values().filter_map(|node| self.app(node)) {
            let selected = if selector.starts_with("pipewire:") {
                app.id == selector
            } else {
                app.name.eq_ignore_ascii_case(selector)
                    || app
                        .binary
                        .as_deref()
                        .is_some_and(|binary| binary.eq_ignore_ascii_case(selector))
                    || app.bundle_id.as_deref() == Some(selector)
            };
            if selected {
                matches.insert(app.id);
            }
        }
        ensure!(
            !matches.is_empty(),
            "application_audio.app: {selector:?} not found; valid application IDs (up to 10): {} (the app must expose a playback stream)",
            self.list()
                .into_iter()
                .take(10)
                .map(|app| app.id)
                .collect::<Vec<_>>()
                .join(", ")
        );
        ensure!(
            matches.len() == 1,
            "application_audio.app: {selector:?} is ambiguous; use one of: {}",
            matches.iter().cloned().collect::<Vec<_>>().join(", ")
        );
        matches
            .into_iter()
            .next()
            .context("application_audio.app: no selected identity")
    }

    fn desired_links(
        &self,
        identity: &str,
        capture: u32,
        policy: &mut AppLinkPolicy,
        started: bool,
    ) -> Result<BTreeSet<AppLink>> {
        policy.generations.retain(|id, generation| {
            self.nodes
                .get(id)
                .is_some_and(|node| node.generation == *generation)
        });
        policy
            .skipped
            .retain(|id| policy.generations.contains_key(id));
        policy
            .failures
            .retain(|id, _| policy.generations.contains_key(id));
        policy
            .generations
            .extend(self.nodes.iter().map(|(&id, node)| (id, node.generation)));
        let mut desired = BTreeSet::new();
        if capture == u32::MAX {
            return Ok(desired);
        }
        let inputs: Vec<_> = self
            .ports
            .iter()
            .filter(|(_, p)| p.node == capture && p.input)
            .collect();
        for (&node_id, node) in &self.nodes {
            if policy.skipped.contains(&node_id)
                || self.app(node).is_none_or(|app| app.id != identity)
            {
                continue;
            }
            let outputs: Vec<_> = self
                .ports
                .iter()
                .filter(|(_, p)| p.node == node_id && !p.input)
                .collect();
            // Node.info and Port globals arrive separately. Never mistake the
            // first stereo port for mono, including when monitor ports follow.
            if outputs.len() != node.output_count as usize {
                continue;
            }
            let outputs: Vec<_> = outputs.into_iter().filter(|(_, p)| !p.monitor).collect();
            if outputs.len() > 2 {
                policy.reject(node_id, started, format!(
                    "application_audio.app: {identity:?} has a {}-channel playback stream; Linux supports mono/stereo application streams only",
                    outputs.len()
                ))?;
                continue;
            }
            let mut stream_links = BTreeSet::new();
            for (index, &(input_id, input)) in inputs.iter().enumerate() {
                // Mono feeds both stereo inputs. Otherwise match channel labels;
                // only unlabeled stereo may fall back to stable port order.
                let output = if outputs.len() == 1 {
                    outputs.first().copied()
                } else {
                    outputs
                        .iter()
                        .copied()
                        .find(|(_, p)| !p.channel.is_empty() && p.channel == input.channel)
                        .or_else(|| {
                            if outputs.len() == 2
                                && outputs.iter().all(|(_, p)| p.channel.is_empty())
                            {
                                outputs.get(index).copied()
                            } else {
                                None
                            }
                        })
                };
                if outputs.len() == 2 && output.is_none() {
                    policy.reject(node_id, started, format!(
                        "application_audio.app: {identity:?} playback channels {:?} cannot be mapped to stereo FL/FR; Linux supports mono, FL/FR stereo or unlabeled stereo",
                        outputs.iter().map(|(_, port)| port.channel.as_str()).collect::<Vec<_>>()
                    ))?;
                    stream_links.clear();
                    break;
                }
                if let Some((&output_id, _)) = output {
                    stream_links.insert(AppLink {
                        output_node: node_id,
                        output_port: output_id,
                        input_node: capture,
                        input_port: *input_id,
                    });
                }
            }
            desired.extend(stream_links);
        }
        Ok(desired)
    }
}

// Separate from hardware/default metadata: the same watcher serves enumeration
// and the live capture graph, including Node/Client info absent from globals.
struct ApplicationWatch {
    _listener: pw::registry::Listener,
    _registry: pw::registry::RegistryRc,
    state: Rc<RefCell<Applications>>,
}

impl ApplicationWatch {
    fn new(
        core: &pw::core::CoreRc,
        include_ports: bool,
        proxy_ids: Rc<RefCell<BTreeSet<u32>>>,
    ) -> Result<Self> {
        // Do not classify this registry as an owned application proxy: its
        // errors still indicate a broken capture connection, not one bad app.
        let registry = core.get_registry_rc()?;
        let state = Rc::new(RefCell::new(Applications::default()));
        let nodes = Rc::new(RefCell::new(BTreeMap::new()));
        let clients = Rc::new(RefCell::new(BTreeMap::new()));
        let weak_registry = registry.downgrade();
        let added = state.clone();
        let removed = state.clone();
        let keep_nodes = nodes.clone();
        let keep_clients = clients.clone();
        // A local incarnation counter also works when object.serial is absent.
        // Removal and ID reuse can arrive during the same loop iteration.
        let generation = Cell::new(0_u64);
        let listener = registry
            .add_listener_local()
            .global(move |global| {
                let Some(registry) = weak_registry.upgrade() else {
                    return;
                };
                let id = global.id;
                if global.type_ == pw::types::ObjectType::Node
                    && global.props.and_then(|p| p.get("media.class"))
                        == Some("Stream/Output/Audio")
                {
                    let Ok(node) = registry.bind::<pw::node::Node, _>(global) else {
                        return;
                    };
                    proxy_ids.borrow_mut().insert(node.upcast_ref().id());
                    generation.set(generation.get().wrapping_add(1));
                    let mut entry = AppNode {
                        generation: generation.get(),
                        ..AppNode::default()
                    };
                    if let Some(props) = global.props {
                        entry.props.update(props);
                        entry.client = props.get("client.id").and_then(|v| v.parse().ok());
                    }
                    added.borrow_mut().nodes.insert(id, entry);
                    let state = added.clone();
                    let listener = node
                        .add_listener_local()
                        .info(move |info| {
                            if let Some(node) = state.borrow_mut().nodes.get_mut(&id) {
                                if let Some(props) = info.props() {
                                    node.props.update(props);
                                    if let Some(client) = props.get("client.id") {
                                        node.client = client.parse().ok();
                                    }
                                }
                                node.informed = true;
                                node.output_count = info.n_output_ports();
                                node.playing = matches!(info.state(), pw::node::NodeState::Running);
                            }
                        })
                        .register();
                    keep_nodes.borrow_mut().insert(id, (listener, node));
                } else if global.type_ == pw::types::ObjectType::Client {
                    let Ok(client) = registry.bind::<pw::client::Client, _>(global) else {
                        return;
                    };
                    proxy_ids.borrow_mut().insert(client.upcast_ref().id());
                    let mut entry = AppClient::default();
                    if let Some(props) = global.props {
                        entry.props.update(props);
                    }
                    added.borrow_mut().clients.insert(id, entry);
                    let state = added.clone();
                    let listener = client
                        .add_listener_local()
                        .info(move |info| {
                            if let Some(client) = state.borrow_mut().clients.get_mut(&id) {
                                if let Some(props) = info.props() {
                                    client.props.update(props);
                                }
                                client.informed = true;
                            }
                        })
                        .register();
                    keep_clients.borrow_mut().insert(id, (listener, client));
                } else if include_ports && global.type_ == pw::types::ObjectType::Port {
                    let mut entry = AudioPort::default();
                    if let Some(props) = global.props {
                        entry.update(props);
                    }
                    added.borrow_mut().ports.insert(id, entry);
                }
            })
            .global_remove(move |id| {
                let mut state = removed.borrow_mut();
                state.nodes.remove(&id);
                state.clients.remove(&id);
                state.ports.remove(&id);
                nodes.borrow_mut().remove(&id);
                clients.borrow_mut().remove(&id);
            })
            .register();
        Ok(Self {
            _listener: listener,
            _registry: registry,
            state,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct AppLink {
    output_node: u32,
    output_port: u32,
    input_node: u32,
    input_port: u32,
}

struct OwnedAppLink {
    _proxy_listener: pw::proxy::ProxyListener,
    _listener: pw::link::LinkListener,
    _link: pw::link::Link,
    failure: Rc<RefCell<Option<String>>>,
    negotiated: Rc<Cell<bool>>,
    created: Instant,
}

impl OwnedAppLink {
    fn new(
        core: &pw::core::CoreRc,
        pair: AppLink,
        proxy_ids: Rc<RefCell<BTreeSet<u32>>>,
    ) -> Result<Self> {
        let link = core.create_object::<pw::link::Link>(
            "link-factory",
            &properties! {
                "link.output.node" => pair.output_node.to_string(),
                "link.output.port" => pair.output_port.to_string(),
                "link.input.node" => pair.input_node.to_string(),
                "link.input.port" => pair.input_port.to_string(),
                // Dropping this proxy destroys only our extra link, not playback's.
                "object.linger" => "false",
                "link.passive" => "true",
            },
        )?;
        let failure = Rc::new(RefCell::new(None));
        let failed = failure.clone();
        let removed = failure.clone();
        let proxy_listener = link
            .upcast_ref()
            .add_listener_local()
            .error(move |_, _, message| *failed.borrow_mut() = Some(message.to_owned()))
            .removed(move || *removed.borrow_mut() = Some("application link removed".into()))
            .register();
        let failed = failure.clone();
        let negotiated = Rc::new(Cell::new(false));
        let linked = negotiated.clone();
        let listener = link
            .add_listener_local()
            .info(move |info| {
                if let pw::link::LinkState::Error(error) = info.state() {
                    *failed.borrow_mut() = Some(error.to_owned());
                }
                linked.set(matches!(
                    info.state(),
                    pw::link::LinkState::Paused | pw::link::LinkState::Active
                ));
            })
            .register();
        proxy_ids.borrow_mut().insert(link.upcast_ref().id());
        Ok(Self {
            _proxy_listener: proxy_listener,
            _listener: listener,
            _link: link,
            failure,
            negotiated,
            created: Instant::now(),
        })
    }
}

#[derive(Default)]
struct AppLinkPolicy {
    // At most one entry per live playback node; pruned by desired_links.
    generations: BTreeMap<u32, u64>,
    skipped: BTreeSet<u32>,
    failures: BTreeMap<u32, Instant>,
    // Historical local IDs of owned Node/Client/Link proxies, including zombies
    // awaiting remove_id. PipeWire recycles IDs, so this is bounded by the proxy
    // allocator's high-water mark, not stream churn or capture duration.
    // Invariant: create/connect all non-owned proxies before starting this
    // watcher, and retain them through capture. Only owned proxies may later
    // reuse IDs; never add the core, capture stream or either registry here.
    proxy_ids: Rc<RefCell<BTreeSet<u32>>>,
}

impl AppLinkPolicy {
    fn reject(&mut self, node: u32, started: bool, message: String) -> Result<()> {
        ensure!(started, "{message}");
        if self.skipped.insert(node) {
            isocast_diagnostics::output::deferred(
                "warn",
                module_path!(),
                format!("skipping application playback node {node}: {message}"),
            );
        }
        self.failures.remove(&node);
        Ok(())
    }

    fn failed(&mut self, node: u32, started: bool, now: Instant, message: &str) -> Result<()> {
        let since = *self.failures.entry(node).or_insert(now);
        if now.saturating_duration_since(since) >= TIMEOUT {
            self.reject(node, started, format!("application_audio.link: {message}"))?;
        }
        Ok(())
    }
}

fn reconcile_links(
    core: &pw::core::CoreRc,
    desired: &BTreeSet<AppLink>,
    links: &mut BTreeMap<AppLink, OwnedAppLink>,
    policy: &mut AppLinkPolicy,
    started: bool,
) -> Result<()> {
    links.retain(|pair, _| desired.contains(pair) && !policy.skipped.contains(&pair.output_node));
    // Only a fully negotiated stream resets its retry window. One good
    // channel must not hide a persistently refused sibling channel.
    policy.failures.retain(|node, _| {
        desired.iter().any(|pair| {
            pair.output_node == *node
                && links
                    .get(pair)
                    .is_none_or(|link| !link.negotiated.get() || link.failure.borrow().is_some())
        })
    });
    for &pair in desired {
        if policy.skipped.contains(&pair.output_node) {
            continue;
        }
        if let Some(link) = links.get(&pair) {
            let failure = link.failure.borrow();
            let Some(message) = failure.as_ref() else {
                if !link.negotiated.get() {
                    policy.failed(
                        pair.output_node,
                        started,
                        Instant::now(),
                        "link negotiation timed out",
                    )?;
                }
                continue;
            };
            policy.failed(pair.output_node, started, Instant::now(), message)?;
            // Node/port removal can race creation. Let registry removals arrive
            // before retrying; a persistent linking failure still surfaces.
            if policy.skipped.contains(&pair.output_node)
                || link.created.elapsed() < Duration::from_millis(250)
            {
                continue;
            }
        }
        links.remove(&pair);
        match OwnedAppLink::new(core, pair, policy.proxy_ids.clone()) {
            Ok(link) => {
                links.insert(pair, link);
            }
            Err(error) => {
                // A local factory/create failure has no proxy that can later
                // negotiate. Surface it before the generic startup deadline.
                ensure!(started, "application_audio.link: {error}");
                policy.failed(
                    pair.output_node,
                    started,
                    Instant::now(),
                    &error.to_string(),
                )?;
            }
        }
    }
    links.retain(|pair, _| !policy.skipped.contains(&pair.output_node));
    Ok(())
}

fn fatal_capture_error(
    application: bool,
    id: u32,
    result: i32,
    owned_proxies: &BTreeSet<u32>,
) -> bool {
    // A vanished global can reject a pending bind on its new proxy ID; destroy
    // of an already-removed proxy reports core ENOENT. Both are benign races.
    // Node errors also fan out to all bound Node resources (impl-node), not
    // just our links. Restrict other non-core exemptions to owned app proxies,
    // retaining their IDs through the zombie interval until server remove_id.
    !application
        || (result != -libc::ENOENT && (id == pw::core::PW_ID_CORE || !owned_proxies.contains(&id)))
}

fn default_name(value: &str) -> Option<String> {
    // PipeWire's own SPA JSON parser handles escaped node names; avoid a
    // hand-written JSON subset or another dependency just for default metadata.
    let value = CString::new(value).ok()?;
    // SAFETY: the string is terminated and lives through this copying call.
    let raw = unsafe { pw::sys::pw_properties_new_string(value.as_ptr()) };
    let raw = std::ptr::NonNull::new(raw)?;
    // SAFETY: new_string returned a uniquely owned pw_properties allocation.
    let props = unsafe { pw::properties::PropertiesBox::from_raw(raw) };
    props.get("name").map(str::to_owned)
}

fn iterate(mainloop: &WorkerLoop) -> Result<()> {
    mainloop.iterate(POLL)
}

#[derive(Default)]
struct Status {
    ready: Option<u32>,
    error: Option<String>,
}

struct StreamData {
    application_clock: Option<ApplicationClock>,
    sink: Arc<AudioSink>,
    status: Rc<RefCell<Status>>,
    channels: u32,
    pick: Vec<usize>,
    samples: Vec<f32>,
    concealed_samples: u64,
    corrupted_chunks: u64,
    bridged_frames: u64,
    clock_offset: i64,
    sink_monitor: bool,
    links: Rc<RefCell<BTreeMap<u32, (u32, u32)>>>,
    position: *const spa::sys::spa_io_position,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GraphClock {
    id: u32,
    nsec: u64,
    duration: u64,
    num: u32,
    denom: u32,
}

impl StreamData {
    fn monitor_delay(&self, node: u32, graph: u64) -> u64 {
        if !self.sink_monitor || self.position.is_null() {
            return 0;
        }
        let Some(peer) = linked_peer(&self.links.borrow(), node) else {
            return 0;
        };
        let Some(clock) = self.graph_clock() else {
            return 0;
        };
        monitor_delay(false, Some(peer), clock, graph)
    }

    fn graph_clock(&self) -> Option<GraphClock> {
        if self.position.is_null() {
            return None;
        }
        // The IO mapping belongs to PipeWire until io_changed replaces/removes
        // it on this main loop. Its clock is written by the external driver;
        // access only aligned scalar fields through atomic loads.
        let read = || {
            // SAFETY: io_changed validated this native mapping's size/alignment.
            // Read only aligned atomic scalars, not a reference to the whole clock.
            unsafe {
                let clock = std::ptr::addr_of!((*self.position).clock);
                GraphClock {
                    id: AtomicU32::from_ptr(std::ptr::addr_of!((*clock).id).cast_mut())
                        .load(Ordering::Relaxed),
                    nsec: AtomicU64::from_ptr(std::ptr::addr_of!((*clock).nsec).cast_mut())
                        .load(Ordering::Relaxed),
                    duration: AtomicU64::from_ptr(std::ptr::addr_of!((*clock).duration).cast_mut())
                        .load(Ordering::Relaxed),
                    num: AtomicU32::from_ptr(std::ptr::addr_of!((*clock).rate.num).cast_mut())
                        .load(Ordering::Relaxed),
                    denom: AtomicU32::from_ptr(std::ptr::addr_of!((*clock).rate.denom).cast_mut())
                        .load(Ordering::Relaxed),
                }
            }
        };
        let clock = read();
        let next = read();
        if clock.id != next.id
            || clock.duration != next.duration
            || clock.num != next.num
            || clock.denom != next.denom
        {
            return None;
        }
        Some(next)
    }
}

#[derive(Default)]
struct ApplicationClock {
    previous: Option<(Option<u32>, Time, bool)>,
}

impl ApplicationClock {
    fn silence(
        &mut self,
        driver: Option<u32>,
        at: Time,
        frames: u64,
        silent: bool,
    ) -> Option<(Time, usize)> {
        let previous = self
            .previous
            .replace((driver, at.add_ticks(frames, RATE), silent));
        let (previous_driver, end, was_silent) = previous?;
        let (previous_driver, driver) = (previous_driver?, driver?);
        let gap = at.ticks_since(end, RATE);
        // An unlinked application runs silence on the fallback driver. Its
        // phase can differ from the playback driver's by one quantum. Bridge
        // only that silent clock handover, never missing active graph cycles,
        // and never shift the timestamp of actual application samples.
        (previous_driver != driver && (silent || was_silent) && gap > 0 && gap as u64 <= frames)
            .then_some((end, gap as usize))
    }
}

fn linked_peer(links: &BTreeMap<u32, (u32, u32)>, node: u32) -> Option<u32> {
    let mut peers = links.values().filter(|&&(input, _)| input == node);
    let &(_, peer) = peers.next()?;
    if peers.any(|&(_, output)| output != peer) {
        return None;
    }
    Some(peer)
}

fn monitor_delay(microphone: bool, peer: Option<u32>, clock: GraphClock, graph: u64) -> u64 {
    // Only a sink that drives this cycle supplies last cycle's monitor samples.
    // Do not infer the peer from default metadata: desktop capture can relink.
    // Like pw_time.delay, this uses the current graph configuration for queued
    // buffers. Callback lag must not toggle the correction on a stable graph.
    // Buffers spanning a driver/quantum change can still use the newer state.
    if microphone
        || peer != Some(clock.id)
        || clock.id == u32::MAX
        || graph == 0
        || clock.nsec < graph
        || clock.num == 0
        || clock.denom == 0
    {
        return 0;
    }
    let nanos = u128::from(clock.duration) * u128::from(clock.num) * 1_000_000_000
        / u128::from(clock.denom);
    // Reject implausible/uninitialized graph quanta without overflowing or
    // applying an unbounded timestamp adjustment to a recording.
    if nanos == 0 || nanos > 1_000_000_000 {
        return 0;
    }
    nanos as u64
}

impl Drop for StreamData {
    fn drop(&mut self) {
        // One summary per capture, never a per-buffer log on the callback.
        if self.concealed_samples > 0 || self.corrupted_chunks > 0 || self.bridged_frames > 0 {
            isocast_diagnostics::output::deferred(
                "warn",
                module_path!(),
                format!(
                    "replaced {} non-finite samples with silence; skipped {} corrupted chunks; bridged {} silent application frames",
                    self.concealed_samples, self.corrupted_chunks, self.bridged_frames
                ),
            );
        }
    }
}

fn capture_worker(
    requested: &Target,
    channels: Option<&[u32]>,
    sink: &Arc<AudioSink>,
    cancel: &AtomicBool,
    ready: &mpsc::SyncSender<Result<u32>>,
    started: &mut bool,
) -> Result<()> {
    let version = require_buffer_timestamps()?;
    let inventory = enumerate(cancel, matches!(requested, Target::Application(_)))?;
    let microphone = matches!(requested, Target::Microphone(_));
    let application = match requested {
        Target::Application(app) => Some(inventory.apps.resolve(app)?),
        _ => None,
    };
    let is_application = application.is_some();
    let target = if let Target::Microphone(device) = requested {
        let device = device.as_str();
        let id = if matches!(device, "default" | "main") {
            inventory
                .input
                .as_deref()
                .context("microphone.device: no default PipeWire source")?
        } else {
            device
        };
        Some(
            inventory
                .nodes
                .values()
                .find(|node| node.input && (node.id == id || node.name == id))
                .with_context(|| {
                    format!("microphone.device: {id:?} not found; use a node.name from devices")
                })?,
        )
    } else {
        None
    };
    let count = target.map_or(2, |node| node.channels);
    let pick = selection(count, channels)?;
    let mut props = properties! {
        "media.type" => "Audio",
        "media.category" => "Capture",
        "media.role" => "Production",
        "node.name" => "isocast.capture",
        "node.description" => "Isocast capture",
    };
    if let Some(target) = target {
        props.insert("target.object", target.id.as_str());
        // Never switch a recording to another input when this device vanishes.
        props.insert("node.dont-reconnect", "true");
        // Capture native channels in their original order, not a stereo remix.
        props.insert("stream.dont-remix", "true");
    } else if is_application {
        // Let the session manager configure DSP ports, but never choose a
        // hardware/default source. Competing adapter.auto-port-config races
        // WirePlumber's PortConfig activation. We own only the extra links;
        // PipeWire mixes them and keeps processing silence when none remain.
        props.insert("node.autoconnect", "false");
        props.insert("node.passive", "true");
        props.insert("node.always-process", "true");
        props.insert("stream.dont-remix", "true");
    } else {
        // With no target.object the session manager selects the default sink.
        props.insert("stream.capture.sink", "true");
    }
    let deadline = Instant::now() + TIMEOUT;
    let mainloop = WorkerLoop::new()?;
    let context = capture_context(&mainloop, version)?;
    let core = context
        .connect_rc(None)
        .context("connecting audio stream to PipeWire")?;
    let status = Rc::new(RefCell::new(Status::default()));
    let errors = status.clone();
    let mut link_policy = AppLinkPolicy::default();
    let proxy_ids = link_policy.proxy_ids.clone();
    let _core_listener = core
        .add_listener_local()
        .error(move |id, _, result, message| {
            // App-link errors/removals are handled by their owned proxies;
            // racing removal of a playback node must not end the recording.
            if fatal_capture_error(is_application, id, result, &proxy_ids.borrow()) {
                errors.borrow_mut().error = Some(message.to_owned());
            }
        })
        .register();
    let stream = mainloop.stream(pw::stream::StreamBox::new(
        &core,
        "Isocast audio capture",
        props,
    )?);
    let registry = core.get_registry_rc()?;
    let links = Rc::new(RefCell::new(BTreeMap::new()));
    let added = links.clone();
    let removed = links.clone();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            if global.type_ != pw::types::ObjectType::Link {
                return;
            }
            let Some(props) = global.props else {
                return;
            };
            let Some(input) = props
                .get("link.input.node")
                .and_then(|v| v.parse::<u32>().ok())
            else {
                return;
            };
            let Some(output) = props
                .get("link.output.node")
                .and_then(|v| v.parse::<u32>().ok())
            else {
                return;
            };
            added.borrow_mut().insert(global.id, (input, output));
        })
        .global_remove(move |id| {
            removed.borrow_mut().remove(&id);
        })
        .register();
    let clock_offset = clock_offset()?;
    // Share only the Send sink, never a PipeWire object.
    let data = StreamData {
        application_clock: is_application.then(ApplicationClock::default),
        sink: sink.clone(),
        status: status.clone(),
        channels: 0,
        pick,
        samples: Vec::new(),
        concealed_samples: 0,
        corrupted_chunks: 0,
        bridged_frames: 0,
        clock_offset,
        sink_monitor: matches!(requested, Target::Desktop),
        links,
        position: std::ptr::null(),
    };
    let _listener = stream.add_local_listener_with_user_data(data)
        .io_changed(|_, data, id, area, size| {
            if id != spa::sys::SPA_IO_Position { return; }
            data.position = if size as usize >= std::mem::size_of::<spa::sys::spa_io_position>()
                && !area.is_null() && area.cast::<spa::sys::spa_io_position>().is_aligned()
            {
                area.cast()
            } else {
                std::ptr::null()
            };
        })
        .state_changed(move |_, data, _, state| match state {
            StreamState::Error(error) => data.status.borrow_mut().error = Some(error),
            StreamState::Unconnected => data.status.borrow_mut().error = Some("audio stream disconnected".into()),
            StreamState::Paused | StreamState::Streaming if is_application => {
                // A known app may be paused or have lost its last stream while
                // we connected. Start bounded; stay armed for future streams.
                data.status.borrow_mut().ready = Some(count);
            }
            _ => {}
        })
        .param_changed(move |_, data, id, param| {
            if id != spa::param::ParamType::Format.as_raw() { return; }
            let Some(param) = param else { data.channels = 0; return; };
            let mut format = AudioInfoRaw::new();
            if format.parse(param).is_err() || format.format() != AudioFormat::F32LE
                || format.rate() != RATE || format.channels() != count
            {
                data.status.borrow_mut().error = Some(format!(
                    "audio.format: expected interleaved F32LE/{RATE} Hz/{count} channels, got {format:?}"
                ));
                return;
            }
            data.channels = count;
        })
        .process(|stream, data| {
            if data.channels == 0 || data.status.borrow().error.is_some() { return; }
            if let Err(error) = process(stream, data) {
                data.status.borrow_mut().error = Some(format!("{error:#}"));
            }
        }).register()?;
    let mut format = AudioInfoRaw::new();
    format.set_format(AudioFormat::F32LE);
    format.set_rate(RATE);
    format.set_channels(count);
    // Preserve the device's native positions/order, without stereo remixing.
    if let Some(position) = target.and_then(|node| node.position) {
        format.set_position(position);
    } else if !microphone {
        let mut position = [0; spa::param::audio::MAX_CHANNELS];
        position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
        position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
        format.set_position(position);
    }
    let object = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: format.into(),
    };
    let bytes = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .map_err(|error| anyhow!("serializing PipeWire audio format: {error:?}"))?
    .0
    .into_inner();
    let pod = Pod::from_bytes(&bytes).context("invalid serialized PipeWire audio format")?;
    let buffers = capture_buffers()?;
    let buffers = Pod::from_bytes(&buffers).context("invalid serialized PipeWire audio buffers")?;
    // No RT_PROCESS: callbacks and the engine sink run on this owned main-loop
    // worker, not PipeWire's realtime thread. This also keeps Rc state confined.
    stream.connect(
        spa::utils::Direction::Input,
        None,
        if is_application {
            pw::stream::StreamFlags::MAP_BUFFERS
        } else {
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS
        },
        &mut [pod, buffers],
    )?;
    // Declared after the stream: own link proxies/listeners drop before it and
    // before the core, including on early errors/cancellation.
    // Create the stream and registry before the watch can allocate owned proxy
    // ids. Keep these non-owned proxies alive until capture has ended.
    let app_watch = if is_application {
        Some(ApplicationWatch::new(
            &core,
            true,
            link_policy.proxy_ids.clone(),
        )?)
    } else {
        None
    };
    let mut app_links = BTreeMap::new();
    let mut check_realtime = None;
    let capture_loop = CaptureLoop::register();
    while !cancel.load(Ordering::Acquire) {
        iterate(&mainloop)?;
        if check_realtime.is_some_and(|at: Instant| at.elapsed() >= Duration::from_secs(1)) {
            // RTKit grants asynchronously and can silently deny a request.
            // Observe the actual data thread, never mistake a request for a grant.
            check_realtime = None;
            if !capture_loop.observe(&context, &stream) {
                static WARN_NOT_REALTIME: std::sync::Once = std::sync::Once::new();
                WARN_NOT_REALTIME.call_once(|| {
                    isocast_diagnostics::output::deferred(
                        "warn",
                        module_path!(),
                        "PipeWire audio data loop is not realtime; short graph cycles may be lost under load. Allow RTKit realtime scheduling for this session or configure RLIMIT_RTPRIO for isocastd. Recording will continue.".into(),
                    );
                });
            }
        }
        let mut graph_ready = true;
        if let (Some(identity), Some(watch)) = (&application, &app_watch) {
            let desired = watch.state.borrow().desired_links(
                identity,
                stream.node_id(),
                &mut link_policy,
                *started,
            )?;
            reconcile_links(&core, &desired, &mut app_links, &mut link_policy, *started)?;
            graph_ready = watch.state.borrow().ready(identity, stream.node_id())
                && desired.iter().all(|pair| {
                    app_links.get(pair).is_some_and(|link| {
                        link.negotiated.get() && link.failure.borrow().is_none()
                    })
                });
        }
        let mut state = status.borrow_mut();
        if let Some(error) = state.error.take() {
            bail!("{error}");
        }
        if !*started {
            if let Some(count) = state.ready.filter(|_| graph_ready) {
                ready
                    .send(Ok(count))
                    .context("PipeWire startup caller disconnected")?;
                *started = true;
                check_realtime = Some(Instant::now());
            } else {
                ensure!(
                    Instant::now() < deadline,
                    "PipeWire audio stream or initial application links were not ready within 5 seconds; check the session manager and selected device"
                );
            }
        }
    }
    Ok(())
}

// A raw buffer is needed for pw_buffer.time (not exposed by Buffer in 0.10.1).
// The guard returns it on every path, including conversion/timestamp errors.
struct Buffer<'a> {
    stream: &'a pw::stream::Stream,
    raw: std::ptr::NonNull<pw::sys::pw_buffer>,
}

impl Drop for Buffer<'_> {
    fn drop(&mut self) {
        // SAFETY: exactly one buffer dequeued from this still-live stream;
        // neither the guard nor its buffer is shared or returned twice.
        unsafe { self.stream.queue_raw_buffer(self.raw.as_ptr()) };
    }
}

fn process(stream: &pw::stream::Stream, state: &mut StreamData) -> Result<()> {
    // Bound each dispatch even if the graph keeps filling while we drain it,
    // so cancellation and error handling cannot be starved by capture traffic.
    for _ in 0..64 {
        // SAFETY: called on the stream's process/main-loop thread, with no
        // concurrent dequeuer. The guard below immediately takes ownership.
        let Some(raw) = std::ptr::NonNull::new(unsafe { stream.dequeue_raw_buffer() }) else {
            break;
        };
        let buffer = Buffer { stream, raw };
        // SAFETY: PipeWire owns the live buffer until this guard queues it.
        let raw = unsafe { buffer.raw.as_ref() };
        // SAFETY: buffer is a PipeWire-provided spa_buffer or null; no pointer
        // comes from media bytes. as_ref rejects null before dereferencing.
        let spa_buffer =
            unsafe { raw.buffer.as_ref() }.context("audio.buffer: missing SPA buffer")?;
        ensure!(
            spa_buffer.n_datas == 1,
            "audio.buffer: expected one interleaved data plane"
        );
        // SAFETY: n_datas is one and this allocation belongs to the dequeued
        // buffer; null is checked before accessing it.
        let data =
            unsafe { spa_buffer.datas.as_ref() }.context("audio.buffer: missing data plane")?;
        // SAFETY: chunk is owned by this live data plane; reject a null chunk.
        let chunk = unsafe { data.chunk.as_ref() }.context("audio.buffer: missing chunk")?;
        if chunk.flags & spa::sys::SPA_CHUNK_FLAG_CORRUPTED as i32 != 0 {
            state.corrupted_chunks = state.corrupted_chunks.saturating_add(1);
            continue;
        }
        if chunk.size == 0 {
            continue;
        }
        ensure!(
            data.maxsize as usize <= MAX_BUFFER_BYTES,
            "audio.buffer: exceeds 16 MiB limit"
        );
        ensure!(!data.data.is_null(), "audio.buffer: samples are not mapped");
        ensure!(
            data.type_ == spa::sys::SPA_DATA_MemPtr || data.type_ == spa::sys::SPA_DATA_MemFd,
            "audio.buffer: unsupported data memory type {}",
            data.type_
        );
        // SAFETY: MAP_BUFFERS supplies a mapping of maxsize bytes, bounded
        // above; we only read it while the buffer is dequeued.
        let bytes =
            unsafe { std::slice::from_raw_parts(data.data.cast::<u8>(), data.maxsize as usize) };
        let concealed = convert(
            bytes,
            chunk.offset as usize,
            chunk.size as usize,
            chunk.stride,
            state.channels,
            &state.pick,
            &mut state.samples,
        )?;
        state.concealed_samples = state.concealed_samples.saturating_add(concealed);
        if state.samples.is_empty() {
            continue;
        }
        let timing = stream.time().context("reading PipeWire audio clock")?;
        let rate = timing.rate();
        ensure!(
            raw.time > 0 && rate.num > 0 && rate.denom > 0,
            "audio.timestamp: PipeWire supplied an invalid graph clock"
        );
        // pw_buffer.time is this buffer's graph-cycle time, even when several
        // cycles queued before this non-RT callback. Subtract the capture path
        // and resampler delay; do not subtract callback latency a second time.
        // A driving sink's monitor is one cycle old in PipeWire's scheduler;
        // that cycle is not included in pw_time.delay.
        let timestamp = timestamp(
            raw.time,
            state.clock_offset,
            timing.delay(),
            rate.num,
            rate.denom,
            timing.buffered(),
            state.monitor_delay(stream.node_id(), raw.time),
        )?;
        // Never attribute a queued buffer to a newer driver's shared clock.
        // Every delivered buffer advances the timeline, even if its driver is
        // unknown. Never bridge across that uncertainty using an older buffer.
        if state.application_clock.is_some() {
            let driver = state
                .graph_clock()
                .filter(|clock| clock.nsec == raw.time)
                .map(|clock| clock.id);
            if let Some(timeline) = &mut state.application_clock
                && let Some((at, frames)) = timeline.silence(
                    driver,
                    timestamp,
                    (state.samples.len() / state.pick.len()) as u64,
                    state.samples.iter().all(|&sample| sample == 0.0),
                )
            {
                let silence = vec![0.0; frames * state.pick.len()];
                (state.sink.samples)(&silence, at);
                state.bridged_frames = state.bridged_frames.saturating_add(frames as u64);
            }
        }
        (state.sink.samples)(&state.samples, timestamp);
        state.status.borrow_mut().ready = Some(state.pick.len() as u32);
    }
    Ok(())
}

fn capture_context(
    mainloop: &WorkerLoop,
    version: (u32, u32, u32),
) -> Result<pw::context::ContextRc> {
    // Before 1.4 the ordinary client config omits module-rt. Newer PipeWire
    // folds it into client.conf. Only capture needs a realtime data loop;
    // device enumeration must not load another RTKit helper thread.
    let props = (version < (1, 4, 0)).then(|| properties! { "config.name" => "client-rt.conf" });
    Ok(pw::context::ContextRc::new(mainloop.main_loop(), props)?)
}

fn capture_buffers() -> Result<Vec<u8>> {
    // Non-RT callbacks need a queue while their worker is descheduled. The
    // converter otherwise fixates to two buffers (one plus the async reserve).
    // Fix the count: a range's default is ignored when intersecting the
    // converter's range. This queues delivery, not timestamps or extra silence.
    let object = spa::pod::Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamBuffers,
        id: spa::sys::SPA_PARAM_Buffers,
        properties: vec![spa::pod::Property::new(
            spa::sys::SPA_PARAM_BUFFERS_buffers,
            spa::pod::Value::Int(16),
        )],
    };
    Ok(spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .map_err(|error| anyhow!("serializing PipeWire audio buffers: {error:?}"))?
    .0
    .into_inner())
}

fn realtime_policy(policy: i32) -> bool {
    matches!(
        policy & !libc::SCHED_RESET_ON_FORK,
        libc::SCHED_FIFO | libc::SCHED_RR
    )
}

fn server_policies() -> Option<Vec<Option<i32>>> {
    use std::os::unix::fs::MetadataExt;
    use std::path::PathBuf;

    // The server exclusively flocks <socket>.lock even with socket activation
    // (where SO_PEERCRED on the listener can identify systemd instead).
    let mut socket =
        PathBuf::from(std::env::var_os("PIPEWIRE_REMOTE").unwrap_or_else(|| "pipewire-0".into()));
    if !socket.is_absolute() {
        let dir = std::env::var_os("PIPEWIRE_RUNTIME_DIR")
            .or_else(|| std::env::var_os("XDG_RUNTIME_DIR"))?;
        socket = PathBuf::from(dir).join(socket);
    }
    let mut lock = socket.into_os_string();
    lock.push(".lock");
    let stat = std::fs::metadata(lock).ok()?;
    let key = format!(
        "{:02x}:{:02x}:{}",
        libc::major(stat.dev()),
        libc::minor(stat.dev()),
        stat.ino()
    );
    let pid = std::fs::read_to_string("/proc/locks")
        .ok()?
        .lines()
        .find_map(|line| {
            let mut f = line.split_whitespace().skip(1);
            if f.next()? != "FLOCK" || f.next()? != "ADVISORY" || f.next()? != "WRITE" {
                return None;
            }
            let pid = f.next()?.parse::<u32>().ok()?;
            (f.next()? == key).then_some(pid)
        })?;
    let mut policies = Vec::new();
    for task in std::fs::read_dir(format!("/proc/{pid}/task")).ok()? {
        let path = task.ok()?.path();
        let name = std::fs::read_to_string(path.join("comm")).ok()?;
        if is_data_loop(name.trim_end()) {
            policies.push(
                std::fs::read_to_string(path.join("sched"))
                    .ok()
                    .and_then(|s| sched_policy(&s)),
            );
        }
    }
    Some(policies)
}

fn is_data_loop(name: &str) -> bool {
    matches!(name, "pw-data-loop" | "data-loop")
        || name
            .strip_prefix("data-loop.")
            .is_some_and(|n| n.parse::<u32>().is_ok())
}

fn sched_policy(sched: &str) -> Option<i32> {
    sched.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        (key.trim() == "policy")
            .then(|| value.trim().parse().ok())
            .flatten()
    })
}

pub(crate) fn scheduling_checks() -> [protocol::api::Check; 2] {
    fn check(
        name: &str,
        policies: &[Option<i32>],
        absent: &str,
        retry: &str,
    ) -> protocol::api::Check {
        use protocol::api::{Check, CheckStatus};
        let rt = policies
            .iter()
            .filter(|p| p.is_some_and(realtime_policy))
            .count();
        let ordinary = policies.iter().flatten().count() - rt;
        let ok = !policies.is_empty() && rt == policies.len();
        let (detail, fix) = if policies.is_empty() {
            (absent.into(), Some(retry.into()))
        } else if ordinary > 0 {
            (format!("{rt} of {} data loops run realtime. RTKit/polkit may have denied realtime because there is no local login session.", policies.len()),
             Some("Log in at the computer itself. On a headless or remote machine, an administrator can allow RTKit realtime scheduling for your user.".into()))
        } else {
            (
                format!("{rt} of {} data loops confirmed realtime", policies.len()),
                (!ok).then(|| retry.into()),
            )
        };
        Check {
            name: name.into(),
            status: if ok {
                CheckStatus::Ok
            } else {
                CheckStatus::Warning
            },
            detail,
            fix,
        }
    }

    let server = server_policies().unwrap_or_default();
    let capture: Vec<_> = CAPTURE_TIDS
        .lock()
        .unwrap()
        .iter()
        .map(|(_, tid)| tid.and_then(thread_policy))
        .collect();
    [
        check(
            "PipeWire server realtime",
            &server,
            "Could not identify the PipeWire server data loop.",
            "Check that PipeWire is running and its threads are visible to Isocast.",
        ),
        check(
            "Isocast audio capture realtime",
            &capture,
            "No active audio capture data loop measured.",
            "Start desktop or microphone capture and run doctor again after one second.",
        ),
    ]
}

// Only audio capture workers register here; screen, enumeration and playback
// threads can have identical names. Unregister before tearing down the stream.
static CAPTURE_TIDS: Mutex<Vec<(thread::ThreadId, Option<i32>)>> = Mutex::new(Vec::new());

struct CaptureLoop(thread::ThreadId);

impl Drop for CaptureLoop {
    fn drop(&mut self) {
        CAPTURE_TIDS.lock().unwrap().retain(|(id, _)| *id != self.0);
    }
}

fn thread_policy(tid: i32) -> Option<i32> {
    // SAFETY: reads the kernel policy (including RTKit's out-of-process grants).
    let policy = unsafe { libc::sched_getscheduler(tid) };
    (policy >= 0).then_some(policy)
}

impl CaptureLoop {
    fn register() -> Self {
        let id = thread::current().id();
        CAPTURE_TIDS.lock().unwrap().push((id, None));
        Self(id)
    }

    fn observe(&self, context: &pw::context::Context, stream: &pw::stream::Stream) -> bool {
        let tid = capture_loop_tid(context, stream);
        if let Some(entry) = CAPTURE_TIDS
            .lock()
            .unwrap()
            .iter_mut()
            .find(|(id, _)| *id == self.0)
        {
            entry.1 = tid;
        }
        tid.and_then(thread_policy).is_some_and(realtime_policy)
    }
}

fn capture_loop_tid(context: &pw::context::Context, stream: &pw::stream::Stream) -> Option<i32> {
    unsafe extern "C" fn tid(
        _loop: *mut spa::sys::spa_loop,
        _async: bool,
        _seq: u32,
        _data: *const std::ffi::c_void,
        _size: usize,
        _user_data: *mut std::ffi::c_void,
    ) -> i32 {
        // SAFETY: this reads only the calling thread's kernel ID.
        unsafe { libc::gettid() }
    }

    // SAFETY: the context owns this live data loop. Its owning worker is the
    // sole outside writer of the invoke queue (as for stream/node operations).
    // The blocking invoke returns the callback result and borrows no data.
    unsafe {
        // PipeWire 1.1 added per-stream loop selection. Keep compatibility
        // with 1.0.5 (one data loop per capture context).
        let symbol = libc::dlsym(libc::RTLD_DEFAULT, c"pw_stream_get_data_loop".as_ptr());
        let loop_ptr = if symbol.is_null() {
            let data = pw::sys::pw_context_get_data_loop(context.as_raw_ptr());
            if data.is_null() {
                return None;
            }
            pw::sys::pw_data_loop_get_loop(data)
        } else {
            type GetLoop = unsafe extern "C" fn(*mut pw::sys::pw_stream) -> *mut pw::sys::pw_loop;
            std::mem::transmute::<*mut libc::c_void, GetLoop>(symbol)(stream.as_raw_ptr())
        };
        let loop_ = loop_ptr.as_ref()?;
        let id = pw::spa::spa_interface_call_method!(
            loop_.loop_,
            spa::sys::spa_loop_methods,
            invoke,
            Some(tid),
            0,
            std::ptr::null(),
            0,
            true,
            std::ptr::null_mut()
        );
        (id > 0 && id != libc::gettid()).then_some(id)
    }
}

fn require_buffer_timestamps() -> Result<(u32, u32, u32)> {
    // The time field was appended to pw_buffer in 1.0.5. Check the loaded
    // library before accessing it, even if built with more recent headers.
    // SAFETY: PipeWire returns a static, NUL-terminated library-version string.
    let version =
        unsafe { std::ffi::CStr::from_ptr(pw::sys::pw_get_library_version()) }.to_string_lossy();
    let mut parts = version
        .split('.')
        .map(|part| part.parse::<u32>().unwrap_or(0));
    let version_number = (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    );
    ensure!(
        version_number >= (1, 0, 5),
        "PipeWire audio capture requires libpipewire >= 1.0.5 for per-buffer timestamps; loaded {version}"
    );
    Ok(version_number)
}

fn selection(count: u32, channels: Option<&[u32]>) -> Result<Vec<usize>> {
    ensure!(
        (1..=MAX_CHANNELS).contains(&count),
        "microphone.channels: device reports {count} channels; supported range is 1..={MAX_CHANNELS}"
    );
    let chosen = channels.map_or_else(|| (1..=count.min(2)).collect::<Vec<_>>(), <[u32]>::to_vec);
    ensure!(
        !chosen.is_empty()
            && chosen.len() <= 2
            && chosen.iter().all(|channel| (1..=count).contains(channel))
            && (chosen.len() != 2 || chosen[0] != chosen[1]),
        "microphone.channels: select one or two distinct 1-based channels in 1..={count}"
    );
    Ok(chosen
        .into_iter()
        .map(|channel| (channel - 1) as usize)
        .collect())
}

fn convert(
    bytes: &[u8],
    offset: usize,
    size: usize,
    stride: i32,
    channels: u32,
    pick: &[usize],
    output: &mut Vec<f32>,
) -> Result<u64> {
    ensure!(
        (1..=MAX_CHANNELS).contains(&channels),
        "audio.buffer: invalid channel count"
    );
    let frame_bytes = channels as usize * std::mem::size_of::<f32>();
    ensure!(
        stride == 0 || stride == frame_bytes as i32,
        "audio.buffer: invalid interleaved stride {stride}"
    );
    ensure!(
        size <= MAX_BUFFER_BYTES && size.is_multiple_of(frame_bytes),
        "audio.buffer: partial frame or oversized chunk"
    );
    ensure!(!bytes.is_empty(), "audio.buffer: empty mapped data");
    // SPA chunk offsets are relative to maxsize; a wrapped chunk still must
    // fit contiguously in the mapping after normalizing its starting offset.
    let offset = offset % bytes.len();
    let end = offset
        .checked_add(size)
        .context("audio.buffer: chunk offset overflow")?;
    let bytes = bytes
        .get(offset..end)
        .context("audio.buffer: chunk exceeds mapped data")?;
    ensure!(
        !pick.is_empty() && pick.len() <= 2 && pick.iter().all(|&c| c < channels as usize),
        "audio.buffer: invalid selected channels"
    );
    output.clear();
    output.reserve(size / frame_bytes * pick.len());
    let mut concealed = 0;
    for frame in bytes.chunks_exact(frame_bytes) {
        for &channel in pick {
            let start = channel * 4;
            let value = f32::from_le_bytes([
                frame[start],
                frame[start + 1],
                frame[start + 2],
                frame[start + 3],
            ]);
            if value.is_finite() {
                output.push(value);
            } else {
                output.push(0.0);
                concealed += 1;
            }
        }
    }
    Ok(concealed)
}

fn clock_offset() -> Result<i64> {
    // Pick the narrowest of three brackets to minimize scheduling skew. Both
    // clocks are monotonic, but system::now has its own lazy process origin.
    let mut best = (i64::MAX, 0);
    for _ in 0..3 {
        let before = crate::system::now().as_nanos();
        let mut ts = std::mem::MaybeUninit::<libc::timespec>::uninit();
        // SAFETY: valid output storage; CLOCK_MONOTONIC matches PipeWire's clock.
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, ts.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error()).context("reading PipeWire host clock");
        }
        // SAFETY: successful clock_gettime initialized the timespec.
        let ts = unsafe { ts.assume_init() };
        let after = crate::system::now().as_nanos();
        let monotonic = i128::from(ts.tv_sec) * 1_000_000_000 + i128::from(ts.tv_nsec);
        let offset = i64::try_from(i128::from(before + (after - before) / 2) - monotonic)
            .context("PipeWire clock offset overflow")?;
        if after - before < best.0 {
            best = (after - before, offset);
        }
    }
    Ok(best.1)
}

fn timestamp(
    graph: u64,
    offset: i64,
    delay: i64,
    num: u32,
    denom: u32,
    buffered: u64,
    monitor: u64,
) -> Result<Time> {
    ensure!(
        denom != 0 && num != 0,
        "audio.timestamp: invalid graph rate"
    );
    // Recompute from each absolute graph timestamp; no rounded packet duration
    // is ever accumulated, and missed graph cycles remain visible as gaps.
    let nanos = i128::from(graph) + i128::from(offset)
        - i128::from(delay) * i128::from(num) * 1_000_000_000 / i128::from(denom)
        - i128::from(buffered) * 1_000_000_000 / i128::from(RATE)
        - i128::from(monitor);
    Ok(Time::from_nanos(
        i64::try_from(nanos).context("audio.timestamp: overflow")?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_buffer_pod_requests_a_fixed_queue() {
        let bytes = capture_buffers().unwrap();
        let (remaining, value) =
            spa::pod::deserialize::PodDeserializer::deserialize_any_from(&bytes).unwrap();
        assert!(remaining.is_empty());
        let spa::pod::Value::Object(object) = value else {
            panic!("capture buffers must be an object");
        };
        assert_eq!(object.type_, spa::sys::SPA_TYPE_OBJECT_ParamBuffers);
        assert_eq!(object.id, spa::sys::SPA_PARAM_Buffers);
        assert_eq!(object.properties.len(), 1);
        assert_eq!(
            object.properties[0].key,
            spa::sys::SPA_PARAM_BUFFERS_buffers
        );
        assert_eq!(object.properties[0].value, spa::pod::Value::Int(16));
    }

    #[test]
    fn realtime_policies_exclude_ordinary_scheduling() {
        assert!(!realtime_policy(libc::SCHED_OTHER));
        assert!(!realtime_policy(libc::SCHED_BATCH));
        assert!(!realtime_policy(libc::SCHED_IDLE));
        for policy in [libc::SCHED_FIFO, libc::SCHED_RR] {
            assert!(realtime_policy(policy));
            assert!(realtime_policy(policy | libc::SCHED_RESET_ON_FORK));
        }
    }

    #[test]
    fn reads_data_loop_policy_not_main_thread_or_priority() {
        assert!(!is_data_loop("pipewire"));
        assert!(!is_data_loop("pipewire-audio"));
        for name in ["pw-data-loop", "data-loop", "data-loop.12"] {
            assert!(is_data_loop(name));
        }
        for (text, expected) in [
            (
                "pw-data-loop (896, #threads: 3)\npolicy   : 0\nprio : 120",
                Some(false),
            ),
            (
                "data-loop.1 (900, #threads: 3)\npolicy   : 2\nprio : 79",
                Some(true),
            ),
            ("policy : unknown\nprio : 1", None),
        ] {
            assert_eq!(sched_policy(text).map(realtime_policy), expected);
        }
    }

    // Run with ISOCAST_EXPECT_RT=1 for an authorized RTKit/direct-RT session,
    // or =0 to require the denied path. No implicit skip of a failed grant.
    #[test]
    #[ignore = "needs a running PipeWire session with a default sink"]
    fn capture_queue_and_data_loop_scheduling() {
        let worker = WorkerLoop::new().unwrap();
        let context = capture_context(&worker, require_buffer_timestamps().unwrap()).unwrap();
        let core = context.connect_rc(None).unwrap();
        let stream = worker.stream(
            pw::stream::StreamBox::new(
                &core,
                "isocast capture queue test",
                properties! {
                    "media.type" => "Audio",
                    "media.category" => "Capture",
                    "stream.capture.sink" => "true",
                    "node.name" => "isocast.capture-test",
                },
            )
            .unwrap(),
        );
        let buffers = Rc::new(Cell::new(0));
        let added = buffers.clone();
        let processed = Rc::new(Cell::new(0));
        let seen = processed.clone();
        let owner = thread::current().id();
        let _listener = stream
            .add_local_listener_with_user_data(())
            .add_buffer(move |_, _, _| added.set(added.get() + 1))
            .process(move |stream, _| {
                assert_eq!(thread::current().id(), owner);
                while let Some(_buffer) = stream.dequeue_buffer() {
                    seen.set(seen.get() + 1);
                }
            })
            .register()
            .unwrap();
        let mut format = AudioInfoRaw::new();
        format.set_format(AudioFormat::F32LE);
        format.set_rate(RATE);
        format.set_channels(2);
        let format = spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &spa::pod::Value::Object(spa::pod::Object {
                type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
                id: spa::param::ParamType::EnumFormat.as_raw(),
                properties: format.into(),
            }),
        )
        .unwrap()
        .0
        .into_inner();
        let buffer_bytes = capture_buffers().unwrap();
        stream
            .connect(
                spa::utils::Direction::Input,
                None,
                pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
                &mut [
                    Pod::from_bytes(&format).unwrap(),
                    Pod::from_bytes(&buffer_bytes).unwrap(),
                ],
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            iterate(&worker).unwrap();
        }
        assert_eq!(buffers.get(), 16);
        assert!(processed.get() > 10, "capture did not process audio");
        let realtime = CaptureLoop::register().observe(&context, &stream);
        eprintln!(
            "capture buffers: {}, data loop realtime: {realtime}",
            buffers.get()
        );
        if let Ok(expected) = std::env::var("ISOCAST_EXPECT_RT") {
            assert!(matches!(expected.as_str(), "0" | "1"));
            assert_eq!(realtime, expected == "1");
        }
    }

    fn app_node(id: Option<&str>, name: &str, binary: Option<&str>, pid: u32) -> AppNode {
        AppNode {
            props: AppProperties {
                id: id.map(str::to_owned),
                name: Some(name.to_owned()),
                binary: binary.map(str::to_owned),
                pid: Some(pid.to_string()),
            },
            informed: true,
            playing: true,
            output_count: 1,
            ..AppNode::default()
        }
    }

    fn audio_port(node: u32, input: bool, channel: &str) -> AudioPort {
        AudioPort {
            node,
            input,
            channel: channel.into(),
            monitor: false,
        }
    }

    #[test]
    fn application_identity_prefers_real_id_then_binary_then_name_not_pid() {
        let node = app_node(Some("org.example.Player"), "Player", Some("player"), 123);
        let app = node.props.application(None, true).unwrap();
        assert_eq!(app.id, "pipewire:application:org.example.Player");
        assert_eq!(app.bundle_id.as_deref(), Some("org.example.Player"));
        assert_eq!(app.binary.as_deref(), Some("player"));
        assert_eq!(app.pid, 123);
        let node = app_node(None, "Player", Some("player"), 987);
        let app = node.props.application(None, false).unwrap();
        assert_eq!(app.id, "pipewire:binary:player");
        assert_eq!(app.bundle_id, None);
        assert!(!app.playing);
        let node = app_node(None, "Player", None, 999);
        assert_eq!(
            node.props.application(None, false).unwrap().id,
            "pipewire:name:Player"
        );
        assert!(AppProperties::default().application(None, false).is_none());
    }

    #[test]
    fn application_client_info_fallback_waits_for_complete_identity() {
        let mut apps = Applications::default();
        let mut node = app_node(None, "Stream title", None, 0);
        node.props.pid = None;
        node.client = Some(40);
        apps.nodes.insert(20, node);
        apps.clients.insert(40, AppClient::default());
        assert!(apps.list().is_empty());
        apps.clients.insert(
            40,
            AppClient {
                props: AppProperties {
                    id: Some("org.example.Player".into()),
                    binary: Some("player".into()),
                    pid: Some("4321".into()),
                    ..AppProperties::default()
                },
                informed: true,
            },
        );
        let list = apps.list();
        assert_eq!(list[0].id, "pipewire:application:org.example.Player");
        assert_eq!(list[0].name, "Stream title");
        assert_eq!(list[0].pid, 4321);
        assert_eq!(list[0].binary.as_deref(), Some("player"));
    }

    #[test]
    fn selectors_are_exact_for_canonical_ids_and_aliases_must_be_unambiguous() {
        let mut apps = Applications::default();
        apps.nodes
            .insert(1, app_node(Some("A"), "Player", Some("player"), 10));
        apps.nodes
            .insert(2, app_node(Some("A"), "Player tab", Some("player"), 20));
        assert_eq!(apps.list().len(), 1);
        assert_eq!(apps.resolve("PLAYER").unwrap(), "pipewire:application:A");
        assert!(apps.resolve("pipewire:application:a").is_err());
        apps.nodes
            .insert(3, app_node(Some("B"), "Player", Some("player"), 30));
        assert!(
            apps.resolve("Player")
                .unwrap_err()
                .to_string()
                .contains("ambiguous")
        );
        assert!(apps.resolve("player").is_err());
        assert_eq!(
            apps.resolve("pipewire:application:A").unwrap(),
            "pipewire:application:A"
        );
        assert!(apps.resolve("missing").is_err());
        assert!(validate_application("").is_err());
        assert!(validate_application("player\0other").is_err());
    }

    #[test]
    fn desired_links_follow_all_streams_and_restarts_without_other_apps() {
        let mut apps = Applications::default();
        let mut policy = AppLinkPolicy::default();
        apps.nodes
            .insert(10, app_node(Some("A"), "Player", Some("player"), 100));
        apps.nodes
            .insert(20, app_node(Some("A"), "Player", Some("player"), 200));
        apps.nodes
            .insert(30, app_node(Some("B"), "Player", Some("player"), 300));
        apps.nodes.get_mut(&10).unwrap().output_count = 2;
        for (id, node, input, channel) in [
            (11, 10, false, "FL"),
            (12, 10, false, "FR"),
            (21, 20, false, "MONO"),
            (31, 30, false, "MONO"),
            (91, 90, true, "FR"),
            (92, 90, true, "FL"),
        ] {
            apps.ports.insert(id, audio_port(node, input, channel));
        }
        let links = apps
            .desired_links("pipewire:application:A", 90, &mut policy, false)
            .unwrap();
        let pairs: BTreeSet<_> = links
            .iter()
            .map(|l| (l.output_port, l.input_port))
            .collect();
        assert_eq!(
            pairs,
            BTreeSet::from([(11, 92), (12, 91), (21, 91), (21, 92)])
        );
        // Pausing and losing every stream is not a source failure. The resolved
        // identity is retained outside this changing graph, never a PID/node ID.
        apps.nodes.get_mut(&10).unwrap().playing = false;
        assert_eq!(
            apps.desired_links("pipewire:application:A", 90, &mut policy, true)
                .unwrap(),
            links
        );
        apps.nodes.clear();
        assert!(
            apps.desired_links("pipewire:application:A", 90, &mut policy, true)
                .unwrap()
                .is_empty()
        );
        apps.nodes.insert(
            50,
            app_node(Some("A"), "Renamed player", Some("new-binary"), 999),
        );
        apps.ports.insert(51, audio_port(50, false, "MONO"));
        let links = apps
            .desired_links("pipewire:application:A", 90, &mut policy, true)
            .unwrap();
        assert_eq!(links.len(), 2);
        assert!(
            links
                .iter()
                .all(|link| link.output_node == 50 && link.output_port == 51)
        );
        assert!(
            apps.desired_links("pipewire:application:A", u32::MAX, &mut policy, true)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn desired_links_skip_monitor_ports_and_map_unlabeled_stereo() {
        let mut apps = Applications::default();
        let mut policy = AppLinkPolicy::default();
        apps.nodes
            .insert(10, app_node(None, "Player", Some("player"), 100));
        apps.nodes.get_mut(&10).unwrap().output_count = 3;
        apps.ports.insert(11, audio_port(10, false, ""));
        apps.ports.insert(12, audio_port(10, false, ""));
        apps.ports.insert(
            13,
            AudioPort {
                monitor: true,
                ..audio_port(10, false, "MONO")
            },
        );
        apps.ports.insert(91, audio_port(90, true, "FL"));
        apps.ports.insert(92, audio_port(90, true, "FR"));
        let links = apps
            .desired_links("pipewire:binary:player", 90, &mut policy, false)
            .unwrap();
        let pairs: BTreeSet<_> = links
            .iter()
            .map(|l| (l.output_port, l.input_port))
            .collect();
        assert_eq!(pairs, BTreeSet::from([(11, 91), (12, 92)]));
        // Removing a port cannot retain links to that stale global ID.
        apps.ports.remove(&12);
        let links = apps
            .desired_links("pipewire:binary:player", 90, &mut policy, true)
            .unwrap();
        assert!(links.is_empty());
    }

    #[test]
    fn capture_handle_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<Capture>();
    }

    #[test]
    fn app_metadata_updates_preserve_absent_fields_and_clear_empty_values() {
        let mut props = AppProperties::default();
        props
            .update(properties! { "application.id" => "A", "application.name" => "Player" }.dict());
        props.update(properties! { "application.name" => "New title" }.dict());
        assert_eq!(
            props.application(None, true).unwrap().id,
            "pipewire:application:A"
        );
        assert_eq!(props.name.as_deref(), Some("New title"));
        props.update(properties! { "application.id" => "" }.dict());
        assert_eq!(
            props.application(None, true).unwrap().id,
            "pipewire:name:New title"
        );
    }

    #[test]
    fn application_streams_reject_surround_and_unmappable_stereo() {
        let mut apps = Applications::default();
        let mut policy = AppLinkPolicy::default();
        apps.nodes
            .insert(10, app_node(Some("A"), "Player", None, 100));
        apps.nodes.get_mut(&10).unwrap().output_count = 3;
        apps.ports.insert(91, audio_port(90, true, "FL"));
        apps.ports.insert(92, audio_port(90, true, "FR"));
        for (id, channel) in [(11, "FL"), (12, "FR"), (13, "FC")] {
            apps.ports.insert(id, audio_port(10, false, channel));
        }
        assert!(
            apps.desired_links("pipewire:application:A", 90, &mut policy, false)
                .unwrap_err()
                .to_string()
                .contains("mono/stereo")
        );
        apps.ports.remove(&13);
        apps.nodes.get_mut(&10).unwrap().output_count = 2;
        apps.ports.get_mut(&12).unwrap().channel = "FC".into();
        assert!(
            apps.desired_links("pipewire:application:A", 90, &mut policy, false)
                .unwrap_err()
                .to_string()
                .contains("cannot be mapped")
        );
    }

    #[test]
    fn incomplete_output_set_never_links_first_stereo_port_as_mono() {
        let mut apps = Applications::default();
        let mut policy = AppLinkPolicy::default();
        let mut node = app_node(Some("A"), "Player", None, 100);
        node.output_count = 3;
        apps.nodes.insert(10, node);
        apps.ports.insert(91, audio_port(90, true, "FL"));
        apps.ports.insert(92, audio_port(90, true, "FR"));
        apps.ports.insert(11, audio_port(10, false, "FL"));
        assert!(
            apps.desired_links("pipewire:application:A", 90, &mut policy, false)
                .unwrap()
                .is_empty()
        );
        apps.ports.insert(12, audio_port(10, false, "FR"));
        // Count all output globals before excluding monitor ports.
        assert!(
            apps.desired_links("pipewire:application:A", 90, &mut policy, false)
                .unwrap()
                .is_empty()
        );
        apps.ports.insert(
            13,
            AudioPort {
                monitor: true,
                ..audio_port(10, false, "MONO")
            },
        );
        let links = apps
            .desired_links("pipewire:application:A", 90, &mut policy, false)
            .unwrap();
        assert_eq!(
            links
                .iter()
                .map(|l| (l.output_port, l.input_port))
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([(11, 91), (12, 92)])
        );
    }

    #[test]
    fn later_invalid_stream_is_skipped_without_removing_good_stream_links() {
        for channels in [vec!["FL", "FR", "FC"], vec!["FL", "FC"]] {
            let mut apps = Applications::default();
            let mut policy = AppLinkPolicy::default();
            apps.nodes
                .insert(10, app_node(Some("A"), "Player", None, 100));
            apps.ports.insert(11, audio_port(10, false, "MONO"));
            apps.ports.insert(91, audio_port(90, true, "FL"));
            apps.ports.insert(92, audio_port(90, true, "FR"));
            let good = apps
                .desired_links("pipewire:application:A", 90, &mut policy, false)
                .unwrap();
            let mut bad = app_node(Some("A"), "Player", None, 200);
            bad.output_count = channels.len() as u32;
            apps.nodes.insert(20, bad);
            for (index, channel) in channels.iter().enumerate() {
                apps.ports
                    .insert(21 + index as u32, audio_port(20, false, channel));
            }
            assert!(
                apps.desired_links("pipewire:application:A", 90, &mut policy, false)
                    .is_err()
            );
            assert_eq!(
                apps.desired_links("pipewire:application:A", 90, &mut policy, true)
                    .unwrap(),
                good
            );
            assert_eq!(policy.skipped, BTreeSet::from([20]));
            assert_eq!(
                apps.desired_links("pipewire:application:A", 90, &mut policy, true)
                    .unwrap(),
                good
            );
            apps.nodes.remove(&20);
            apps.ports.retain(|_, port| port.node != 20);
            apps.desired_links("pipewire:application:A", 90, &mut policy, true)
                .unwrap();
            assert!(policy.skipped.is_empty());
            // A fresh stream using the old global ID is eligible again.
            apps.nodes
                .insert(20, app_node(Some("A"), "Player", None, 200));
            apps.ports.insert(21, audio_port(20, false, "MONO"));
            assert_eq!(
                apps.desired_links("pipewire:application:A", 90, &mut policy, true)
                    .unwrap()
                    .len(),
                4
            );
        }
    }

    #[test]
    fn persistent_link_refusal_fails_startup_but_skips_only_later_stream() {
        let now = Instant::now();
        let mut policy = AppLinkPolicy::default();
        policy.generations.insert(20, 0);
        policy.failed(20, false, now, "permission denied").unwrap();
        policy
            .failed(20, false, now + TIMEOUT - POLL, "permission denied")
            .unwrap();
        assert!(policy.skipped.is_empty());
        assert!(
            policy
                .failed(20, false, now + TIMEOUT, "permission denied")
                .is_err()
        );
        assert!(policy.skipped.is_empty());
        policy
            .failed(20, true, now + TIMEOUT, "permission denied")
            .unwrap();
        assert_eq!(policy.skipped, BTreeSet::from([20]));
        assert!(policy.failures.is_empty());
        let mut apps = Applications::default();
        apps.nodes
            .insert(10, app_node(Some("A"), "Player", None, 100));
        apps.nodes
            .insert(20, app_node(Some("A"), "Player", None, 200));
        apps.ports.insert(11, audio_port(10, false, "MONO"));
        apps.ports.insert(21, audio_port(20, false, "MONO"));
        apps.ports.insert(91, audio_port(90, true, "FL"));
        apps.ports.insert(92, audio_port(90, true, "FR"));
        let links = apps
            .desired_links("pipewire:application:A", 90, &mut policy, true)
            .unwrap();
        assert_eq!(links.len(), 2);
        assert!(links.iter().all(|link| link.output_node == 10));
        policy.failed(10, true, now, "temporary removal").unwrap();
        apps.nodes.clear();
        apps.desired_links("pipewire:application:A", 90, &mut policy, true)
            .unwrap();
        assert!(policy.failures.is_empty());
        assert!(policy.skipped.is_empty());
    }

    #[test]
    fn reused_node_id_clears_old_skip_and_failure_without_an_intermediate_poll() {
        for skip in [false, true] {
            let mut apps = Applications::default();
            let mut policy = AppLinkPolicy::default();
            let mut node = app_node(Some("A"), "Player", None, 100);
            node.generation = 1;
            apps.nodes.insert(20, node);
            apps.ports.insert(21, audio_port(20, false, "MONO"));
            apps.ports.insert(91, audio_port(90, true, "FL"));
            apps.ports.insert(92, audio_port(90, true, "FR"));
            let good = apps
                .desired_links("pipewire:application:A", 90, &mut policy, true)
                .unwrap();
            assert_eq!(good.len(), 2);
            if skip {
                policy.reject(20, true, "refused".into()).unwrap();
            } else {
                policy.failed(20, true, Instant::now(), "refused").unwrap();
            }

            // Simulate global_remove and global(new incarnation) in one iterate:
            // desired_links never observes a snapshot with node 20 missing.
            apps.nodes.remove(&20);
            let mut replacement = app_node(Some("A"), "Player", None, 200);
            replacement.generation = 2;
            apps.nodes.insert(20, replacement);
            assert_eq!(
                apps.desired_links("pipewire:application:A", 90, &mut policy, true)
                    .unwrap(),
                good
            );
            assert!(policy.skipped.is_empty());
            assert!(policy.failures.is_empty());
            assert_eq!(policy.generations, BTreeMap::from([(20, 2)]));
            apps.nodes.clear();
            apps.desired_links("pipewire:application:A", 90, &mut policy, true)
                .unwrap();
            assert!(policy.generations.is_empty());
        }
    }

    #[test]
    fn missing_application_lists_at_most_ten_valid_ids_and_excludes_own_process() {
        let mut apps = Applications::default();
        for index in 0..12 {
            apps.nodes.insert(
                index,
                app_node(Some(&format!("App{index:02}")), "Player", None, 100),
            );
        }
        apps.nodes.insert(
            20,
            app_node(Some("Own"), "Isocast", None, std::process::id()),
        );
        assert!(
            apps.list()
                .iter()
                .all(|app| app.id != "pipewire:application:Own")
        );
        let message = apps.resolve("missing").unwrap_err().to_string();
        assert!(message.contains("pipewire:application:App00"));
        assert!(message.contains("pipewire:application:App09"));
        assert!(!message.contains("pipewire:application:App10"));
        assert_eq!(message.matches("pipewire:application:").count(), 10);
    }

    #[test]
    fn application_clock_bridges_only_silent_driver_handover() {
        let clock = GraphClock {
            id: 10,
            nsec: 1,
            duration: 1024,
            num: 1,
            denom: RATE,
        };
        let start = Time::from_nanos(1_000_000_000);
        let end = start.add_ticks(1024, RATE);
        for (driver, before_silent, after_silent, gap, fill) in [
            (20, true, false, 696, true),
            (20, false, true, 328, true),
            (10, true, true, 696, false),
            (20, false, false, 696, false),
            (20, true, true, 1720, false),
            (20, true, true, -1024, false),
            (20, true, true, 0, false),
        ] {
            let mut timeline = ApplicationClock::default();
            assert_eq!(
                timeline.silence(Some(clock.id), start, 1024, before_silent),
                None
            );
            let at = Time::from_nanos(end.as_nanos() + gap * 1_000_000_000 / i64::from(RATE));
            assert_eq!(
                timeline.silence(Some(driver), at, 1024, after_silent),
                fill.then_some((end, gap as usize))
            );
        }
    }

    #[test]
    fn application_clock_records_buffers_with_unknown_driver() {
        let mut timeline = ApplicationClock::default();
        let start = Time::from_nanos(1_000_000_000);
        assert_eq!(timeline.silence(Some(10), start, 1024, true), None);
        let next = start.add_ticks(1024, RATE);
        assert_eq!(timeline.silence(None, next, 1024, true), None);
        assert_eq!(
            timeline.previous,
            Some((None, next.add_ticks(1024, RATE), true))
        );
        assert_eq!(
            timeline.silence(Some(20), next.add_ticks(1200, RATE), 1024, false),
            None
        );
        let end = next.add_ticks(2224, RATE);
        assert_eq!(
            timeline.silence(Some(30), end.add_ticks(128, RATE), 1024, true),
            Some((end, 128))
        );
    }

    #[test]
    fn application_proxy_removal_is_not_a_core_disconnect() {
        // Historical watcher Node/Client and Link IDs, including dropped proxies.
        let owned = BTreeSet::from([38, 40, 41]);
        assert!(!fatal_capture_error(
            true,
            pw::core::PW_ID_CORE,
            -libc::ENOENT,
            &owned
        ));
        assert!(!fatal_capture_error(true, 38, -libc::ENOENT, &owned));
        assert!(!fatal_capture_error(true, 39, -libc::ENOENT, &owned));
        assert!(!fatal_capture_error(true, 38, -libc::EIO, &owned));
        assert!(!fatal_capture_error(true, 40, -libc::EACCES, &owned));
        assert!(!fatal_capture_error(true, 41, -libc::EIO, &owned));
        assert!(fatal_capture_error(true, 39, -libc::EACCES, &owned));
        assert!(fatal_capture_error(true, 39, -libc::EIO, &owned));
        assert!(fatal_capture_error(
            true,
            pw::core::PW_ID_CORE,
            -libc::EPIPE,
            &owned
        ));
        assert!(fatal_capture_error(
            true,
            pw::core::PW_ID_CORE,
            -libc::EPROTO,
            &owned
        ));
        assert!(fatal_capture_error(
            false,
            pw::core::PW_ID_CORE,
            -libc::ENOENT,
            &owned
        ));
        assert!(fatal_capture_error(false, 38, -libc::ENOENT, &owned));
        assert!(fatal_capture_error(false, 38, -libc::EIO, &owned));
    }

    #[test]
    fn channels_are_one_based_distinct_and_preserve_mono() {
        assert_eq!(selection(1, None).unwrap(), [0]);
        assert_eq!(selection(8, None).unwrap(), [0, 1]);
        assert_eq!(selection(8, Some(&[8, 2])).unwrap(), [7, 1]);
        for channels in [&[][..], &[0], &[9], &[1, 1], &[1, 2, 3]] {
            assert!(selection(8, Some(channels)).is_err());
        }
        assert!(selection(0, None).is_err());
        assert!(selection(65, None).is_err());
    }

    #[test]
    fn conversion_honors_chunk_offset_and_channel_order() {
        let mut bytes = vec![0xff; 3]; // deliberately unaligned
        for sample in [1.0_f32, 2.0, 3.0, 4.0, 5.0, 6.0] {
            bytes.extend(sample.to_le_bytes());
        }
        let mut output = Vec::new();
        convert(&bytes, 3, 24, 12, 3, &[2, 0], &mut output).unwrap();
        assert_eq!(output, [3.0, 1.0, 6.0, 4.0]);
        convert(&bytes, 3, 24, 0, 3, &[1], &mut output).unwrap();
        assert_eq!(output, [2.0, 5.0]);
    }

    #[test]
    fn malformed_chunks_are_rejected_without_indexing_out_of_bounds() {
        let bytes = [0; 16];
        let mut output = Vec::new();
        for (offset, size, stride) in [
            (8, 16, 8),
            (0, 15, 8),
            (0, 16, -8),
            (0, 16, 4),
            (usize::MAX, 8, 8),
        ] {
            assert!(convert(&bytes, offset, size, stride, 2, &[0], &mut output).is_err());
        }
        assert!(convert(&bytes, 0, 16, 8, 2, &[2], &mut output).is_err());
        assert!(convert(&[], 0, 4, 4, 1, &[0], &mut output).is_err());
    }

    #[test]
    fn conversion_conceals_non_finite_samples_and_normalizes_offset() {
        let bytes: Vec<_> = [0.25_f32, f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.5]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect();
        let mut output = Vec::new();
        assert_eq!(
            convert(
                &bytes,
                bytes.len() * 3,
                bytes.len(),
                4,
                1,
                &[0],
                &mut output
            )
            .unwrap(),
            3
        );
        assert_eq!(output, [0.25, 0.0, 0.0, 0.0, -0.5]);
        assert_eq!(
            convert(&bytes, bytes.len() + 16, 4, 4, 1, &[0], &mut output).unwrap(),
            0
        );
        assert_eq!(output, [-0.5]);
        assert_eq!(convert(&bytes, 0, 8, 8, 2, &[0], &mut output).unwrap(), 0);
        assert_eq!(output, [0.25]);
        assert_eq!(
            convert(&bytes, 0, 8, 8, 2, &[0, 1], &mut output).unwrap(),
            1
        );
        assert_eq!(output, [0.25, 0.0]);
    }

    #[test]
    fn timestamp_maps_engine_origin_and_does_not_accumulate_rounding() {
        let graph = 9_000_000_000_000;
        let offset = -8_999_000_000_000;
        assert_eq!(
            timestamp(graph, offset, 480, 1, RATE, 48, 0)
                .unwrap()
                .as_nanos(),
            989_000_000
        );
        let first = timestamp(graph, offset, 0, 1, RATE, 0, 0).unwrap();
        let hour = timestamp(graph + 3_600_000_000_000, offset, 0, 1, RATE, 0, 0).unwrap();
        assert_eq!(hour.ticks_since(first, RATE), 3_600 * i64::from(RATE));
        assert!(timestamp(graph, offset, 0, 1, 0, 0, 0).is_err());
    }

    #[test]
    fn linked_peer_requires_one_distinct_output_node() {
        let mut links = BTreeMap::new();
        assert_eq!(linked_peer(&links, 10), None);
        links.insert(1, (20, 30));
        assert_eq!(linked_peer(&links, 10), None);
        links.insert(2, (10, 40));
        assert_eq!(linked_peer(&links, 10), Some(40));
        links.insert(3, (10, 40));
        assert_eq!(linked_peer(&links, 10), Some(40));
        links.insert(4, (10, 50));
        assert_eq!(linked_peer(&links, 10), None);
    }

    #[test]
    fn monitor_correction_requires_the_linked_sink_to_drive_this_buffer() {
        let clock = GraphClock {
            id: 46,
            nsec: 9_000_000_000,
            duration: 2048,
            num: 1,
            denom: 48_000,
        };
        assert_eq!(
            monitor_delay(false, Some(46), clock, clock.nsec),
            42_666_666
        );
        assert_eq!(monitor_delay(false, Some(59), clock, clock.nsec), 0);
        assert_eq!(monitor_delay(true, Some(46), clock, clock.nsec), 0);
        assert_eq!(monitor_delay(false, None, clock, clock.nsec), 0);
        assert_eq!(
            monitor_delay(false, Some(46), clock, clock.nsec - 1),
            42_666_666
        );
        for cycles in 1..=64 {
            assert_eq!(
                monitor_delay(false, Some(46), clock, clock.nsec - cycles * 42_666_666),
                42_666_666
            );
        }
        assert_eq!(monitor_delay(false, Some(46), clock, clock.nsec + 1), 0);
        let clock = GraphClock {
            duration: 1024,
            denom: 44_100,
            ..clock
        };
        assert_eq!(
            monitor_delay(false, Some(46), clock, clock.nsec),
            23_219_954
        );
        let clock = GraphClock {
            duration: 512,
            num: 2,
            ..clock
        };
        assert_eq!(
            monitor_delay(false, Some(46), clock, clock.nsec),
            23_219_954
        );
    }

    #[test]
    fn monitor_correction_rejects_invalid_clocks_and_timestamp_overflow() {
        let clock = GraphClock {
            id: 46,
            nsec: 9_000_000_000,
            duration: 2048,
            num: 1,
            denom: RATE,
        };
        for invalid in [
            GraphClock { num: 0, ..clock },
            GraphClock { denom: 0, ..clock },
            GraphClock { nsec: 0, ..clock },
            GraphClock {
                duration: 0,
                ..clock
            },
            GraphClock {
                duration: u64::MAX,
                num: u32::MAX,
                ..clock
            },
            GraphClock {
                duration: u64::from(RATE) + 1,
                ..clock
            },
            GraphClock {
                id: u32::MAX,
                ..clock
            },
        ] {
            assert_eq!(
                monitor_delay(false, Some(invalid.id), invalid, invalid.nsec),
                0
            );
        }
        let correction = monitor_delay(false, Some(46), clock, clock.nsec);
        assert_eq!(
            timestamp(clock.nsec, -8_000_000_000, 480, 1, RATE, 48, correction)
                .unwrap()
                .as_nanos(),
            946_333_334
        );
        assert!(timestamp(u64::MAX, 0, 0, 1, RATE, 0, correction).is_err());
        assert!(timestamp(1, i64::MIN, 0, 1, RATE, 0, correction).is_err());
    }
}
