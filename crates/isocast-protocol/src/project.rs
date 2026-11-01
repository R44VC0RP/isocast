//! Recording projects: a folder per recording session.
//!
//! Every recording session writes into its own folder inside the
//! recordings folder (`recording.folder`, or `start`'s `folder`):
//! `<YYYY-MM-DD HH-MM-SS> Untitled/`, holding every take's program and ISO
//! files and their manifests. A
//! hidden `.isocast-project.json` in it gives the project a stable id;
//! its name is the folder's name after the date and time, so a project
//! renamed in the Finder keeps its id and shows the new name.
//!
//! `rename_project` renames the folder (only while nothing records into
//! it; one atomic rename on the same disk). Manifests list their files
//! relative to themselves, so they still resolve.
//! Recordings made before projects stay where they are.

use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::located::Located;

/// The hidden file that makes a folder a project.
pub const PROJECT_FILE: &str = ".isocast-project.json";

/// A new project's name.
pub const DEFAULT_NAME: &str = "Untitled";

/// The longest name, in characters.
pub const MAX_NAME: usize = 120;

/// The format version of [`ProjectFile`].
pub const PROJECT_VERSION: u32 = 1;

/// `.isocast-project.json`, as kept in a project's folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProjectFile {
    pub version: u32,
    /// Stays the same when the project is renamed: `20261014-153012`
    /// (with `-2`, `-3`… when several begin in the same second).
    pub id: String,
    /// The date and time the folder's name begins with
    /// (`2026-10-14 15-30-12`); the rest of the name is the project's.
    pub prefix: String,
    /// When it was made, local time, RFC 3339.
    pub created_at: String,
}

/// A recording project, as `list_projects` and its events give it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Project {
    pub id: String,
    /// The folder's name after its date and time (`Untitled` at first).
    pub name: String,
    /// Its folder: every take's files and manifests are in it.
    pub folder: PathBuf,
    /// When it was made, local time, RFC 3339.
    pub created_at: String,
    /// A recording is writing into it (it can't be renamed until it
    /// stops).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recording: bool,
    /// Every take's manifest (`….session.json`), oldest first.
    #[serde(default)]
    pub takes: Vec<PathBuf>,
    /// Every media file the manifests list (program and ISO files), in
    /// take order.
    #[serde(default)]
    pub files: Vec<PathBuf>,
}

/// The answer to `list_projects`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Projects {
    /// Newest first.
    pub projects: Vec<Project>,
    /// The recordings folders looked in.
    pub folders: Vec<PathBuf>,
}

/// Parameters of `list_projects`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListProjectsParams {
    /// At most this many, newest first: 100 if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// Parameters of `rename_project`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RenameProjectParams {
    /// The project's id (from `list_projects` or a `project_created`
    /// event).
    pub project: String,
    /// Its new name: the folder becomes `<date> <time> <name>`.
    pub name: String,
}

/// The answer to `rename_project`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProjectRenamed {
    /// The project after.
    pub project: Project,
    pub old_name: String,
    pub old_folder: PathBuf,
    /// Things to know, like the name getting a number because the folder
    /// was taken.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// A name with surrounding spaces trimmed, or why it can't be one, naming
/// the `name` field.
pub fn check_name(name: &str) -> Result<String, Located> {
    let trimmed = name.trim();
    let problem = if trimmed.is_empty() {
        Some("is empty".to_string())
    } else if trimmed.chars().count() > MAX_NAME {
        Some(format!("is longer than {MAX_NAME} characters"))
    } else if trimmed.starts_with('.') {
        Some("starts with a dot (that would hide the folder)".to_string())
    } else {
        trimmed
            .chars()
            .find(|c| matches!(c, '/' | ':' | '\\') || c.is_control())
            .map(|c| format!("has {c:?}, which a folder name can't"))
    };
    match problem {
        None => Ok(trimmed.to_string()),
        Some(problem) => Err(Located {
            path: "name".into(),
            message: format!(
                "{problem}; a name is 1 to {MAX_NAME} characters with no `/`, `:`, `\\` or \
                 control characters, not starting with `.`"
            ),
        }),
    }
}

/// Whether a folder name looks like a project folder's,
/// `<YYYY-MM-DD HH-MM-SS> <name>` (as plugin sandboxes match them).
pub fn named_like_project(name: &str) -> bool {
    let bytes = name.as_bytes();
    let pattern = b"dddd-dd-dd dd-dd-dd ";
    bytes.len() > pattern.len()
        && pattern.iter().zip(bytes).all(|(p, b)| match p {
            b'd' => b.is_ascii_digit(),
            p => p == b,
        })
        && !name[pattern.len()..].contains('/')
}

/// Folders directly in `root` named like project folders that aren't
/// projects (no project file).
pub fn named_like_but_not(root: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|entry| named_like_project(&entry.file_name().to_string_lossy()))
        .map(|entry| entry.path())
        .filter(|folder| read(folder).is_none())
        .collect()
}

/// The project in `folder`, if it is one.
pub fn read(folder: &Path) -> Option<ProjectFile> {
    let path = folder.join(PROJECT_FILE);
    let meta = path.symlink_metadata().ok()?;
    if !meta.is_file() || meta.len() > 64 * 1024 {
        return None;
    }
    let file: ProjectFile = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    (!file.id.is_empty()).then_some(file)
}

/// A project's name: its folder's name after the date and time, or the
/// whole name if it no longer begins with them.
pub fn name_of(folder: &Path, file: &ProjectFile) -> String {
    let name = folder
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match name.strip_prefix(&file.prefix) {
        Some(rest) if rest.starts_with(' ') && rest.len() > 1 => rest[1..].to_string(),
        Some("") => String::new(),
        _ => name,
    }
}

/// Make a new project folder in `root`: `<prefix> Untitled` (numbered if
/// taken), with its project file. `stamp` is the id's base
/// (`20261014-153012`).
pub fn create(
    root: &Path,
    prefix: &str,
    stamp: &str,
    created_at: &str,
) -> std::io::Result<(PathBuf, ProjectFile)> {
    std::fs::create_dir_all(root)?;
    for n in 1..1000u32 {
        let (folder, id) = match n {
            1 => (
                root.join(format!("{prefix} {DEFAULT_NAME}")),
                stamp.to_string(),
            ),
            n => (
                root.join(format!("{prefix} {DEFAULT_NAME} {n}")),
                format!("{stamp}-{n}"),
            ),
        };
        // An id another project (renamed since) has is skipped too.
        if id_taken(root, prefix, &id) {
            continue;
        }
        match std::fs::create_dir(&folder) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
        let file = ProjectFile {
            version: PROJECT_VERSION,
            id,
            prefix: prefix.to_string(),
            created_at: created_at.to_string(),
        };
        let json = serde_json::to_vec_pretty(&file).map_err(std::io::Error::other)?;
        if let Err(err) = std::fs::write(folder.join(PROJECT_FILE), json) {
            let _ = std::fs::remove_dir(&folder);
            return Err(err);
        }
        return Ok((folder, file));
    }
    Err(std::io::Error::other(format!(
        "no free project folder name in {}",
        root.display()
    )))
}

/// Whether a project in `root` whose folder begins with `prefix` has `id`.
fn id_taken(root: &Path, prefix: &str, id: &str) -> bool {
    std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(prefix))
        .any(|entry| read(&entry.path()).is_some_and(|file| file.id == id))
}

/// Remove a project folder that holds nothing but the engine's own files
/// (a recording that failed before writing any): true if removed.
pub fn discard_if_empty(folder: &Path) -> bool {
    const OWN: &[&str] = &[PROJECT_FILE, ".isocast.lock", ".DS_Store"];
    let Ok(entries) = std::fs::read_dir(folder) else {
        return false;
    };
    let names: Vec<_> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name())
        .collect();
    if names
        .iter()
        .any(|name| !OWN.iter().any(|own| name == std::ffi::OsStr::new(own)))
    {
        return false;
    }
    for name in names {
        let _ = std::fs::remove_file(folder.join(name));
    }
    std::fs::remove_dir(folder).is_ok()
}

/// The project folders directly inside `root`, with their project files.
pub fn scan(root: &Path) -> Vec<(PathBuf, ProjectFile)> {
    std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|entry| {
            let folder = entry.path();
            read(&folder).map(|file| (folder, file))
        })
        .collect()
}

/// The folders recordings in `root` may be in: `root` itself (recordings
/// from before projects) and its newest `limit` project folders (by name,
/// which begins with the date and time they were made).
pub fn recording_folders(root: &Path, limit: usize) -> Vec<PathBuf> {
    let mut names: Vec<_> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .map(|entry| entry.file_name())
        .filter(|name| !name.to_string_lossy().starts_with('.'))
        .collect();
    names.sort();
    let mut folders = vec![root.to_path_buf()];
    folders.extend(
        names
            .iter()
            .rev()
            .map(|name| root.join(name))
            .filter(|folder| read(folder).is_some())
            .take(limit),
    );
    folders
}

/// A project as `list_projects` shows it: its takes (manifests) and their
/// files, read from its folder.
pub fn describe(folder: &Path, file: &ProjectFile, recording: bool) -> Project {
    let mut takes = Vec::new();
    for entry in std::fs::read_dir(folder).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        if name.ends_with(".session.json") {
            takes.push(entry.path());
        }
    }
    // Oldest first: manifests are named for when their take began.
    takes.sort_by_key(|path| {
        let started = std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|json| json["started_at"].as_str().map(str::to_string));
        (started, path.clone())
    });
    let files = takes
        .iter()
        .filter_map(|path| {
            let json: crate::manifest::SessionManifest =
                serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
            Some(
                json.files
                    .into_iter()
                    .filter(|f| Path::new(&f.path).components().count() == 1)
                    .map(|f| folder.join(f.path))
                    .collect::<Vec<_>>(),
            )
        })
        .flatten()
        .collect();
    Project {
        id: file.id.clone(),
        name: name_of(folder, file),
        folder: folder.to_path_buf(),
        created_at: file.created_at.clone(),
        recording,
        takes,
        files,
    }
}

/// `path` with `from` (a folder it is in) replaced by `to`, if it is in it.
pub fn moved(path: &Path, from: &Path, to: &Path) -> Option<PathBuf> {
    path.strip_prefix(from).ok().map(|rest| to.join(rest))
}

/// The recordings folders recordings were made in lately (newest last):
/// the engine's, and standalone `isocast record`'s (#56), kept beside the
/// setup file. A registry that can't be read falls back to the last good
/// one (`….bak`), never to an empty list while one is there.
pub fn remembered_roots() -> Vec<PathBuf> {
    let path = remembered_path();
    read_roots(&path)
        .or_else(|| read_roots(&path.with_extension("json.bak")))
        .unwrap_or_default()
}

fn read_roots(path: &Path) -> Option<Vec<PathBuf>> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Remember `root` as the newest recordings folder (20 are kept). Whether
/// it is new to the list.
///
/// The engine and standalone `isocast record` both do this, so the whole
/// read, merge and write holds an exclusive lock on `….json.lock` beside
/// it, and the new list is written to a file of this call's own
/// (`create_new`) and renamed into place: no torn file, no lost root. A
/// copy is kept as `….json.bak`; an unreadable registry is moved aside
/// (`….json.unreadable`) and the list continues from that copy.
pub fn remember_root(root: &Path) -> std::io::Result<bool> {
    use std::io::Write;
    let path = remembered_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.with_extension("json.lock"))?;
    lock.lock()?;
    let current = read_roots(&path);
    let mut roots = current
        .clone()
        .or_else(|| read_roots(&path.with_extension("json.bak")))
        .unwrap_or_default();
    let new = !roots.iter().any(|p| p == root);
    roots.retain(|p| p != root);
    roots.push(root.to_owned());
    if roots.len() > 20 {
        roots.drain(..roots.len() - 20);
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let partial = path.with_extension(format!("json.{}-{nanos}.partial", std::process::id()));
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&partial)?;
        file.write_all(&serde_json::to_vec(&roots).map_err(std::io::Error::other)?)?;
        file.sync_all()?;
        if current.is_none() && path.exists() {
            // Unreadable (not ours to judge): kept aside, not overwritten.
            std::fs::rename(&path, path.with_extension("json.unreadable"))?;
        }
        crate::fs::replace(&partial, &path)?;
        // The last good list, for a registry later found unreadable.
        let bak_partial = partial.with_extension("bak.partial");
        std::fs::copy(&path, &bak_partial)
            .and_then(|_| crate::fs::replace(&bak_partial, path.with_extension("json.bak")))
            .inspect_err(|_| {
                let _ = std::fs::remove_file(&bak_partial);
            })
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    written.map(|()| new)
}

fn remembered_path() -> PathBuf {
    crate::api::setup_path().with_extension("recording-folders.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_checked_and_trimmed() {
        assert_eq!(check_name("  Rust talk ").unwrap(), "Rust talk");
        for bad in ["", "   ", ".hidden", "a/b", "a:b", "a\nb"] {
            let err = check_name(bad).unwrap_err();
            assert_eq!(err.path, "name", "{bad:?}");
        }
        assert!(check_name(&"x".repeat(MAX_NAME + 1)).is_err());
    }

    #[test]
    fn project_names_are_recognised() {
        assert!(named_like_project("2026-10-14 15-30-12 Untitled"));
        assert!(named_like_project("2026-10-14 15-30-12 My Talk 2"));
        assert!(!named_like_project("2026-10-14 15-30-12"));
        assert!(!named_like_project("legacy"));
        assert!(!named_like_project("2026-10-14 15:30:12 x"));
    }

    #[test]
    fn projects_are_made_named_and_found() {
        let root = std::env::temp_dir().join(format!("isocast-projects-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (a, fa) = create(&root, "2026-10-14 15-30-12", "20261014-153012", "t").unwrap();
        let (b, fb) = create(&root, "2026-10-14 15-30-12", "20261014-153012", "t").unwrap();
        assert_eq!(fa.id, "20261014-153012");
        assert_eq!(fb.id, "20261014-153012-2");
        assert_eq!(name_of(&a, &fa), "Untitled");
        assert_eq!(name_of(&b, &fb), "Untitled 2");
        let renamed = root.join("2026-10-14 15-30-12 Talk");
        std::fs::rename(&a, &renamed).unwrap();
        assert_eq!(read(&renamed).unwrap().id, fa.id);
        assert_eq!(name_of(&renamed, &fa), "Talk");
        // The renamed project's id isn't handed out again.
        let (c, fc) = create(&root, "2026-10-14 15-30-12", "20261014-153012", "t").unwrap();
        assert_eq!(fc.id, "20261014-153012-3");
        assert_eq!(scan(&root).len(), 3);
        assert!(discard_if_empty(&c));
        std::fs::write(b.join("x.mov"), b"").unwrap();
        assert!(!discard_if_empty(&b));
        let _ = std::fs::remove_dir_all(&root);
    }
}
