//! Plugins: folders with an `isocast-plugin.toml` manifest, installed into
//! the plugins folder with `isocast plugin add` (design:
//! docs/scenes-as-code.md, "7. Plugins").
//!
//! A manifest names the plugin, the plugin API it was made for, the
//! permissions it asks for, and what it provides: an external process that
//! is a client of the method table (any language), and kinds for setups
//! (`<plugin id>/<kind id>`: feeds, nodes, filters, transitions, audio
//! filters and sources). Permissions are granted at install, stored with the
//! installed plugin and enforced at run time.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::Issue;

/// The manifest's file name, in the plugin's folder.
pub const MANIFEST: &str = "isocast-plugin.toml";

/// The plugin API versions this Isocast runs. A plugin made for another is
/// installed as `incompatible`: kept, but not run.
pub const API_VERSIONS: &[u32] = &[1];

/// The longest plugin id: its socket lives in a folder named after it, and
/// Unix socket paths are short.
pub const MAX_ID: usize = 40;

/// Methods a plugin can't be granted: with them it could get more than it
/// was granted itself ([`ungrantable_reason`]).
pub const UNGRANTABLE: &[&str] = &[
    "plugin_add",
    "plugin_set_enabled",
    "websocket_enable",
    "websocket_disable",
];

/// Why a plugin can't be granted `method`, if it can't.
pub fn ungrantable_reason(method: &str) -> Option<&'static str> {
    match method {
        "plugin_add" | "plugin_set_enabled" => {
            Some("it could install or enable plugins with more than it was granted itself")
        }
        // The obs-websocket server answers its clients with every method,
        // outside any plugin's grant (#103).
        "websocket_enable" => Some(
            "the obs-websocket server answers its clients with every method, so a plugin that \
             could turn it on, or set its address or password, could call anything",
        ),
        "websocket_disable" => Some(
            "the obs-websocket server's settings are the user's: with this a plugin could turn \
             off the server the user's OBS tools connect to",
        ),
        _ => None,
    }
}

/// A plugin's `isocast-plugin.toml`. A plugin uses only the sections it
/// needs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Lowercase letters, digits and dashes (at most 40), unique among
    /// installed plugins. Its kinds are named `<id>/<kind id>` in setups.
    pub id: String,
    pub name: String,
    /// The plugin's version, semver (like 0.3.1).
    pub version: String,
    /// The plugin API it was made for: 1.
    pub api: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// What it may do, asked for at install: nothing beyond these is
    /// possible.
    #[serde(default, skip_serializing_if = "Permissions::is_empty")]
    pub permissions: Permissions,
    /// Long-running scripts that write data keys.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub feeds: Vec<FeedSpec>,
    /// Content kinds for scene items.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<NodeSpec>,
    /// Video filters (WGSL shaders).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<ShaderSpec>,
    /// Scene transitions (WGSL shaders).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transitions: Vec<ShaderSpec>,
    /// Audio filters (WebAssembly components).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audio_filters: Vec<AudioFilterSpec>,
    /// Sources (WebAssembly components).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<SourceSpec>,
    /// An external process isocastd runs and supervises: a client of the
    /// method table, limited to the methods in `permissions.methods`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<ProcessSpec>,
}

/// What a plugin may do. Asked for in the manifest, granted at install
/// (`plugin add --allow`), and enforced at run time.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Permissions {
    /// Origins it may connect to, like `wss://irc-ws.chat.twitch.tv`. A
    /// process plugin gets the network on or off as a whole: it can't
    /// connect out unless this lists something (it can always serve pages,
    /// which it should bind to 127.0.0.1).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub network: Vec<String>,
    /// Data key prefixes it may read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data_read: Vec<String>,
    /// Data key prefixes it may write.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data_write: Vec<String>,
    /// Methods of the method table its process may call (`get`, `watch`,
    /// `switch_scene`, ...); every other call is refused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<String>,
    /// Files and folders it may read besides its own folder (absolute, or
    /// starting with `~/`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// The recordings folders: `read` (read recording files, nothing
    /// else) or `write` (also add new files inside a project's folder,
    /// like a transcript or an export; never change or delete a file that
    /// exists). Without it a process plugin can't read recordings at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recordings: Option<RecordingsAccess>,
}

/// Access to the recordings folders, granted with `permissions.recordings`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RecordingsAccess {
    /// Read recording files and list the folders.
    Read,
    /// Read, and create new files inside project folders (never modify,
    /// rename or delete an existing file).
    Write,
}

impl RecordingsAccess {
    pub fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

impl Permissions {
    pub fn is_empty(&self) -> bool {
        self.lists().iter().all(|(_, list)| list.is_empty()) && self.recordings.is_none()
    }

    /// Each list by its field name.
    fn lists(&self) -> [(&'static str, &Vec<String>); 5] {
        [
            ("methods", &self.methods),
            ("network", &self.network),
            ("data_read", &self.data_read),
            ("data_write", &self.data_write),
            ("files", &self.files),
        ]
    }

    /// The same permissions, each list sorted and without repeats.
    pub fn normalized(&self) -> Self {
        let tidy = |list: &Vec<String>| {
            let mut list = list.clone();
            list.sort();
            list.dedup();
            list
        };
        Self {
            network: tidy(&self.network),
            data_read: tidy(&self.data_read),
            data_write: tidy(&self.data_write),
            methods: tidy(&self.methods),
            files: tidy(&self.files),
            recordings: self.recordings,
        }
    }

    /// Whether both grant exactly the same, in any order.
    pub fn same_as(&self, other: &Self) -> bool {
        self.normalized() == other.normalized()
    }

    /// What these ask for that `granted` doesn't give.
    pub fn beyond(&self, granted: &Self) -> Self {
        let more = |asked: &Vec<String>, given: &Vec<String>| {
            asked
                .iter()
                .filter(|item| !given.contains(item))
                .cloned()
                .collect()
        };
        Self {
            network: more(&self.network, &granted.network),
            data_read: more(&self.data_read, &granted.data_read),
            data_write: more(&self.data_write, &granted.data_write),
            methods: more(&self.methods, &granted.methods),
            files: more(&self.files, &granted.files),
            recordings: self
                .recordings
                .filter(|asked| granted.recordings < Some(*asked)),
        }
        .normalized()
    }

    /// One line per list that isn't empty (`methods: get, watch`), or
    /// "nothing".
    pub fn describe(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .lists()
            .iter()
            .filter(|(_, list)| !list.is_empty())
            .map(|(name, list)| format!("{name}: {}", list.join(", ")))
            .collect();
        match self.recordings {
            Some(RecordingsAccess::Read) => {
                lines.push("recordings: read (read every recording file)".into())
            }
            Some(RecordingsAccess::Write) => lines.push(
                "recordings: write (read every recording file, and add new files inside \
                 project folders; never change or delete existing ones)"
                    .into(),
            ),
            None => {}
        }
        if lines.is_empty() {
            vec!["nothing (no methods, network, data, files or recordings)".into()]
        } else {
            lines
        }
    }

    /// The methods it may call, for messages: `get, watch` or `nothing`.
    pub fn methods_text(&self) -> String {
        if self.methods.is_empty() {
            "nothing".into()
        } else {
            self.methods.join(", ")
        }
    }
}

/// An external process that is a client of the method table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessSpec {
    /// The program and its arguments, run in the plugin's folder: a program
    /// found on PATH (like `python3`) or a path inside the plugin.
    pub command: Vec<String>,
    /// Start it whenever the plugin is installed and enabled (when
    /// isocastd starts, and at once on `plugin add`). Otherwise it runs
    /// only when a setup uses something it provides.
    #[serde(default)]
    pub autostart: bool,
}

/// A parameter of a plugin kind, or a prop of a node or feed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Param {
    #[serde(rename = "type")]
    pub ty: ParamType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
    /// For numbers (`f32`, `i32`, and each part of `vec2` and `vec4`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    /// For `enum`: the values it can take.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ParamType {
    F32,
    I32,
    Bool,
    String,
    /// `#rrggbb` or `#rrggbbaa`.
    Color,
    /// One of `values`.
    Enum,
    /// A path.
    File,
    Vec2,
    Vec4,
}

impl ParamType {
    fn numeric(self) -> bool {
        matches!(self, Self::F32 | Self::I32 | Self::Vec2 | Self::Vec4)
    }
}

/// A feed: a long-running script that writes data keys.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FeedSpec {
    pub id: String,
    /// The script, inside the plugin's folder.
    pub script: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub props: BTreeMap<String, Param>,
}

/// A content kind for scene items, and what draws it: exactly one of
/// `shader`, `component`, `page` or `process`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NodeSpec {
    pub id: String,
    /// A WGSL file with a `content` function; props are its params.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shader: Option<String>,
    /// A TSX component, run by the scene host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,
    /// An HTML page, shown by a browser source at the item's size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<String>,
    /// The plugin's own process draws it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub process: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub props: BTreeMap<String, Param>,
}

/// A video filter or a transition written as a WGSL shader.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShaderSpec {
    pub id: String,
    /// The WGSL file, inside the plugin's folder.
    pub shader: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, Param>,
}

/// An audio filter written as a WebAssembly component.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AudioFilterSpec {
    pub id: String,
    /// The .wasm component, inside the plugin's folder.
    pub component: String,
    /// It takes a second source's audio as a sidechain.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sidechain: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, Param>,
}

/// A source written as a WebAssembly component.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceSpec {
    pub id: String,
    /// The .wasm component, inside the plugin's folder.
    pub component: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, Param>,
}

/// What a kind is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum KindClass {
    Feed,
    Node,
    Filter,
    Transition,
    AudioFilter,
    Source,
}

impl KindClass {
    /// The manifest section that declares it.
    pub fn section(self) -> &'static str {
        match self {
            Self::Feed => "feeds",
            Self::Node => "nodes",
            Self::Filter => "filters",
            Self::Transition => "transitions",
            Self::AudioFilter => "audio_filters",
            Self::Source => "sources",
        }
    }
}

/// A kind a plugin provides, as setups name it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct KindInfo {
    /// `<plugin id>/<kind id>`.
    pub kind: String,
    pub what: KindClass,
}

/// A plugin's kind where a setup uses it: `kind = "<plugin id>/<kind id>"`
/// with its params flat beside it, as built-in kinds' settings are
/// (`{ kind = "gain/gain", gain_db = -6 }`). Kept as given whether or not
/// the plugin is installed, so a setup from another Mac loads, runs (the
/// kind bypassed) and saves unchanged; isocastd checks the params against
/// the installed plugin's manifest. Params save in name order.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginKind {
    /// `<plugin id>/<kind id>`.
    pub kind: String,
    pub params: serde_json::Map<String, serde_json::Value>,
}

impl PluginKind {
    /// Whether `kind` names a plugin's kind rather than a built-in one.
    pub fn names_plugin(kind: &str) -> bool {
        kind.contains('/')
    }

    /// An object with a `kind` naming a plugin's kind, as one; `None` for
    /// anything else.
    pub fn from_value(value: &serde_json::Value) -> Option<Self> {
        let object = value.as_object()?;
        let kind = object.get("kind")?.as_str()?;
        if !Self::names_plugin(kind) {
            return None;
        }
        let mut params = object.clone();
        params.remove("kind");
        Some(Self {
            kind: kind.to_string(),
            params,
        })
    }

    /// The plugin's id and the kind's.
    pub fn parts(&self) -> (&str, &str) {
        self.kind.split_once('/').unwrap_or((&self.kind, ""))
    }

    /// What is wrong with its name, if anything.
    pub fn name_problem(&self) -> Option<String> {
        let (plugin, kind) = self.parts();
        (!crate::is_valid_id(plugin) || !crate::is_valid_id(kind)).then(|| {
            format!(
                "{:?} isn't a plugin's kind: give `<plugin id>/<kind id>` (lowercase letters, \
                 digits and dashes), like \"gain/gain\"; `isocast plugin list` lists the \
                 installed ones",
                self.kind
            )
        })
    }
}

impl Serialize for PluginKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(self.params.len() + 1))?;
        map.serialize_entry("kind", &self.kind)?;
        for (name, value) in &self.params {
            map.serialize_entry(name, value)?;
        }
        map.end()
    }
}

/// An installed plugin, as checking a setup's use of its kinds needs it.
pub struct Installed<'a> {
    pub manifest: &'a Manifest,
    /// Why its kinds are bypassed (disabled, incompatible), if they are.
    pub bypassed: Option<String>,
}

/// The param every audio filter whose manifest asks for a sidechain takes:
/// the audio source whose sound it hears, as the compressor's.
pub const SIDECHAIN_PARAM: &str = "sidechain_source";

/// Check a setup's uses of plugin kinds against the installed plugins
/// (by id): problems where an installed plugin's kind is used wrongly (no
/// such kind, a param it doesn't have or out of range), and notes where
/// one is kept but bypassed (not installed, disabled, incompatible), with
/// what to do. A kind's name and sidechain source are checked with the
/// rest of the setup ([`crate::Setup::validate`]).
pub fn check_kinds(
    setup: &crate::Setup,
    installed: &BTreeMap<String, Installed>,
) -> (Vec<Issue>, Vec<String>) {
    let mut issues = crate::Issues::default();
    let mut notes = Vec::new();
    for (i, source) in setup.sources.iter().enumerate() {
        for (j, filter) in source.audio_filters.iter().enumerate() {
            let crate::AudioFilterKind::Plugin(used) = &filter.kind else {
                continue;
            };
            if used.name_problem().is_some() {
                continue;
            }
            let path = format!("sources[{i}].audio_filters[{j}]");
            let (plugin, kind) = used.parts();
            let Some(found) = installed.get(plugin) else {
                notes.push(format!(
                    "{path}: the plugin {plugin:?} isn't installed, so {:?} is kept and bypassed \
                     (the sound passes through); install it with `isocast plugin add FOLDER \
                     --allow`",
                    used.kind
                ));
                continue;
            };
            let Some(spec) = found.manifest.audio_filters.iter().find(|s| s.id == kind) else {
                let ids: Vec<&str> = found
                    .manifest
                    .audio_filters
                    .iter()
                    .map(|s| s.id.as_str())
                    .collect();
                issues.add(
                    format!("{path}.kind"),
                    match ids.is_empty() {
                        true => format!("the plugin {plugin:?} has no audio filters"),
                        false => format!(
                            "the plugin {plugin:?} has no audio filter {kind:?}; its audio \
                             filters: {}",
                            ids.iter()
                                .map(|id| format!("{plugin}/{id}"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    },
                );
                continue;
            };
            let mut unknown = crate::UnknownFields::new();
            for (name, value) in &used.params {
                match spec.params.get(name) {
                    Some(param) => {
                        if let Some(message) = value_problem(param, value) {
                            issues.add(format!("{path}.{name}"), message);
                        }
                    }
                    None if name == SIDECHAIN_PARAM && spec.sidechain => {
                        if !value.is_string() {
                            issues.add(
                                format!("{path}.{name}"),
                                format!("{value} isn't a source id"),
                            );
                        }
                    }
                    None => {
                        unknown.insert(name.clone(), value.clone());
                    }
                }
            }
            let mut own: Vec<String> = spec.params.keys().cloned().collect();
            if spec.sidechain {
                own.push(SIDECHAIN_PARAM.into());
            }
            crate::fields::report(
                &mut issues,
                &path,
                &unknown,
                &format!("a {} audio filter", used.kind),
                &own,
                crate::filter::FILTER_FIELDS,
            );
            if let Some(reason) = &found.bypassed {
                notes.push(format!(
                    "{path}: the plugin {plugin:?} {reason}, so {:?} is kept and bypassed (the \
                     sound passes through)",
                    used.kind
                ));
            }
        }
    }
    (issues.0, notes)
}

/// Why a manifest couldn't be read: TOML or a field's type, or the
/// problems found checking it, each naming its field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestError {
    pub issues: Vec<Issue>,
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let lines: Vec<String> = self
            .issues
            .iter()
            .map(|issue| match issue.path.as_str() {
                "" => issue.message.clone(),
                path => format!("{path}: {}", issue.message),
            })
            .collect();
        f.write_str(&lines.join("\n"))
    }
}

impl std::error::Error for ManifestError {}

impl Manifest {
    /// Read a manifest's text and check it; `folder`, if given, is the
    /// plugin's folder, where the files it names must be.
    pub fn parse(text: &str, folder: Option<&Path>) -> Result<Self, ManifestError> {
        let one = |path: String, message: String| ManifestError {
            issues: vec![Issue { path, message }],
        };
        let value: serde_json::Value = toml::from_str(text)
            .map_err(|err: toml::de::Error| one(String::new(), err.to_string()))?;
        let manifest: Self = crate::located::from_value(&value)
            .map_err(|located| one(located.path, located.message))?;
        let issues = manifest.check(folder);
        if issues.is_empty() {
            Ok(manifest)
        } else {
            Err(ManifestError { issues })
        }
    }

    /// Read `<folder>/isocast-plugin.toml` and check it.
    pub fn read(folder: &Path) -> Result<Self, ManifestError> {
        let path = folder.join(MANIFEST);
        let text = std::fs::read_to_string(&path).map_err(|err| ManifestError {
            issues: vec![Issue {
                path: String::new(),
                message: format!(
                    "can't read {} ({err}); a plugin is a folder with an {MANIFEST} in it",
                    path.display()
                ),
            }],
        })?;
        Self::parse(&text, Some(folder))
    }

    /// Problems with the manifest, each naming its field. With `folder`,
    /// the files it names must exist there.
    pub fn check(&self, folder: Option<&Path>) -> Vec<Issue> {
        let mut issues = Vec::new();
        let mut add = |path: String, message: String| issues.push(Issue { path, message });
        if !crate::is_valid_id(&self.id) || self.id.len() > MAX_ID {
            add(
                "id".into(),
                format!(
                    "{:?} must be lowercase letters, digits and dashes, at most {MAX_ID}",
                    self.id
                ),
            );
        }
        if self.name.trim().is_empty() {
            add("name".into(), "give the plugin a name".into());
        }
        if Version::parse(&self.version).is_none() {
            add(
                "version".into(),
                format!(
                    "{:?} isn't a semantic version; give MAJOR.MINOR.PATCH, like 0.3.1",
                    self.version
                ),
            );
        }
        if self.api == 0 {
            add(
                "api".into(),
                format!("0 isn't a plugin API version; this Isocast runs {API_VERSIONS:?}"),
            );
        }
        check_permissions(&self.permissions, &mut add);
        let files = |section: &str, file: &str, add: &mut dyn FnMut(String, String)| {
            if let Some(message) = file_problem(file, folder) {
                add(section.into(), message);
            }
        };
        for (i, feed) in self.feeds.iter().enumerate() {
            let at = format!("feeds[{i}]");
            files(&format!("{at}.script"), &feed.script, &mut add);
            check_params(&format!("{at}.props"), &feed.props, &mut add);
        }
        for (i, node) in self.nodes.iter().enumerate() {
            let at = format!("nodes[{i}]");
            let renderers: Vec<(&str, &String)> = [
                ("shader", &node.shader),
                ("component", &node.component),
                ("page", &node.page),
            ]
            .into_iter()
            .filter_map(|(name, file)| file.as_ref().map(|file| (name, file)))
            .collect();
            match renderers.len() + usize::from(node.process) {
                1 => {}
                0 => add(
                    at.clone(),
                    "say what draws it: one of `shader`, `component`, `page` or `process = true`"
                        .into(),
                ),
                _ => add(
                    at.clone(),
                    "give only one of `shader`, `component`, `page` and `process`".into(),
                ),
            }
            for (name, file) in renderers {
                files(&format!("{at}.{name}"), file, &mut add);
            }
            if node.process && self.process.is_none() {
                add(
                    format!("{at}.process"),
                    "the plugin has no [process] to draw it".into(),
                );
            }
            check_params(&format!("{at}.props"), &node.props, &mut add);
        }
        for (section, specs) in [
            ("filters", &self.filters),
            ("transitions", &self.transitions),
        ] {
            for (i, spec) in specs.iter().enumerate() {
                let at = format!("{section}[{i}]");
                files(&format!("{at}.shader"), &spec.shader, &mut add);
                check_params(&format!("{at}.params"), &spec.params, &mut add);
            }
        }
        for (i, spec) in self.audio_filters.iter().enumerate() {
            let at = format!("audio_filters[{i}]");
            files(&format!("{at}.component"), &spec.component, &mut add);
            check_params(&format!("{at}.params"), &spec.params, &mut add);
            for (name, param) in &spec.params {
                if matches!(param.ty, ParamType::Vec2 | ParamType::Vec4) {
                    add(
                        format!("{at}.params.{name}.type"),
                        "an audio filter's params can't be vec2 or vec4 (a component gets f32, \
                         i32, bool, string, color, enum and file values); use an f32 per part"
                            .into(),
                    );
                }
                if name == SIDECHAIN_PARAM {
                    add(
                        format!("{at}.params.{name}"),
                        format!(
                            "{SIDECHAIN_PARAM} is the param `sidechain = true` gives a filter; \
                             leave it out of params"
                        ),
                    );
                }
            }
        }
        for (i, spec) in self.sources.iter().enumerate() {
            let at = format!("sources[{i}]");
            files(&format!("{at}.component"), &spec.component, &mut add);
            check_params(&format!("{at}.params"), &spec.params, &mut add);
        }
        for (class, ids) in self.kind_ids() {
            let mut seen = Vec::new();
            for (i, id) in ids.iter().enumerate() {
                let path = format!("{}[{i}].id", class.section());
                if !crate::is_valid_id(id) {
                    add(
                        path,
                        format!("{id:?} must be lowercase letters, digits and dashes"),
                    );
                } else if seen.contains(id) {
                    add(
                        path,
                        format!("another of its {} is {id:?}", class.section()),
                    );
                }
                seen.push(id.clone());
            }
        }
        if let Some(process) = &self.process {
            match process.command.first() {
                None => add(
                    "process.command".into(),
                    "give the program to run and its arguments, like [\"python3\", \"bot.py\"]"
                        .into(),
                ),
                Some(program) if program.trim().is_empty() => {
                    add("process.command[0]".into(), "the program is empty".into());
                }
                Some(program)
                    if program.contains('/')
                        || (cfg!(windows) && program.contains(['\\', ':'])) =>
                {
                    if let Some(message) = file_problem(program, folder) {
                        add("process.command[0]".into(), message);
                    }
                }
                Some(_) => {}
            }
        }
        if self.process.is_none() && self.kinds().is_empty() {
            add(
                String::new(),
                "the plugin provides nothing: add a [process] or a [[feeds]], [[nodes]], \
                 [[filters]], [[transitions]], [[audio_filters]] or [[sources]] section"
                    .into(),
            );
        }
        issues
    }

    fn kind_ids(&self) -> [(KindClass, Vec<String>); 6] {
        fn ids<T>(list: &[T], id: fn(&T) -> &String) -> Vec<String> {
            list.iter().map(|item| id(item).clone()).collect()
        }
        [
            (KindClass::Feed, ids(&self.feeds, |s| &s.id)),
            (KindClass::Node, ids(&self.nodes, |s| &s.id)),
            (KindClass::Filter, ids(&self.filters, |s| &s.id)),
            (KindClass::Transition, ids(&self.transitions, |s| &s.id)),
            (KindClass::AudioFilter, ids(&self.audio_filters, |s| &s.id)),
            (KindClass::Source, ids(&self.sources, |s| &s.id)),
        ]
    }

    /// The kinds it provides, as setups name them.
    pub fn kinds(&self) -> Vec<KindInfo> {
        self.kind_ids()
            .into_iter()
            .flat_map(|(what, ids)| {
                ids.into_iter().map(move |id| KindInfo {
                    kind: format!("{}/{id}", self.id),
                    what,
                })
            })
            .collect()
    }

    /// Whether this Isocast runs the plugin API it was made for.
    pub fn compatible(&self) -> bool {
        API_VERSIONS.contains(&self.api)
    }
}

/// What is wrong with a file a manifest names, if anything: it must be a
/// relative path inside the plugin's folder, and exist there.
fn file_problem(file: &str, folder: Option<&Path>) -> Option<String> {
    let path = Path::new(file);
    if file.is_empty() {
        return Some("give a file inside the plugin's folder".into());
    }
    if path.is_absolute()
        || path.components().any(|part| {
            matches!(
                part,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        })
    {
        return Some(format!(
            "{file:?} must be a path inside the plugin's folder (relative, without `..`)"
        ));
    }
    let folder = folder?;
    (!folder.join(path).exists()).then(|| format!("{file:?} isn't in {}", folder.display()))
}

fn check_permissions(permissions: &Permissions, add: &mut dyn FnMut(String, String)) {
    let methods = crate::api::method_names();
    for (i, method) in permissions.methods.iter().enumerate() {
        let path = format!("permissions.methods[{i}]");
        if let Some(reason) = ungrantable_reason(method) {
            add(
                path,
                format!("a plugin can't be granted `{method}`: {reason}"),
            );
        } else if !methods.iter().any(|name| name == method) {
            add(
                path,
                format!(
                    "{method:?} isn't a method; methods: {}",
                    methods
                        .iter()
                        .filter(|name| !UNGRANTABLE.contains(&name.as_str()))
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
        }
    }
    for (i, origin) in permissions.network.iter().enumerate() {
        if let Some(message) = origin_problem(origin) {
            add(format!("permissions.network[{i}]"), message);
        }
    }
    for (field, list) in [
        ("data_read", &permissions.data_read),
        ("data_write", &permissions.data_write),
    ] {
        for (i, prefix) in list.iter().enumerate() {
            if prefix.is_empty() || prefix.chars().any(|c| c.is_whitespace() || c.is_control()) {
                add(
                    format!("permissions.{field}[{i}]"),
                    format!("{prefix:?} isn't a data key prefix (no spaces, not empty)"),
                );
            }
        }
    }
    for (i, file) in permissions.files.iter().enumerate() {
        if !file.starts_with('/') && !file.starts_with("~/") {
            add(
                format!("permissions.files[{i}]"),
                format!("{file:?} must be absolute or start with ~/"),
            );
        }
    }
}

/// What is wrong with a network origin, if anything: `scheme://host` or
/// `scheme://host:port`, for http, https, ws or wss.
fn origin_problem(origin: &str) -> Option<String> {
    let usage = "give an origin like https://api.example.com or wss://irc-ws.chat.twitch.tv:443 \
                 (http, https, ws or wss; no path)";
    let Some((scheme, rest)) = origin.split_once("://") else {
        return Some(format!("{origin:?} isn't an origin; {usage}"));
    };
    if !["http", "https", "ws", "wss"].contains(&scheme) {
        return Some(format!("{origin:?} has scheme {scheme:?}; {usage}"));
    }
    let (host, port) = match rest.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (rest, None),
    };
    let host_ok = !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '*');
    let port_ok = port.is_none_or(|port| port.parse::<u16>().is_ok_and(|port| port > 0));
    (!host_ok || !port_ok).then(|| format!("{origin:?} isn't an origin; {usage}"))
}

fn check_params(at: &str, params: &BTreeMap<String, Param>, add: &mut dyn FnMut(String, String)) {
    for (name, param) in params {
        let path = format!("{at}.{name}");
        let named = name.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if !named {
            add(
                path.clone(),
                format!("{name:?} must be lowercase letters, digits and underscores"),
            );
        }
        if (param.min.is_some() || param.max.is_some()) && !param.ty.numeric() {
            add(
                path.clone(),
                "min and max are only for f32, i32, vec2 and vec4".into(),
            );
        }
        if let (Some(min), Some(max)) = (param.min, param.max)
            && min > max
        {
            add(
                format!("{path}.min"),
                format!("{min} is over the maximum {max}"),
            );
        }
        match (param.ty, param.values.is_empty()) {
            (ParamType::Enum, true) => {
                add(
                    format!("{path}.values"),
                    "list the values an enum takes".into(),
                );
            }
            (ty, false) if ty != ParamType::Enum => {
                add(format!("{path}.values"), "only an enum has values".into());
            }
            _ => {}
        }
        if let Some(default) = &param.default
            && let Some(message) = value_problem(param, default)
        {
            add(format!("{path}.default"), message);
        }
    }
}

/// What is wrong with `value` for `param`, if anything.
pub fn value_problem(param: &Param, value: &serde_json::Value) -> Option<String> {
    use serde_json::Value;
    let range = |n: f64| -> Option<String> {
        if let Some(min) = param.min
            && n < min
        {
            return Some(format!("{n} is under the minimum {min}"));
        }
        if let Some(max) = param.max
            && n > max
        {
            return Some(format!("{n} is over the maximum {max}"));
        }
        None
    };
    let numbers = |count: usize| -> Option<String> {
        match value.as_array() {
            Some(items) if items.len() == count && items.iter().all(Value::is_number) => {
                items.iter().find_map(|n| range(n.as_f64()?))
            }
            _ => Some(format!("{value} isn't {count} numbers")),
        }
    };
    match param.ty {
        ParamType::F32 => match value.as_f64() {
            Some(n) => range(n),
            None => Some(format!("{value} isn't a number")),
        },
        ParamType::I32 => match value.as_i64() {
            Some(n) if i32::try_from(n).is_ok() => range(n as f64),
            _ => Some(format!("{value} isn't a whole number")),
        },
        ParamType::Bool => (!value.is_boolean()).then(|| format!("{value} isn't true or false")),
        ParamType::String | ParamType::File => {
            (!value.is_string()).then(|| format!("{value} isn't a string"))
        }
        ParamType::Color => {
            let ok = value.as_str().is_some_and(|text| {
                text.strip_prefix('#').is_some_and(|hex| {
                    (hex.len() == 6 || hex.len() == 8) && hex.chars().all(|c| c.is_ascii_hexdigit())
                })
            });
            (!ok).then(|| format!("{value} isn't a colour like \"#ff8800\" or \"#ff880080\""))
        }
        ParamType::Enum => match value.as_str() {
            Some(text) if param.values.iter().any(|v| v == text) => None,
            _ => Some(format!("{value} isn't one of: {}", param.values.join(", "))),
        },
        ParamType::Vec2 => numbers(2),
        ParamType::Vec4 => numbers(4),
    }
}

/// A semantic version (MAJOR.MINOR.PATCH, with an optional `-pre` and a
/// `+build` that ordering ignores).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Option<String>,
}

impl Version {
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.split_once('+').map_or(text, |(version, _)| version);
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) if !pre.is_empty() => (core, Some(pre.to_string())),
            Some(_) => return None,
            None => (text, None),
        };
        let mut parts = core.split('.');
        let mut number = || -> Option<u64> {
            let part = parts.next()?;
            if part.is_empty() || (part.len() > 1 && part.starts_with('0')) {
                return None;
            }
            part.parse().ok()
        };
        let version = Self {
            major: number()?,
            minor: number()?,
            patch: number()?,
            pre,
        };
        parts.next().is_none().then_some(version)
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => std::cmp::Ordering::Equal,
                // A pre-release comes before its release.
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (Some(a), Some(b)) => a.cmp(b),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Parameters of `plugin_add`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginAddParams {
    /// The plugin's folder (with its isocast-plugin.toml), as an absolute
    /// path.
    pub path: PathBuf,
    /// Keep a link to the folder instead of copying it (for developing a
    /// plugin): it is read again each time the plugin starts.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub link: bool,
    /// The permissions granted, which must be exactly what the manifest
    /// asks for. Without it nothing is installed, and the error lists what
    /// the plugin asks for. An upgrade that asks for no more than was
    /// granted before needs none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<Permissions>,
    /// Install it even if the same or a newer version is installed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force: bool,
}

/// Which plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginTarget {
    /// The plugin's id.
    pub id: String,
    /// Remove it even though the live setup uses its kinds: the setup
    /// keeps them as they are, bypassed (a filter's sound passes through),
    /// until the plugin is added again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force: bool,
}

/// Where a setup uses plugin `id`'s kinds: `sources[1].audio_filters[0]
/// (gain/gain)`.
pub fn uses(setup: &crate::Setup, id: &str) -> Vec<String> {
    let mut found = Vec::new();
    for (i, source) in setup.sources.iter().enumerate() {
        for (j, filter) in source.audio_filters.iter().enumerate() {
            if let crate::AudioFilterKind::Plugin(used) = &filter.kind
                && used.parts().0 == id
            {
                found.push(format!("sources[{i}].audio_filters[{j}] ({})", used.kind));
            }
        }
    }
    found
}

/// Parameters of `plugin_set_enabled`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginEnableParams {
    /// The plugin's id.
    pub id: String,
    /// true runs it again (and forgets a crash count that disabled it);
    /// false stops its process and bypasses its kinds.
    pub enabled: bool,
}

/// The installed plugins. Returned by `plugin_list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Plugins {
    /// Where they are installed.
    pub folder: PathBuf,
    pub plugins: Vec<PluginInfo>,
}

/// An installed plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub api: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Its files: its copy in the plugins folder, or the folder it links
    /// to.
    pub path: PathBuf,
    /// It links to the folder it was added from (`--link`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub linked: bool,
    /// The kinds it provides for setups.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<KindInfo>,
    pub state: PluginState,
    /// Why it is disabled, incompatible or failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Its process, if it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<ProcessInfo>,
    /// Where its process's output and its components' log lines go.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<PathBuf>,
    /// What it was granted at install.
    pub granted: Permissions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PluginState {
    Ok,
    /// Turned off (`plugin disable`, or after crashing too often): its
    /// process doesn't run and its kinds are bypassed.
    Disabled,
    /// Made for a plugin API this Isocast doesn't run; its kinds are
    /// bypassed.
    Incompatible,
    /// Its manifest can't be read any more (a linked folder moved, say).
    Failed,
}

/// A plugin's process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessInfo {
    pub state: ProcessState,
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Times it was restarted after exiting on its own, since isocastd
    /// started or the plugin was enabled.
    pub restarts: u32,
    /// How it last ended, like "killed by signal 9 (12 s ago)".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_exit: Option<String>,
    /// It runs in a sandbox (macOS Seatbelt): no camera, microphone or
    /// screen, the home folder unreadable but for its own folders, writes
    /// only in its data folder, the network only if granted.
    pub sandboxed: bool,
    /// The socket it reaches isocastd on (its `ISOCAST_SOCKET`), where
    /// every call is checked against its grant.
    pub socket: PathBuf,
    /// Its output.
    pub log: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProcessState {
    Running,
    /// It exited and starts again after a pause (1 s, doubling to 60 s).
    Restarting,
    /// Not running: the plugin is disabled, or isn't started on its own
    /// (no `autostart`).
    Stopped,
}

/// What a plugin change did. Returned by `plugin_add`, `plugin_remove`
/// and `plugin_set_enabled`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PluginChange {
    pub action: PluginAction,
    pub plugin: PluginInfo,
    /// Things to know, like what this Isocast doesn't run yet.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PluginAction {
    Added,
    /// A newer version replaced the installed one.
    Upgraded,
    /// The same or an older version replaced it (`--force`).
    Reinstalled,
    Removed,
    Enabled,
    Disabled,
    /// Its process exited on its own; it is restarted, or disabled if it
    /// keeps crashing.
    Crashed,
    /// Its process started again after crashing.
    Restarted,
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
id = "twitch-chat"
name = "Twitch chat"
version = "0.3.1"
api = 1
description = "Anonymous Twitch chat as data and as an overlay."
license = "MIT"

[permissions]
network = ["wss://irc-ws.chat.twitch.tv"]
data_read = ["twitch"]
data_write = ["twitch"]
methods = []
files = []

[[feeds]]
id = "chat"
script = "chat.ts"
props = { channel = { type = "string", doc = "The channel to read." } }

[[nodes]]
id = "chat-box"
component = "ChatBox.tsx"
props.channel = { type = "string" }
props.max = { type = "i32", default = 8, min = 1, max = 50, doc = "Lines shown." }

[[filters]]
id = "glow"
shader = "glow.wgsl"
params.amount = { type = "f32", default = 0.5, min = 0.0, max = 1.0, doc = "How strong the glow is." }

[[transitions]]
id = "ripple"
shader = "ripple.wgsl"

[[audio_filters]]
id = "deesser"
component = "deesser.wasm"
sidechain = false

[[sources]]
id = "spectrum"
component = "spectrum.wasm"

[process]
command = ["python3", "bot.py"]
autostart = true
"#;

    fn messages(text: &str) -> Vec<String> {
        Manifest::parse(text, None)
            .unwrap_err()
            .issues
            .into_iter()
            .map(|issue| format!("{}: {}", issue.path, issue.message))
            .collect()
    }

    #[test]
    fn the_designs_example_and_the_deck_read() {
        let full = Manifest::parse(FULL, None).unwrap();
        let kinds: Vec<String> = full.kinds().into_iter().map(|k| k.kind).collect();
        assert_eq!(
            kinds,
            [
                "twitch-chat/chat",
                "twitch-chat/chat-box",
                "twitch-chat/glow",
                "twitch-chat/ripple",
                "twitch-chat/deesser",
                "twitch-chat/spectrum"
            ]
        );
        assert!(full.compatible());
        let deck = include_str!("../../../examples/plugins/deck/isocast-plugin.toml");
        let deck = Manifest::parse(deck, None).unwrap();
        assert_eq!(deck.id, "deck");
        assert!(deck.process.as_ref().is_some_and(|p| p.autostart));
        assert!(
            deck.permissions
                .methods
                .contains(&"switch_scene".to_string())
        );
    }

    #[test]
    fn problems_name_their_fields() {
        let text = FULL
            .replace(
                "methods = []",
                "methods = [\"get\", \"shutdwn\", \"plugin_add\"]",
            )
            .replace("min = 1, max = 50", "min = 1, max = 5")
            .replace(
                "\"wss://irc-ws.chat.twitch.tv\"",
                "\"irc-ws.chat.twitch.tv\"",
            );
        let found = messages(&text);
        assert!(
            found.iter().any(|m| m
                .starts_with("permissions.methods[1]: \"shutdwn\" isn't a method; methods: ")
                && m.contains("shutdown")
                && !m.contains("plugin_add,")),
            "{found:?}"
        );
        assert!(
            found
                .iter()
                .any(|m| m
                    .starts_with("permissions.methods[2]: a plugin can't be granted `plugin_add`")),
            "{found:?}"
        );
        assert!(
            found
                .iter()
                .any(|m| m == "nodes[0].props.max.default: 8 is over the maximum 5"),
            "{found:?}"
        );
        assert!(
            found
                .iter()
                .any(|m| m.starts_with("permissions.network[0]: ")),
            "{found:?}"
        );
        // Field types and unknown fields, with their paths.
        let found = messages(&FULL.replace("autostart = true", "autostart = true\nautostrat = 1"));
        assert!(
            found[0].starts_with("process.autostrat: unknown field `autostrat`"),
            "{found:?}"
        );
        let found = messages(&FULL.replace("api = 1", "api = \"one\""));
        assert!(found[0].starts_with("api: "), "{found:?}");
        let found = messages("id = \"Deck\"\nname = \"\"\nversion = \"1.0\"\napi = 1\n");
        assert!(found.iter().any(|m| m.starts_with("id: ")), "{found:?}");
        assert!(
            found.iter().any(|m| m.starts_with("version: ")),
            "{found:?}"
        );
        assert!(
            found
                .iter()
                .any(|m| m.starts_with(": the plugin provides nothing")),
            "{found:?}"
        );
        let found = messages(&FULL.replace("\"glow.wgsl\"", "\"../glow.wgsl\""));
        assert!(
            found.iter().any(|m| m.starts_with("filters[0].shader: ")),
            "{found:?}"
        );
        assert!(file_problem("/outside.wgsl", None).is_some());
        #[cfg(windows)]
        for path in [
            r"\outside.wgsl",
            r"C:\outside.wgsl",
            r"C:outside.wgsl",
            r"..\outside.wgsl",
        ] {
            assert!(file_problem(path, None).is_some(), "{path}");
            let command = format!("command = [{}]", serde_json::to_string(path).unwrap());
            let found = messages(&FULL.replace("command = [\"python3\", \"bot.py\"]", &command));
            assert!(
                found.iter().any(|m| m.starts_with("process.command[0]: ")),
                "{found:?}"
            );
        }
    }

    #[test]
    fn versions_order_and_permissions_compare() {
        let v = |text| Version::parse(text).unwrap();
        assert!(v("0.3.1") < v("0.10.0"));
        assert!(v("1.0.0-beta") < v("1.0.0"));
        assert_eq!(v("1.2.3+build.5"), v("1.2.3"));
        for bad in ["1.0", "1.0.0.0", "01.0.0", "1.0.0-", "a.b.c"] {
            assert_eq!(Version::parse(bad), None, "{bad}");
        }
        let asked = Permissions {
            methods: vec!["watch".into(), "get".into(), "marker".into()],
            ..Default::default()
        };
        let granted = Permissions {
            methods: vec!["get".into(), "watch".into()],
            ..Default::default()
        };
        assert!(!asked.same_as(&granted));
        assert_eq!(asked.beyond(&granted).methods, ["marker"]);
        assert!(granted.same_as(&Permissions {
            methods: vec!["watch".into(), "get".into(), "get".into()],
            ..Default::default()
        }));
    }

    const USES: &str = r##"
version = 1
name = "t"
canvas = { width = 16, height = 16, fps = 30 }
[[sources]]
id = "music"
kind = "tone"
audio_filters = [
  { kind = "gain/gain", gain_db = -6, fault = "none" },
  { kind = "gain/ducker", sidechain_source = "voice", ratio = 4 },
  { id = "typo", kind = "gain/gain", gain = 3, gain_db = 40 },
  { id = "other", kind = "gain/echo" },
  { id = "away", kind = "chorus/wide", depth = 2 },
]
[[sources]]
id = "voice"
kind = "tone"
[recording]
folder = "."
program = { enabled = false, encoder = { codec = "hevc" } }
"##;

    #[test]
    fn plugin_kinds_in_setups_round_trip_and_are_checked_by_name() {
        let setup = crate::Setup::from_toml(USES).unwrap();
        let toml = setup.to_toml().unwrap();
        assert_eq!(crate::Setup::from_toml(&toml).unwrap(), setup);
        let json = serde_json::to_string(&setup).unwrap();
        assert_eq!(crate::Setup::from_json(&json).unwrap(), setup);
        // Kept as given: the kind and every param, nothing reported unknown.
        let crate::AudioFilterKind::Plugin(gain) = &setup.sources[0].audio_filters[0].kind else {
            panic!("not a plugin kind");
        };
        assert_eq!(gain.kind, "gain/gain");
        assert_eq!(gain.params["gain_db"], -6);
        assert!(setup.sources[0].audio_filters[0].unknown.is_empty());
        assert_eq!(setup.sources[0].audio_filters[1].sidechain(), Some("voice"));
        assert!(setup.validate().is_empty(), "{:?}", setup.validate());
        assert_eq!(
            uses(&setup, "gain"),
            [
                "sources[0].audio_filters[0] (gain/gain)",
                "sources[0].audio_filters[1] (gain/ducker)",
                "sources[0].audio_filters[2] (gain/gain)",
                "sources[0].audio_filters[3] (gain/echo)",
            ]
        );
        // A bad name, and a sidechain that isn't another audio source.
        let bad = USES.replace("\"chorus/wide\"", "\"Chorus/\"").replace(
            "sidechain_source = \"voice\"",
            "sidechain_source = \"music\"",
        );
        let issues = crate::Setup::from_toml(&bad).unwrap().validate();
        let paths: Vec<&str> = issues.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "sources.music.audio_filters[1].sidechain_source",
                "sources.music.audio_filters[4].kind"
            ],
            "{issues:?}"
        );
        // A built-in kind misspelt says plugin kinds exist too.
        let err = crate::Setup::from_toml(&USES.replace("\"gain/echo\"", "\"gian\"")).unwrap_err();
        assert!(
            err.to_string().contains("or a plugin's audio filter"),
            "{err}"
        );
    }

    #[test]
    fn plugin_kinds_are_checked_against_the_installed_manifests() {
        let manifest = Manifest::parse(
            r#"
id = "gain"
name = "Gain"
version = "0.1.0"
api = 1
[[audio_filters]]
id = "gain"
component = "gain.wasm"
params.gain_db = { type = "f32", default = 0.0, min = -30.0, max = 30.0 }
params.fault = { type = "enum", values = ["none", "trap"], default = "none" }
[[audio_filters]]
id = "ducker"
component = "ducker.wasm"
sidechain = true
params.ratio = { type = "f32", default = 10.0, min = 1.0, max = 32.0 }
"#,
            None,
        )
        .unwrap();
        let setup = crate::Setup::from_toml(USES).unwrap();
        let installed = |bypassed: Option<&str>| {
            BTreeMap::from([(
                "gain".to_string(),
                Installed {
                    manifest: &manifest,
                    bypassed: bypassed.map(String::from),
                },
            )])
        };
        let (problems, notes) = check_kinds(&setup, &installed(None));
        let found: Vec<String> = problems
            .iter()
            .map(|p| format!("{}: {}", p.path, p.message))
            .collect();
        assert_eq!(found.len(), 3, "{found:#?}");
        assert!(found[0].starts_with("sources[0].audio_filters[2].gain_db: 40 is over"));
        assert!(found[1].starts_with("sources[0].audio_filters[2].gain: "));
        assert!(
            found[1].contains("did you mean \"gain_db\""),
            "{}",
            found[1]
        );
        assert!(found[2].starts_with("sources[0].audio_filters[3].kind: "));
        assert!(found[2].ends_with("its audio filters: gain/gain, gain/ducker"));
        // A plugin that isn't installed is a note, with what to do.
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].starts_with("sources[0].audio_filters[4]: the plugin \"chorus\""));
        assert!(notes[0].contains("isocast plugin add"));
        // One that is turned off: a note for each use.
        let (_, notes) = check_kinds(&setup, &installed(Some("is turned off")));
        assert_eq!(notes.len(), 4, "{notes:#?}");
        assert!(notes[0].contains("is turned off"));
    }
}
