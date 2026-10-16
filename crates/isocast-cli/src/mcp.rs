//! `isocast mcp`: the MCP server. A thin client of the daemon: every tool
//! is one call on the same method table the CLI uses.

use std::sync::Arc;

use anyhow::Result;
use isocast_protocol::api::{
    Applied, ApplyParams, CapacityEstimate, CapacityParams, Doctor, DoctorParams, EngineInfo,
    Frame, FrameParams, ImportObsParams, Live, Marked, Method, Plan, Refreshed, Report, Snapshot,
    SnapshotParams, StartParams, Started, Status, StreamDestinationParams, StreamDestinations,
    StreamKey, StreamKeyParams, StreamKeyTarget, StreamStartParams, Streaming, Switched,
    TuneParams, Tuning,
};
use isocast_protocol::change::Change;
use isocast_protocol::data::{DataGetParams, DataRevision, DataSetParams, DataValues};
use isocast_protocol::devices::{Devices, PermissionsReport};
use isocast_protocol::hotkeys::{
    Catalogue, Hotkeys, InputParams as HotkeyInput, SetParams as HotkeysSet,
};
use isocast_protocol::library::{
    Library, LibraryChange, LibraryCreateParams, LibraryDocument, LibraryImportObsParams,
    LibraryImportParams, LibraryRenameParams, LibrarySwitchParams, LibraryTarget,
};
use isocast_protocol::obs::{Discovery, Imported};
use isocast_protocol::optimize::{
    ObsDiscoverParams, OptimizeParams, Optimized, SpeedTest, SpeedTestParams,
};
use isocast_protocol::plugin::{PluginAddParams, PluginChange, PluginTarget, Plugins};
use isocast_protocol::project::{
    ListProjectsParams, ProjectRenamed, Projects, RenameProjectParams,
};
use isocast_protocol::stream::StreamService;
use isocast_protocol::timeline::{
    ExportStarted, RetakeParams, Retaken, TimelineAngleParams, TimelineClosed,
    TimelineExportParams, TimelineInfo, TimelineList, TimelineMoveParams, TimelineNewParams,
    TimelineOpenParams, TimelineRangeParams, TimelineRemoveParams, TimelineSplitParams,
    TimelineTarget, TimelineTrimParams,
};
use isocast_protocol::{Setup, Transition};
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::client::Client;

mod events;

use isocast_protocol::replay::{ReplaySaved, ReplayStatus};
use isocast_protocol::websocket::{WebsocketEnableParams, WebsocketStatus};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CameraNoArgs {}

use events::{Events, EventsRead, GetEventsParams};

/// Run the MCP server on stdin and stdout until the client disconnects.
pub fn run() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let isocast = Isocast {
            events: Events::start(),
        };
        let service = isocast.serve(rmcp::transport::stdio()).await?;
        service.waiting().await?;
        Ok(())
    })
}

#[derive(Clone)]
struct Isocast {
    /// isocastd's events, kept for get_events.
    events: Arc<Events>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ChangeParams {
    /// A whole new setup to replace the live one (the shape get_setup
    /// returns; get_setup_schema has its JSON Schema).
    #[serde(default)]
    #[schemars(schema_with = "setup_schema")]
    setup: Option<serde_json::Value>,
    /// Or a JSON merge patch on the live setup. Lists of items with ids
    /// (sources and scenes by "id", scene items by "id" or else "source") merge by id:
    /// {"sources":[{"id":"facecam","device":"Capture Card"}]} changes one
    /// field, {"sources":[{"id":"iphone","remove":true}]} removes a source.
    patch: Option<serde_json::Value>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
struct GetSetupParams {
    /// Also return where every item is drawn, as laid out (`resolved`).
    #[serde(default)]
    resolved: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ApplyChangeParams {
    /// The revision from plan_change or get_setup (0 when there is no live
    /// setup yet). The change is refused if the live setup has moved on.
    revision: u64,
    #[serde(flatten)]
    change: ChangeParams,
    /// One step of a gesture (many changes a second, e.g. moving a source
    /// smoothly): applied at once and saved to disk a moment after the last
    /// step instead of before replying.
    #[serde(default)]
    live: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct StartRecordingParams {
    /// The setup to record; the live setup if absent (the shape get_setup
    /// returns; get_setup_schema has its JSON Schema).
    #[serde(default)]
    #[schemars(schema_with = "setup_schema")]
    setup: Option<serde_json::Value>,
    /// Absolute folder for the files instead of the setup's
    /// recording.folder.
    folder: Option<String>,
    /// Stop on its own after this many seconds.
    seconds: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CheckSetupParams {
    /// The setup to check; the live setup if absent (the shape get_setup
    /// returns; get_setup_schema has its JSON Schema).
    #[serde(default)]
    #[schemars(schema_with = "setup_schema")]
    setup: Option<serde_json::Value>,
    /// Check this folder instead of the setup's recording.folder.
    folder: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EstimateCapacityParams {
    /// The setup to judge; the live setup (at its sources' live formats)
    /// if absent (the shape get_setup returns).
    #[serde(default)]
    #[schemars(schema_with = "setup_schema")]
    setup: Option<serde_json::Value>,
    /// Count the stream's encode. Default: the live setup's if it streams
    /// now; a given setup's if it has a stream.
    stream: Option<bool>,
    /// A video source id whose ISO file would be turned on: `admission`
    /// says how to add it so the setup still fits, and why.
    add_iso: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PreviewFrameParams {
    /// A source id in the live setup, or a quick source like "screen",
    /// "display:NAME", "camera:NAME" or "test-pattern".
    source: Option<String>,
    /// Or a scene id. With neither, the program (while recording, exactly
    /// the frame it recorded last).
    scene: Option<String>,
    /// Or, with neither, the scene in preview.
    #[serde(default)]
    preview: bool,
    /// Scale the picture down to at most this many pixels wide (default
    /// 1280).
    width: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct LiveFrameParams {
    /// "program" (the default), "preview" (studio mode's preview scene) or
    /// a video source id of the live setup.
    target: Option<String>,
    /// Scale the picture down to at most this many pixels wide (default
    /// 1280).
    width: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EngineParams {
    /// true keeps the live engine running; false lets it stop once nothing
    /// (a recording, a stream, a client showing frames) needs it.
    running: bool,
}

/// live_engine's reply. MCP needs a tool's result to be an object, so the
/// engine (absent once stopped) is a field rather than the whole reply.
#[derive(Debug, Serialize, JsonSchema)]
struct LiveEngine {
    /// The live engine's sources and how each is doing; null once it has
    /// stopped.
    engine: Option<EngineInfo>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SetAudioParams {
    /// The id of an audio source in the live setup.
    source: String,
    /// Downmix stereo to mono before balance in the program and monitoring.
    mono: Option<bool>,
    /// Volume change in the program mix, in dB (-60 to 24; 0 is as
    /// captured).
    gain_db: Option<f64>,
    /// Leave it out of the program mix (true) or put it back (false).
    muted: Option<bool>,
    /// Shift its audio later (positive) or earlier, in ms, in every file;
    /// applies from the next recording.
    offset_ms: Option<i32>,
    /// Where it sits between left (-1) and right (1) in the program mix;
    /// 0 is the centre.
    balance: Option<f64>,
    /// Hear it through the setup's monitoring device: "monitor_only"
    /// (heard, but left out of the program mix), "monitor_and_output", or
    /// "off".
    monitor: Option<isocast_protocol::Monitoring>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SetIsoAudioParams {
    /// The id of a video source in the live setup: its ISO file.
    source: String,
    /// The audio sources' ids that file carries, in track order ([] for
    /// no audio). Null or absent puts it back to recording.iso.audio (or,
    /// without that list, the source's own sound).
    #[serde(default)]
    audio: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SwitchSceneParams {
    /// The id of a scene in the setup.
    scene: String,
    /// How it comes on air, like {"kind":"fade","duration_ms":500},
    /// {"kind":"swipe","direction":"up"} or {"kind":"cut"}; the scene's
    /// transition (else the setup's) if absent.
    transition: Option<Transition>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PreviewSceneParams {
    /// The id of a scene in the setup.
    scene: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TakeParams {
    /// How the preview scene comes on air; its transition (else the
    /// setup's) if absent.
    transition: Option<Transition>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RefreshBrowserParams {
    /// The browser source's id.
    source: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ImportObsCollectionParams {
    /// The collection: an absolute path to its .json file, or its name in
    /// OBS's scenes folder, like "Recording".
    collection: String,
    /// The OBS profile with the frame rate and recording settings (a name
    /// or a basic.ini path); found automatically if absent.
    profile: Option<String>,
    /// Import onto a canvas this wide and high, scaling positions and
    /// sizes; the collection's own canvas if absent.
    width: Option<u32>,
    height: Option<u32>,
    /// The canvas frame rate; the profile's if absent.
    fps: Option<u32>,
}

#[derive(Debug, JsonSchema)]
struct InitSetupParams {
    #[serde(flatten)]
    options: isocast_protocol::init::InitOptions,
    /// Make it the live setup, replacing any (the old one stays in the
    /// setup file's .bak). Otherwise it is only returned, for plan_change.
    #[serde(default)]
    apply: bool,
}

/// Read as the API's `init_setup` params, whose errors name the option
/// even though it is flattened.
impl<'de> Deserialize<'de> for InitSetupParams {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // The trait's, not the inherent one serde derives for `remote`.
        let isocast_protocol::api::InitParams { options, apply } =
            <isocast_protocol::api::InitParams as Deserialize>::deserialize(deserializer)?;
        Ok(Self { options, apply })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ImportDocumentsParams {
    /// A whole setup (as get_setup returns one): its collection part and
    /// its profile part are both imported.
    #[serde(default)]
    #[schemars(schema_with = "setup_schema")]
    setup: Option<serde_json::Value>,
    /// Or a scene collection, as get_collection_or_profile returns one: the
    /// setup's version, name, canvas.width/height, sources, scenes,
    /// transition and recording.program.audio / recording.iso.sources /
    /// recording.iso.audio / recording.iso.source_audio.
    #[serde(default)]
    #[schemars(schema_with = "setup_schema")]
    collection: Option<serde_json::Value>,
    /// And/or a profile: version, name, canvas.fps, audio, stream and the
    /// rest of recording (folder, keep_free_gb, program and iso settings).
    #[serde(default)]
    #[schemars(schema_with = "setup_schema")]
    profile: Option<serde_json::Value>,
    /// The name to give them; their own if absent.
    name: Option<String>,
    /// Switch to what was imported (refused while recording or streaming).
    #[serde(default)]
    switch: bool,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
struct RequestPermissionsToolParams {
    /// Only this one: screen_recording, camera, microphone or system_audio
    /// (all not decided yet if left out).
    #[serde(default)]
    permission: Option<isocast_protocol::devices::PermissionKind>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct OpenPermissionSettingsParams {
    /// screen_recording, camera, microphone or system_audio.
    permission: isocast_protocol::devices::PermissionKind,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AddMarkerParams {
    /// What happens at this moment, like "intro" or "retake".
    label: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct StreamKeyToolParams {
    /// The stream key from the service's dashboard. It goes into the
    /// Keychain and is never shown again; prefer `isocast stream key set`
    /// in a terminal, which keeps it out of this conversation.
    key: String,
    /// A destination ("main", or an id from get_status's streams): its
    /// own key (with service and server only for one not applied yet).
    destination: Option<String>,
    /// The service it is for; the live setup's [stream] service if absent.
    service: Option<StreamService>,
    /// For "custom": the server it belongs to (rtmp://host/app).
    server: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DestinationIdParams {
    /// The destination's id, from get_status's streams.
    id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct StreamKeyTargetParams {
    /// A destination ("main", or an id from get_status's streams).
    destination: Option<String>,
    /// The service; the live setup's [stream] service if absent.
    service: Option<StreamService>,
    /// For "custom": the server the key belongs to.
    server: Option<String>,
}

#[tool_router]
impl Isocast {
    #[tool(
        description = "List recording encoders with their display labels and supported containers. Use this before changing program or ISO encoder settings.",
        annotations(read_only_hint = true)
    )]
    async fn list_encoder_options(
        &self,
    ) -> Result<Json<Vec<isocast_protocol::EncoderOption>>, String> {
        self.call(Method::EncoderOptions).await
    }

    #[tool(
        description = "Get current profile hotkeys, global registration warnings, held audio overrides and Studio Mode state.",
        annotations(read_only_hint = true)
    )]
    async fn hotkeys_get(&self) -> Result<Json<Hotkeys>, String> {
        self.call(Method::HotkeysGet).await
    }

    #[tool(
        description = "Replace all profile bindings with bindings, or patch by binding id (null removes). Keys use ctrl+alt+shift+cmd+key syntax. hotkeys_actions lists typed arguments, key names and defaults. Invalid/conflicting chords change nothing.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn hotkeys_set(
        &self,
        Parameters(Strict(params)): Parameters<Strict<HotkeysSet>>,
    ) -> Result<Json<Hotkeys>, String> {
        self.call(Method::HotkeysSet(Box::new(params))).await
    }

    #[tool(
        description = "List bindable action ids, labels, groups, strict argument schemas, reserved actions, key names and opt-in defaults.",
        annotations(read_only_hint = true)
    )]
    async fn hotkeys_actions(&self) -> Result<Json<Catalogue>, String> {
        self.call(Method::HotkeysActions).await
    }

    #[tool(
        description = "Forward focused key input: keys and phase (press or release), including globally registered chords. Matching native and focused press edges within 100 ms fire once. Send releases on blur; persistent clients use the same socket for both edges.",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    async fn hotkeys_input(
        &self,
        Parameters(Strict(params)): Parameters<Strict<HotkeyInput>>,
    ) -> Result<Json<serde_json::Value>, String> {
        self.call(Method::HotkeysInput(params)).await
    }
    #[tool(
        description = "The scene collections (scenes, sources, transitions and the canvas \
                       size) and profiles (frame rate, recording, stream and audio settings) \
                       the live setup can be made of, as in OBS, and which of each is in use \
                       (`collection`, `profile`). The live setup is the pair in use: \
                       apply_change edits it, and the change is saved into the right one.",
        annotations(read_only_hint = true)
    )]
    async fn list_collections_and_profiles(&self) -> Result<Json<Library>, String> {
        self.call(Method::Library).await
    }

    #[tool(
        description = "One scene collection or profile (kind: collection or profile, and its \
                       id) as a document, to export it or read one that isn't in use.",
        annotations(read_only_hint = true)
    )]
    async fn get_collection_or_profile(
        &self,
        Parameters(Strict(params)): Parameters<Strict<LibraryTarget>>,
    ) -> Result<Json<LibraryDocument>, String> {
        self.call(Method::LibraryGet(params)).await
    }

    #[tool(
        description = "Make a scene collection (one empty scene on the canvas in use) or a \
                       profile (60 fps, HEVC program and ISO files to ~/Movies/Isocast), or a \
                       copy of one (copy_of: its id, OBS's Duplicate); with switch, start using \
                       it.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn create_collection_or_profile(
        &self,
        Parameters(Strict(params)): Parameters<Strict<LibraryCreateParams>>,
    ) -> Result<Json<LibraryChange>, String> {
        self.call(Method::LibraryCreate(Box::new(params))).await
    }

    #[tool(
        description = "Rename a scene collection or profile; its id stays. Renaming the \
                       collection in use renames the live setup.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn rename_collection_or_profile(
        &self,
        Parameters(Strict(params)): Parameters<Strict<LibraryRenameParams>>,
    ) -> Result<Json<LibraryChange>, String> {
        self.call(Method::LibraryRename(Box::new(params))).await
    }

    #[tool(
        description = "Delete a scene collection or profile that isn't in use (switch away \
                       first); its file is kept in the library's deleted folder.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    async fn delete_collection_or_profile(
        &self,
        Parameters(Strict(params)): Parameters<Strict<LibraryTarget>>,
    ) -> Result<Json<LibraryChange>, String> {
        self.call(Method::LibraryDelete(params)).await
    }

    #[tool(
        description = "Start using another scene collection, profile or both (by id): the live \
                       setup becomes that pair at once and the live engine follows (a new \
                       collection goes on air from its first scene). Refused while recording \
                       or streaming, and when the pair would have problems (each named by \
                       field and part).",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn switch_collection_or_profile(
        &self,
        Parameters(Strict(params)): Parameters<Strict<LibrarySwitchParams>>,
    ) -> Result<Json<LibraryChange>, String> {
        self.call(Method::LibrarySwitch(params)).await
    }

    #[tool(
        description = "Add a scene collection and/or profile from documents (as \
                       get_collection_or_profile returns them), or both from a whole setup. \
                       Refused if one has problems of its own; problems with the other half in \
                       use are reported. With switch, start using them.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn import_collection_or_profile(
        &self,
        Parameters(Strict(params)): Parameters<Strict<ImportDocumentsParams>>,
    ) -> Result<Json<LibraryChange>, String> {
        fn read<T: serde::de::DeserializeOwned>(
            field: &str,
            value: Option<serde_json::Value>,
        ) -> Result<Option<T>, String> {
            value
                .map(|value| {
                    isocast_protocol::located::from_value(&value)
                        .map_err(|err| err.within(field).to_string())
                })
                .transpose()
        }
        let collection = read("collection", params.collection)?;
        let profile = read("profile", params.profile)?;
        self.call(Method::LibraryImport(Box::new(LibraryImportParams {
            setup: parse_setup(params.setup)?,
            collection,
            profile,
            name: params.name,
            switch: params.switch,
        })))
        .await
    }

    #[tool(
        description = "Add an OBS Studio scene collection (its name in OBS's scenes folder, or \
                       an absolute .json path) as a scene collection, and/or an OBS profile \
                       (its name, folder or basic.ini) as a profile, with devices matched \
                       against this Mac. Returns what was made and what couldn't come over. \
                       Give both to bring Source Record's ISO settings into the profile.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn import_obs_collection_or_profile(
        &self,
        Parameters(Strict(params)): Parameters<Strict<LibraryImportObsParams>>,
    ) -> Result<Json<LibraryChange>, String> {
        self.call(Method::LibraryImportObs(Box::new(params))).await
    }

    #[tool(
        description = "The installed plugins: id, version, what they provide (a process, and \
                       kinds like twitch-chat/glow), state (ok, disabled, incompatible or \
                       failed, and why), their process (running or restarting, restarts, how it \
                       last ended, its log) and the permissions they were granted.",
        annotations(read_only_hint = true)
    )]
    async fn list_plugins(&self) -> Result<Json<Plugins>, String> {
        self.call(Method::PluginList).await
    }

    #[tool(
        description = "Install a plugin from a local folder with an isocast-plugin.toml (path: \
                       absolute). `grant` must be exactly the permissions its manifest asks \
                       for: without it nothing is installed and the error lists them, so the \
                       user can see them before you grant them. A newer version replaces the \
                       installed one (needing a grant only if it asks for more); force \
                       reinstalls the same or an older version; link runs it from the folder \
                       instead of a copy. Its process, if any, starts at once, sandboxed, and \
                       may call only the methods it was granted.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn add_plugin(
        &self,
        Parameters(Strict(params)): Parameters<Strict<PluginAddParams>>,
    ) -> Result<Json<PluginChange>, String> {
        self.call(Method::PluginAdd(Box::new(params))).await
    }

    #[tool(
        description = "Stop a plugin's process and remove the plugin (by id); its data folder \
                       is kept if it wrote anything there. Refused while the live setup uses \
                       its kinds (the error lists where) unless `force`: the setup then keeps \
                       them, bypassed, until the plugin is added again.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    async fn remove_plugin(
        &self,
        Parameters(Strict(params)): Parameters<Strict<PluginTarget>>,
    ) -> Result<Json<PluginChange>, String> {
        self.call(Method::PluginRemove(params)).await
    }

    #[tool(
        description = "Read the data store: values by key (dotted ids like score.home) that \
                       text in scenes shows (a text source's or text content's `data = \
                       \"score.home\"`). Every key, the `keys` given, or those under `prefix` \
                       (`score` covers score and score.home); with the store's revision, which \
                       keys are kept across restarts (persisted), and asked keys that aren't \
                       set (missing).",
        annotations(read_only_hint = true)
    )]
    async fn get_data(
        &self,
        Parameters(Strict(params)): Parameters<Strict<DataGetParams>>,
    ) -> Result<Json<DataValues>, String> {
        self.call(Method::DataGet(Some(params))).await
    }

    #[tool(
        description = "Set data keys to JSON values, several at once (all of them, or none if \
                       one is refused): `values` like {\"title\": \"Hello\", \"score.home\": 3}. \
                       Text showing a key (`data = \"title\"` on a text source or text content) \
                       changes in the program within a frame or two, with no setup change, so \
                       use this for anything that changes while recording: titles, scores, \
                       counters. Keys: lowercase letters, digits, dashes and underscores \
                       between dots, at most 128 characters; values at most 64 KB as JSON. \
                       `persist: true` keeps them with the scene collection across restarts \
                       (false stops keeping them; absent leaves that as it was). Returns the \
                       revision and the keys that changed.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn set_data(
        &self,
        Parameters(Strict(params)): Parameters<Strict<DataSetParams>>,
    ) -> Result<Json<DataRevision>, String> {
        self.call(Method::DataSet(Box::new(params))).await
    }

    #[tool(
        description = "The recording projects, newest first: each recording session writes \
                       into its own folder (`<date> <time> Untitled` in the recordings folder) \
                       with every take's program and ISO files, manifests and exports. Each \
                       has an id (stable across renames), name, folder, takes (manifests) and \
                       files. Optional `limit` (100 if absent).",
        annotations(read_only_hint = true)
    )]
    async fn list_projects(
        &self,
        Parameters(Strict(params)): Parameters<Strict<ListProjectsParams>>,
    ) -> Result<Json<Projects>, String> {
        self.call(Method::ListProjects(Some(params))).await
    }

    #[tool(
        description = "Rename a recording project (by id, from list_projects): its folder \
                       becomes `<date> <time> <name>` (numbered if taken). Refused while a \
                       recording or an export writes into it. Manifests and timelines keep \
                       finding its files. Returns the project, its old name and old folder.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn rename_project(
        &self,
        Parameters(Strict(params)): Parameters<Strict<RenameProjectParams>>,
    ) -> Result<Json<ProjectRenamed>, String> {
        self.call(Method::RenameProject(params)).await
    }

    #[tool(
        description = "Make a timeline (optional `name`) and make it current: from now on every \
                       take that finishes (stop_recording, or split_take) is added to its end as \
                       a clip. A timeline is takes in order, like a clip bar: retake records the \
                       last one again, remove_clip and move_clip edit the order, and \
                       export_timeline joins them into one file. Returns the timeline.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn new_timeline(
        &self,
        Parameters(Strict(params)): Parameters<Strict<TimelineNewParams>>,
    ) -> Result<Json<TimelineInfo>, String> {
        self.call(Method::TimelineNew(Some(params))).await
    }

    #[tool(
        description = "A timeline (the current one unless `timeline` gives an id): its clips in \
                       order, each with its id (c1, c2…), place and length on the timeline, \
                       take (session manifest), status and files (program and ISO files), and \
                       the takes taken off it (their files are kept where they are).",
        annotations(read_only_hint = true)
    )]
    async fn get_timeline(
        &self,
        Parameters(Strict(params)): Parameters<Strict<TimelineTarget>>,
    ) -> Result<Json<TimelineInfo>, String> {
        self.call(Method::TimelineGet(Some(params))).await
    }

    #[tool(
        description = "Every timeline (id, name, clips, length) and which is current (the one \
                       finished takes are added to).",
        annotations(read_only_hint = true)
    )]
    async fn list_timelines(&self) -> Result<Json<TimelineList>, String> {
        self.call(Method::TimelineList).await
    }

    #[tool(
        description = "Make a timeline current (by id), so finished takes are added to it.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn open_timeline(
        &self,
        Parameters(Strict(params)): Parameters<Strict<TimelineOpenParams>>,
    ) -> Result<Json<TimelineInfo>, String> {
        self.call(Method::TimelineOpen(params)).await
    }

    #[tool(
        description = "Make no timeline current: takes are only recorded, as without \
                       timelines. Returns the id of the timeline that was current.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn close_timeline(&self) -> Result<Json<TimelineClosed>, String> {
        self.call(Method::TimelineClose).await
    }

    #[tool(
        description = "Take a clip (by id, like c2) off a timeline (the current one unless \
                       `timeline` is given). Only the timeline changes: the take's files stay \
                       where they were recorded, listed under the timeline's `removed`. Returns \
                       the timeline.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn remove_clip(
        &self,
        Parameters(Strict(params)): Parameters<Strict<TimelineRemoveParams>>,
    ) -> Result<Json<TimelineInfo>, String> {
        self.call(Method::TimelineRemove(params)).await
    }

    #[tool(
        description = "Move a clip (by id) to another place on a timeline: `index` counts from \
                       0 (the first). Returns the timeline.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn move_clip(
        &self,
        Parameters(Strict(params)): Parameters<Strict<TimelineMoveParams>>,
    ) -> Result<Json<TimelineInfo>, String> {
        self.call(Method::TimelineMove(params)).await
    }

    #[tool(
        description = "Move a clip's in and/or out point (`in_seconds`, `out_seconds`, on the \
                       clip's take's own clock, rounded to its frames). Only the timeline \
                       changes, never the take's files; one undoable edit. Returns the timeline.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn trim_clip(
        &self,
        Parameters(Strict(params)): Parameters<Strict<TimelineTrimParams>>,
    ) -> Result<Json<TimelineInfo>, String> {
        self.call(Method::TimelineTrim(params)).await
    }

    #[tool(
        description = "Cut the clip at `at_seconds` (on the timeline, rounded to the frames \
                       there) in two; the second gets a new id. One undoable edit. Returns the \
                       timeline.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn split_clip(
        &self,
        Parameters(Strict(params)): Parameters<Strict<TimelineSplitParams>>,
    ) -> Result<Json<TimelineInfo>, String> {
        self.call(Method::TimelineSplit(params)).await
    }

    #[tool(
        description = "Take `start_seconds` to `end_seconds` (on the timeline) out and close \
                       the gap (ripple delete): clips across its ends are trimmed, clips in it \
                       taken off. The takes' files are untouched; one undoable edit. Returns \
                       the timeline.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn delete_range(
        &self,
        Parameters(Strict(params)): Parameters<Strict<TimelineRangeParams>>,
    ) -> Result<Json<TimelineInfo>, String> {
        self.call(Method::TimelineDeleteRange(params)).await
    }

    #[tool(
        description = "Choose what the edited video shows from `start_seconds` to \
                       `end_seconds` on the timeline: `source` is `program` or an ISO source \
                       every clip there recorded (clips are split at the span's ends). \
                       export_timeline then writes an edited file with these angles and the \
                       program's sound. One undoable edit. Returns the timeline.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn set_angle(
        &self,
        Parameters(Strict(params)): Parameters<Strict<TimelineAngleParams>>,
    ) -> Result<Json<TimelineInfo>, String> {
        self.call(Method::TimelineAngle(params)).await
    }

    #[tool(
        description = "Take back the last edit of a timeline (trim, split, delete, angle, \
                       remove, move; not a recorded take). Returns the timeline, with how many \
                       edits can still be undone and redone.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn undo_edit(
        &self,
        Parameters(Strict(params)): Parameters<Strict<TimelineTarget>>,
    ) -> Result<Json<TimelineInfo>, String> {
        self.call(Method::TimelineUndo(Some(params))).await
    }

    #[tool(
        description = "Put back the last edit undo_edit took back. Returns the timeline.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn redo_edit(
        &self,
        Parameters(Strict(params)): Parameters<Strict<TimelineTarget>>,
    ) -> Result<Json<TimelineInfo>, String> {
        self.call(Method::TimelineRedo(Some(params))).await
    }

    #[tool(
        description = "Record the last take again, on the current timeline. Not recording: take \
                       the last clip off (its files stay) and start recording in the same folder \
                       (or `folder`; `seconds` stops it on its own); while a stop is \
                       finishing, the same once it has. Recording: drop the take being recorded \
                       from the timeline (its files stay) and begin a new take about a quarter \
                       second later without stopping any device; `begins_at` and \
                       `begins_in_seconds` say when (what is said before then is in the dropped \
                       take). Returns the new take's files and what was taken off. Program \
                       export needs every clip's program file.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn retake(
        &self,
        Parameters(Strict(params)): Parameters<Strict<RetakeParams>>,
    ) -> Result<Json<Retaken>, String> {
        self.call(Method::Retake(Some(params))).await
    }

    #[tool(
        description = "Join a timeline's clips (the current one unless `timeline` is given), in \
                       order, into one program file and one file per ISO source every clip has, \
                       each cut frame-accurately to the clips' in and out points (video copied \
                       as recorded except from a cut to the next keyframe; audio re-encoded once \
                       and cross-faded so it stays in sync at every join), an EDITED file when \
                       clips show other angles (set_angle), plus an .export.json manifest, next \
                       to the first take (or in \
                       `folder`). Sources some clips lack are skipped, saying why. It runs in \
                       the background: returns at once with the files it will write; \
                       get_events shows export_progress and export_finished.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn export_timeline(
        &self,
        Parameters(Strict(params)): Parameters<Strict<TimelineExportParams>>,
    ) -> Result<Json<ExportStarted>, String> {
        self.call(Method::TimelineExport(Some(params))).await
    }

    #[tool(
        description = "Go live: stream the program (canvas and program mix) as H.264/AAC over \
                       RTMP or RTMPS to the live setup's [stream] destinations (twitch, youtube, \
                       kick, facebook or custom servers; every enabled one, or `only` those \
                       ids), starting the live engine if needed; destinations already live are \
                       left alone, and recordings start and stop independently. Destinations \
                       with the same bitrate and size share one encoder. Each needs its stream \
                       key set. Returns the first destination's status plus every \
                       destination's (`destinations`) once each is live or failed, or an error \
                       saying what to do. get_status shows each one's health (`streams`).",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn start_stream(
        &self,
        Parameters(Strict(params)): Parameters<Strict<StreamStartParams>>,
    ) -> Result<Json<Streaming>, String> {
        self.call(Method::StreamStart(Some(params))).await
    }

    #[tool(
        description = "Add a stream destination to the live setup: a service (twitch, youtube, \
                       kick, facebook, custom), its server (custom and kick need one; never with \
                       the key in it), optional id (made from the service), enabled, and its \
                       own video_bitrate_kbps or width/height (else the main destination's; a \
                       different bitrate or size needs an encoder of its own, refused at start \
                       when this Mac's encoders lack room). The first one becomes the [stream] \
                       section itself (id main). Then set_stream_key with its destination id. \
                       Returns every destination's status.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn stream_destination_add(
        &self,
        Parameters(Strict(params)): Parameters<Strict<StreamDestinationParams>>,
    ) -> Result<Json<StreamDestinations>, String> {
        self.call(Method::StreamDestinationAdd(Box::new(params)))
            .await
    }

    #[tool(
        description = "Change a stream destination by id (main is the [stream] section): \
                       service, server, enabled, video_bitrate_kbps, width/height, or `clear` \
                       some of server, video_bitrate_kbps, size. Takes effect at its next \
                       start. Returns every destination's status.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn stream_destination_set(
        &self,
        Parameters(Strict(params)): Parameters<Strict<StreamDestinationParams>>,
    ) -> Result<Json<StreamDestinations>, String> {
        self.call(Method::StreamDestinationSet(Box::new(params)))
            .await
    }

    #[tool(
        description = "Remove a stream destination by id (not main: turn it off with \
                       stream_destination_set instead; not while it streams). Its stored key \
                       stays in the Keychain until clear_stream_key. Returns every \
                       destination's status.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    async fn stream_destination_remove(
        &self,
        Parameters(Strict(params)): Parameters<Strict<DestinationIdParams>>,
    ) -> Result<Json<StreamDestinations>, String> {
        self.call(Method::StreamDestinationRemove { id: params.id })
            .await
    }

    #[tool(
        description = "Start the replay buffer: keep the last recording.replay.seconds (30 by \
                       default) of the encoded program, video and program mix, in memory, \
                       starting the live engine if needed; recording and streaming are \
                       independent of it. It shares the stream's encoder when the setup has a \
                       [stream]. Then save_replay writes the last seconds to a file. Returns \
                       its status.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn start_replay(&self) -> Result<Json<ReplayStatus>, String> {
        self.call(Method::ReplayStart).await
    }

    #[tool(
        description = "Stop the replay buffer, discarding what it holds (save_replay first to \
                       keep it). Returns its final status.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    async fn stop_replay(&self) -> Result<Json<ReplayStatus>, String> {
        self.call(Method::ReplayStop).await
    }

    #[tool(
        description = "Save the replay buffer: write the last recording.replay.seconds of the \
                       program, up to this moment, to a new MOV/MP4 in the recordings folder \
                       (the running recording's project folder, if one records), without \
                       interrupting recording or streaming. Returns the file's path, length, \
                       frames and any missed frames.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn save_replay(&self) -> Result<Json<ReplaySaved>, String> {
        self.call(Method::ReplaySave).await
    }

    #[tool(
        description = "The replay buffer's state, seconds held, memory used and its limit, \
                       format, whether it shares the stream's encoder, missed frames and the \
                       last save.",
        annotations(read_only_hint = true, idempotent_hint = true)
    )]
    async fn replay_status(&self) -> Result<Json<ReplayStatus>, String> {
        self.call(Method::ReplayStatus).await
    }

    #[tool(
        description = "The obs-websocket v5 compatibility server (OBS tools such as Stream \
                       Deck, Touch Portal and Streamer.bot drive Isocast through it): whether \
                       it is on and listening, its ws:// address, whether it asks for a \
                       password, connected clients, and why it isn't listening if it should be.",
        annotations(read_only_hint = true, idempotent_hint = true)
    )]
    async fn websocket_status(&self) -> Result<Json<WebsocketStatus>, String> {
        self.call(Method::WebsocketStatus).await
    }

    #[tool(
        description = "Turn on the obs-websocket v5 server (off by default; kept across \
                       restarts), optionally setting port (4455), bind (127.0.0.1, this Mac \
                       only; 0.0.0.0 for every network), password (kept in the Keychain) or \
                       no_password (loopback bind only; web pages are then refused). Without a stored password one is generated and returned \
                       once as generated_password, for the user to enter in the OBS tool. \
                       Returns its status.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn websocket_enable(
        &self,
        Parameters(Strict(params)): Parameters<Strict<WebsocketEnableParams>>,
    ) -> Result<Json<WebsocketStatus>, String> {
        self.call(Method::WebsocketEnable(Some(params))).await
    }

    #[tool(
        description = "Turn off the obs-websocket server, disconnecting its clients. Returns \
                       its status.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn websocket_disable(&self) -> Result<Json<WebsocketStatus>, String> {
        self.call(Method::WebsocketDisable).await
    }

    #[tool(
        description = "Stop streaming (to every destination, or `only` those ids; the others \
                       keep going). Returns the final status of the first stopped, plus every \
                       destination's: frames sent and dropped, reconnects and the last error.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn stop_stream(
        &self,
        Parameters(Strict(params)): Parameters<Strict<StreamStartParams>>,
    ) -> Result<Json<Streaming>, String> {
        self.call(Method::StreamStop(Some(params))).await
    }

    #[tool(
        description = "Start Isocast Virtual Camera. Takes no arguments: configure virtual_camera.target/scene/source/width/height through plan_change and apply_change first. Requires its signed/approved extension; never installs it. Recording and streaming stay independent.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn virtual_camera_start(
        &self,
        Parameters(Strict(_)): Parameters<Strict<CameraNoArgs>>,
    ) -> Result<Json<isocast_protocol::virtual_camera::VirtualCameraStatus>, String> {
        self.call(Method::VirtualCameraStart).await
    }

    #[tool(
        description = "Stop feeding Isocast Virtual Camera. Other applications see the extension's Isocast waiting card.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn virtual_camera_stop(
        &self,
        Parameters(Strict(_)): Parameters<Strict<CameraNoArgs>>,
    ) -> Result<Json<isocast_protocol::virtual_camera::VirtualCameraStatus>, String> {
        self.call(Method::VirtualCameraStop).await
    }

    #[tool(
        description = "Remove Isocast Virtual Camera from macOS (the release app's camera extension), for deleting Isocast cleanly: stops the camera, then macOS asks the user for an administrator password; waits for it (up to about 4 minutes). Ask the user first. A later virtual_camera_start installs it again.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true
        )
    )]
    async fn virtual_camera_remove(
        &self,
        Parameters(Strict(_)): Parameters<Strict<CameraNoArgs>>,
    ) -> Result<Json<isocast_protocol::virtual_camera::VirtualCameraRemoval>, String> {
        self.call(Method::VirtualCameraRemove).await
    }

    #[tool(
        description = "Developer mode, for testing a build locally. With no arguments, read it: \
                       whether the current profile has it on, whether isocastd is writing its \
                       verbose records and traces, when it turns itself off, its logs folder and \
                       budget, and the latest per-second trace (each recording output's frames, \
                       drops, late frames, encode submit and capture-to-submit times, writer \
                       queue; compositor and pacing). With `enabled` (and/or `auto_off_hours`, 0 \
                       for never; 24 by default), change it: every API call and debug messages \
                       are logged and frame timing traced while recording, in their own log \
                       segments (read them with get_logs). Never disturbs a running recording.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn developer_mode(
        &self,
        Parameters(Strict(params)): Parameters<Strict<isocast_protocol::developer::DebugSetParams>>,
    ) -> Result<Json<isocast_protocol::developer::DeveloperStatus>, String> {
        if params.enabled.is_none() && params.auto_off_hours.is_none() {
            self.call(Method::DebugStatus).await
        } else {
            self.call(Method::DebugSet(params)).await
        }
    }

    #[tool(
        description = "Read virtual camera state, output format, sent/dropped frame counts, enqueue latency and last error. diagnostic-iosurface means a development probe, not an installed camera.",
        annotations(read_only_hint = true, idempotent_hint = true)
    )]
    async fn virtual_camera_status(
        &self,
        Parameters(Strict(_)): Parameters<Strict<CameraNoArgs>>,
    ) -> Result<Json<isocast_protocol::virtual_camera::VirtualCameraStatus>, String> {
        self.call(Method::VirtualCameraStatus).await
    }

    #[tool(
        description = "Keep a stream key in the macOS Keychain for a streaming service (the live \
                       setup's by default). It is never written to setups, logs or status. \
                       Returns where it is kept, not the key.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn set_stream_key(
        &self,
        Parameters(Strict(params)): Parameters<Strict<StreamKeyToolParams>>,
    ) -> Result<Json<StreamKey>, String> {
        self.call(Method::StreamKeySet(Box::new(StreamKeyParams {
            key: params.key,
            target: StreamKeyTarget {
                destination: params.destination,
                service: params.service,
                server: params.server,
            },
        })))
        .await
    }

    #[tool(
        description = "Remove the stored stream key for a streaming service (the live setup's by \
                       default).",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true
        )
    )]
    async fn clear_stream_key(
        &self,
        Parameters(Strict(params)): Parameters<Strict<StreamKeyTargetParams>>,
    ) -> Result<Json<StreamKey>, String> {
        self.call(Method::StreamKeyClear(Box::new(StreamKeyTarget {
            destination: params.destination,
            service: params.service,
            server: params.server,
        })))
        .await
    }

    #[tool(
        description = "Whether Isocast is idle or recording, with live frame and drop counters, \
                       the files being written, the scene on air, the last session's report, \
                       the stream (state, bitrates, dropped frames, congestion, reconnects, \
                       whether a key is set), the live engine's frame rate (engine.fps of \
                       canvas.fps), each live source's size and frame rate (engine.sources: \
                       fps opened at, received_fps measured), isocastd's CPU use \
                       (cpu_percent, of all cores), the recording disk's free space \
                       (recording_free_bytes) and how fast the program file fills it at most \
                       (program_bytes_per_second), and which isocastd answered (daemon: \
                       version, build, commit, executable, pid; an engine left running from \
                       before an app update reports the old build).",
        annotations(read_only_hint = true)
    )]
    async fn get_status(&self) -> Result<Json<Status>, String> {
        self.call(Method::Status).await
    }

    #[tool(
        description = "Generate a sanitized offline bug-report zip or preview every included file. Includes environment, logs, metrics, crashes and manifests, never media. Nothing is uploaded unless submit is true: then ISSUE.md becomes a public GitHub issue and the zip is kept privately for maintainers (pass preview_id to send exactly a reviewed preview). Only submit when the user asked to send the report; on failure the zip is kept and submit_error says why.",
        annotations(read_only_hint = false)
    )]
    async fn bug_report(
        &self,
        Parameters(params): Parameters<isocast_protocol::diagnostics::BugReportParams>,
    ) -> Result<CallToolResult, String> {
        let Json(report): Json<isocast_protocol::diagnostics::BugReport> =
            self.call(Method::BugReport(params)).await?;
        let sent = match (&report.submitted, &report.submit_error) {
            (Some(sent), _) => format!("Sent: {} (report {}).", sent.issue_url, sent.report_id),
            (None, Some(why)) => format!(
                "Not sent: {why}. The zip is kept at {}; attach it to a new GitHub issue by hand.",
                report
                    .archive
                    .as_deref()
                    .map_or("?".into(), |p| p.display().to_string())
            ),
            (None, None) => "No upload occurred.".into(),
        };
        let mut result = CallToolResult::success(vec![ContentBlock::text(format!(
            "{} files. Preview ID: {}. Use read_bug_report_file for bounded contents. {sent}",
            report.files.len(),
            report.preview_id.as_deref().unwrap_or("archive written")
        ))]);
        result.structured_content = Some(serde_json::to_value(report).map_err(|e| e.to_string())?);
        Ok(result)
    }

    #[tool(
        description = "Read a UTF-8 page (default 4096 bytes, max 16384) from a redacted bug-report preview. Use its preview_id and a file-index name; next_offset continues. Preview expires after five idle minutes.",
        annotations(read_only_hint = true)
    )]
    async fn read_bug_report_file(
        &self,
        Parameters(params): Parameters<isocast_protocol::diagnostics::ReportFileParams>,
    ) -> Result<Json<isocast_protocol::diagnostics::ReportFile>, String> {
        self.call(Method::ReportFile(params)).await
    }

    #[tool(
        description = "Read structured engine/client logs with severity/component filters and a cursor for subsequent reads.",
        annotations(read_only_hint = true)
    )]
    async fn get_logs(
        &self,
        Parameters(params): Parameters<isocast_protocol::diagnostics::LogsParams>,
    ) -> Result<Json<isocast_protocol::diagnostics::Logs>, String> {
        self.call(Method::Logs(params)).await
    }

    #[tool(
        description = "What happened since you last looked: warnings (like a file dropping \
                       frames), recordings starting and stopping (with the report), scene \
                       switches, markers, stream and source state, the newest setup_changed and \
                       live status, and `connection` events when isocastd goes away or comes \
                       back. The server keeps the newest 500 whether or not anyone reads. Pass \
                       the reply's `next` as `after` to get only newer events; `kinds` filters, \
                       `wait_seconds` waits for one to arrive.",
        annotations(read_only_hint = true)
    )]
    async fn get_events(
        &self,
        Parameters(Strict(params)): Parameters<Strict<GetEventsParams>>,
    ) -> Result<Json<EventsRead>, String> {
        let events = self.events.clone();
        tokio::task::spawn_blocking(move || {
            events.ensure_watching();
            events.read(params)
        })
        .await
        .map_err(|err| format!("{err}"))?
        .map(Json)
    }

    #[tool(
        description = "Displays, windows, cameras (capture cards, Continuity Camera, iPhone \
                       screens over USB), microphones with their ids and formats, audio \
                       outputs (for audio.monitoring_device), and the screen/camera/microphone \
                       permissions.",
        annotations(read_only_hint = true)
    )]
    async fn list_devices(&self) -> Result<Json<Devices>, String> {
        self.call(Method::Devices).await
    }

    #[tool(
        description = "The live setup document (sources, scenes, recording settings) and its \
                       revision, with the scene collection and profile it is kept in. Revision \
                       0 means no setup has been applied yet. With resolved: true, also where \
                       every item is drawn as laid out (items with a `layout`): each item's \
                       rect in its parent, its box on the canvas and its transform, with the \
                       layout's generation (frames carry the one they show).",
        annotations(read_only_hint = true)
    )]
    async fn get_setup(
        &self,
        Parameters(Strict(params)): Parameters<Strict<GetSetupParams>>,
    ) -> Result<Json<Live>, String> {
        let params = params
            .resolved
            .then_some(isocast_protocol::api::GetParams { resolved: true });
        self.call(Method::Get(params)).await
    }

    #[tool(
        description = "The JSON Schema of a setup document (what get_setup returns and \
                       plan_change, apply_change, start_recording and check_setup take): every \
                       source kind, scene item field, filter, transition and recording setting, \
                       with descriptions. Large; read it before writing a setup or a patch by \
                       hand.",
        annotations(read_only_hint = true)
    )]
    async fn get_setup_schema(&self) -> Result<CallToolResult, String> {
        let schema = serde_json::to_string(&Setup::json_schema()).map_err(|err| err.to_string())?;
        Ok(CallToolResult::success(vec![ContentBlock::text(schema)]))
    }

    #[tool(
        description = "Check this Mac against a setup (the live setup if none is given): \
                       permissions, hardware encoders, whether each source's device is \
                       connected and what format it would record, and the recording disk's \
                       free space and speed. Each check has a status and, if needed, a fix.",
        annotations(read_only_hint = true)
    )]
    async fn check_setup(
        &self,
        Parameters(Strict(params)): Parameters<Strict<CheckSetupParams>>,
    ) -> Result<Json<Doctor>, String> {
        self.call(Method::Doctor(Box::new(DoctorParams {
            setup: parse_setup(params.setup)?,
            folder: params.folder.map(Into::into),
        })))
        .await
    }

    #[tool(
        description = "Predict instantly, without benchmarking, whether a setup's files (the \
                       live setup's if none is given) fit this Mac's H.264/HEVC media engines, \
                       from measured per-chip data: `capacity` is fits, no_margin or \
                       cannot_keep_up, `load` the busiest engine's share, and `suggestions` \
                       the closest changes that fit (each a patch for plan_change, or a \
                       method like stream_stop; `after_take` ones wait for the recording to \
                       end). With add_iso, `admission` says how to turn that ISO file on so it \
                       fits (smaller, slower, or left off). tune_encoders measures instead.",
        annotations(read_only_hint = true)
    )]
    async fn estimate_capacity(
        &self,
        Parameters(Strict(params)): Parameters<Strict<EstimateCapacityParams>>,
    ) -> Result<Json<CapacityEstimate>, String> {
        self.call(Method::EstimateCapacity(Box::new(CapacityParams {
            setup: parse_setup(params.setup)?,
            stream: params.stream,
            add_iso: params.add_iso,
        })))
        .await
    }

    #[tool(
        description = "Benchmark the hardware encoders on a setup's video files (the live \
                       setup if none is given), all at once as in a recording, and say whether \
                       this Mac keeps up: `capacity` is fits, no_margin (keeps up, nothing to \
                       spare) or cannot_keep_up (frames will drop). If it doesn't fit, it \
                       benchmarks changes closest first (a smaller program file, a lower \
                       quality preset, half the frame rate, one ISO file fewer) and recommends \
                       the closest that fits as `change`, a patch for plan_change. start_recording \
                       warns when a setup exceeds what this measured. Takes up to a minute or \
                       two; refused while recording.",
        annotations(read_only_hint = true)
    )]
    async fn tune_encoders(
        &self,
        Parameters(Strict(params)): Parameters<Strict<CheckSetupParams>>,
    ) -> Result<Json<Tuning>, String> {
        self.call(Method::Tune(Box::new(TuneParams {
            setup: parse_setup(params.setup)?,
            folder: params.folder.map(Into::into),
        })))
        .await
    }

    #[tool(
        description = "See what is happening, as a PNG image: the program by default (while \
                       recording, exactly the frame it recorded last), the preview scene, a \
                       scene by id, a source of the live setup (of the recording, while \
                       recording), or a quick source like \"screen\" or \"camera:NAME\".",
        annotations(read_only_hint = true)
    )]
    async fn preview_frame(
        &self,
        Parameters(Strict(params)): Parameters<Strict<PreviewFrameParams>>,
    ) -> Result<CallToolResult, String> {
        let Json(snapshot): Json<Snapshot> = self
            .call(Method::Snapshot(SnapshotParams {
                source: params.source,
                scene: params.scene,
                preview: params.preview,
                width: Some(params.width.unwrap_or(1280)),
            }))
            .await?;
        let of = match (&snapshot.source, &snapshot.scene) {
            (Some(source), _) => format!("source {source}"),
            (None, Some(scene)) => format!("scene {scene}"),
            (None, None) => String::new(),
        };
        Ok(CallToolResult::success(vec![
            ContentBlock::text(format!("{of} at {}×{}", snapshot.width, snapshot.height)),
            ContentBlock::image(snapshot.png_base64, "image/png"),
        ]))
    }

    #[tool(
        description = "See the live engine's newest frame as a JPEG: the program (exactly what \
                       records and streams), studio mode's preview scene, or a running video \
                       source. Starts the live engine if it isn't running, which opens the live \
                       setup's devices.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn live_frame(
        &self,
        Parameters(Strict(params)): Parameters<Strict<LiveFrameParams>>,
    ) -> Result<CallToolResult, String> {
        let Json(frame): Json<Frame> = self
            .call(Method::Frame(FrameParams {
                target: params.target.unwrap_or_else(|| "program".into()),
                max_width: Some(params.width.unwrap_or(1280)),
                after: None,
                timeout_ms: Some(5000),
                quality: Some(0.85),
                fps: None,
            }))
            .await?;
        Ok(CallToolResult::success(vec![
            ContentBlock::text(format!(
                "{} at {}×{}",
                frame.target, frame.width, frame.height
            )),
            ContentBlock::image(frame.jpeg_base64, "image/jpeg"),
        ]))
    }

    #[tool(
        description = "Keep the live engine running (running: true), as OBS does, so the live \
                       setup's sources, program and mix run all the time and recording starts \
                       at once; or let it stop once nothing needs it (running: false). Returns \
                       {engine}: the engine's sources and how each is doing, or null once \
                       stopped.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn live_engine(
        &self,
        Parameters(Strict(params)): Parameters<Strict<EngineParams>>,
    ) -> Result<Json<LiveEngine>, String> {
        let Json(engine) = self
            .call(Method::Engine {
                running: params.running,
            })
            .await?;
        Ok(Json(LiveEngine { engine }))
    }

    #[tool(
        description = "Find OBS Studio's scene collections and profiles on this Mac (its \
                       settings folder, or `folder`) without importing anything: each \
                       collection with scene and source counts and the profile an import would \
                       use (`profile`, a basic.ini path), each profile's canvas, frame rate, \
                       recording encoder, format and folder, and stream service and bitrate \
                       (never the key; `key_in_file` says OBS keeps one in plain text), and \
                       files that couldn't be read. Offer the collections to the user, then \
                       import_obs_collection_or_profile with the collection's `file` and its \
                       `profile` (stream_key only if the user agreed to move the key).",
        annotations(read_only_hint = true)
    )]
    async fn discover_obs(
        &self,
        Parameters(Strict(params)): Parameters<Strict<ObsDiscoverParams>>,
    ) -> Result<Json<Discovery>, String> {
        self.call(Method::ObsDiscover(Some(params))).await
    }

    #[tool(
        description = "Measure the upload speed to a streaming service's ingest (the live \
                       setup's unless `service`/`server` say) with a few seconds of filler \
                       video: Twitch's bandwidth test mode (never on air) or a custom server \
                       (a relay would put the filler on air for a few seconds: ask the user \
                       first), using the stored stream key. YouTube, Facebook and Kick have no test \
                       mode, and without a key nothing can be tested: then `measured` is false, \
                       `reason` says why, and `recommended_kbps` is a safe default. Refused \
                       while streaming.",
        annotations(read_only_hint = true)
    )]
    async fn test_upload_speed(
        &self,
        Parameters(Strict(params)): Parameters<Strict<SpeedTestParams>>,
    ) -> Result<Json<SpeedTest>, String> {
        self.call(Method::SpeedTest(Box::new(params))).await
    }

    #[tool(
        description = "Optimize the live setup for `goal` recording, streaming or both (OBS's \
                       Auto-Configuration Wizard). Recording favours quality and ISO files \
                       (HEVC High, an ISO file per captured source); streaming sets the \
                       service and a bitrate from the upload speed (upload_kbps, or measured \
                       like test_upload_speed; never for a custom server, test it first) and sizes \
                       the stream to it. Then it benchmarks \
                       this Mac like tune_encoders and steps down where it can't keep up. \
                       Returns a one-line `summary`, the optimized setup, its differences from \
                       the live setup, the benchmark and notes; with apply it becomes the live \
                       setup. Takes a minute or two; refused while recording or streaming.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn optimize_setup(
        &self,
        Parameters(Strict(params)): Parameters<Strict<OptimizeParams>>,
    ) -> Result<Json<Optimized>, String> {
        self.call(Method::Optimize(Box::new(params))).await
    }

    #[tool(
        description = "Read an OBS Studio scene collection into an Isocast setup: sources (ids \
                       made from their names), scenes and items, filters and the transition, \
                       devices matched against this Mac. Returns the setup, what couldn't come \
                       over and why, notes to check, and counts. Doesn't change the live setup: \
                       pass the setup to plan_change and apply_change.",
        annotations(read_only_hint = true)
    )]
    async fn import_obs_collection(
        &self,
        Parameters(Strict(params)): Parameters<Strict<ImportObsCollectionParams>>,
    ) -> Result<Json<Imported>, String> {
        self.call(Method::ImportObs(Box::new(ImportObsParams {
            collection: params.collection,
            profile: params.profile,
            width: params.width,
            height: params.height,
            fps: params.fps,
        })))
        .await
    }

    #[tool(
        description = "Make a good starting setup from this Mac's devices: the main display, the \
                       best real camera (USB, then Continuity, then built-in; virtual cameras \
                       only if nothing else), the default microphone and desktop audio, in \
                       scenes screen-and-camera (camera as a corner picture-in-picture), camera \
                       and screen, recording the 1080p60 program plus ISO files of the screen \
                       and camera to ~/Movies/Isocast. Every choice can be overridden. Devices \
                       without permission become placeholders, and the notes say what to do. \
                       With apply, it becomes the live setup.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    async fn init_setup(
        &self,
        Parameters(Strict(params)): Parameters<Strict<InitSetupParams>>,
    ) -> Result<Json<isocast_protocol::init::Initialized>, String> {
        self.call(Method::InitSetup(Box::new(
            isocast_protocol::api::InitParams {
                options: params.options,
                apply: params.apply,
            },
        )))
        .await
    }

    #[tool(
        description = "The screen recording, camera and microphone permissions, without asking, \
                       with the next step for each that isn't granted: request it, or turn it \
                       on in System Settings (which app, which pane, and whether isocastd must \
                       restart afterwards).",
        annotations(read_only_hint = true)
    )]
    async fn get_permissions(&self) -> Result<Json<PermissionsReport>, String> {
        self.call(Method::Permissions).await
    }

    #[tool(
        description = "Restart isocastd, which only sees a permission granted in System \
                       Settings (screen recording, above all) once restarted; the live setup \
                       is kept. Refused, atomically by isocastd, while it records, streams, \
                       exports or feeds the virtual camera.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn restart_isocastd(&self) -> Result<String, String> {
        let events = self.events.clone();
        tokio::task::spawn_blocking(move || {
            crate::client::restart()?;
            events.ensure_watching();
            Ok(())
        })
        .await
        .map_err(|err| format!("{err}"))?
        .map(|()| "isocastd restarted".to_string())
        .map_err(|err: anyhow::Error| format!("{err:#}"))
    }

    #[tool(
        description = "Ask macOS for screen recording, camera and microphone access where it \
                       hasn't been decided yet (or only `permission`). Its prompts appear on the \
                       user's screen; waits up to a minute for answers and returns the \
                       permissions with next steps.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn request_permissions(
        &self,
        Parameters(Strict(params)): Parameters<Strict<RequestPermissionsToolParams>>,
    ) -> Result<Json<PermissionsReport>, String> {
        self.call(Method::RequestPermissions(params.permission.map(
            |permission| isocast_protocol::api::RequestPermissionsParams {
                permission: Some(permission),
            },
        )))
        .await
    }

    #[tool(
        description = "Open System Settings on the user's screen at the pane where a \
                       permission (screen_recording, camera, microphone or system_audio) is \
                       turned on by \
                       hand, for one that was refused.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn open_permission_settings(
        &self,
        Parameters(Strict(params)): Parameters<Strict<OpenPermissionSettingsParams>>,
    ) -> Result<Json<isocast_protocol::api::Opened>, String> {
        self.call(Method::OpenPermissionSettings {
            permission: params.permission,
        })
        .await
    }

    #[tool(
        description = "Work out what a change to the live setup would do, without making it: \
                       the differences (paths use ids) and any problems. Give either a whole \
                       setup or a patch.",
        annotations(read_only_hint = true)
    )]
    async fn plan_change(
        &self,
        Parameters(Strict(params)): Parameters<Strict<ChangeParams>>,
    ) -> Result<Json<Plan>, String> {
        let change = to_change(params)?;
        self.call(Method::Plan(Box::new(change))).await
    }

    #[tool(
        description = "Make a change to the live setup, atomically, if it is still at \
                       `revision` and the result has no problems. It takes effect at once, in a \
                       recording or stream too, except that sources being recorded keep their \
                       devices until the recording stops and the canvas can't change while \
                       recording or streaming.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    async fn apply_change(
        &self,
        Parameters(Strict(params)): Parameters<Strict<ApplyChangeParams>>,
    ) -> Result<Json<Applied>, String> {
        let change = to_change(params.change)?;
        self.call(Method::Apply(Box::new(ApplyParams {
            revision: params.revision,
            change,
            live: params.live,
        })))
        .await
    }

    #[tool(
        description = "Start recording the live setup (or the given setup, which then replaces \
                       the live setup): the program and every ISO source, each to its own file. \
                       Returns once every file is created.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    async fn start_recording(
        &self,
        Parameters(Strict(params)): Parameters<Strict<StartRecordingParams>>,
    ) -> Result<Json<Started>, String> {
        self.call(Method::Start(Box::new(StartParams {
            setup: parse_setup(params.setup)?,
            folder: params.folder.map(Into::into),
            seconds: params.seconds,
        })))
        .await
    }

    #[tool(
        description = "Stop recording and finish every file. Returns the report: each file, \
                       its frames, dropped frames, audio dropouts and the manifest path.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn stop_recording(&self) -> Result<Json<Report>, String> {
        self.call(Method::Stop).await
    }

    #[tool(
        description = "Pause the same recording files on a shared video/audio boundary. Streaming continues. Markers land at the paused playhead; resume before split or retake.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn pause_recording(&self) -> Result<Json<isocast_protocol::api::RecordingPause>, String> {
        self.call(Method::Pause).await
    }

    #[tool(
        description = "Resume a paused recording into its existing files. Paused time is cut out of the files and recorded in the manifest.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn resume_recording(
        &self,
    ) -> Result<Json<isocast_protocol::api::RecordingPause>, String> {
        self.call(Method::Resume).await
    }

    #[tool(
        description = "Split the recording into a new take at the next whole second without \
                       stopping any device: the current files end there and new files, with \
                       their own manifest, begin. Returns the new take's files.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn split_take(&self) -> Result<Json<Started>, String> {
        self.call(Method::Split).await
    }

    #[tool(
        description = "Change an audio source's program mix in the live setup: gain, mute, \
                       sync offset, balance and monitoring (hearing it through the setup's \
                       audio.monitoring_device). A running recording follows all but the \
                       offset at once. Each source's level, and the monitor's, is in \
                       get_status while recording.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn set_audio(
        &self,
        Parameters(Strict(params)): Parameters<Strict<SetAudioParams>>,
    ) -> Result<Json<Applied>, String> {
        let events = self.events.clone();
        tokio::task::spawn_blocking(move || {
            events.ensure_watching();
            let mut client = Client::connect_or_launch()?;
            let change = crate::client::AudioChange {
                mono: params.mono,
                gain_db: params.gain_db,
                muted: params.muted,
                offset_ms: params.offset_ms,
                balance: params.balance,
                monitor: params.monitor,
            };
            crate::client::set_audio(&mut client, &params.source, &change)
        })
        .await
        .map_err(|err| format!("{err}"))?
        .map(Json)
        .map_err(|err| format!("{err:#}"))
    }

    #[tool(
        description = "Choose the audio tracks of one source's ISO file in the live setup \
                       (recording.iso.source_audio): a camera's file with the mic only, a \
                       screen's with desktop audio only. Other ISO files keep \
                       recording.iso.audio, or their own sound. Applies from the next \
                       recording; the manifest lists each file's tracks.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn set_iso_audio(
        &self,
        Parameters(Strict(params)): Parameters<Strict<SetIsoAudioParams>>,
    ) -> Result<Json<Applied>, String> {
        let events = self.events.clone();
        tokio::task::spawn_blocking(move || {
            events.ensure_watching();
            let mut client = Client::connect_or_launch()?;
            crate::client::set_iso_audio(&mut client, &params.source, params.audio.as_deref())
        })
        .await
        .map_err(|err| format!("{err}"))?
        .map(Json)
        .map_err(|err| format!("{err:#}"))
    }

    #[tool(
        description = "Put a scene on air, by id, with a transition: cut, fade, swipe, slide, \
                       fade_to_color, luma_wipe, stinger or move (the scene's transition, else \
                       the setup's, by default). While recording, the switch is written to the \
                       manifest's scene timeline.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn switch_scene(
        &self,
        Parameters(Strict(params)): Parameters<Strict<SwitchSceneParams>>,
    ) -> Result<Json<Switched>, String> {
        self.call(Method::SwitchScene {
            scene: params.scene,
            transition: params.transition,
        })
        .await
    }

    #[tool(
        description = "Stage a scene in preview, by id, without changing what is on air; \
                       take_preview then puts it on air.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn preview_scene(
        &self,
        Parameters(Strict(params)): Parameters<Strict<PreviewSceneParams>>,
    ) -> Result<Json<Switched>, String> {
        self.call(Method::Preview {
            scene: params.scene,
        })
        .await
    }

    #[tool(
        description = "Leave Studio Mode by clearing its staged preview. Program stays on air.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn clear_preview(&self) -> Result<Json<Switched>, String> {
        self.call(Method::ClearPreview).await
    }

    #[tool(
        description = "Put the preview scene on air with a transition (the scene's, else the \
                       setup's, by default); the scene that was on air moves to preview.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn take_preview(
        &self,
        Parameters(Strict(params)): Parameters<Strict<TakeParams>>,
    ) -> Result<Json<Switched>, String> {
        self.call(Method::Take {
            transition: params.transition,
        })
        .await
    }

    #[tool(
        description = "Mark this moment in the recording with a label; it is written to the \
                       manifest with its time.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn add_marker(
        &self,
        Parameters(Strict(params)): Parameters<Strict<AddMarkerParams>>,
    ) -> Result<Json<Marked>, String> {
        self.call(Method::Marker {
            label: params.label,
        })
        .await
    }

    #[tool(
        description = "Reload a browser source's page in the running recording, bypassing the \
                       cache, for example after changing the page or when it stopped updating.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn refresh_browser(
        &self,
        Parameters(Strict(params)): Parameters<Strict<RefreshBrowserParams>>,
    ) -> Result<Json<Refreshed>, String> {
        self.call(Method::RefreshBrowser {
            source: params.source,
        })
        .await
    }
}

#[tool_handler(
    name = "isocast",
    instructions = "Isocast records every source (screens, cameras, iPhone \
screens, microphones, system audio) to its own file on one clock, plus a session.json \
manifest that lines the files up. Typical flow: get_status and list_devices to see what \
exists; get_setup for the live setup (init_setup makes a first one from this Mac's devices; \
get_setup_schema describes every field); plan_change then apply_change (with the plan's \
revision) to edit it; start_recording; switch_scene and add_marker while recording; \
get_events for warnings like dropped frames; stop_recording returns the report with every file. To go live, add a stream to the setup \
(a patch like {\"stream\":{\"service\":\"twitch\"}}), set_stream_key, then start_stream. \
The live setup is a scene collection (scenes, sources, canvas size) plus a profile \
(frame rate, recording, stream, audio), as in OBS: list_collections_and_profiles, \
switch_collection_or_profile and import_obs_collection_or_profile swap and add them. \
Plugins (local folders with an isocast-plugin.toml) are installed with add_plugin, granting \
exactly the permissions they ask for, and listed with list_plugins. \
Text in scenes can show a data key (`data = \"score.home\"`); set_data changes it live \
(get_data reads it), with no setup change. \
Timelines line takes up like a clip bar: new_timeline makes one current, and every take that \
finishes is added to it; retake records the last one again, remove_clip and move_clip edit \
it, trim_clip, split_clip, delete_range and set_angle cut it (undo_edit, redo_edit; takes' \
files are never touched), export_timeline joins it into one file. \
Each recording session writes into its own project folder; list_projects finds them and \
rename_project names one once it has stopped. \
Sources and scenes are addressed by id. Errors name the field and list valid options."
)]
impl ServerHandler for Isocast {}

fn to_change(params: ChangeParams) -> Result<Change, String> {
    match (parse_setup(params.setup)?, params.patch) {
        (Some(setup), None) => Ok(Change::Setup(Box::new(setup))),
        (None, Some(patch)) => Ok(Change::Patch(patch)),
        _ => Err("give either `setup` (a whole setup) or `patch`, not both or neither".into()),
    }
}

/// A setup parameter. The setup's full JSON Schema is large (tens of
/// kilobytes), so tools that take one describe it briefly and
/// get_setup_schema returns it: repeating it in every tool's input schema
/// cost agents hundreds of kilobytes of context.
fn setup_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "type": ["object", "null"] })
}

/// Read a setup parameter, naming the path of anything wrong
/// ("setup.canvas.fsp: unknown field ...").
fn parse_setup(setup: Option<serde_json::Value>) -> Result<Option<Setup>, String> {
    setup
        .map(|value| {
            Setup::from_value(value).map_err(|err| match err.path() {
                "" => format!("setup: {}", err.message()),
                path => format!("setup.{path}: {}", err.message()),
            })
        })
        .transpose()
}

/// Tool parameters that refuse names the tool doesn't have, listing the
/// ones it does, so a typo fails instead of being ignored (a
/// start_recording with "duration" for "seconds" recorded until stopped).
/// serde's deny_unknown_fields can't be used: two of them flatten.
struct Strict<T>(T);

impl<'de, T: DeserializeOwned + JsonSchema> Deserialize<'de> for Strict<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let value = serde_json::Value::deserialize(deserializer)?;
        if let Some(object) = value.as_object() {
            let schema = schemars::SchemaGenerator::default().into_root_schema_for::<T>();
            let known: Vec<&str> = schema
                .get("properties")
                .and_then(serde_json::Value::as_object)
                .map(|properties| properties.keys().map(String::as_str).collect())
                .unwrap_or_default();
            if let Some(unknown) = object.keys().find(|key| !known.contains(&key.as_str())) {
                return Err(D::Error::custom(format!(
                    "unknown parameter `{unknown}`; this tool takes: {}",
                    known.join(", ")
                )));
            }
        }
        // Errors name the path: "transition.direction: unknown variant …".
        isocast_protocol::located::from_value(&value)
            .map(Strict)
            .map_err(D::Error::custom)
    }
}

impl<T: JsonSchema> JsonSchema for Strict<T> {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        T::schema_name()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        T::json_schema(generator)
    }

    fn inline_schema() -> bool {
        T::inline_schema()
    }
}

impl Isocast {
    /// Call the daemon (launching it if needed) off the async runtime,
    /// watching its events first so get_events has what the call causes.
    async fn call<T: DeserializeOwned + Send + 'static>(
        &self,
        method: Method,
    ) -> Result<Json<T>, String> {
        let events = self.events.clone();
        tokio::task::spawn_blocking(move || {
            events.ensure_watching();
            let mut client = Client::connect_or_launch()?;
            // Like the CLI's status/start: warn (stderr, the MCP host's
            // log) once if the engine is older than this isocast (#49).
            static STALE_CHECK: std::sync::Once = std::sync::Once::new();
            STALE_CHECK.call_once(|| crate::client::check_engine(&mut client));
            client.call(method)
        })
        .await
        .map_err(|err| format!("{err}"))?
        .map(Json)
        .map_err(|err: anyhow::Error| format!("{err:#}"))
    }
}
