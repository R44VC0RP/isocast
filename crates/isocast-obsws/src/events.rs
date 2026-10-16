//! OBS events from Isocast's: translated one to one where Isocast has the
//! same event, and found by comparing the live setup before and after each
//! `setup_changed` for the rest (mute, volume, names, scene items,
//! transitions), so they fire whoever made the change.

use std::collections::{HashMap, HashSet};

use isocast_protocol::api::{EngineState, Event, EventKind, EventKinds, Status, TakeCause};
use isocast_protocol::library::{LibraryAction, LibraryKind};
use isocast_protocol::replay::ReplayState;
use isocast_protocol::stream::StreamState;
use isocast_protocol::virtual_camera::VirtualCameraState;
use serde_json::{Value, json};

use crate::requests::{Ctx, db_to_mul, monitor_type, settings_of, transition_name};
use crate::world::{self, World, uuid};

/// obs-websocket's EventSubscription bits.
pub mod sub {
    pub const GENERAL: u32 = 1 << 0;
    pub const CONFIG: u32 = 1 << 1;
    pub const SCENES: u32 = 1 << 2;
    pub const INPUTS: u32 = 1 << 3;
    pub const TRANSITIONS: u32 = 1 << 4;
    pub const OUTPUTS: u32 = 1 << 6;
    pub const SCENE_ITEMS: u32 = 1 << 7;
    pub const UI: u32 = 1 << 10;
    /// Every low-volume category (with Canvases, 1 << 11).
    pub const ALL: u32 = (1 << 12) - 1;
    pub const INPUT_VOLUME_METERS: u32 = 1 << 16;
}

/// One OBS event: its subscription bit, type and data.
pub struct ObsEvent {
    pub subscription: u32,
    pub kind: &'static str,
    pub data: Value,
}

fn ev(subscription: u32, kind: &'static str, data: Value) -> ObsEvent {
    ObsEvent {
        subscription,
        kind,
        data,
    }
}

/// The Isocast events a session watches.
pub fn kinds(meters: bool) -> EventKinds {
    let mut kinds = [
        EventKind::SetupChanged,
        EventKind::Started,
        EventKind::Stopped,
        EventKind::SceneChanged,
        EventKind::PreviewChanged,
        EventKind::PreviewCleared,
        EventKind::RecordingPaused,
        EventKind::Stream,
        EventKind::VirtualCamera,
        EventKind::Replay,
        EventKind::ReplaySaved,
        EventKind::LibraryChanged,
    ]
    .into_iter()
    .fold(EventKinds::NONE, EventKinds::with);
    if meters {
        kinds = kinds.with(EventKind::Levels);
    }
    kinds
}

/// What a session last saw, to tell what changed.
pub struct Tracker {
    world: World,
    studio: bool,
    recording: bool,
    record_path: Option<String>,
    stream: StreamState,
    camera: VirtualCameraState,
    replay: ReplayState,
}

impl Tracker {
    pub fn new(ctx: &Ctx) -> Option<Self> {
        let status = ctx.status().ok()?;
        let world = World::new(ctx.live().ok()?, Some(&status));
        let mut tracker = Self {
            world,
            studio: false,
            recording: false,
            record_path: None,
            stream: StreamState::Offline,
            camera: VirtualCameraState::Stopped,
            replay: ReplayState::Off,
        };
        tracker.outputs_from(&status);
        Some(tracker)
    }

    fn outputs_from(&mut self, status: &Status) {
        self.studio = status.preview.is_some();
        self.recording = status.state != EngineState::Idle;
        self.record_path = crate::requests::program_file(status);
        self.stream = status
            .stream
            .as_ref()
            .map_or(StreamState::Offline, |s| s.state);
        self.camera = status
            .virtual_camera
            .as_ref()
            .map_or(VirtualCameraState::Stopped, |c| c.state);
        self.replay = status.replay.as_ref().map_or(ReplayState::Off, |r| r.state);
    }

    /// After missing events: everything that differs from what the session
    /// last saw.
    pub fn resync(&mut self, ctx: &Ctx) -> Vec<ObsEvent> {
        let mut out = self.setup_changed(ctx);
        let Ok(status) = ctx.status() else {
            return out;
        };
        if status.scene != self.world.program
            && let Some(scene) = status.scene.clone()
        {
            out.extend(self.translate(
                ctx,
                Event::SceneChanged {
                    scene,
                    take: None,
                    time_seconds: None,
                    transition: None,
                },
            ));
        }
        let before = (
            self.studio,
            self.recording,
            self.stream,
            self.camera,
            self.replay,
            self.record_path.clone(),
        );
        self.outputs_from(&status);
        if self.studio != before.0 {
            out.push(ev(
                sub::UI,
                "StudioModeStateChanged",
                json!({ "studioModeEnabled": self.studio }),
            ));
        }
        if self.recording != before.1 {
            let (state, path) = if self.recording {
                ("STARTED", self.record_path.clone())
            } else {
                ("STOPPED", before.5)
            };
            out.push(record(self.recording, state, path));
        }
        if self.stream != before.2
            && let Some(e) = stream_event(before.2, self.stream)
        {
            out.push(e);
        }
        if self.camera != before.3
            && let Some(e) = camera_event(self.camera)
        {
            out.push(e);
        }
        if self.replay != before.4 {
            out.push(replay_event(self.replay));
        }
        out
    }

    /// The OBS events one Isocast event means.
    pub fn translate(&mut self, ctx: &Ctx, event: Event) -> Vec<ObsEvent> {
        match event {
            Event::SetupChanged { .. } => self.setup_changed(ctx),
            Event::SceneChanged { scene, .. } => {
                self.world.program = Some(scene.clone());
                let (name, id) = self.world.scene_ref(Some(&scene));
                vec![ev(
                    sub::SCENES,
                    "CurrentProgramSceneChanged",
                    json!({ "sceneName": name, "sceneUuid": id }),
                )]
            }
            Event::PreviewChanged { scene } => {
                let mut out = Vec::new();
                if !self.studio {
                    self.studio = true;
                    out.push(ev(
                        sub::UI,
                        "StudioModeStateChanged",
                        json!({ "studioModeEnabled": true }),
                    ));
                }
                self.world.preview = Some(scene.clone());
                let (name, id) = self.world.scene_ref(Some(&scene));
                out.push(ev(
                    sub::SCENES,
                    "CurrentPreviewSceneChanged",
                    json!({ "sceneName": name, "sceneUuid": id }),
                ));
                out
            }
            Event::PreviewCleared => {
                self.world.preview = None;
                if !self.studio {
                    return Vec::new();
                }
                self.studio = false;
                vec![ev(
                    sub::UI,
                    "StudioModeStateChanged",
                    json!({ "studioModeEnabled": false }),
                )]
            }
            Event::Started {
                files,
                cause,
                manifest,
                ..
            } => {
                let path = program_in(manifest.as_deref())
                    .or_else(|| files.last().map(|f| f.to_string_lossy().into_owned()));
                let split = cause == Some(TakeCause::Split) && self.recording;
                self.record_path = path.clone().or(self.record_path.take());
                if split {
                    return vec![ev(
                        sub::OUTPUTS,
                        "RecordFileChanged",
                        json!({ "newOutputPath": path }),
                    )];
                }
                if self.recording && cause == Some(TakeCause::Retake) {
                    return vec![ev(
                        sub::OUTPUTS,
                        "RecordFileChanged",
                        json!({ "newOutputPath": path }),
                    )];
                }
                self.recording = true;
                vec![
                    record(false, "STARTING", None),
                    record(true, "STARTED", path),
                ]
            }
            Event::Stopped { .. } => {
                if !self.recording {
                    return Vec::new();
                }
                self.recording = false;
                vec![
                    record(true, "STOPPING", None),
                    record(false, "STOPPED", self.record_path.clone()),
                ]
            }
            Event::RecordingPaused { recording, .. } => vec![record(
                true,
                if recording.paused {
                    "PAUSED"
                } else {
                    "RESUMED"
                },
                None,
            )],
            // OBS has one stream: the main destination's (#73).
            Event::Stream { stream } if stream.id != isocast_protocol::stream::MAIN_DESTINATION => {
                Vec::new()
            }
            Event::Stream { stream } => {
                let before = std::mem::replace(&mut self.stream, stream.state);
                if before == stream.state {
                    return Vec::new();
                }
                stream_event(before, stream.state).into_iter().collect()
            }
            Event::VirtualCamera { camera } => {
                let before = std::mem::replace(&mut self.camera, camera.state);
                if before == camera.state {
                    return Vec::new();
                }
                camera_event(camera.state).into_iter().collect()
            }
            Event::Replay { replay } => {
                let before = std::mem::replace(&mut self.replay, replay.state);
                if before == replay.state {
                    return Vec::new();
                }
                vec![replay_event(replay.state)]
            }
            Event::ReplaySaved { saved } => vec![ev(
                sub::OUTPUTS,
                "ReplayBufferSaved",
                json!({ "savedReplayPath": saved.path }),
            )],
            Event::LibraryChanged { action, kind, .. } => {
                if action != LibraryAction::Switched {
                    return Vec::new();
                }
                let Ok(live) = ctx.live() else {
                    return Vec::new();
                };
                let mut out = Vec::new();
                if kind != Some(LibraryKind::Profile)
                    && let Some(collection) = &live.collection
                {
                    out.push(ev(
                        sub::CONFIG,
                        "CurrentSceneCollectionChanged",
                        json!({ "sceneCollectionName": collection.name }),
                    ));
                }
                if kind != Some(LibraryKind::Collection)
                    && let Some(profile) = &live.profile
                {
                    out.push(ev(
                        sub::CONFIG,
                        "CurrentProfileChanged",
                        json!({ "profileName": profile.name }),
                    ));
                }
                out.extend(self.setup_changed(ctx));
                out
            }
            Event::Levels { levels } => {
                let inputs: Vec<Value> = levels
                    .iter()
                    .filter_map(|level| {
                        let source = self.world.source(&level.source)?;
                        let channels: Vec<Value> = match &level.channels {
                            Some(channels) => channels
                                .iter()
                                .map(|c| meter(c.rms_db, c.peak_db))
                                .collect(),
                            None => vec![meter(level.rms_db, level.peak_db)],
                        };
                        Some(json!({
                            "inputName": self.world.input_name(&source.id),
                            "inputUuid": uuid("input", &source.id),
                            "inputLevelsMul": channels,
                        }))
                    })
                    .collect();
                vec![ev(
                    sub::INPUT_VOLUME_METERS,
                    "InputVolumeMeters",
                    json!({ "inputs": inputs }),
                )]
            }
            _ => Vec::new(),
        }
    }

    /// Compare the live setup with the one the session last saw.
    fn setup_changed(&mut self, ctx: &Ctx) -> Vec<ObsEvent> {
        let Ok(live) = ctx.live() else {
            return Vec::new();
        };
        if live.revision == self.world.revision && live.setup == self.world.setup {
            return Vec::new();
        }
        let mut new = World::new(live, None);
        new.program = self.world.program.take();
        new.preview = self.world.preview.take();
        let old = std::mem::replace(&mut self.world, new);
        diff(&old, &self.world)
    }
}

/// The program file a take's manifest lists, if it can be read.
fn program_in(manifest: Option<&std::path::Path>) -> Option<String> {
    let manifest = manifest?;
    let value: Value = serde_json::from_slice(&std::fs::read(manifest).ok()?).ok()?;
    let file = value["files"]
        .as_array()?
        .iter()
        .find(|f| f["source"] == isocast_protocol::PROGRAM)?["file"]
        .as_str()?;
    Some(manifest.with_file_name(file).to_string_lossy().into_owned())
}

fn meter(rms_db: f64, peak_db: f64) -> Value {
    let mag = db_to_mul(rms_db.max(-100.0));
    let peak = db_to_mul(peak_db.max(-100.0));
    json!([mag, peak, peak])
}

fn record(active: bool, state: &str, path: Option<String>) -> ObsEvent {
    ev(
        sub::OUTPUTS,
        "RecordStateChanged",
        json!({
            "outputActive": active,
            "outputState": format!("OBS_WEBSOCKET_OUTPUT_{state}"),
            "outputPath": path,
        }),
    )
}

fn output(kind: &'static str, active: bool, state: &str) -> ObsEvent {
    ev(
        sub::OUTPUTS,
        kind,
        json!({
            "outputActive": active,
            "outputState": format!("OBS_WEBSOCKET_OUTPUT_{state}"),
        }),
    )
}

fn stream_event(before: StreamState, after: StreamState) -> Option<ObsEvent> {
    let (active, state) = match after {
        StreamState::Connecting => (false, "STARTING"),
        StreamState::Live if before == StreamState::Reconnecting => (true, "RECONNECTED"),
        StreamState::Live => (true, "STARTED"),
        StreamState::Reconnecting => (true, "RECONNECTING"),
        StreamState::Offline | StreamState::Failed => {
            if matches!(before, StreamState::Offline | StreamState::Failed) {
                return None;
            }
            (false, "STOPPED")
        }
    };
    Some(output("StreamStateChanged", active, state))
}

fn camera_event(state: VirtualCameraState) -> Option<ObsEvent> {
    let (active, state) = match state {
        VirtualCameraState::Starting => (false, "STARTING"),
        VirtualCameraState::Running => (true, "STARTED"),
        VirtualCameraState::Stopping => (true, "STOPPING"),
        VirtualCameraState::Stopped | VirtualCameraState::Failed => (false, "STOPPED"),
    };
    Some(output("VirtualcamStateChanged", active, state))
}

fn replay_event(state: ReplayState) -> ObsEvent {
    match state {
        ReplayState::Running => output("ReplayBufferStateChanged", true, "STARTED"),
        _ => output("ReplayBufferStateChanged", false, "STOPPED"),
    }
}

/// The OBS events between two setups.
fn diff(old: &World, new: &World) -> Vec<ObsEvent> {
    let mut out = Vec::new();
    // Scenes.
    let old_scenes: HashSet<&str> = old.scenes().iter().map(|s| s.id.as_str()).collect();
    let new_scenes: HashSet<&str> = new.scenes().iter().map(|s| s.id.as_str()).collect();
    let mut list_changed = false;
    for scene in old.scenes() {
        if !new_scenes.contains(scene.id.as_str()) {
            list_changed = true;
            out.push(ev(
                sub::SCENES,
                "SceneRemoved",
                json!({ "sceneName": old.scene_name(&scene.id), "sceneUuid": uuid("scene", &scene.id), "isGroup": false }),
            ));
        }
    }
    for scene in new.scenes() {
        if !old_scenes.contains(scene.id.as_str()) {
            list_changed = true;
            out.push(ev(
                sub::SCENES,
                "SceneCreated",
                json!({ "sceneName": new.scene_name(&scene.id), "sceneUuid": uuid("scene", &scene.id), "isGroup": false }),
            ));
        } else if old.scene_name(&scene.id) != new.scene_name(&scene.id) {
            list_changed = true;
            out.push(ev(
                sub::SCENES,
                "SceneNameChanged",
                json!({ "sceneUuid": uuid("scene", &scene.id), "oldSceneName": old.scene_name(&scene.id), "sceneName": new.scene_name(&scene.id) }),
            ));
        }
    }
    let order = |w: &World| w.scenes().iter().map(|s| s.id.clone()).collect::<Vec<_>>();
    if list_changed || order(old) != order(new) {
        out.push(ev(
            sub::SCENES,
            "SceneListChanged",
            json!({ "scenes": new.scene_list() }),
        ));
    }

    // Inputs.
    for source in old.sources() {
        if new.source(&source.id).is_none() {
            out.push(ev(
                sub::INPUTS,
                "InputRemoved",
                json!({ "inputName": old.input_name(&source.id), "inputUuid": uuid("input", &source.id) }),
            ));
        }
    }
    for source in new.sources() {
        let name = new.input_name(&source.id);
        let id = uuid("input", &source.id);
        let Some(before) = old.source(&source.id) else {
            let mut data = new.input_entry(source);
            data["inputSettings"] = settings_of(source);
            data["defaultInputSettings"] = json!({});
            out.push(ev(sub::INPUTS, "InputCreated", data));
            continue;
        };
        let old_name = old.input_name(&source.id);
        if old_name != name {
            out.push(ev(
                sub::INPUTS,
                "InputNameChanged",
                json!({ "inputUuid": id, "oldInputName": old_name, "inputName": name }),
            ));
        }
        let (a, b) = (&before.audio, &source.audio);
        if a.muted != b.muted {
            out.push(ev(
                sub::INPUTS,
                "InputMuteStateChanged",
                json!({ "inputName": name, "inputUuid": id, "inputMuted": b.muted }),
            ));
        }
        if a.gain_db != b.gain_db {
            out.push(ev(
                sub::INPUTS,
                "InputVolumeChanged",
                json!({ "inputName": name, "inputUuid": id, "inputVolumeMul": db_to_mul(b.gain_db), "inputVolumeDb": b.gain_db }),
            ));
        }
        if a.balance != b.balance {
            out.push(ev(
                sub::INPUTS,
                "InputAudioBalanceChanged",
                json!({ "inputName": name, "inputUuid": id, "inputAudioBalance": (b.balance + 1.0) / 2.0 }),
            ));
        }
        if a.offset_ms != b.offset_ms {
            out.push(ev(
                sub::INPUTS,
                "InputAudioSyncOffsetChanged",
                json!({ "inputName": name, "inputUuid": id, "inputAudioSyncOffset": b.offset_ms }),
            ));
        }
        if a.monitor != b.monitor {
            out.push(ev(
                sub::INPUTS,
                "InputAudioMonitorTypeChanged",
                json!({ "inputName": name, "inputUuid": id, "monitorType": monitor_type(b.monitor) }),
            ));
        }
        let settings = settings_of(source);
        if settings_of(before) != settings {
            out.push(ev(
                sub::INPUTS,
                "InputSettingsChanged",
                json!({ "inputName": name, "inputUuid": id, "inputSettings": settings }),
            ));
        }
    }

    // Scene items, in scenes both setups have.
    for scene in new.scenes() {
        let Some(before) = old.scene(&scene.id) else {
            continue;
        };
        let (scene_name, scene_uuid) = (new.scene_name(&scene.id), uuid("scene", &scene.id));
        let ids = |s: &isocast_protocol::Scene| -> Vec<i64> {
            s.items
                .iter()
                .enumerate()
                .map(|(i, item)| world::item_id(&s.id, item, i))
                .collect()
        };
        let (old_ids, new_ids) = (ids(before), ids(scene));
        let old_items: HashMap<i64, (usize, &isocast_protocol::SceneItem)> = old_ids
            .iter()
            .zip(before.items.iter().enumerate())
            .map(|(id, item)| (*id, item))
            .collect();
        for (id, (_, item)) in &old_items {
            if !new_ids.contains(id) {
                let (source_name, source_uuid, _, _) = old.item_source(item);
                out.push(ev(
                    sub::SCENE_ITEMS,
                    "SceneItemRemoved",
                    json!({ "sceneName": scene_name, "sceneUuid": scene_uuid, "sourceName": source_name, "sourceUuid": source_uuid, "sceneItemId": id }),
                ));
            }
        }
        for (index, (id, item)) in new_ids.iter().zip(scene.items.iter()).enumerate() {
            let Some((_, was)) = old_items.get(id) else {
                let (source_name, source_uuid, _, _) = new.item_source(item);
                out.push(ev(
                    sub::SCENE_ITEMS,
                    "SceneItemCreated",
                    json!({ "sceneName": scene_name, "sceneUuid": scene_uuid, "sourceName": source_name, "sourceUuid": source_uuid, "sceneItemId": id, "sceneItemIndex": index }),
                ));
                continue;
            };
            if was.visible != item.visible {
                out.push(ev(
                    sub::SCENE_ITEMS,
                    "SceneItemEnableStateChanged",
                    json!({ "sceneName": scene_name, "sceneUuid": scene_uuid, "sceneItemId": id, "sceneItemEnabled": item.visible }),
                ));
            }
            if was.locked != item.locked {
                out.push(ev(
                    sub::SCENE_ITEMS,
                    "SceneItemLockStateChanged",
                    json!({ "sceneName": scene_name, "sceneUuid": scene_uuid, "sceneItemId": id, "sceneItemLocked": item.locked }),
                ));
            }
        }
        let kept_old: Vec<i64> = old_ids
            .iter()
            .copied()
            .filter(|id| new_ids.contains(id))
            .collect();
        let kept_new: Vec<i64> = new_ids
            .iter()
            .copied()
            .filter(|id| old_ids.contains(id))
            .collect();
        if kept_old != kept_new {
            let items: Vec<Value> = new_ids
                .iter()
                .enumerate()
                .map(|(i, id)| json!({ "sceneItemId": id, "sceneItemIndex": i }))
                .collect();
            out.push(ev(
                sub::SCENE_ITEMS,
                "SceneItemListReindexed",
                json!({ "sceneName": scene_name, "sceneUuid": scene_uuid, "sceneItems": items }),
            ));
        }
    }

    // The transition.
    if let (Some(a), Some(b)) = (&old.setup, &new.setup) {
        let (old_name, _) = transition_name(&a.transition);
        let (new_name, _) = transition_name(&b.transition);
        if old_name != new_name {
            out.push(ev(
                sub::TRANSITIONS,
                "CurrentSceneTransitionChanged",
                json!({ "transitionName": new_name, "transitionUuid": uuid("transition", new_name) }),
            ));
        }
        if a.transition.duration_ms != b.transition.duration_ms {
            out.push(ev(
                sub::TRANSITIONS,
                "CurrentSceneTransitionDurationChanged",
                json!({ "transitionDuration": b.transition.duration_ms }),
            ));
        }
    }
    out
}
