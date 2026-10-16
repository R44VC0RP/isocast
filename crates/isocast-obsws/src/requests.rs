//! obs-websocket requests, each answered through the method table.

use std::sync::atomic::Ordering;

use isocast_protocol::api::{
    ApplyParams, EngineState, Live, Method, RpcError, SnapshotParams, StartParams, Status,
};
use isocast_protocol::change::Change;
use isocast_protocol::library::{Library, LibrarySwitchParams};
use isocast_protocol::replay::ReplayState;
use isocast_protocol::stream::StreamState;
use isocast_protocol::virtual_camera::VirtualCameraState;
use isocast_protocol::{Scene, SceneItem, Source};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::session::Stats;
use crate::world::{self, World, uuid};
use crate::{OBS_VERSION, OBS_WEBSOCKET_VERSION, RPC_VERSION, Shared};

/// The requests this server maps, in the protocol's order. Every other
/// request answers `UnknownRequestType` (204).
pub const SUPPORTED: &[&str] = &[
    // General
    "GetVersion",
    "GetStats",
    "BroadcastCustomEvent",
    "Sleep",
    // Config
    "GetSceneCollectionList",
    "SetCurrentSceneCollection",
    "GetProfileList",
    "SetCurrentProfile",
    "GetVideoSettings",
    "GetRecordDirectory",
    // Scenes
    "GetSceneList",
    "GetGroupList",
    "GetCurrentProgramScene",
    "SetCurrentProgramScene",
    "GetCurrentPreviewScene",
    "SetCurrentPreviewScene",
    "SetSceneName",
    "GetSceneSceneTransitionOverride",
    // Inputs
    "GetInputList",
    "GetInputKindList",
    "GetSpecialInputs",
    "SetInputName",
    "GetInputSettings",
    "SetInputSettings",
    "GetInputMute",
    "SetInputMute",
    "ToggleInputMute",
    "GetInputVolume",
    "SetInputVolume",
    "GetInputAudioBalance",
    "SetInputAudioBalance",
    "GetInputAudioSyncOffset",
    "SetInputAudioSyncOffset",
    "GetInputAudioMonitorType",
    "SetInputAudioMonitorType",
    // Transitions
    "GetTransitionKindList",
    "GetSceneTransitionList",
    "GetCurrentSceneTransition",
    "SetCurrentSceneTransition",
    "SetCurrentSceneTransitionDuration",
    "TriggerStudioModeTransition",
    // Scene items
    "GetSceneItemList",
    "GetSceneItemId",
    "GetSceneItemSource",
    "GetSceneItemTransform",
    "GetSceneItemEnabled",
    "SetSceneItemEnabled",
    "GetSceneItemLocked",
    "SetSceneItemLocked",
    "GetSceneItemIndex",
    // Sources
    "GetSourceActive",
    "GetSourceScreenshot",
    // Outputs
    "GetVirtualCamStatus",
    "ToggleVirtualCam",
    "StartVirtualCam",
    "StopVirtualCam",
    "GetReplayBufferStatus",
    "ToggleReplayBuffer",
    "StartReplayBuffer",
    "StopReplayBuffer",
    "SaveReplayBuffer",
    "GetLastReplayBufferReplay",
    // Stream
    "GetStreamStatus",
    "ToggleStream",
    "StartStream",
    "StopStream",
    // Record
    "GetRecordStatus",
    "ToggleRecord",
    "StartRecord",
    "StopRecord",
    "ToggleRecordPause",
    "PauseRecord",
    "ResumeRecord",
    "SplitRecordFile",
    "CreateRecordChapter",
    // UI
    "GetStudioModeEnabled",
    "SetStudioModeEnabled",
];

/// obs-websocket's RequestStatus codes this server answers with.
pub mod code {
    pub const SUCCESS: u16 = 100;
    pub const MISSING_REQUEST_TYPE: u16 = 203;
    pub const UNKNOWN_REQUEST_TYPE: u16 = 204;
    pub const UNSUPPORTED_REQUEST_BATCH_EXECUTION_TYPE: u16 = 206;
    pub const MISSING_REQUEST_FIELD: u16 = 300;
    pub const INVALID_REQUEST_FIELD: u16 = 400;
    pub const INVALID_REQUEST_FIELD_TYPE: u16 = 401;
    pub const REQUEST_FIELD_OUT_OF_RANGE: u16 = 402;
    pub const REQUEST_FIELD_EMPTY: u16 = 403;
    pub const OUTPUT_RUNNING: u16 = 500;
    pub const OUTPUT_NOT_RUNNING: u16 = 501;
    pub const OUTPUT_PAUSED: u16 = 502;
    pub const OUTPUT_NOT_PAUSED: u16 = 503;
    pub const STUDIO_MODE_NOT_ACTIVE: u16 = 506;
    pub const RESOURCE_NOT_FOUND: u16 = 600;
    pub const INVALID_RESOURCE_STATE: u16 = 604;
    pub const RESOURCE_NOT_CONFIGURABLE: u16 = 606;
    pub const REQUEST_PROCESSING_FAILED: u16 = 702;
    pub const CANNOT_ACT: u16 = 703;
}

/// A request that failed: its RequestStatus code and comment.
#[derive(Debug)]
pub struct Fail {
    pub code: u16,
    pub comment: String,
}

impl Fail {
    pub fn new(code: u16, comment: impl Into<String>) -> Self {
        Self {
            code,
            comment: comment.into(),
        }
    }
}

impl From<RpcError> for Fail {
    fn from(error: RpcError) -> Self {
        let code = match error.code {
            RpcError::INVALID_PARAMS => code::INVALID_REQUEST_FIELD,
            RpcError::WRONG_STATE => code::INVALID_RESOURCE_STATE,
            RpcError::NOT_PERMITTED => code::CANNOT_ACT,
            _ => code::REQUEST_PROCESSING_FAILED,
        };
        Fail::new(code, error.message)
    }
}

type Answer = Result<Value, Fail>;

/// One client's view of the engine while answering its requests.
pub struct Ctx<'a> {
    pub shared: &'a Shared,
    pub peer: &'a str,
    pub stats: &'a Stats,
}

impl Ctx<'_> {
    fn call<T: DeserializeOwned>(&self, method: Method) -> Result<T, Fail> {
        let value = self.shared.engine.call(method, self.peer)?;
        serde_json::from_value(value).map_err(|err| {
            Fail::new(
                code::REQUEST_PROCESSING_FAILED,
                format!("isocastd's reply didn't read: {err}"),
            )
        })
    }

    fn act(&self, method: Method) -> Result<Value, Fail> {
        Ok(self.shared.engine.call(method, self.peer)?)
    }

    pub fn status(&self) -> Result<Status, Fail> {
        self.call(Method::Status)
    }

    pub fn live(&self) -> Result<Live, Fail> {
        self.call(Method::Get(None))
    }

    pub fn world(&self) -> Result<World, Fail> {
        let live = self.live()?;
        let status = self.status()?;
        Ok(World::new(live, Some(&status)))
    }

    /// Apply a merge patch to the live setup, planning it again if another
    /// client changed the setup in between.
    fn patch(&self, make: impl Fn(&World) -> Result<Value, Fail>, live: bool) -> Result<(), Fail> {
        let mut last = None;
        for _ in 0..3 {
            let world = World::new(self.live()?, None);
            if world.setup.is_none() {
                return Err(no_setup());
            }
            let patch = make(&world)?;
            let method = Method::Apply(Box::new(ApplyParams {
                revision: world.revision,
                change: Change::Patch(patch),
                live,
            }));
            match self.shared.engine.call(method, self.peer) {
                Ok(_) => return Ok(()),
                Err(err) if err.message.contains("plan the change again") => last = Some(err),
                Err(err) => return Err(err.into()),
            }
        }
        Err(last.map_or_else(no_setup, Fail::from))
    }
}

fn no_setup() -> Fail {
    Fail::new(
        code::INVALID_RESOURCE_STATE,
        "Isocast has no setup yet; create one (isocast init) first",
    )
}

/// The request's fields.
struct Data<'a>(&'a Value);

impl<'a> Data<'a> {
    fn get(&self, key: &str) -> Option<&'a Value> {
        self.0.get(key).filter(|v| !v.is_null())
    }

    fn str(&self, key: &str) -> Result<Option<&'a str>, Fail> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::String(s)) if s.is_empty() => Err(Fail::new(
                code::REQUEST_FIELD_EMPTY,
                format!("The field value of `{key}` cannot be empty."),
            )),
            Some(Value::String(s)) => Ok(Some(s)),
            Some(_) => Err(Fail::new(
                code::INVALID_REQUEST_FIELD_TYPE,
                format!("The field value of `{key}` must be a string."),
            )),
        }
    }

    fn need_str(&self, key: &str) -> Result<&'a str, Fail> {
        self.str(key)?.ok_or_else(|| missing(key))
    }

    fn bool(&self, key: &str) -> Result<Option<bool>, Fail> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(_) => Err(Fail::new(
                code::INVALID_REQUEST_FIELD_TYPE,
                format!("The field value of `{key}` must be boolean."),
            )),
        }
    }

    fn need_bool(&self, key: &str) -> Result<bool, Fail> {
        self.bool(key)?.ok_or_else(|| missing(key))
    }

    fn number(&self, key: &str, min: f64, max: f64) -> Result<Option<f64>, Fail> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Number(n)) => {
                let n = n.as_f64().unwrap_or(f64::NAN);
                if !(min..=max).contains(&n) {
                    return Err(Fail::new(
                        code::REQUEST_FIELD_OUT_OF_RANGE,
                        format!("The field value of `{key}` is outside {min} to {max}."),
                    ));
                }
                Ok(Some(n))
            }
            Some(_) => Err(Fail::new(
                code::INVALID_REQUEST_FIELD_TYPE,
                format!("The field value of `{key}` must be a number."),
            )),
        }
    }

    fn need_number(&self, key: &str, min: f64, max: f64) -> Result<f64, Fail> {
        self.number(key, min, max)?.ok_or_else(|| missing(key))
    }
}

fn missing(key: &str) -> Fail {
    Fail::new(
        code::MISSING_REQUEST_FIELD,
        format!("Your request is missing the `{key}` field."),
    )
}

fn scene<'w>(world: &'w World, data: &Data) -> Result<&'w Scene, Fail> {
    let name = data.str("sceneName")?;
    let id = data.str("sceneUuid")?;
    if name.is_none() && id.is_none() {
        return Err(missing("sceneName` or `sceneUuid"));
    }
    world.find_scene(name, id).ok_or_else(|| {
        Fail::new(
            code::RESOURCE_NOT_FOUND,
            format!(
                "No scene was found by the {} `{}`. Scenes: {}.",
                if id.is_some() { "uuid" } else { "name" },
                id.or(name).unwrap_or_default(),
                names(world.scenes().iter().map(|s| world.scene_name(&s.id)))
            ),
        )
    })
}

fn input<'w>(world: &'w World, data: &Data) -> Result<&'w Source, Fail> {
    let name = data.str("inputName")?;
    let id = data.str("inputUuid")?;
    if name.is_none() && id.is_none() {
        return Err(missing("inputName` or `inputUuid"));
    }
    world.find_input(name, id).ok_or_else(|| {
        Fail::new(
            code::RESOURCE_NOT_FOUND,
            format!(
                "No input was found by the {} `{}`. Inputs: {}.",
                if id.is_some() { "uuid" } else { "name" },
                id.or(name).unwrap_or_default(),
                names(world.sources().iter().map(|s| world.input_name(&s.id)))
            ),
        )
    })
}

/// An input with sound, for the audio requests.
fn audio_input<'w>(world: &'w World, data: &Data) -> Result<&'w Source, Fail> {
    let source = input(world, data)?;
    if !world::has_audio(source) {
        return Err(Fail::new(
            code::INVALID_RESOURCE_STATE,
            format!(
                "The specified input does not support audio: `{}` is a {} source.",
                world.input_name(&source.id),
                source.kind.label()
            ),
        ));
    }
    Ok(source)
}

fn item<'w>(world: &World, scene: &'w Scene, data: &Data) -> Result<(usize, &'w SceneItem), Fail> {
    let id = data.need_number("sceneItemId", 0.0, f64::from(i32::MAX))? as i64;
    world.find_item(scene, id).ok_or_else(|| {
        Fail::new(
            code::RESOURCE_NOT_FOUND,
            format!(
                "No scene items were found in scene `{}` with the id `{id}`.",
                world.scene_name(&scene.id)
            ),
        )
    })
}

fn names(all: impl Iterator<Item = String>) -> String {
    let all: Vec<String> = all.map(|n| format!("`{n}`")).collect();
    if all.is_empty() {
        "none".into()
    } else {
        all.join(", ")
    }
}

/// Patch one field of a source's `audio`.
fn audio_patch(id: &str, field: &str, value: Value) -> Value {
    json!({ "sources": [{ "id": id, "audio": { field: value } }] })
}

/// Patch fields of the top-level item at `index` of `scene`: just that
/// item when every item of the scene can be told apart by its key (lists
/// of keyed objects merge item by item), else the whole list.
fn item_patch(world: &World, scene_id: &str, index: usize, fields: Value) -> Result<Value, Fail> {
    let scene = world
        .scene(scene_id)
        .ok_or_else(|| Fail::new(code::RESOURCE_NOT_FOUND, "the scene went away"))?;
    let keyed = scene.items.iter().all(|item| item.key().is_some());
    let items = if keyed {
        let item = &scene.items[index];
        let mut patch = Map::new();
        if let Some(id) = &item.id {
            patch.insert("id".into(), json!(id));
        } else if let Some(source) = &item.source {
            patch.insert("source".into(), json!(source));
        } else if let Some(nested) = &item.scene {
            patch.insert("scene".into(), json!(nested));
        }
        patch.extend(fields.as_object().cloned().unwrap_or_default());
        vec![Value::Object(patch)]
    } else {
        let mut all: Vec<Value> = scene
            .items
            .iter()
            .map(|item| serde_json::to_value(item).unwrap_or_default())
            .collect();
        if let (Some(target), Some(fields)) = (all[index].as_object_mut(), fields.as_object()) {
            target.extend(fields.clone());
        }
        all
    };
    Ok(json!({ "scenes": [{ "id": scene_id, "items": items }] }))
}

fn recording(status: &Status) -> bool {
    status.state != EngineState::Idle
}

/// Whole milliseconds: OBS's durations are integers, and typed clients
/// refuse `2326.0`.
fn millis(seconds: f64) -> u64 {
    (seconds.max(0.0) * 1000.0).round() as u64
}

fn timecode(seconds: f64) -> String {
    let ms = (seconds.max(0.0) * 1000.0).round() as u64;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

pub fn db_to_mul(db: f64) -> f64 {
    if db <= -100.0 {
        0.0
    } else {
        10f64.powf(db / 20.0)
    }
}

fn mul_to_db(mul: f64) -> f64 {
    if mul <= 0.0 {
        -100.0
    } else {
        20.0 * mul.log10()
    }
}

/// OBS's name and kind for the setup's transition.
pub fn transition_name(transition: &isocast_protocol::Transition) -> (&'static str, &'static str) {
    let kind = serde_json::to_value(transition)
        .ok()
        .and_then(|v| v.get("kind").and_then(Value::as_str).map(String::from))
        .unwrap_or_else(|| "cut".into());
    if transition.is_cut() {
        return ("Cut", "cut_transition");
    }
    match kind.as_str() {
        "fade" => ("Fade", "fade_transition"),
        "swipe" => ("Swipe", "swipe_transition"),
        "slide" => ("Slide", "slide_transition"),
        "fade_to_color" => ("Fade to Color", "fade_to_color_transition"),
        "luma_wipe" => ("Luma Wipe", "wipe_transition"),
        "stinger" => ("Stinger", "obs_stinger_transition"),
        _ => ("Cut", "cut_transition"),
    }
}

fn transition_entry(name: &str, kind: &str) -> Value {
    json!({
        "transitionName": name,
        "transitionUuid": uuid("transition", name),
        "transitionKind": kind,
        "transitionFixed": kind == "cut_transition",
        "transitionConfigurable": false,
    })
}

/// Answer one request: its responseData (`Null` for none) or why it
/// failed.
pub fn handle(ctx: &Ctx, request_type: &str, data: &Value) -> Answer {
    let d = Data(data);
    match request_type {
        // General
        "GetVersion" => Ok(json!({
            "obsVersion": OBS_VERSION,
            "obsWebSocketVersion": OBS_WEBSOCKET_VERSION,
            "rpcVersion": RPC_VERSION,
            "availableRequests": SUPPORTED,
            "supportedImageFormats": ["png"],
            "platform": std::env::consts::OS,
            "platformDescription": format!("Isocast {} (obs-websocket compatibility)", ctx.shared.isocast_version),
        })),
        "GetStats" => {
            let status = ctx.status()?;
            let world = World::new(ctx.live()?, Some(&status));
            let files = status
                .live
                .as_ref()
                .map(|l| l.files.as_slice())
                .unwrap_or_default();
            let program = files.iter().find(|f| f.source == isocast_protocol::PROGRAM);
            let skipped: u64 = files.iter().map(|f| f.dropped_frames).sum();
            Ok(json!({
                "cpuUsage": status.cpu_percent.unwrap_or(0.0),
                "memoryUsage": 0.0,
                "availableDiskSpace": status.recording_free_bytes.map_or(0.0, |b| b as f64 / 1048576.0),
                "activeFps": world.setup.as_ref().map_or(0, |s| s.canvas.fps),
                "averageFrameRenderTime": 0.0,
                "renderSkippedFrames": 0,
                "renderTotalFrames": program.map_or(0, |f| f.frames),
                "outputSkippedFrames": skipped,
                "outputTotalFrames": files.iter().map(|f| f.frames).sum::<u64>(),
                "webSocketSessionIncomingMessages": ctx.stats.incoming.load(Ordering::Relaxed),
                "webSocketSessionOutgoingMessages": ctx.stats.outgoing.load(Ordering::Relaxed),
            }))
        }
        "BroadcastCustomEvent" => {
            let event = d.get("eventData").ok_or_else(|| missing("eventData"))?;
            if !event.is_object() {
                return Err(Fail::new(
                    code::INVALID_REQUEST_FIELD_TYPE,
                    "The field value of `eventData` must be an object.",
                ));
            }
            crate::session::broadcast(ctx.shared, event.clone());
            Ok(Value::Null)
        }
        "Sleep" => {
            let millis = d.number("sleepMillis", 0.0, 50000.0)?;
            let frames = d.number("sleepFrames", 0.0, 10000.0)?;
            let ms = match (millis, frames) {
                (Some(ms), _) => ms,
                (None, Some(frames)) => {
                    let fps = World::new(ctx.live()?, None)
                        .setup
                        .map_or(60, |s| s.canvas.fps.max(1));
                    frames * 1000.0 / f64::from(fps)
                }
                (None, None) => return Err(missing("sleepMillis")),
            };
            std::thread::sleep(std::time::Duration::from_millis(ms as u64));
            Ok(Value::Null)
        }

        // Config
        "GetSceneCollectionList" => {
            let library: Library = ctx.call(Method::Library)?;
            let current = library.collections.iter().find(|c| c.current);
            Ok(json!({
                "currentSceneCollectionName": current.map(|c| c.name.clone()),
                "sceneCollections": library.collections.iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
            }))
        }
        "SetCurrentSceneCollection" => {
            let name = d.need_str("sceneCollectionName")?;
            let library: Library = ctx.call(Method::Library)?;
            let entry = library
                .collections
                .iter()
                .find(|c| c.name == name || c.id == name)
                .ok_or_else(|| {
                    Fail::new(
                        code::RESOURCE_NOT_FOUND,
                        format!(
                            "No scene collection was found by the name `{name}`. Collections: {}.",
                            names(library.collections.iter().map(|c| c.name.clone()))
                        ),
                    )
                })?;
            ctx.act(Method::LibrarySwitch(LibrarySwitchParams {
                collection: Some(entry.id.clone()),
                profile: None,
            }))?;
            Ok(Value::Null)
        }
        "GetProfileList" => {
            let library: Library = ctx.call(Method::Library)?;
            let current = library.profiles.iter().find(|c| c.current);
            Ok(json!({
                "currentProfileName": current.map(|c| c.name.clone()),
                "profiles": library.profiles.iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
            }))
        }
        "SetCurrentProfile" => {
            let name = d.need_str("profileName")?;
            let library: Library = ctx.call(Method::Library)?;
            let entry = library
                .profiles
                .iter()
                .find(|c| c.name == name || c.id == name)
                .ok_or_else(|| {
                    Fail::new(
                        code::RESOURCE_NOT_FOUND,
                        format!(
                            "No profile was found by the name `{name}`. Profiles: {}.",
                            names(library.profiles.iter().map(|c| c.name.clone()))
                        ),
                    )
                })?;
            ctx.act(Method::LibrarySwitch(LibrarySwitchParams {
                collection: None,
                profile: Some(entry.id.clone()),
            }))?;
            Ok(Value::Null)
        }
        "GetVideoSettings" => {
            let world = World::new(ctx.live()?, None);
            let setup = world.setup.as_ref().ok_or_else(no_setup)?;
            Ok(json!({
                "fpsNumerator": setup.canvas.fps,
                "fpsDenominator": 1,
                "baseWidth": setup.canvas.width,
                "baseHeight": setup.canvas.height,
                "outputWidth": setup.canvas.width,
                "outputHeight": setup.canvas.height,
            }))
        }
        "GetRecordDirectory" => {
            let world = World::new(ctx.live()?, None);
            let setup = world.setup.as_ref().ok_or_else(no_setup)?;
            Ok(json!({ "recordDirectory": setup.recording.folder }))
        }

        // Scenes
        "GetSceneList" => {
            let world = ctx.world()?;
            let (program_name, program_uuid) = world.scene_ref(world.program.as_deref());
            let (preview_name, preview_uuid) = world.scene_ref(world.preview.as_deref());
            Ok(json!({
                "currentProgramSceneName": program_name,
                "currentProgramSceneUuid": program_uuid,
                "currentPreviewSceneName": preview_name,
                "currentPreviewSceneUuid": preview_uuid,
                "scenes": world.scene_list(),
            }))
        }
        "GetGroupList" => Ok(json!({ "groups": [] })),
        "GetCurrentProgramScene" => {
            let world = ctx.world()?;
            let Some(program) = world.program.as_deref() else {
                return Err(Fail::new(
                    code::INVALID_RESOURCE_STATE,
                    "There is no program scene yet.",
                ));
            };
            let (name, id) = world.scene_ref(Some(program));
            Ok(json!({
                "sceneName": name, "sceneUuid": id,
                "currentProgramSceneName": name, "currentProgramSceneUuid": id,
            }))
        }
        "SetCurrentProgramScene" => {
            let world = World::new(ctx.live()?, None);
            let scene = scene(&world, &d)?;
            ctx.act(Method::SwitchScene {
                scene: scene.id.clone(),
                transition: None,
            })?;
            Ok(Value::Null)
        }
        "GetCurrentPreviewScene" => {
            let world = ctx.world()?;
            let Some(preview) = world.preview.as_deref() else {
                return Err(Fail::new(
                    code::STUDIO_MODE_NOT_ACTIVE,
                    "Studio mode is not enabled.",
                ));
            };
            let (name, id) = world.scene_ref(Some(preview));
            Ok(json!({
                "sceneName": name, "sceneUuid": id,
                "currentPreviewSceneName": name, "currentPreviewSceneUuid": id,
            }))
        }
        "SetCurrentPreviewScene" => {
            let world = ctx.world()?;
            if world.preview.is_none() {
                return Err(Fail::new(
                    code::STUDIO_MODE_NOT_ACTIVE,
                    "Studio mode is not enabled.",
                ));
            }
            let scene = scene(&world, &d)?;
            ctx.act(Method::Preview {
                scene: scene.id.clone(),
            })?;
            Ok(Value::Null)
        }
        "SetSceneName" => {
            let new = d.need_str("newSceneName")?.to_string();
            let world = World::new(ctx.live()?, None);
            let target = scene(&world, &d)?.id.clone();
            if world
                .scenes()
                .iter()
                .any(|s| s.id != target && world.scene_name(&s.id) == new)
            {
                return Err(Fail::new(
                    601,
                    format!("A scene already exists by the name `{new}`."),
                ));
            }
            ctx.patch(
                |_| Ok(json!({ "scenes": [{ "id": target, "name": new }] })),
                false,
            )?;
            Ok(Value::Null)
        }
        "GetSceneSceneTransitionOverride" => {
            let world = World::new(ctx.live()?, None);
            let scene = scene(&world, &d)?;
            Ok(match &scene.transition {
                Some(t) => json!({
                    "transitionName": transition_name(t).0,
                    "transitionDuration": t.duration_ms,
                }),
                None => json!({ "transitionName": null, "transitionDuration": null }),
            })
        }

        // Inputs
        "GetInputList" => {
            let world = World::new(ctx.live()?, None);
            let kind = d.str("inputKind")?;
            let inputs: Vec<Value> = world
                .sources()
                .iter()
                .filter(|s| kind.is_none_or(|k| world::input_kind(s) == k))
                .map(|s| world.input_entry(s))
                .collect();
            Ok(json!({ "inputs": inputs }))
        }
        "GetInputKindList" => {
            let unversioned = d.bool("unversioned")?.unwrap_or(false);
            let kinds: Vec<&str> = world::INPUT_KINDS
                .iter()
                .map(|k| {
                    if unversioned {
                        world::unversioned(k)
                    } else {
                        k
                    }
                })
                .collect();
            Ok(json!({ "inputKinds": kinds }))
        }
        "GetSpecialInputs" => {
            let world = World::new(ctx.live()?, None);
            let of = |kind: &str| -> Vec<String> {
                world
                    .sources()
                    .iter()
                    .filter(|s| s.kind.label() == kind)
                    .map(|s| world.input_name(&s.id))
                    .collect()
            };
            let desktop = of("system_audio");
            let mics = of("microphone");
            Ok(json!({
                "desktop1": desktop.first(), "desktop2": desktop.get(1),
                "mic1": mics.first(), "mic2": mics.get(1), "mic3": mics.get(2), "mic4": mics.get(3),
            }))
        }
        "SetInputName" => {
            let new = d.need_str("newInputName")?.to_string();
            let world = World::new(ctx.live()?, None);
            let target = input(&world, &d)?.id.clone();
            if world
                .sources()
                .iter()
                .any(|s| s.id != target && world.input_name(&s.id) == new)
            {
                return Err(Fail::new(
                    601,
                    format!("A source already exists by the name `{new}`."),
                ));
            }
            ctx.patch(
                |_| Ok(json!({ "sources": [{ "id": target, "name": new }] })),
                false,
            )?;
            Ok(Value::Null)
        }
        "GetInputSettings" => {
            let world = World::new(ctx.live()?, None);
            let source = input(&world, &d)?;
            Ok(json!({
                "inputSettings": settings_of(source),
                "inputKind": world::input_kind(source),
            }))
        }
        "SetInputSettings" => {
            let settings = d
                .get("inputSettings")
                .ok_or_else(|| missing("inputSettings"))?;
            let Some(settings) = settings.as_object() else {
                return Err(Fail::new(
                    code::INVALID_REQUEST_FIELD_TYPE,
                    "The field value of `inputSettings` must be an object.",
                ));
            };
            let overlay = d.bool("overlay")?.unwrap_or(true);
            let world = World::new(ctx.live()?, None);
            let source = input(&world, &d)?;
            let fields = source.kind.fields();
            if let Some(unknown) = settings.keys().find(|k| !fields.contains(k)) {
                return Err(Fail::new(
                    code::RESOURCE_NOT_CONFIGURABLE,
                    format!(
                        "inputSettings.{unknown}: a {} source has no such setting; Isocast's \
                         settings for it: {}",
                        source.kind.label(),
                        if fields.is_empty() {
                            "none".to_string()
                        } else {
                            fields.join(", ")
                        }
                    ),
                ));
            }
            let id = source.id.clone();
            let mut patch = Map::new();
            patch.insert("id".into(), json!(id));
            if !overlay {
                // Every setting not given goes back to its default.
                for field in fields {
                    patch.insert(field.clone(), Value::Null);
                }
            }
            patch.extend(settings.clone());
            let patch = json!({ "sources": [Value::Object(patch)] });
            ctx.patch(|_| Ok(patch.clone()), false)?;
            Ok(Value::Null)
        }
        "GetInputMute" => {
            let world = World::new(ctx.live()?, None);
            let source = audio_input(&world, &d)?;
            Ok(json!({ "inputMuted": source.audio.muted }))
        }
        "SetInputMute" | "ToggleInputMute" => {
            let given = if request_type == "SetInputMute" {
                Some(d.need_bool("inputMuted")?)
            } else {
                None
            };
            let world = World::new(ctx.live()?, None);
            let id = audio_input(&world, &d)?.id.clone();
            let mut muted = false;
            ctx.patch(
                |world| {
                    let now = world.source(&id).is_some_and(|s| s.audio.muted);
                    let next = given.unwrap_or(!now);
                    Ok(audio_patch(&id, "muted", json!(next)))
                },
                false,
            )?;
            if let Some(source) = World::new(ctx.live()?, None).source(&id) {
                muted = source.audio.muted;
            }
            Ok(if given.is_some() {
                Value::Null
            } else {
                json!({ "inputMuted": muted })
            })
        }
        "GetInputVolume" => {
            let world = World::new(ctx.live()?, None);
            let source = audio_input(&world, &d)?;
            let db = source.audio.gain_db;
            Ok(json!({ "inputVolumeMul": db_to_mul(db), "inputVolumeDb": db }))
        }
        "SetInputVolume" => {
            let mul = d.number("inputVolumeMul", 0.0, 20.0)?;
            let db = d.number("inputVolumeDb", -100.0, 26.0)?;
            let db = match (mul, db) {
                (Some(_), Some(_)) => {
                    return Err(Fail::new(
                        404,
                        "You may only specify one volume field (`inputVolumeMul` or `inputVolumeDb`).",
                    ));
                }
                (Some(mul), None) => mul_to_db(mul),
                (None, Some(db)) => db,
                (None, None) => return Err(missing("inputVolumeMul` or `inputVolumeDb")),
            };
            // Isocast's gain goes from -60 to 24 dB.
            let db = db.clamp(-60.0, 24.0);
            let world = World::new(ctx.live()?, None);
            let id = audio_input(&world, &d)?.id.clone();
            ctx.patch(|_| Ok(audio_patch(&id, "gain_db", json!(db))), true)?;
            Ok(Value::Null)
        }
        "GetInputAudioBalance" => {
            let world = World::new(ctx.live()?, None);
            let source = audio_input(&world, &d)?;
            Ok(json!({ "inputAudioBalance": (source.audio.balance + 1.0) / 2.0 }))
        }
        "SetInputAudioBalance" => {
            let balance = d.need_number("inputAudioBalance", 0.0, 1.0)?;
            let world = World::new(ctx.live()?, None);
            let id = audio_input(&world, &d)?.id.clone();
            ctx.patch(
                |_| Ok(audio_patch(&id, "balance", json!(balance * 2.0 - 1.0))),
                true,
            )?;
            Ok(Value::Null)
        }
        "GetInputAudioSyncOffset" => {
            let world = World::new(ctx.live()?, None);
            let source = audio_input(&world, &d)?;
            Ok(json!({ "inputAudioSyncOffset": source.audio.offset_ms }))
        }
        "SetInputAudioSyncOffset" => {
            // OBS allows -950 to 20000 ms; Isocast -2000 to 2000.
            let offset = d.need_number("inputAudioSyncOffset", -2000.0, 2000.0)?;
            let world = World::new(ctx.live()?, None);
            let id = audio_input(&world, &d)?.id.clone();
            ctx.patch(
                |_| Ok(audio_patch(&id, "offset_ms", json!(offset.round() as i64))),
                false,
            )?;
            Ok(Value::Null)
        }
        "GetInputAudioMonitorType" => {
            let world = World::new(ctx.live()?, None);
            let source = audio_input(&world, &d)?;
            Ok(json!({ "monitorType": monitor_type(source.audio.monitor) }))
        }
        "SetInputAudioMonitorType" => {
            let monitor = match d.need_str("monitorType")? {
                "OBS_MONITORING_TYPE_NONE" => "off",
                "OBS_MONITORING_TYPE_MONITOR_ONLY" => "monitor_only",
                "OBS_MONITORING_TYPE_MONITOR_AND_OUTPUT" => "monitor_and_output",
                other => {
                    return Err(Fail::new(
                        code::INVALID_REQUEST_FIELD,
                        format!(
                            "monitorType: `{other}` is not one of OBS_MONITORING_TYPE_NONE, \
                             OBS_MONITORING_TYPE_MONITOR_ONLY, OBS_MONITORING_TYPE_MONITOR_AND_OUTPUT"
                        ),
                    ));
                }
            };
            let world = World::new(ctx.live()?, None);
            let id = audio_input(&world, &d)?.id.clone();
            ctx.patch(|_| Ok(audio_patch(&id, "monitor", json!(monitor))), false)?;
            Ok(Value::Null)
        }

        // Transitions
        "GetTransitionKindList" => Ok(json!({
            "transitionKinds": ["cut_transition", "fade_transition", "swipe_transition",
                "slide_transition", "fade_to_color_transition", "wipe_transition",
                "obs_stinger_transition"],
        })),
        "GetSceneTransitionList" => {
            let world = World::new(ctx.live()?, None);
            let setup = world.setup.as_ref().ok_or_else(no_setup)?;
            let (name, kind) = transition_name(&setup.transition);
            let mut list = vec![
                transition_entry("Cut", "cut_transition"),
                transition_entry("Fade", "fade_transition"),
            ];
            if name != "Cut" && name != "Fade" {
                list.push(transition_entry(name, kind));
            }
            Ok(json!({
                "currentSceneTransitionName": name,
                "currentSceneTransitionUuid": uuid("transition", name),
                "currentSceneTransitionKind": kind,
                "transitions": list,
            }))
        }
        "GetCurrentSceneTransition" => {
            let world = World::new(ctx.live()?, None);
            let setup = world.setup.as_ref().ok_or_else(no_setup)?;
            let (name, kind) = transition_name(&setup.transition);
            let fixed = kind == "cut_transition";
            Ok(json!({
                "transitionName": name,
                "transitionUuid": uuid("transition", name),
                "transitionKind": kind,
                "transitionFixed": fixed,
                "transitionDuration": if fixed { Value::Null } else { json!(setup.transition.duration_ms) },
                "transitionConfigurable": false,
                "transitionSettings": null,
            }))
        }
        "SetCurrentSceneTransition" => {
            let wanted = d.need_str("transitionName")?.to_string();
            ctx.patch(
                |world| {
                    let setup = world.setup.as_ref().ok_or_else(no_setup)?;
                    let current = transition_name(&setup.transition).0;
                    let old = serde_json::to_value(&setup.transition).unwrap_or_default();
                    let new = match wanted.as_str() {
                        name if name == current => return Ok(json!({})),
                        "Cut" => json!({ "kind": "cut" }),
                        "Fade" => json!({
                            "kind": "fade",
                            "duration_ms": if setup.transition.duration_ms == 0 { 300 } else { setup.transition.duration_ms },
                        }),
                        other => {
                            return Err(Fail::new(
                                code::RESOURCE_NOT_FOUND,
                                format!(
                                    "No transition was found by the name `{other}`. \
                                     Transitions: `Cut`, `Fade`{}.",
                                    if current != "Cut" && current != "Fade" {
                                        format!(", `{current}`")
                                    } else {
                                        String::new()
                                    }
                                ),
                            ));
                        }
                    };
                    // Fields of the old kind go.
                    let mut patch = Map::new();
                    for key in old.as_object().into_iter().flatten().map(|(k, _)| k) {
                        patch.insert(key.clone(), Value::Null);
                    }
                    patch.extend(new.as_object().cloned().unwrap_or_default());
                    Ok(json!({ "transition": patch }))
                },
                false,
            )?;
            Ok(Value::Null)
        }
        "SetCurrentSceneTransitionDuration" => {
            let ms = d.need_number("transitionDuration", 50.0, 20000.0)?;
            let max = f64::from(isocast_protocol::MAX_TRANSITION_MS);
            if ms > max {
                return Err(Fail::new(
                    code::REQUEST_FIELD_OUT_OF_RANGE,
                    format!("transitionDuration: Isocast's transitions take at most {max} ms"),
                ));
            }
            ctx.patch(
                |world| {
                    let setup = world.setup.as_ref().ok_or_else(no_setup)?;
                    let kind = serde_json::to_value(&setup.transition)
                        .ok()
                        .and_then(|v| v.get("kind").cloned())
                        .unwrap_or(json!("cut"));
                    Ok(json!({ "transition": { "kind": kind, "duration_ms": ms.round() as u32 } }))
                },
                false,
            )?;
            Ok(Value::Null)
        }
        "TriggerStudioModeTransition" => {
            let status = ctx.status()?;
            if status.preview.is_none() {
                return Err(Fail::new(
                    code::STUDIO_MODE_NOT_ACTIVE,
                    "Studio mode is not enabled.",
                ));
            }
            ctx.act(Method::Take { transition: None })?;
            Ok(Value::Null)
        }

        // Scene items
        "GetSceneItemList" => {
            let world = World::new(ctx.live()?, None);
            let scene = scene(&world, &d)?;
            Ok(json!({ "sceneItems": world.item_list(scene) }))
        }
        "GetSceneItemId" => {
            let world = World::new(ctx.live()?, None);
            let scene = scene(&world, &d)?;
            let name = d.need_str("sourceName")?;
            let offset = d
                .number("searchOffset", -1.0, f64::from(i32::MAX))?
                .unwrap_or(0.0) as i64;
            let matches: Vec<(usize, &SceneItem)> = scene
                .items
                .iter()
                .enumerate()
                .filter(|(_, item)| {
                    world.item_source(item).0 == name
                        || item.source.as_deref() == Some(name)
                        || item.scene.as_deref() == Some(name)
                        || item.id.as_deref() == Some(name)
                })
                .collect();
            let found = if offset < 0 {
                matches.last()
            } else {
                matches.get(offset as usize)
            };
            let (index, item) = found.ok_or_else(|| {
                Fail::new(
                    code::RESOURCE_NOT_FOUND,
                    format!(
                        "No scene items were found in the specified scene by that name or offset: `{name}`."
                    ),
                )
            })?;
            Ok(json!({ "sceneItemId": world::item_id(&scene.id, item, *index) }))
        }
        "GetSceneItemSource" => {
            let world = World::new(ctx.live()?, None);
            let scene = scene(&world, &d)?;
            let (_, item) = item(&world, scene, &d)?;
            let (name, id, _, _) = world.item_source(item);
            Ok(json!({ "sourceName": name, "sourceUuid": id }))
        }
        "GetSceneItemTransform" => {
            let world = World::new(ctx.live()?, None);
            let scene = scene(&world, &d)?;
            let (_, item) = item(&world, scene, &d)?;
            Ok(json!({ "sceneItemTransform": world::transform(item) }))
        }
        "GetSceneItemEnabled" => {
            let world = World::new(ctx.live()?, None);
            let scene = scene(&world, &d)?;
            let (_, item) = item(&world, scene, &d)?;
            Ok(json!({ "sceneItemEnabled": item.visible }))
        }
        "GetSceneItemLocked" => {
            let world = World::new(ctx.live()?, None);
            let scene = scene(&world, &d)?;
            let (_, item) = item(&world, scene, &d)?;
            Ok(json!({ "sceneItemLocked": item.locked }))
        }
        "GetSceneItemIndex" => {
            let world = World::new(ctx.live()?, None);
            let scene = scene(&world, &d)?;
            let (index, _) = item(&world, scene, &d)?;
            Ok(json!({ "sceneItemIndex": index }))
        }
        "SetSceneItemEnabled" | "SetSceneItemLocked" => {
            let (field, key) = if request_type == "SetSceneItemEnabled" {
                ("visible", "sceneItemEnabled")
            } else {
                ("locked", "sceneItemLocked")
            };
            let value = d.need_bool(key)?;
            let world = World::new(ctx.live()?, None);
            let scene_id = scene(&world, &d)?.id.clone();
            let item_id = d.need_number("sceneItemId", 0.0, f64::from(i32::MAX))? as i64;
            ctx.patch(
                |world| {
                    let scene = world.scene(&scene_id).ok_or_else(|| {
                        Fail::new(code::RESOURCE_NOT_FOUND, "the scene went away")
                    })?;
                    let (index, _) = world.find_item(scene, item_id).ok_or_else(|| {
                        Fail::new(
                            code::RESOURCE_NOT_FOUND,
                            format!(
                                "No scene items were found in scene `{}` with the id `{item_id}`.",
                                world.scene_name(&scene_id)
                            ),
                        )
                    })?;
                    item_patch(world, &scene_id, index, json!({ field: value }))
                },
                false,
            )?;
            Ok(Value::Null)
        }

        // Sources
        "GetSourceActive" => {
            let world = ctx.world()?;
            let name = d.str("sourceName")?;
            let id = d.str("sourceUuid")?;
            if name.is_none() && id.is_none() {
                return Err(missing("sourceName` or `sourceUuid"));
            }
            if let Some(source) = world.find_input(name, id) {
                let (active, showing) = world.source_active(&source.id);
                return Ok(json!({ "videoActive": active, "videoShowing": showing }));
            }
            if let Some(scene) = world.find_scene(name, id) {
                let active = world.program.as_deref() == Some(scene.id.as_str());
                let showing = active || world.preview.as_deref() == Some(scene.id.as_str());
                return Ok(json!({ "videoActive": active, "videoShowing": showing }));
            }
            Err(Fail::new(
                code::RESOURCE_NOT_FOUND,
                format!(
                    "No source was found by the name of `{}`.",
                    id.or(name).unwrap_or_default()
                ),
            ))
        }
        "GetSourceScreenshot" => {
            let format = d.need_str("imageFormat")?;
            if !format.eq_ignore_ascii_case("png") {
                return Err(Fail::new(
                    code::INVALID_REQUEST_FIELD,
                    format!("imageFormat: Isocast makes `png` screenshots, not `{format}`."),
                ));
            }
            let width = d.number("imageWidth", 8.0, 4096.0)?.map(|w| w as u32);
            let world = World::new(ctx.live()?, None);
            let name = d.str("sourceName")?;
            let id = d.str("sourceUuid")?;
            if name.is_none() && id.is_none() {
                return Err(missing("sourceName` or `sourceUuid"));
            }
            let (source, scene) = match (world.find_input(name, id), world.find_scene(name, id)) {
                (Some(source), _) => (Some(source.id.clone()), None),
                (None, Some(scene)) => (None, Some(scene.id.clone())),
                (None, None) => {
                    return Err(Fail::new(
                        code::RESOURCE_NOT_FOUND,
                        format!(
                            "No source was found by the name of `{}`.",
                            id.or(name).unwrap_or_default()
                        ),
                    ));
                }
            };
            let snapshot: isocast_protocol::api::Snapshot =
                ctx.call(Method::Snapshot(SnapshotParams {
                    source,
                    scene,
                    preview: false,
                    width,
                }))?;
            Ok(json!({ "imageData": format!("data:image/png;base64,{}", snapshot.png_base64) }))
        }

        // Outputs: virtual camera and replay buffer
        "GetVirtualCamStatus" => {
            let status = ctx.status()?;
            Ok(json!({ "outputActive": vcam_active(&status) }))
        }
        "StartVirtualCam" | "StopVirtualCam" | "ToggleVirtualCam" => {
            let active = vcam_active(&ctx.status()?);
            let start = match request_type {
                "StartVirtualCam" => true,
                "StopVirtualCam" => false,
                _ => !active,
            };
            if start == active && request_type != "ToggleVirtualCam" {
                return Err(output_state(active));
            }
            ctx.act(if start {
                Method::VirtualCameraStart
            } else {
                Method::VirtualCameraStop
            })?;
            Ok(if request_type == "ToggleVirtualCam" {
                json!({ "outputActive": start })
            } else {
                Value::Null
            })
        }
        "GetReplayBufferStatus" => {
            let status = ctx.status()?;
            Ok(json!({ "outputActive": replay_active(&status) }))
        }
        "StartReplayBuffer" | "StopReplayBuffer" | "ToggleReplayBuffer" => {
            let active = replay_active(&ctx.status()?);
            let start = match request_type {
                "StartReplayBuffer" => true,
                "StopReplayBuffer" => false,
                _ => !active,
            };
            if start == active && request_type != "ToggleReplayBuffer" {
                return Err(output_state(active));
            }
            ctx.act(if start {
                Method::ReplayStart
            } else {
                Method::ReplayStop
            })?;
            Ok(if request_type == "ToggleReplayBuffer" {
                json!({ "outputActive": start })
            } else {
                Value::Null
            })
        }
        "SaveReplayBuffer" => {
            if !replay_active(&ctx.status()?) {
                return Err(output_state(false));
            }
            ctx.act(Method::ReplaySave)?;
            Ok(Value::Null)
        }
        "GetLastReplayBufferReplay" => {
            let status = ctx.status()?;
            let path = status
                .replay
                .as_ref()
                .and_then(|r| r.last_saved.as_ref())
                .map(|s| s.path.clone())
                .ok_or_else(|| {
                    Fail::new(
                        code::RESOURCE_NOT_FOUND,
                        "The replay buffer hasn't saved a replay yet.",
                    )
                })?;
            Ok(json!({ "savedReplayPath": path }))
        }

        // Stream
        "GetStreamStatus" => {
            let status = ctx.status()?;
            let stream = status.stream.as_ref();
            let state = stream.map(|s| s.state);
            let active = matches!(
                state,
                Some(StreamState::Connecting | StreamState::Live | StreamState::Reconnecting)
            );
            let seconds = stream.map_or(0.0, |s| s.elapsed_seconds);
            Ok(json!({
                "outputActive": active,
                "outputReconnecting": state == Some(StreamState::Reconnecting),
                "outputTimecode": timecode(seconds),
                "outputDuration": millis(seconds),
                "outputCongestion": 0.0,
                // Typed clients (obws, goobs) want whole numbers here.
                "outputBytes": stream.map_or(0, |s| (s.total_kbps * 1000.0 / 8.0 * seconds).max(0.0) as u64),
                "outputSkippedFrames": stream.map_or(0, |s| s.dropped_frames),
                "outputTotalFrames": stream.map_or(0, |s| s.frames_sent + s.dropped_frames),
            }))
        }
        "StartStream" | "StopStream" | "ToggleStream" => {
            let status = ctx.status()?;
            let active = status.stream.as_ref().is_some_and(|s| {
                matches!(
                    s.state,
                    StreamState::Connecting | StreamState::Live | StreamState::Reconnecting
                )
            });
            let start = match request_type {
                "StartStream" => true,
                "StopStream" => false,
                _ => !active,
            };
            if start == active && request_type != "ToggleStream" {
                return Err(output_state(active));
            }
            // Every enabled destination, as Start Streaming in the app.
            ctx.act(if start {
                Method::StreamStart(None)
            } else {
                Method::StreamStop(None)
            })?;
            Ok(if request_type == "ToggleStream" {
                json!({ "outputActive": start })
            } else {
                Value::Null
            })
        }

        // Record
        "GetRecordStatus" => {
            let status = ctx.status()?;
            let live = status.live.as_ref();
            let seconds = live.map_or(0.0, |l| l.elapsed_seconds);
            let bytes: u64 = status
                .files
                .iter()
                .filter_map(|f| std::fs::metadata(f).ok())
                .map(|m| m.len())
                .sum();
            Ok(json!({
                "outputActive": recording(&status),
                "outputPaused": live.is_some_and(|l| l.paused),
                "outputTimecode": timecode(seconds),
                "outputDuration": millis(seconds),
                "outputBytes": bytes,
            }))
        }
        "StartRecord" | "StopRecord" | "ToggleRecord" => {
            let status = ctx.status()?;
            let active = recording(&status);
            let start = match request_type {
                "StartRecord" => true,
                "StopRecord" => false,
                _ => !active,
            };
            if start == active && request_type != "ToggleRecord" {
                return Err(output_state(active));
            }
            if start {
                ctx.act(Method::Start(Box::new(StartParams {
                    setup: None,
                    folder: None,
                    seconds: None,
                })))?;
            } else {
                ctx.act(Method::Stop)?;
            }
            Ok(match request_type {
                "ToggleRecord" => json!({ "outputActive": start }),
                "StopRecord" => json!({
                    "outputPath": program_file(&status).unwrap_or_default(),
                }),
                _ => Value::Null,
            })
        }
        "PauseRecord" | "ResumeRecord" | "ToggleRecordPause" => {
            let status = ctx.status()?;
            if !recording(&status) {
                return Err(output_state(false));
            }
            let paused = status.live.as_ref().is_some_and(|l| l.paused);
            let pause = match request_type {
                "PauseRecord" => true,
                "ResumeRecord" => false,
                _ => !paused,
            };
            if pause && paused && request_type == "PauseRecord" {
                return Err(Fail::new(code::OUTPUT_PAUSED, "The output is paused."));
            }
            if !pause && !paused && request_type == "ResumeRecord" {
                return Err(Fail::new(
                    code::OUTPUT_NOT_PAUSED,
                    "The output is not paused.",
                ));
            }
            ctx.act(if pause { Method::Pause } else { Method::Resume })?;
            // obs-websocket answers the toggle with the new state (not in
            // its docs; typed clients such as obws expect it).
            Ok(if request_type == "ToggleRecordPause" {
                json!({ "outputPaused": pause })
            } else {
                Value::Null
            })
        }
        "SplitRecordFile" => {
            if !recording(&ctx.status()?) {
                return Err(output_state(false));
            }
            ctx.act(Method::Split)?;
            Ok(Value::Null)
        }
        "CreateRecordChapter" => {
            let label = d.str("chapterName")?.unwrap_or("Chapter").to_string();
            if !recording(&ctx.status()?) {
                return Err(output_state(false));
            }
            ctx.act(Method::Marker { label })?;
            Ok(Value::Null)
        }

        // UI
        "GetStudioModeEnabled" => {
            let status = ctx.status()?;
            Ok(json!({ "studioModeEnabled": status.preview.is_some() }))
        }
        "SetStudioModeEnabled" => {
            let enable = d.need_bool("studioModeEnabled")?;
            let status = ctx.status()?;
            match (enable, status.preview.is_some()) {
                (true, false) => {
                    let scene = status
                        .scene
                        .clone()
                        .or_else(|| {
                            World::new(ctx.live().ok()?, None)
                                .scenes()
                                .first()
                                .map(|s| s.id.clone())
                        })
                        .ok_or_else(|| {
                            Fail::new(code::INVALID_RESOURCE_STATE, "There are no scenes yet.")
                        })?;
                    ctx.act(Method::Preview { scene })?;
                }
                (false, true) => {
                    ctx.act(Method::ClearPreview)?;
                }
                _ => {}
            }
            Ok(Value::Null)
        }

        "" => Err(Fail::new(
            code::MISSING_REQUEST_TYPE,
            "Your request is missing a `requestType`.",
        )),
        other => Err(Fail::new(
            code::UNKNOWN_REQUEST_TYPE,
            if KNOWN.contains(&other) {
                format!("Isocast does not support `{other}` (obs-websocket compatibility).")
            } else {
                format!("Your request type `{other}` is not valid.")
            },
        )),
    }
}

/// OBS's name for a monitoring setting.
pub fn monitor_type(monitor: isocast_protocol::Monitoring) -> &'static str {
    match monitor {
        isocast_protocol::Monitoring::Off => "OBS_MONITORING_TYPE_NONE",
        isocast_protocol::Monitoring::MonitorOnly => "OBS_MONITORING_TYPE_MONITOR_ONLY",
        isocast_protocol::Monitoring::MonitorAndOutput => "OBS_MONITORING_TYPE_MONITOR_AND_OUTPUT",
    }
}

/// The program file of the running recording: the file at the program's
/// place in `live.files` (the same order as `files`), else the last.
pub fn program_file(status: &Status) -> Option<String> {
    let index = status.live.as_ref().and_then(|live| {
        (live.files.len() == status.files.len())
            .then(|| {
                live.files
                    .iter()
                    .position(|f| f.source == isocast_protocol::PROGRAM)
            })
            .flatten()
    });
    index
        .and_then(|i| status.files.get(i))
        .or(status.files.last())
        .map(|f| f.to_string_lossy().into_owned())
}

fn vcam_active(status: &Status) -> bool {
    status.virtual_camera.as_ref().is_some_and(|c| {
        matches!(
            c.state,
            VirtualCameraState::Starting | VirtualCameraState::Running
        )
    })
}

fn replay_active(status: &Status) -> bool {
    status
        .replay
        .as_ref()
        .is_some_and(|r| r.state == ReplayState::Running)
}

fn output_state(active: bool) -> Fail {
    if active {
        Fail::new(code::OUTPUT_RUNNING, "The output is already running.")
    } else {
        Fail::new(code::OUTPUT_NOT_RUNNING, "The output is not running.")
    }
}

/// An input's settings: its kind's own fields.
pub fn settings_of(source: &Source) -> Value {
    let value = serde_json::to_value(source).unwrap_or_default();
    let fields = source.kind.fields();
    let settings: Map<String, Value> = value
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, _)| fields.contains(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    Value::Object(settings)
}

/// Every request obs-websocket 5.x has, so those Isocast doesn't map get a
/// clear "not supported" comment rather than "not valid".
const KNOWN: &[&str] = &[
    "GetCanvasList",
    "GetPersistentData",
    "SetPersistentData",
    "CreateSceneCollection",
    "CreateProfile",
    "RemoveProfile",
    "GetProfileParameter",
    "SetProfileParameter",
    "SetVideoSettings",
    "GetStreamServiceSettings",
    "SetStreamServiceSettings",
    "SetRecordDirectory",
    "GetSourceFilterKindList",
    "GetSourceFilterList",
    "GetSourceFilterDefaultSettings",
    "CreateSourceFilter",
    "RemoveSourceFilter",
    "SetSourceFilterName",
    "GetSourceFilter",
    "SetSourceFilterIndex",
    "SetSourceFilterSettings",
    "SetSourceFilterEnabled",
    "CallVendorRequest",
    "GetHotkeyList",
    "TriggerHotkeyByName",
    "TriggerHotkeyByKeySequence",
    "CreateInput",
    "RemoveInput",
    "GetInputDefaultSettings",
    "GetInputAudioTracks",
    "SetInputAudioTracks",
    "GetInputDeinterlaceMode",
    "SetInputDeinterlaceMode",
    "GetInputDeinterlaceFieldOrder",
    "SetInputDeinterlaceFieldOrder",
    "GetInputPropertiesListPropertyItems",
    "PressInputPropertiesButton",
    "GetMediaInputStatus",
    "SetMediaInputCursor",
    "OffsetMediaInputCursor",
    "TriggerMediaInputAction",
    "GetOutputList",
    "GetOutputStatus",
    "ToggleOutput",
    "StartOutput",
    "StopOutput",
    "GetOutputSettings",
    "SetOutputSettings",
    "GetGroupSceneItemList",
    "CreateSceneItem",
    "RemoveSceneItem",
    "DuplicateSceneItem",
    "SetSceneItemTransform",
    "SetSceneItemIndex",
    "GetSceneItemBlendMode",
    "SetSceneItemBlendMode",
    "CreateScene",
    "RemoveScene",
    "SetSceneSceneTransitionOverride",
    "SaveSourceScreenshot",
    "SendStreamCaption",
    "SetCurrentSceneTransitionSettings",
    "GetCurrentSceneTransitionCursor",
    "SetTBarPosition",
    "OpenInputPropertiesDialog",
    "OpenInputFiltersDialog",
    "OpenInputInteractDialog",
    "GetMonitorList",
    "OpenVideoMixProjector",
    "OpenSourceProjector",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timecodes_and_volumes() {
        assert_eq!(timecode(3723.4567), "01:02:03.457");
        assert!((db_to_mul(-6.0) - 0.501).abs() < 0.001);
        assert!((mul_to_db(0.5) + 6.02).abs() < 0.01);
        assert_eq!(mul_to_db(0.0), -100.0);
    }
}
