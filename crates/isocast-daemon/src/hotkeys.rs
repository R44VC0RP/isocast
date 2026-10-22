//! Profile registration and action dispatch. Nothing runs on the media tick;
//! native callbacks only send edges to one worker. Holds overlay the setup in
//! memory, so a crash/restart never saves a momentary unmute into the profile.

use parking_lot::Mutex;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

use base64::Engine;
#[cfg(target_os = "macos")]
use isocast_engine::hotkeys::GlobalHotkeys;
use isocast_engine::hotkeys::{Edge, Key};
use isocast_protocol::Setup;
use isocast_protocol::api::{
    ApplyParams, Event, Method, Request, RpcError, SnapshotParams, StartParams, StreamStartParams,
};
use isocast_protocol::caller::Caller;
use isocast_protocol::change::Change;
use isocast_protocol::hotkeys::{
    self, Arguments, Binding, Bindings, Chord, Hotkeys, InputParams, InputSource, Phase,
    Registration, SetParams,
};
use isocast_protocol::library::LibrarySwitchParams;
use isocast_protocol::timeline::{TimelineExportParams, TimelineMoveParams, TimelineRemoveParams};
use serde_json::{Value, json};

use crate::{Daemon, State, failed};

#[derive(Default)]
struct Registered {
    identity: (Option<String>, Option<String>),
    bindings: Bindings,
    registrations: Vec<Registration>,
    generation: u64,
    config: hotkeys::Config,
    warnings: Vec<String>,
    pending: bool,
}

#[derive(Default)]
pub(crate) struct HotkeyRuntime {
    refresh: Mutex<()>,
    registered: Mutex<Registered>,
    sender: OnceLock<mpsc::SyncSender<Input>>,
    overflow: Arc<AtomicBool>,
}

enum Input {
    /// A key edge, its source, owner, time, the plugin whose request sent
    /// it (`hotkeys_input`), if one did: its action is then checked as that
    /// plugin's request (#103), and the caller who sent it: its action runs
    /// as that caller (a network caller's is held to its policy, and its
    /// changes carry its name; decision 25). Native keys are local.
    Key(Edge, InputSource, u64, Instant, Option<String>, Caller),
    Disconnect(u64),
}

/// Match the two delivery paths, not two intentional presses on one path.
/// Times are stamped when enqueued, before a slow action can delay the worker.
const DEDUPE_WINDOW: Duration = Duration::from_millis(100);
type PressToken = (u64, String, InputSource, u64);

struct RecentPress {
    token: PressToken,
    at: Instant,
    id: u64,
    paired: bool,
}

#[derive(Default)]
struct Presses {
    down: HashMap<PressToken, u64>,
    active: HashSet<u64>,
    recent: VecDeque<RecentPress>,
    next: u64,
    generation: u64,
}

impl Presses {
    fn press(&mut self, token: PressToken, at: Instant) -> Option<u64> {
        if self.generation != token.0 {
            self.down.clear();
            self.active.clear();
            self.recent.clear();
            self.generation = token.0;
        }
        if self.down.contains_key(&token) {
            return None;
        }
        self.recent
            .retain(|p| at.saturating_duration_since(p.at) <= DEDUPE_WINDOW);
        if let Some(previous) = self.recent.iter_mut().find(|p| {
            !p.paired && p.token.0 == token.0 && p.token.1 == token.1 && p.token.2 != token.2
        }) {
            previous.paired = true;
            self.down.insert(token, previous.id);
            return None;
        }
        self.next += 1;
        let id = self.next;
        self.down.insert(token.clone(), id);
        self.active.insert(id);
        self.recent.push_back(RecentPress {
            token,
            at,
            id,
            paired: false,
        });
        // A bounded history even under input bursts; holds live separately.
        if self.recent.len() > 256 {
            self.recent.pop_front();
        }
        Some(id)
    }

    fn release(&mut self, token: &PressToken) -> Option<u64> {
        let id = self.down.remove(token)?;
        self.down.retain(|_, stroke| *stroke != id);
        self.active.remove(&id).then_some(id)
    }

    fn disconnect(&mut self, owner: u64) -> Vec<u64> {
        let ids: HashSet<_> = self
            .down
            .iter()
            .filter(|(token, _)| token.3 == owner)
            .map(|(_, id)| *id)
            .collect();
        self.down.retain(|_, id| !ids.contains(id));
        ids.into_iter()
            .filter(|id| self.active.remove(id))
            .collect()
    }
}

/// Focused push keys belong to the socket delivering them. Losing a client
/// releases them, even when its window never got to send key-up/blur.
pub(crate) struct FocusedConnection<'a> {
    daemon: &'a Daemon,
    pub(crate) id: u64,
}

impl Drop for FocusedConnection<'_> {
    fn drop(&mut self) {
        if let Some(sender) = self.daemon.hotkeys.sender.get()
            && sender.try_send(Input::Disconnect(self.id)).is_err()
        {
            self.daemon.hotkeys.overflow.store(true, Ordering::Release);
        }
    }
}

fn invalid(message: impl Into<String>) -> RpcError {
    RpcError {
        code: RpcError::INVALID_PARAMS,
        message: message.into(),
    }
}

impl State {
    /// Push-to-mute wins over simultaneous push-to-talk on the same source.
    pub(crate) fn held_sources(&self) -> BTreeMap<String, bool> {
        let mut result = BTreeMap::new();
        for (source, mute) in self.hotkey_holds.values() {
            if !self
                .setup
                .as_ref()
                .is_some_and(|setup| setup.source(source).is_some_and(|s| s.kind.has_audio()))
            {
                continue;
            }
            result
                .entry(source.clone())
                .and_modify(|v| *v |= *mute)
                .or_insert(*mute);
        }
        result
    }

    pub(crate) fn prune_hotkey_holds(&mut self) {
        let setup = &self.setup;
        self.hotkey_holds.retain(|_, (id, _)| {
            setup
                .as_ref()
                .is_some_and(|s| s.source(id).is_some_and(|source| source.kind.has_audio()))
        });
    }

    pub(crate) fn with_hotkey_holds<'a>(&self, setup: &'a Setup) -> std::borrow::Cow<'a, Setup> {
        if self.hotkey_holds.is_empty() {
            return std::borrow::Cow::Borrowed(setup);
        }
        let mut setup = setup.clone();
        for (id, mute) in self.held_sources() {
            if let Some(source) = setup.sources.iter_mut().find(|s| s.id == id) {
                source.audio.muted = mute;
            }
        }
        std::borrow::Cow::Owned(setup)
    }
}

impl Daemon {
    pub(crate) fn focused_connection(&self) -> FocusedConnection<'_> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        FocusedConnection {
            daemon: self,
            id: NEXT.fetch_add(1, Ordering::Relaxed),
        }
    }

    pub(crate) fn start_hotkeys(self: &Arc<Self>) {
        let (tx, rx) = mpsc::sync_channel(256);
        let _ = self.hotkeys.sender.set(tx);
        let weak = Arc::downgrade(self);
        std::thread::spawn(move || {
            let mut pressed = Presses::default();
            while let Ok(input) = rx.recv() {
                let Some(daemon) = weak.upgrade() else {
                    break;
                };
                if daemon.hotkeys.overflow.swap(false, Ordering::AcqRel) {
                    for _ in rx.try_iter() {}
                    pressed = Presses::default();
                    daemon.clear_hotkey_holds();
                    daemon.publish(Event::Warning { message: "hotkey input queue overflowed; pending actions cancelled and push keys released".into() });
                    continue;
                }
                let (edge, source, owner, at, plugin, caller) = match input {
                    Input::Key(edge, source, owner, at, plugin, caller) => {
                        (edge, source, owner, at, plugin, caller)
                    }
                    Input::Disconnect(owner) => {
                        let released = pressed.disconnect(owner);
                        let mut state = daemon.state.lock();
                        let count = state.hotkey_holds.len();
                        for id in released {
                            state.hotkey_holds.remove(&format!("press:{id}"));
                        }
                        if count != state.hotkey_holds.len()
                            && let (Some(engine), Some(setup)) = (&state.engine, &state.setup)
                            && let Err(message) =
                                engine.update(&state.with_hotkey_holds(setup), state.revision)
                        {
                            eprintln!("isocastd: releasing disconnected push keys: {message}");
                        }
                        continue;
                    }
                };
                let binding = {
                    let registered = daemon.hotkeys.registered.lock();
                    if registered.generation != edge.generation {
                        continue;
                    }
                    registered.bindings.get(&edge.binding).cloned()
                };
                let Some(binding) = binding else {
                    continue;
                };
                let token = (edge.generation, edge.binding.clone(), source, owner);
                let stroke = match edge.phase {
                    Phase::Press => pressed.press(token, at),
                    Phase::Release => pressed.release(&token).filter(|_| binding.hold()),
                };
                let Some(stroke) = stroke else {
                    continue;
                };
                let hold_id = format!("press:{stroke}");
                let result = {
                    let _caller = plugin.as_deref().map(crate::PluginCaller::set);
                    // Every path an action takes (methods, setup patches,
                    // library switches) sees who pressed it.
                    let _entered = crate::callers::enter(caller.clone());
                    daemon.fire_hotkey(&hold_id, &binding, edge.phase)
                };
                if let Err(error) = &result {
                    daemon.note_refusal(&caller, &format!("hotkey {}", binding.action), error);
                }
                let error = result.err().map(|e| {
                    let field = if e.message.starts_with("args") || e.message.starts_with("action:")
                    {
                        ""
                    } else {
                        "action: "
                    };
                    hotkeys::bounded(
                        &format!("hotkeys.{}.{field}{}", edge.binding, e.message),
                        8192,
                    )
                });
                if let Some(message) = &error {
                    daemon.publish(Event::Warning {
                        message: format!(
                            "hotkeys.{} ({}): {message}",
                            edge.binding, binding.action
                        ),
                    });
                }
                daemon.publish(Event::HotkeyFired {
                    binding: edge.binding,
                    action: binding.action,
                    source,
                    phase: edge.phase,
                    error,
                });
            }
        });
    }

    pub(crate) fn refresh_hotkeys(&self) {
        if self.hotkeys.sender.get().is_none() {
            return;
        }
        let _refresh = self.hotkeys.refresh.lock();
        let mut registered = self.hotkeys.registered.lock();
        let (identity, config) = {
            let state = self.state.read();
            let current = state.file.current();
            (
                (
                    current.map(|c| c.collection.clone()),
                    current.map(|c| c.profile.clone()),
                ),
                state
                    .setup
                    .as_ref()
                    .map(|s| s.hotkeys.clone())
                    .unwrap_or_default(),
            )
        };
        if registered.identity == identity && registered.config == config {
            return;
        }
        let (bindings, issues) = config.usable();
        let mut warnings = if issues.is_empty() {
            Vec::new()
        } else {
            vec![hotkeys::describe(&issues)]
        };
        let generation = registered.generation + 1;
        *registered = Registered {
            identity: identity.clone(),
            config: config.clone(),
            bindings: bindings.clone(),
            generation,
            pending: true,
            warnings: warnings.clone(),
            ..Default::default()
        };
        drop(registered);
        self.clear_hotkey_holds();
        let keys: Vec<_> = bindings
            .iter()
            .filter_map(|(id, b)| {
                Chord::parse(&b.keys).ok().map(|chord| Key {
                    binding: id.clone(),
                    chord,
                })
            })
            .collect();
        let sender = self.hotkeys.sender.get().unwrap().clone();
        let overflow = self.hotkeys.overflow.clone();
        // Overflow is signalled by a separate flag; callbacks never block the
        // main thread, including while an output start/stop takes a long time.
        let on_key = Arc::new(move |edge| {
            // The queue fits a whole profile's edges. A full queue is handled
            // below by the same enqueue path for focused input.
            if sender
                .try_send(Input::Key(
                    edge,
                    InputSource::Global,
                    0,
                    Instant::now(),
                    None,
                    Caller::local(),
                ))
                .is_err()
            {
                overflow.store(true, Ordering::Release);
            }
        });
        #[cfg(target_os = "macos")]
        let registrations = if crate::PARENT_OWNED.load(Ordering::SeqCst) {
            // Foreground record previously opened no global shortcuts. A
            // private engine must not steal the app's chords or fire actions
            // from an otherwise inactive profile. Focused API input still works.
            keys.into_iter()
                .map(|key| Registration {
                    binding: key.binding,
                    global: false,
                    warning: None,
                })
                .collect()
        } else {
            isocast_platform_macos::hotkeys::MacHotkeys.replace(keys, generation, on_key)
        };
        #[cfg(not(target_os = "macos"))]
        let registrations: Vec<Registration> = {
            let _ = on_key;
            keys.into_iter().map(|key| Registration { binding: key.binding, global: false,
                warning: Some("global registration is not implemented on this platform; use focused input".into()) }).collect()
        };
        warnings.extend(
            registrations
                .iter()
                .filter_map(|r| {
                    r.warning
                        .as_ref()
                        .map(|w| format!("hotkeys.{}.keys: {w}", r.binding))
                })
                .collect::<Vec<_>>(),
        );
        let mut registered = self.hotkeys.registered.lock();
        *registered = Registered {
            identity,
            bindings,
            registrations,
            generation,
            config,
            warnings: warnings.clone(),
            pending: false,
        };
        drop(registered);
        for message in warnings {
            eprintln!("isocastd: {message}");
            self.publish(Event::Warning { message });
        }
    }

    pub(crate) fn hotkeys_get(&self) -> Hotkeys {
        let registered = self.hotkeys.registered.lock();
        let state = self.state.read();
        Hotkeys {
            profile: state.file.current().map(|c| c.profile.clone()),
            revision: state.revision,
            bindings: registered.bindings.clone(),
            configured: state
                .setup
                .as_ref()
                .map(|s| s.hotkeys.clone())
                .unwrap_or_default(),
            warnings: registered.warnings.clone(),
            pending: registered.pending
                || state
                    .setup
                    .as_ref()
                    .is_some_and(|s| s.hotkeys != registered.config),
            registrations: registered.registrations.clone(),
            held_sources: state.held_sources(),
            studio_mode: state.preview.is_some(),
        }
    }

    pub(crate) fn hotkeys_set(&self, params: SetParams) -> Result<Hotkeys, RpcError> {
        let (revision, mut setup) = {
            let state = self.state.read();
            let setup = state
                .setup
                .as_ref()
                .ok_or_else(|| state.no_setup("hotkeys need a profile"))?;
            (state.revision, setup.clone())
        };
        let mut value = setup.hotkeys.value.clone().unwrap_or(json!({}));
        match (params.bindings, params.patch) {
            (Some(all), None) => value = serde_json::to_value(all).unwrap(),
            (None, Some(patch)) => {
                let bindings = value.as_object_mut().ok_or_else(|| {
                    invalid(
                        "hotkeys: stored configuration is not an object; replace it with bindings",
                    )
                })?;
                for (id, binding) in patch {
                    if let Some(binding) = binding {
                        bindings.insert(id, serde_json::to_value(binding).unwrap());
                    } else {
                        if bindings.remove(&id).is_none() {
                            return Err(invalid(format!(
                                "patch.{}: no such binding; valid ids: {}",
                                hotkeys::bounded(&id, 64),
                                bindings
                                    .keys()
                                    .take(16)
                                    .map(|s| hotkeys::bounded(s, 64))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            )));
                        }
                    }
                }
            }
            _ => {
                return Err(invalid(
                    "bindings, patch: give exactly one (a whole bindings map, or a patch with null to unset)",
                ));
            }
        }
        let (mut bindings, issues) = hotkeys::Config {
            value: Some(value),
            load_warning: None,
        }
        .usable();
        if !issues.is_empty() {
            return Err(invalid(hotkeys::describe(&issues)));
        }
        for binding in bindings.values_mut() {
            binding.keys = Chord::parse(&binding.keys).unwrap().canonical();
        }
        // A whole setup replacement avoids merge-patch retaining bindings that
        // a whole-map replacement explicitly removed.
        setup.hotkeys = bindings.into();
        self.apply(ApplyParams {
            revision: params.revision.unwrap_or(revision),
            change: Change::Setup(Box::new(setup)),
            live: false,
        })?;
        Ok(self.hotkeys_get())
    }

    pub(crate) fn hotkeys_input(&self, params: InputParams, owner: u64) -> Result<Value, RpcError> {
        let chord = Chord::parse(&params.keys).map_err(|e| invalid(format!("keys: {e}")))?;
        let registered = self.hotkeys.registered.lock();
        if registered.pending {
            return Err(invalid(
                "hotkeys: registration update in progress; retry after it completes",
            ));
        }
        let binding = registered
            .bindings
            .iter()
            .find(|(_, b)| Chord::parse(&b.keys).ok().as_ref() == Some(&chord));
        let Some((id, _)) = binding else {
            return Ok(json!({"queued":false}));
        };
        let sender = self
            .hotkeys
            .sender
            .get()
            .ok_or_else(|| invalid("hotkeys are not running"))?;
        if sender
            .try_send(Input::Key(
                Edge {
                    binding: id.clone(),
                    generation: registered.generation,
                    phase: params.phase,
                },
                InputSource::Focused,
                owner,
                Instant::now(),
                crate::PluginCaller::current(),
                crate::callers::current(),
            ))
            .is_err()
        {
            self.hotkeys.overflow.store(true, Ordering::Release);
            return Err(invalid(
                "hotkeys input queue full; held keys will be released",
            ));
        }
        Ok(json!({"queued":true}))
    }

    fn clear_hotkey_holds(&self) {
        let mut state = self.state.lock();
        if state.hotkey_holds.is_empty() {
            return;
        }
        state.hotkey_holds.clear();
        if let (Some(engine), Some(setup)) = (&state.engine, &state.setup)
            && let Err(message) = engine.update(setup, state.revision)
        {
            eprintln!("isocastd: releasing push keys: {message}");
        }
    }

    fn hotkey_hold(
        &self,
        id: &str,
        source: &str,
        mute: bool,
        phase: Phase,
    ) -> Result<(), RpcError> {
        let mut state = self.state.lock();
        let setup = state
            .setup
            .as_ref()
            .ok_or_else(|| state.no_setup("push keys need a setup"))?;
        if phase == Phase::Press {
            check_audio(setup, source)?;
        }
        let previous = state.hotkey_holds.get(id).cloned();
        match phase {
            Phase::Press => {
                state.hotkey_holds.insert(id.into(), (source.into(), mute));
            }
            Phase::Release => {
                state.hotkey_holds.remove(id);
            }
        }
        if let (Some(engine), Some(setup)) = (&state.engine, &state.setup)
            && let Err(error) = engine.update(&state.with_hotkey_holds(setup), state.revision)
        {
            if let Some(previous) = previous {
                state.hotkey_holds.insert(id.into(), previous);
            } else {
                state.hotkey_holds.remove(id);
            }
            return Err(invalid(format!("args.source: {error}")));
        }
        Ok(())
    }

    fn hotkey_patch(&self, patch: Value) -> Result<(), RpcError> {
        let revision = self.state.lock().revision;
        self.apply(ApplyParams {
            revision,
            change: Change::Patch(patch),
            live: false,
        })
        .map(|_| ())
    }

    fn fire_hotkey(
        self: &Arc<Self>,
        id: &str,
        binding: &Binding,
        phase: Phase,
    ) -> Result<(), RpcError> {
        let args = binding.arguments().map_err(invalid)?;
        let action = binding.action.as_str();
        if let Some(reason) = hotkeys::catalogue()
            .actions
            .into_iter()
            .find(|a| a.id == action)
            .and_then(|a| a.unavailable_reason)
        {
            return Err(invalid(format!("action {action}: {reason}")));
        }
        let method = match args {
            Arguments::Empty => match action {
                "recording.start" => Method::Start(Box::new(StartParams {
                    setup: None,
                    folder: None,
                    seconds: None,
                    ..Default::default()
                })),
                "recording.stop" => Method::RequestStop(None),
                "recording.pause" => Method::Pause,
                "recording.resume" => Method::Resume,
                "recording.pause_toggle" => {
                    let control = self.state.lock().control();
                    if control
                        .and_then(|control| control.pause_status(crate::now()))
                        .is_some_and(|recording| recording.paused)
                    {
                        Method::Resume
                    } else {
                        Method::Pause
                    }
                }
                "recording.toggle" => {
                    if self.state.lock().session.is_some() {
                        Method::RequestStop(None)
                    } else {
                        Method::Start(Box::new(StartParams {
                            setup: None,
                            folder: None,
                            seconds: None,
                            ..Default::default()
                        }))
                    }
                }
                "recording.split" => Method::Split,
                "timeline.close" => Method::TimelineClose,
                "studio.toggle" => {
                    let mut state = self.state.lock();
                    if state.preview.is_some() {
                        state.preview = None;
                        if let Some(engine) = &state.engine {
                            engine.set_preview(None);
                        }
                        drop(state);
                        // get/hotkeys_get expose studio state to thin clients.
                        self.publish(Event::PreviewCleared);
                    } else {
                        let scene = state
                            .scene
                            .clone()
                            .ok_or_else(|| invalid("studio.toggle: no program scene"))?;
                        drop(state);
                        self.preview(scene, isocast_protocol::CanvasId::Main)?;
                    }
                    return Ok(());
                }
                _ => return Err(invalid(format!("action: {action} is not available"))),
            },
            // A canvas's streams; the main one's unless args say vertical
            // (as before canvases, with no args).
            Arguments::Stream(args) => {
                let canvas = args.canvas;
                let params = |canvas: isocast_protocol::CanvasId| {
                    (!canvas.is_main()).then(|| StreamStartParams {
                        canvas,
                        ..Default::default()
                    })
                };
                let stop = match action {
                    "streaming.start" => false,
                    "streaming.stop" => true,
                    "streaming.toggle" => self.state.read().streaming_on(canvas),
                    _ => return Err(invalid(format!("action: {action} is not available"))),
                };
                if stop {
                    Method::StreamStop(params(canvas))
                } else {
                    Method::StreamStart(params(canvas))
                }
            }
            // A canvas's replay buffer; the main one's unless args say
            // vertical.
            Arguments::Replay(args) => {
                let canvas = args.canvas;
                let params = isocast_protocol::replay::ReplayParams::of(canvas);
                match action {
                    "replay.start" => Method::ReplayStart(params),
                    "replay.stop" => Method::ReplayStop(params),
                    "replay.save" => Method::ReplaySave(params),
                    "replay.toggle" => {
                        if self.state.read().replay.active_on(canvas) {
                            Method::ReplayStop(params)
                        } else {
                            Method::ReplayStart(params)
                        }
                    }
                    _ => return Err(invalid(format!("action: {action} is not available"))),
                }
            }
            Arguments::Marker(args) => Method::Marker { label: args.label },
            Arguments::Scene(args) => self.hotkey_scene(action, args.scene, args.canvas),
            Arguments::Index(args) => {
                let state = self.state.read();
                let ids = state.scene_ids();
                let scene = ids
                    .get(args.index as usize - 1)
                    .ok_or_else(|| {
                        invalid(format!(
                            "args.index: {} is out of range; valid positions 1..={} ({})",
                            args.index,
                            ids.len(),
                            ids.join(", ")
                        ))
                    })?
                    .clone();
                drop(state);
                self.hotkey_scene(action, scene, isocast_protocol::CanvasId::Main)
            }
            Arguments::Source(args) => {
                if binding.hold() {
                    return self.hotkey_hold(
                        id,
                        &args.source,
                        action == "audio.push_to_mute",
                        phase,
                    );
                }
                let state = self.state.read();
                let setup = state
                    .setup
                    .as_ref()
                    .ok_or_else(|| state.no_setup("audio hotkeys need a setup"))?;
                check_audio(setup, &args.source)?;
                let source = setup.source(&args.source).unwrap();
                let mute = match action {
                    "audio.mute" => true,
                    "audio.unmute" => false,
                    _ => !source.audio.muted,
                };
                drop(state);
                return self
                    .hotkey_patch(json!({"sources":[{"id":args.source,"audio":{"muted":mute}}]}));
            }
            Arguments::Item(args) => {
                let state = self.state.read();
                let setup = state
                    .setup
                    .as_ref()
                    .ok_or_else(|| state.no_setup("item hotkeys need a setup"))?;
                let scene = setup
                    .scenes
                    .iter()
                    .find(|s| s.id == args.scene)
                    .ok_or_else(|| {
                        invalid(format!(
                            "args.scene: unknown {:?}; valid scenes: {}",
                            args.scene,
                            state.scene_ids().join(", ")
                        ))
                    })?;
                let mut items = serde_json::to_value(&scene.items).unwrap();
                if !set_visibility(&mut items, &args.item, action) {
                    let mut ids = Vec::new();
                    item_ids(&items, &mut ids);
                    return Err(invalid(format!(
                        "args.item: unknown {:?}; valid item ids: {}",
                        args.item,
                        ids.join(", ")
                    )));
                }
                drop(state);
                return self.hotkey_patch(json!({"scenes":[{"id":args.scene,"items":items}]}));
            }
            Arguments::Transition(args) => {
                return self.hotkey_patch(json!({"transition":args.transition}));
            }
            // A canvas's preview on air; the main one's unless args say
            // vertical (as before canvases, with no canvas).
            Arguments::Take(args) => Method::Take {
                transition: None,
                canvas: args.canvas,
            },
            Arguments::TakeWith(args) => Method::Take {
                transition: Some(args.transition),
                canvas: args.canvas,
            },
            Arguments::Id(args) => Method::LibrarySwitch(LibrarySwitchParams {
                profile: (action == "profile.switch").then_some(args.id.clone()),
                collection: (action == "collection.switch").then_some(args.id),
            }),
            Arguments::Screenshot(args) => {
                // Fired by a network caller: the file goes inside the folder
                // roots (a guard rail, decision 26), written where checked.
                let path = self.checked_path(
                    "args.path",
                    &args.path.to_string_lossy(),
                    crate::roots::Access::Write,
                )?;
                let shot = self.snapshot(SnapshotParams {
                    source: None,
                    scene: None,
                    preview: false,
                    width: None,
                })?;
                let png = base64::engine::general_purpose::STANDARD
                    .decode(shot.png_base64)
                    .map_err(|e| failed(e.into()))?;
                // Never replace an existing screenshot or recording.
                use std::io::Write;
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .map_err(|e| invalid(format!("args.path: {}: {e}", args.path.display())))?;
                file.write_all(&png)
                    .and_then(|()| file.sync_all())
                    .map_err(|e| failed(e.into()))?;
                return Ok(());
            }
            Arguments::Retake(params) => Method::Retake(Some(params)),
            Arguments::TimelineNew(params) => Method::TimelineNew(Some(params)),
            Arguments::TimelineOpen(params) => Method::TimelineOpen(params),
            Arguments::TimelineRemove(params) => Method::TimelineRemove(TimelineRemoveParams {
                clip: params.clip,
                timeline: params.timeline,
            }),
            Arguments::TimelineMove(params) => Method::TimelineMove(TimelineMoveParams {
                clip: params.clip,
                index: params.index,
                timeline: params.timeline,
            }),
            Arguments::TimelineExport(params) => {
                Method::TimelineExport(Some(TimelineExportParams {
                    timeline: params.timeline,
                    folder: params.folder,
                    vertical: params.vertical,
                }))
            }
        };
        // A key sent by a client on another computer (`hotkeys_input`) is
        // held to the same local-only policy as its own requests.
        if let Some(error) = crate::callers::refusal(&crate::callers::current(), &method) {
            return Err(error);
        }
        let response = self.call(Request::new(0, method));
        response.error.map_or(Ok(()), |mut error| {
            if action.starts_with("timeline.")
                && let Some((field, _)) = error.message.split_once(':')
                && matches!(
                    field,
                    "timeline" | "clip" | "index" | "folder" | "seconds" | "name"
                )
            {
                error.message = format!("args.{}", hotkeys::bounded(&error.message, 2048));
                return Err(error);
            }
            // A canvas that can't do it (none, or following the main one).
            if error.message.starts_with("canvas: ") {
                error.message = format!("args.{}", hotkeys::bounded(&error.message, 2048));
                return Err(error);
            }
            let field = match action {
                "scene.switch" | "scene.preview" => "args.scene",
                "scene.switch_index" | "scene.preview_index" => "args.index",
                "profile.switch" | "collection.switch" => "args.id",
                "transition.take_with" => "args.transition",
                _ => "action",
            };
            error.message = format!("{field}: {}", hotkeys::bounded(&error.message, 2048));
            Err(error)
        })
    }

    /// Select `scene` on `canvas`: into its preview for `scene.preview`, or
    /// while it has one staged (Studio Mode), else on air.
    fn hotkey_scene(
        &self,
        action: &str,
        scene: String,
        canvas: isocast_protocol::CanvasId,
    ) -> Method {
        let staged = {
            let state = self.state.read();
            if canvas.is_main() {
                state.preview.is_some()
            } else {
                state.canvas_previews.contains_key(&canvas)
            }
        };
        if action.starts_with("scene.preview") || staged {
            Method::Preview { scene, canvas }
        } else {
            Method::SwitchScene {
                scene,
                transition: None,
                canvas,
            }
        }
    }
}

fn check_audio(setup: &Setup, source: &str) -> Result<(), RpcError> {
    let valid: Vec<_> = setup
        .sources
        .iter()
        .filter(|s| s.kind.has_audio())
        .map(|s| s.id.as_str())
        .collect();
    if valid.contains(&source) {
        Ok(())
    } else {
        Err(invalid(format!(
            "args.source: unknown/non-audio {source:?}; valid audio sources: {}",
            valid.join(", ")
        )))
    }
}

fn set_visibility(items: &mut Value, id: &str, action: &str) -> bool {
    for item in items.as_array_mut().into_iter().flatten() {
        if item
            .get("id")
            .or_else(|| item.get("source"))
            .and_then(Value::as_str)
            == Some(id)
        {
            let visible = item["visible"].as_bool().unwrap_or(true);
            item["visible"] = json!(match action {
                "item.show" => true,
                "item.hide" => false,
                _ => !visible,
            });
            return true;
        }
        if let Some(children) = item.get_mut("children")
            && set_visibility(children, id, action)
        {
            return true;
        }
    }
    false
}

fn item_ids(items: &Value, ids: &mut Vec<String>) {
    for item in items.as_array().into_iter().flatten() {
        if let Some(id) = item
            .get("id")
            .or_else(|| item.get("source"))
            .and_then(Value::as_str)
        {
            ids.push(id.into());
        }
        item_ids(&item["children"], ids);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(source: InputSource, owner: u64) -> PressToken {
        (1, "marker".into(), source, owner)
    }

    #[test]
    fn both_delivery_orders_fire_and_release_once() {
        let at = Instant::now();
        let global = token(InputSource::Global, 0);
        let focused = token(InputSource::Focused, 7);
        for (first, second) in [(&global, &focused), (&focused, &global)] {
            for release_first in [true, false] {
                let mut presses = Presses::default();
                let id = presses.press(first.clone(), at).unwrap();
                assert_eq!(
                    presses.press(second.clone(), at + Duration::from_millis(20)),
                    None
                );
                let (release, other) = if release_first {
                    (first, second)
                } else {
                    (second, first)
                };
                assert_eq!(presses.release(release), Some(id));
                assert_eq!(presses.release(other), None);
            }
        }
    }

    #[test]
    fn delayed_other_path_after_release_does_not_reactivate_a_hold() {
        let at = Instant::now();
        let global = token(InputSource::Global, 0);
        let focused = token(InputSource::Focused, 1);
        let mut presses = Presses::default();
        let id = presses.press(global.clone(), at).unwrap();
        assert_eq!(presses.release(&global), Some(id));
        assert_eq!(
            presses.press(focused.clone(), at + Duration::from_millis(50)),
            None
        );
        assert_eq!(presses.release(&focused), None);
    }

    #[test]
    fn repeats_are_ignored_but_rapid_distinct_presses_are_kept() {
        let at = Instant::now();
        let focused = token(InputSource::Focused, 1);
        let global = token(InputSource::Global, 0);
        let mut presses = Presses::default();
        for n in 0..3 {
            let time = at + Duration::from_millis(n * 15);
            let id = presses.press(focused.clone(), time).unwrap();
            assert_eq!(presses.press(focused.clone(), time), None);
            assert_eq!(presses.release(&focused), Some(id));
        }
        // The second delivery path gets one counterpart per original press.
        for n in 0..3 {
            assert_eq!(
                presses.press(global.clone(), at + Duration::from_millis(50 + n)),
                None
            );
            assert_eq!(presses.release(&global), None);
        }
    }

    #[test]
    fn cross_path_after_window_and_other_clients_remain_independent() {
        let at = Instant::now();
        let global = token(InputSource::Global, 0);
        let focused = token(InputSource::Focused, 1);
        let mut presses = Presses::default();
        let first = presses.press(global.clone(), at).unwrap();
        assert_eq!(presses.release(&global), Some(first));
        assert!(
            presses
                .press(focused.clone(), at + Duration::from_millis(101))
                .is_some()
        );
        assert!(
            presses
                .press(
                    token(InputSource::Focused, 2),
                    at + Duration::from_millis(110)
                )
                .is_some()
        );
    }

    #[test]
    fn disconnect_releases_paired_hold_and_old_generation_cannot_release_new_one() {
        let at = Instant::now();
        let global = token(InputSource::Global, 0);
        let focused = token(InputSource::Focused, 9);
        let mut presses = Presses::default();
        let first = presses.press(global.clone(), at).unwrap();
        assert_eq!(presses.press(focused, at), None);
        assert_eq!(presses.disconnect(9), vec![first]);
        assert_eq!(presses.release(&global), None);
        let new = (2, "marker".into(), InputSource::Global, 0);
        let next = presses.press(new.clone(), at).unwrap();
        assert_ne!(next, first);
        assert_eq!(presses.release(&global), None);
        assert_eq!(presses.release(&new), Some(next));
    }
}
