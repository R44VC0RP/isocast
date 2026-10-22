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
