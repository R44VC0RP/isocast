//! Scene collections and profiles, as in OBS: the live setup kept in two
//! named parts that are stored side by side and swapped independently.
//!
//! A **scene collection** is what is shown: the sources, the scenes that
//! arrange them, the transitions, the canvas size they are laid out on, and
//! which of its sources the recordings take (the program's audio, the ISO
//! files and their audio). A **profile** is how it is put out: the frame
//! rate, the recording (folder, file names, encoders, the program file's
//! size), the stream and the audio settings. Each part uses the setup's own
//! field names (a collection has `canvas.width`, a profile `canvas.fps`);
//! together they make the live [`Setup`] ([`Setup::from_parts`],
//! [`Setup::into_parts`]).
//!
//! Where things live, and why:
//! - The canvas **size** is the collection's: items are placed in canvas
//!   pixels, so a layout only fits the canvas it was made on, and switching
//!   profiles must never move or clip what a scene shows. (OBS keeps the
//!   base resolution in the profile, so a profile switch can leave scenes
//!   laid out for another canvas.) The **frame rate** is the profile's, an
//!   output choice like OBS's: record at 60, stream at 30.
//! - Audio **devices** are sources, so they are the collection's (OBS also
//!   saves its global audio devices in the scene collection); the
//!   monitoring device and the pan law are the profile's, like OBS's
//!   Settings → Audio.
//! - Which sources are recorded is the collection's (they must exist in
//!   it); whether ISO files are written at all, and how, is the profile's.

use std::collections::BTreeMap;
use std::path::PathBuf;

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::obs::Unsupported;
use crate::stream::StreamSettings;
use crate::{
    AudioSetup, Canvas, Encoder, IsoRecording, Issue, Keyframes, LoadError, ProgramRecording,
    Recording, SETUP_VERSION, Scene, Setup, Source, Transition, default_iso_filename,
    default_keep_free_gb, default_program_filename, default_true,
};

/// A scene collection or a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LibraryKind {
    /// Scenes, sources, transitions and the canvas size.
    Collection,
    /// Frame rate, recording, stream and audio settings.
    Profile,
}

impl LibraryKind {
    pub const ALL: [LibraryKind; 2] = [LibraryKind::Collection, LibraryKind::Profile];

    /// "scene collection" or "profile".
    pub fn label(self) -> &'static str {
        match self {
            Self::Collection => "scene collection",
            Self::Profile => "profile",
        }
    }

    /// The `isocast` command that manages this kind.
    pub fn command(self) -> &'static str {
        match self {
            Self::Collection => "collection",
            Self::Profile => "profile",
        }
    }

    /// The folder this kind is kept in, inside the library's.
    pub fn folder(self) -> &'static str {
        match self {
            Self::Collection => "collections",
            Self::Profile => "profiles",
        }
    }
}

/// A scene collection: what the program shows. Saved as
/// `collections/<id>.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Collection {
    /// Format version. Must be 1.
    pub version: u32,
    /// The collection's name (the live setup's `name`).
    pub name: String,
    /// The canvas size the scenes are laid out on.
    pub canvas: CollectionCanvas,
    #[serde(default)]
    pub sources: Vec<Source>,
    #[serde(default)]
    pub scenes: Vec<Scene>,
    /// Keyframe sets the scenes' items play, by id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keyframes: Vec<Keyframes>,
    #[serde(default, skip_serializing_if = "Transition::is_cut")]
    pub transition: Transition,
    /// Which of the collection's sources the recordings take.
    #[serde(default, skip_serializing_if = "CollectionRecording::is_empty")]
    pub recording: CollectionRecording,
    #[serde(
        default,
        skip_serializing_if = "crate::virtual_camera::VirtualCameraSettings::is_default"
    )]
    pub virtual_camera: crate::virtual_camera::VirtualCameraSettings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CollectionCanvas {
    /// An even number of pixels from 16 to 8192.
    pub width: u32,
    /// An even number of pixels from 16 to 8192.
    pub height: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CollectionRecording {
    #[serde(default, skip_serializing_if = "CollectionProgram::is_empty")]
    pub program: CollectionProgram,
    #[serde(default, skip_serializing_if = "CollectionIso::is_empty")]
    pub iso: CollectionIso,
}

impl CollectionRecording {
    fn is_empty(&self) -> bool {
        self.program.is_empty() && self.iso.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CollectionProgram {
    /// Audio sources mixed into the program recording.
    #[serde(default)]
    pub audio: Vec<String>,
}

impl CollectionProgram {
    fn is_empty(&self) -> bool {
        self.audio.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CollectionIso {
    /// Video sources recorded to files of their own (when the profile's
    /// `recording.iso.enabled`).
    #[serde(default)]
    pub sources: Vec<String>,
    /// Explicit shared ISO tracks; omitted uses each source's own sound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<Vec<String>>,
    /// Tracks for one ISO file, by its source, instead of `audio`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub source_audio: BTreeMap<String, Vec<String>>,
}

impl CollectionIso {
    fn is_empty(&self) -> bool {
        self.sources.is_empty() && self.audio.is_none() && self.source_audio.is_empty()
    }
}

/// A profile: how the program is recorded and streamed. Saved as
/// `profiles/<id>.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Format version. Must be 1.
    pub version: u32,
    /// The profile's name.
    pub name: String,
    pub canvas: ProfileCanvas,
    /// How sources' balance pans them, and where monitored sources play.
    #[serde(default, skip_serializing_if = "AudioSetup::is_default")]
    pub audio: AudioSetup,
    pub recording: ProfileRecording,
    /// Live streaming of the program, if set up (the key stays in the
    /// Keychain).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<StreamSettings>,
    #[serde(default, skip_serializing_if = "crate::hotkeys::Config::is_absent")]
    pub hotkeys: crate::hotkeys::Config,
    #[serde(
        default,
        skip_serializing_if = "crate::developer::DeveloperSettings::is_default"
    )]
    pub developer: crate::developer::DeveloperSettings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileCanvas {
    /// Frames per second, from 1 to 240.
    pub fps: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRecording {
    /// Folder recordings are written to. `~` expands to the home folder.
    pub folder: String,
    pub program: ProfileProgram,
    #[serde(default)]
    pub iso: ProfileIso,
    /// Free space, in GB, a recording never uses.
    #[serde(default = "default_keep_free_gb")]
    pub keep_free_gb: f64,
    /// The replay buffer: how much of the program it keeps.
    #[serde(
        default,
        skip_serializing_if = "crate::replay::ReplaySettings::is_default"
    )]
    pub replay: crate::replay::ReplaySettings,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileProgram {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// File name without extension. Tokens: {date}, {time}.
    #[serde(default = "default_program_filename")]
    pub filename: String,
    pub encoder: Encoder,
    /// The program file's size, if not the canvas's (each side even, at
    /// most the collection's canvas).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileIso {
    /// Write the collection's ISO sources to files of their own.
    #[serde(default)]
    pub enabled: bool,
    /// File name without extension. Tokens: {SOURCE} (upper-case id),
    /// {source}, {date}, {time}.
    #[serde(default = "default_iso_filename")]
    pub filename: String,
    #[serde(default)]
    pub encoder: Encoder,
    /// Record sources and audio through their filters.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub filtered: bool,
}

impl Default for ProfileIso {
    fn default() -> Self {
        let IsoRecording {
            enabled,
            filename,
            encoder,
            filtered,
            ..
        } = IsoRecording::default();
        Self {
            enabled,
            filename,
            encoder,
            filtered,
        }
    }
}

impl Setup {
    /// The live setup made of a scene collection and a profile.
    pub fn from_parts(collection: Collection, profile: Profile) -> Setup {
        let Collection {
            virtual_camera,
            version,
            name,
            canvas: CollectionCanvas { width, height },
            sources,
            scenes,
            keyframes,
            transition,
            recording:
                CollectionRecording {
                    program:
                        CollectionProgram {
                            audio: program_audio,
                        },
                    iso:
                        CollectionIso {
                            sources: iso_sources,
                            audio: iso_audio,
                            source_audio,
                        },
                },
        } = collection;
        let Profile {
            version: _,
            name: _,
            canvas: ProfileCanvas { fps },
            audio,
            recording:
                ProfileRecording {
                    folder,
                    program:
                        ProfileProgram {
                            enabled: program_enabled,
                            filename: program_filename,
                            encoder: program_encoder,
                            width: program_width,
                            height: program_height,
                        },
                    iso:
                        ProfileIso {
                            enabled: iso_enabled,
                            filename: iso_filename,
                            encoder: iso_encoder,
                            filtered,
                        },
                    keep_free_gb,
                    replay,
                },
            stream,
            hotkeys,
            developer,
        } = profile;
        Setup {
            virtual_camera,
            version,
            name,
            canvas: Canvas { width, height, fps },
            sources,
            scenes,
            keyframes,
            transition,
            audio,
            recording: Recording {
                folder,
                program: ProgramRecording {
                    enabled: program_enabled,
                    filename: program_filename,
                    encoder: program_encoder,
                    audio: program_audio,
                    width: program_width,
                    height: program_height,
                },
                iso: IsoRecording {
                    enabled: iso_enabled,
                    sources: iso_sources,
                    filename: iso_filename,
                    encoder: iso_encoder,
                    audio: iso_audio,
                    source_audio,
                    filtered,
                },
                keep_free_gb,
                replay,
            },
            stream,
            hotkeys,
            developer,
        }
    }

    /// The setup's scene collection (named like the setup) and profile
    /// (named `profile_name`). Every field goes to one of them (the
    /// destructuring is exhaustive, so a new setup field must be placed).
    pub fn into_parts(self, profile_name: &str) -> (Collection, Profile) {
        let Setup {
            virtual_camera,
            version,
            name,
            canvas: Canvas { width, height, fps },
            sources,
            scenes,
            keyframes,
            transition,
            audio,
            recording:
                Recording {
                    folder,
                    program:
                        ProgramRecording {
                            enabled: program_enabled,
                            filename: program_filename,
                            encoder: program_encoder,
                            audio: program_audio,
                            width: program_width,
                            height: program_height,
                        },
                    iso:
                        IsoRecording {
                            enabled: iso_enabled,
                            sources: iso_sources,
                            filename: iso_filename,
                            encoder: iso_encoder,
                            audio: iso_audio,
                            source_audio,
                            filtered,
                        },
                    keep_free_gb,
                    replay,
                },
            stream,
            hotkeys,
            developer,
        } = self;
        let collection = Collection {
            virtual_camera,
            version,
            name,
            canvas: CollectionCanvas { width, height },
            sources,
            scenes,
            keyframes,
            transition,
            recording: CollectionRecording {
                program: CollectionProgram {
                    audio: program_audio,
                },
                iso: CollectionIso {
                    sources: iso_sources,
                    audio: iso_audio,
                    source_audio,
                },
            },
        };
        let profile = Profile {
            version,
            name: profile_name.to_string(),
            canvas: ProfileCanvas { fps },
            audio,
            recording: ProfileRecording {
                folder,
                program: ProfileProgram {
                    enabled: program_enabled,
                    filename: program_filename,
                    encoder: program_encoder,
                    width: program_width,
                    height: program_height,
                },
                iso: ProfileIso {
                    enabled: iso_enabled,
                    filename: iso_filename,
                    encoder: iso_encoder,
                    filtered,
                },
                keep_free_gb,
                replay,
            },
            stream,
            hotkeys,
            developer,
        };
        (collection, profile)
    }
}

/// The setup fields a profile holds; everything else (but `version`) is
/// the scene collection's.
const PROFILE_FIELDS: &[&str] = &[
    "hotkeys",
    "canvas.fps",
    "audio",
    "stream",
    "recording.folder",
    "recording.keep_free_gb",
    "recording.replay",
    "recording.program.enabled",
    "recording.program.filename",
    "recording.program.encoder",
    "recording.program.width",
    "recording.program.height",
    "recording.iso.enabled",
    "recording.iso.filename",
    "recording.iso.encoder",
    "recording.iso.filtered",
];

/// Which part of the live setup a field (a setup path like
/// `recording.program.width` or `sources[0].id`) is kept in. None for
/// `version`, which both have.
pub fn part_of(path: &str) -> Option<LibraryKind> {
    let under = |field: &str| {
        path == field
            || path
                .strip_prefix(field)
                .is_some_and(|rest| rest.starts_with(['.', '[']))
    };
    if under("version") {
        None
    } else if PROFILE_FIELDS.iter().any(|field| under(field)) {
        Some(LibraryKind::Profile)
    } else {
        Some(LibraryKind::Collection)
    }
}

impl Collection {
    /// A new, empty collection: one empty scene on a canvas of this size.
    pub fn blank(name: &str, width: u32, height: u32) -> Self {
        Self {
            version: SETUP_VERSION,
            name: name.to_string(),
            canvas: CollectionCanvas { width, height },
            sources: Vec::new(),
            scenes: vec![Scene {
                id: "scene".into(),
                name: Some("Scene".into()),
                layout: None,
                items: Vec::new(),
                transition: None,
            }],
            keyframes: Vec::new(),
            // OBS's default transition.
            transition: Transition::fade(300),
            recording: CollectionRecording::default(),
            virtual_camera: Default::default(),
        }
    }
}

impl Profile {
    /// A new profile with `setup init`'s settings: 60 fps, HEVC program
    /// and ISO files to ~/Movies/Isocast, no stream.
    pub fn starter(name: &str) -> Self {
        Self {
            version: SETUP_VERSION,
            name: name.to_string(),
            canvas: ProfileCanvas { fps: 60 },
            audio: AudioSetup::default(),
            recording: ProfileRecording {
                folder: crate::init::DEFAULT_FOLDER.into(),
                program: ProfileProgram {
                    enabled: true,
                    filename: default_program_filename(),
                    encoder: Encoder::default(),
                    width: None,
                    height: None,
                },
                iso: ProfileIso {
                    enabled: true,
                    ..ProfileIso::default()
                },
                keep_free_gb: default_keep_free_gb(),
                replay: Default::default(),
            },
            stream: None,
            hotkeys: Default::default(),
            developer: Default::default(),
        }
    }
}

/// Read a collection or profile from TOML, or JSON if `json`; a JSON error
/// names the path of what's wrong.
pub fn parse<T: DeserializeOwned>(text: &str, json: bool) -> Result<T, LoadError> {
    if json {
        let mut deserializer = serde_json::Deserializer::from_str(text);
        let value = serde_path_to_error::deserialize(&mut deserializer).map_err(LoadError::json)?;
        deserializer.end().map_err(|error| LoadError::Json {
            path: String::new(),
            message: error.to_string(),
        })?;
        Ok(value)
    } else {
        Ok(toml::from_str(text)?)
    }
}

/// A collection or profile (or anything else) as TOML, as the library's
/// files are written.
pub fn to_toml<T: Serialize>(value: &T) -> Result<String, toml::ser::Error> {
    toml::to_string_pretty(value)
}

/// One scene collection or profile in the library.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LibraryEntry {
    pub kind: LibraryKind,
    /// Its stable id (the file's name).
    pub id: String,
    /// Its name, as menus show it (empty if the file can't be read).
    pub name: String,
    /// It is the one in use.
    pub current: bool,
    pub path: PathBuf,
    /// What it holds, briefly: "3840×2160, 2 scenes, 5 sources" or "60
    /// fps, HEVC to ~/Movies, streams to twitch".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Why the file can't be used, if it can't.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// The scene collections and profiles, and which are in use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Library {
    /// The current scene collection's id, once there is a live setup.
    pub collection: Option<String>,
    /// The current profile's id.
    pub profile: Option<String>,
    pub collections: Vec<LibraryEntry>,
    pub profiles: Vec<LibraryEntry>,
    /// The folder they are kept in.
    pub folder: PathBuf,
}

impl Collection {
    /// "3840×2160, 2 scenes, 5 sources".
    pub fn summary(&self) -> String {
        let count = |n: usize, what: &str| format!("{n} {what}{}", if n == 1 { "" } else { "s" });
        format!(
            "{}×{}, {}, {}",
            self.canvas.width,
            self.canvas.height,
            count(self.scenes.len(), "scene"),
            count(self.sources.len(), "source")
        )
    }
}

impl Profile {
    /// "60 fps, HEVC to ~/Movies, ISO files, streams to twitch".
    pub fn summary(&self) -> String {
        let recording = &self.recording;
        let codec = serde_json::to_value(recording.program.encoder.codec)
            .ok()
            .and_then(|codec| codec.as_str().map(str::to_uppercase))
            .unwrap_or_default();
        let mut text = format!(
            "{} fps, {} to {}",
            self.canvas.fps,
            codec.replace('_', " "),
            recording.folder
        );
        if !recording.program.enabled {
            text.push_str(", no program file");
        }
        if recording.iso.enabled {
            text.push_str(", ISO files");
        }
        if let Some(stream) = &self.stream {
            text.push_str(&format!(", streams to {}", stream.service.id()));
        }
        text
    }
}

/// Which scene collection or profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LibraryTarget {
    pub kind: LibraryKind,
    pub id: String,
}

/// Make a scene collection or profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LibraryCreateParams {
    pub kind: LibraryKind,
    /// Its name.
    pub name: String,
    /// Its id (lowercase letters, digits and dashes); made from the name
    /// if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Copy this one (by id) instead of starting blank (OBS's Duplicate). A
    /// blank collection has one empty scene on the current canvas; a blank
    /// profile records 60 fps HEVC with ISO files to ~/Movies/Isocast.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_of: Option<String>,
    /// Switch to it once made (refused while recording or streaming).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub switch: bool,
}

/// Rename a scene collection or profile; its id stays.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LibraryRenameParams {
    pub kind: LibraryKind,
    pub id: String,
    pub name: String,
}

/// Switch the scene collection, the profile or both at once.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LibrarySwitchParams {
    /// The scene collection to use, by id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection: Option<String>,
    /// The profile to use, by id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

/// Bring in a scene collection, a profile, or both from a whole setup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LibraryImportParams {
    /// A whole setup: its collection part and its profile part, both
    /// imported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<Setup>,
    /// Or a scene collection (as `library_get` returns one)…
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection: Option<Collection>,
    /// …and/or a profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<Profile>,
    /// The name to give them; their own (a setup's for both) if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Switch to what was imported (refused while recording or
    /// streaming).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub switch: bool,
}

/// Bring in an OBS Studio scene collection as a collection and/or an OBS
/// profile as a profile.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LibraryImportObsParams {
    /// An OBS scene collection: its .json file (absolute or `~/...`), or
    /// its name in OBS's scenes folder, like "Recording".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection: Option<String>,
    /// An OBS profile: its name, its folder or its basic.ini path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// The name to give them; OBS's if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Import the collection onto a canvas this wide and high, scaling
    /// positions and sizes about the centre; its own canvas if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Switch to what was imported (refused while recording or
    /// streaming).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub switch: bool,
    /// Move the stream key OBS keeps in plain text in the profile's
    /// service.json into the Keychain, for the imported profile's service
    /// (only when the user agreed to it). Without it the key stays behind.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stream_key: bool,
}

/// A scene collection or profile as a document, for export or to read one
/// that isn't current.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LibraryDocument {
    pub entry: LibraryEntry,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection: Option<Collection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<Profile>,
}

/// What a change to the library did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LibraryChange {
    /// The scene collection made, changed or removed, if one was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection: Option<LibraryEntry>,
    /// The profile made, changed or removed, if one was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<LibraryEntry>,
    /// The current scene collection and profile afterwards.
    pub current_collection: Option<String>,
    pub current_profile: Option<String>,
    /// The live setup's new revision, if the change reached it (a switch,
    /// or renaming the current collection).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    /// Things to know, like where a deleted file was kept.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// From an OBS import: what couldn't come over, and why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unsupported: Vec<Unsupported>,
    /// Problems the current pair would have with what was imported or made
    /// (a switch to it is refused until they are fixed).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<Issue>,
}

/// What happened to the library, for [`crate::api::Event::LibraryChanged`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LibraryAction {
    Created,
    Renamed,
    Deleted,
    Switched,
    Imported,
    /// Setup files from an older Isocast were taken in.
    Migrated,
}

/// `name` in lowercase letters, digits and dashes, for an id; `fallback`
/// if nothing is left.
pub fn slug(name: &str, fallback: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        match c.to_ascii_lowercase() {
            c @ ('a'..='z' | '0'..='9') => out.push(c),
            _ if !out.is_empty() && !out.ends_with('-') => out.push('-'),
            _ => {}
        }
    }
    let out = out.trim_end_matches('-');
    if out.is_empty() {
        fallback.to_string()
    } else {
        out.chars()
            .take(64)
            .collect::<String>()
            .trim_end_matches('-')
            .to_string()
    }
}

/// Whether `id` can name a collection or profile: lowercase letters,
/// digits and dashes, at most 64.
pub fn valid_id(id: &str) -> bool {
    crate::is_valid_id(id) && id.len() <= 64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example() -> Setup {
        let text = include_str!("../../../examples/recording.toml");
        Setup::from_toml(text).unwrap()
    }

    #[test]
    fn a_setup_splits_into_parts_and_back_unchanged() {
        let mut full = example();
        // Every profile field set away from its default.
        full.audio.monitoring_device = "Studio Display".into();
        full.recording.program.width = Some(1920);
        full.recording.program.height = Some(1080);
        full.recording.iso.filtered = true;
        full.recording.keep_free_gb = 5.0;
        full.stream =
            Some(serde_json::from_value(serde_json::json!({"service": "twitch"})).unwrap());
        for setup in [example(), full] {
            let (collection, profile) = setup.clone().into_parts("Default");
            assert_eq!(profile.name, "Default");
            assert_eq!(collection.name, setup.name);
            // Through their files, too.
            let collection: Collection =
                parse(&toml::to_string_pretty(&collection).unwrap(), false).unwrap();
            let profile: Profile =
                parse(&toml::to_string_pretty(&profile).unwrap(), false).unwrap();
            let json: Profile = parse(&serde_json::to_string(&profile).unwrap(), true).unwrap();
            assert_eq!(json, profile);
            assert_eq!(Setup::from_parts(collection, profile), setup);
        }
    }

    #[test]
    fn each_part_keeps_its_own_fields() {
        let (collection, profile) = example().into_parts("Default");
        let collection = toml::to_string_pretty(&collection).unwrap();
        let profile = toml::to_string_pretty(&profile).unwrap();
        assert!(collection.contains("width = 3840") && !collection.contains("fps"));
        assert!(collection.contains("[recording.iso]") && collection.contains("\"iphone\""));
        assert!(!collection.contains("folder") && !collection.contains("encoder"));
        assert!(profile.contains("fps = 60") && profile.contains("folder ="));
        assert!(!profile.contains("width") && !profile.contains("screen"));
        // A field of the other part is refused, naming it.
        let wrong = format!("{collection}\n[stream]\nservice = \"twitch\"\n");
        let error = parse::<Collection>(&wrong, false).unwrap_err();
        assert!(error.to_string().contains("stream"), "{error}");
    }

    #[test]
    fn paths_belong_to_one_part() {
        assert_eq!(part_of("canvas.fps"), Some(LibraryKind::Profile));
        assert_eq!(part_of("canvas.width"), Some(LibraryKind::Collection));
        assert_eq!(
            part_of("recording.program.width"),
            Some(LibraryKind::Profile)
        );
        assert_eq!(
            part_of("recording.program.audio[0]"),
            Some(LibraryKind::Collection)
        );
        assert_eq!(
            part_of("recording.iso.sources"),
            Some(LibraryKind::Collection)
        );
        assert_eq!(part_of("stream.server"), Some(LibraryKind::Profile));
        assert_eq!(part_of("sources[2].device"), Some(LibraryKind::Collection));
        assert_eq!(part_of("streams"), Some(LibraryKind::Collection));
        assert_eq!(part_of("version"), None);
    }

    #[test]
    fn a_blank_collection_and_a_starter_profile_make_a_valid_setup() {
        let setup = Setup::from_parts(
            Collection::blank("New", 1920, 1080),
            Profile::starter("New"),
        );
        assert_eq!(setup.validate(), Vec::new());
        assert_eq!(slug("Ryan's Stream (2)", "x"), "ryan-s-stream-2");
        assert_eq!(slug("…", "collection"), "collection");
    }
}
