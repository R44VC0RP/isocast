//! The folders other computers can use (docs/remote-engines.md, decision
//! 25): every path a network caller chooses, read or written, must resolve
//! inside one of these, symlinks included. The list is the engine's and is
//! changed only on its own machine (`folder_roots_set` is local-only).

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What `folder_roots` and `folder_roots_set` return.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FolderRoots {
    /// Every root in force, the chosen folders first.
    pub roots: Vec<FolderRoot>,
    /// The folders were chosen on this machine (`folder_roots_set`), not
    /// the defaults.
    pub custom: bool,
}

/// One folder other computers can use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FolderRoot {
    pub path: PathBuf,
    /// Only read (fonts): nothing is written there for another computer.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
    /// Why it's a root: `chosen`, `default`, `recordings` (a recordings
    /// folder this machine uses) or `fonts`.
    pub why: String,
}

/// What `folder_roots_set` takes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FolderRootsSetParams {
    /// The folders (absolute or `~/...`) other computers can use, besides
    /// this machine's recordings folders and fonts; the defaults (Movies or
    /// Videos, Pictures, Music, Desktop, Downloads) if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folders: Option<Vec<String>>,
}

/// What `files_list` takes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FilesListParams {
    /// The folder to list (absolute or `~/...`). For a client on another
    /// computer it must be inside a folder other computers can use.
    pub path: PathBuf,
    /// Only files with these extensions (without the dot, any case);
    /// folders are always listed. Every file if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
    /// Only folders.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub folders_only: bool,
}

/// What `files_roots` returns: where an engine file picker starts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FilesRoots {
    /// The folders that can be browsed (for a caller on the engine's own
    /// machine too: the same list other computers get), only those that
    /// exist.
    pub roots: Vec<FolderRoot>,
    /// The engine user's home folder, for showing paths as `~/...`.
    pub home: PathBuf,
}

/// What `files_list` returns: a folder's entries (never their contents).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FilesList {
    /// The folder listed, symlinks resolved.
    pub path: PathBuf,
    /// The folder above it, if the caller may list that too (none at a
    /// root, for a client on another computer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<PathBuf>,
    /// Folders first, then files, each by name; hidden entries are left
    /// out.
    pub entries: Vec<FileEntry>,
    /// More entries than a listing returns ([`FILES_LIST_MAX`]) were there.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// The most entries `files_list` returns.
pub const FILES_LIST_MAX: usize = 2000;

/// One entry of a folder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FileEntry {
    pub name: String,
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub folder: bool,
    /// A file's size in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// When it last changed (seconds since 1970).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<u64>,
}
