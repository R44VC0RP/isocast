//! Project files (#88): a visible `<Project name>.isoprj` in each project
//! folder, which opens the project in the editor.
//!
//! It is JSON: the project's id, name and identity (so a folder that lost
//! its hidden `.isocast-project.json` gets it back), its takes' manifests,
//! and every timeline that uses its takes, with all their edits (clip
//! order, trims, splits, angles, crops (#87), undo history, and any
//! per-clip fields a newer Isocast adds, kept as they are). Paths in it are relative to the
//! file's folder (`../` for takes in a sibling project), so the folder can
//! be copied or moved anywhere, to another disk, Mac or Windows PC, and still
//! open with the same edits. A take that isn't there shows as offline.
//! Relative paths are written with `/` on every OS. Absolute references to
//! other disks keep their native syntax and may need relinking after a move.
//!
//! The engine keeps its working copy of timelines in the timelines folder
//! (written by the recording's own callbacks, with crash recovery); the
//! project file is written from it by one background writer, atomically
//! (a hidden temporary file, synced, then renamed), never while a
//! recording writes into that folder. See docs/timelines.md.

use std::path::{Component, Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::timeline::{Timeline, TimelineInfo, TimelineSummary};

/// The file name extension, without the dot.
pub const EXTENSION: &str = "isoprj";

/// What `format` says in every project file.
pub const FORMAT: &str = "isocast-project";

/// The format version this Isocast writes (and the newest it reads).
pub const ISOPRJ_VERSION: u32 = 1;

/// The exported Uniform Type Identifier (Info.plist).
pub const UTI: &str = "dev.isocast.project";

/// The largest project file read (a timeline with its full undo history
/// is well under a megabyte).
const MAX_BYTES: u64 = 64 * 1024 * 1024;

/// A `.isoprj` file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct IsoprjFile {
    /// Always `isocast-project`.
    pub format: String,
    /// Format version. Currently 1.
    pub version: u32,
    /// The project's id (as `.isocast-project.json` and `list_projects`
    /// give it).
    pub project: String,
    /// The project's name when the file was written.
    pub name: String,
    /// `.isocast-project.json`'s date-and-time prefix and creation time,
    /// so a copy without the hidden file is still the same project.
    pub prefix: String,
    pub created_at: String,
    /// When the file was written, local time, RFC 3339.
    pub saved_at: String,
    /// The takes' manifests (`….session.json`), oldest first, relative to
    /// the file's folder.
    #[serde(default)]
    pub takes: Vec<PathBuf>,
    /// Every timeline that uses this project's takes, with take paths
    /// relative to the file's folder.
    #[serde(default)]
    pub timelines: Vec<Timeline>,
}

/// `<name>.isoprj`, for a project named `name` (`Untitled` if empty).
pub fn file_name(name: &str) -> String {
    let name = name.trim();
    let name = if name.is_empty() || name.starts_with('.') {
        crate::project::DEFAULT_NAME
    } else {
        name
    };
    format!("{}.{EXTENSION}", name.replace(['/', ':'], "-"))
}

/// Whether `path` is named like a project file.
pub fn is_project_file(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == EXTENSION)
        && path
            .file_name()
            .is_some_and(|n| !n.to_string_lossy().starts_with('.'))
}

/// The visible project files directly in `folder`, by name.
pub fn find_in(folder: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_project_file(path) && path.is_file())
        .collect();
    found.sort();
    found
}

/// Read a project file; the error says why it can't be used.
pub fn read(path: &Path) -> Result<IsoprjFile, String> {
    let meta = std::fs::metadata(path).map_err(|err| format!("can't be read: {err}"))?;
    if !meta.is_file() {
        return Err("is not a file".into());
    }
    if meta.len() > MAX_BYTES {
        return Err(format!(
            "is {} MB, more than a project file can be",
            meta.len() / 1_000_000
        ));
    }
    let bytes = std::fs::read(path).map_err(|err| format!("can't be read: {err}"))?;
    let json: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|err| format!("isn't valid JSON: {err}"))?;
    if json["format"] != FORMAT {
        return Err(format!(
            "isn't an Isocast project file (its `format` isn't {FORMAT:?})"
        ));
    }
    let version = json["version"].as_u64().unwrap_or(0);
    if version > u64::from(ISOPRJ_VERSION) {
        return Err(format!(
            "was written by a newer Isocast (format version {version}; this one reads up to \
             {ISOPRJ_VERSION}); update Isocast to open it"
        ));
    }
    serde_json::from_value(json).map_err(|err| format!("is damaged: {err}"))
}

/// Write `file` to `path` so a reader, a crash or a full disk never leaves
/// half of it: a hidden temporary file beside it, synced to disk, then
/// renamed over it (and the folder synced). On any error the old file is
/// untouched and the temporary file removed.
pub fn write_atomic(path: &Path, file: &IsoprjFile) -> std::io::Result<()> {
    use std::io::Write;
    // PathBuf joins use backslashes on Windows. Convert only the known
    // relative take paths at the file boundary, including edit history.
    // Mac files (where a backslash can be part of a name) stay byte-identical.
    #[cfg(windows)]
    let portable = {
        let mut portable = file.clone();
        portable.takes = portable
            .takes
            .iter()
            .map(|path| portable_path(path))
            .collect();
        for timeline in &mut portable.timelines {
            map_paths(timeline, portable_path);
        }
        portable
    };
    #[cfg(windows)]
    let file = &portable;
    let folder = path.parent().unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let partial = folder.join(format!(".{name}.{}-{nanos}.partial", std::process::id()));
    let written = (|| {
        let mut bytes = serde_json::to_vec_pretty(file).map_err(std::io::Error::other)?;
        bytes.push(b'\n');
        let mut out = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&partial)?;
        out.write_all(&bytes)?;
        out.sync_all()?;
        drop(out);
        std::fs::rename(&partial, path)?;
        // The rename itself, on disk (best effort: some file systems
        // can't sync a folder).
        if let Ok(dir) = std::fs::File::open(folder) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    written
}

#[cfg(windows)]
fn portable_path(path: &Path) -> PathBuf {
    if !path.has_root()
        && !matches!(path.components().next(), Some(Component::Prefix(_)))
        && let Some(text) = path.to_str()
    {
        return text.replace('\\', "/").into();
    }
    // Keep drive-relative, UNC and verbatim paths intact. Non-Unicode names
    // still fail serialization instead of being replaced with a different name.
    path.to_path_buf()
}

/// Whether `name` is a temporary file [`write_atomic`] left (killed while
/// writing): `.<something>.isoprj.<pid>-<nanos>.partial`.
pub fn is_partial(name: &str) -> bool {
    name.starts_with('.') && name.ends_with(".partial") && name.contains(".isoprj.")
}

/// `path` without `.` and with `..` taken back, lexically.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `path` relative to the folder `base` (`take.session.json`, or
/// `../Other project/take.session.json`), or `path` itself if they share
/// nothing but the root (another disk), or it is already relative.
pub fn relative_to(path: &Path, base: &Path) -> PathBuf {
    if path.is_relative() {
        return path.to_path_buf();
    }
    let path = normalize(path);
    let base = normalize(base);
    let ours: Vec<Component> = path.components().collect();
    let theirs: Vec<Component> = base.components().collect();
    let common = ours.iter().zip(&theirs).take_while(|(a, b)| a == b).count();
    // Only the root (and maybe /Volumes or /Users) in common: keep it
    // absolute rather than climb to the root.
    let shallow = ours
        .iter()
        .take(common)
        .filter(|c| matches!(c, Component::Normal(_)))
        .count();
    if shallow < 2 && common < theirs.len() {
        return path;
    }
    let mut out = PathBuf::new();
    for _ in common..theirs.len() {
        out.push("..");
    }
    for component in &ours[common..] {
        out.push(component.as_os_str());
    }
    out
}

/// A path from a project file made absolute against `base`, its folder.
pub fn resolve(path: &Path, base: &Path) -> PathBuf {
    match path.is_relative() {
        true => normalize(&base.join(path)),
        false => path.to_path_buf(),
    }
}

/// Apply `f` to every take path in `timeline`: clips, removed takes, takes
/// being recorded, and those in its undo and redo history.
pub fn map_paths(timeline: &mut Timeline, f: impl Fn(&Path) -> PathBuf) {
    for clip in &mut timeline.clips {
        clip.take = f(&clip.take);
    }
    for removed in &mut timeline.removed {
        removed.take = f(&removed.take);
    }
    for take in &mut timeline.recording {
        *take = f(take);
    }
    for revision in timeline.undo.iter_mut().chain(timeline.redo.iter_mut()) {
        for clip in &mut revision.clips {
            clip.take = f(&clip.take);
        }
        for removed in &mut revision.removed {
            removed.take = f(&removed.take);
        }
    }
}

/// Every take path `timeline` names now (clips, removed and being
/// recorded; not its history).
pub fn takes_of(timeline: &Timeline) -> impl Iterator<Item = &Path> {
    timeline
        .clips
        .iter()
        .map(|c| c.take.as_path())
        .chain(timeline.removed.iter().map(|r| r.take.as_path()))
        .chain(timeline.recording.iter().map(PathBuf::as_path))
}

/// Whether `timeline` names a take directly in `folder`.
pub fn uses_folder(timeline: &Timeline, folder: &Path) -> bool {
    takes_of(timeline).any(|take| take.parent() == Some(folder))
}

/// Whether two timelines are the same one: the same [`Timeline::uid`] when
/// both have one (two Macs' `untitled` timelines never are); for one from
/// before uids, the same creation time and the same id or name (an id
/// alone isn't enough: every Mac has an `untitled`).
pub fn same_timeline(a: &Timeline, b: &Timeline) -> bool {
    match (&a.uid, &b.uid) {
        (Some(x), Some(y)) => x == y,
        _ => {
            !a.created_at.is_empty()
                && a.created_at == b.created_at
                && (a.id == b.id || a.name == b.name)
        }
    }
}

/// Parameters of `project_open`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectOpenParams {
    /// A `.isoprj` file, or a project folder (its `.isoprj`, or its takes
    /// if it has none yet).
    pub path: PathBuf,
    /// Which of the project's timelines to open (an id); else the current
    /// timeline if it is one of them, else its newest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeline: Option<String>,
    /// Ask the app to show the editor on it (true if absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor: Option<bool>,
}

/// The answer to `project_open`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProjectOpened {
    /// The project file (written soon if it didn't exist yet).
    pub path: PathBuf,
    /// The project's folder.
    pub folder: PathBuf,
    /// The project's id.
    pub project: String,
    pub name: String,
    /// The timeline opened: now the current one.
    pub timeline: TimelineInfo,
    /// Every timeline of the project.
    pub timelines: Vec<TimelineSummary>,
    /// Takes the timelines name that aren't there (offline): relink them
    /// with `timeline_relink`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub offline: Vec<PathBuf>,
    /// What opening did that is worth knowing (timelines imported, a
    /// project file that couldn't be read).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// A project opened lately, as `recent_projects` gives it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RecentProject {
    /// Its project file.
    pub path: PathBuf,
    pub name: String,
    /// When it was last opened, local time, RFC 3339.
    pub opened_at: String,
    /// The file is still there.
    pub exists: bool,
}

/// The answer to `recent_projects`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RecentProjects {
    /// Newest first, at most [`RECENT_LIMIT`].
    pub projects: Vec<RecentProject>,
}

/// Parameters of `recent_projects`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecentProjectsParams {
    /// Forget them all (File › Open Recent › Clear Menu).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear: bool,
}

/// How many projects `recent_projects` keeps.
pub const RECENT_LIMIT: usize = 10;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_made_relative_and_back() {
        // A Windows absolute path needs a drive as well as a root.
        let root = if cfg!(windows) { "C:/" } else { "/" };
        let base = Path::new(root).join("Users/r/Movies/Isocast/2026-10-16 10-00-00 Talk");
        let base = base.as_path();
        assert!(base.is_absolute());
        let inside = base.join("a.session.json");
        assert_eq!(relative_to(&inside, base), Path::new("a.session.json"));
        let sibling =
            Path::new(root).join("Users/r/Movies/Isocast/2026-10-16 11-00-00 B/b.session.json");
        let rel = relative_to(&sibling, base);
        assert_eq!(rel, Path::new("../2026-10-16 11-00-00 B/b.session.json"));
        assert_eq!(resolve(&rel, base), sibling);
        // Another disk stays absolute.
        let other = Path::new(if cfg!(windows) {
            "D:/t.session.json"
        } else {
            "/Volumes/X/t.session.json"
        });
        assert_eq!(relative_to(other, base), other);
        assert_eq!(resolve(other, base), other);
        // A copied folder resolves against where it is now.
        let copy = Path::new(if cfg!(windows) {
            "D:/Backup/Talk"
        } else {
            "/Volumes/Backup/Talk"
        });
        assert_eq!(
            resolve(Path::new("a.session.json"), copy),
            copy.join("a.session.json")
        );

        // Exercise the actual project-file write/read boundary, not just
        // Path equality (which treats both separators alike on Windows).
        let nested = Path::new("nested").join("a.session.json");
        let clip = serde_json::json!({"id":"c1", "take":nested, "in_seconds":0.0,
            "out_seconds":1.0, "future":"keep\\this"});
        let removed = serde_json::json!({"take":nested, "reason":"removed", "at":""});
        let revision = serde_json::json!({"action":"trimmed", "clips":[clip], "removed":[removed]});
        let file: IsoprjFile = serde_json::from_value(serde_json::json!({
            "format":FORMAT, "version":1, "project":"p", "name":"Talk", "prefix":"",
            "created_at":"", "saved_at":"", "takes":[nested, rel, other],
            "timelines":[{"version":1, "id":"talk", "name":"Talk", "created_at":"",
                "clips":[clip], "removed":[removed], "recording":[nested], "next_clip":2,
                "undo":[revision], "redo":[revision]}]
        }))
        .unwrap();
        #[cfg(windows)]
        let file = {
            let mut file = file;
            file.takes.extend(
                [
                    r"\\server\share\take.session.json",
                    r"\\?\D:\take.session.json",
                    r"D:take.session.json",
                ]
                .map(PathBuf::from),
            );
            file
        };
        let dir = std::env::temp_dir().join(format!("isoprj-paths-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("Talk.isoprj");
        // Previously stored native paths and CRLF JSON still read unchanged.
        let legacy = serde_json::to_string_pretty(&file)
            .unwrap()
            .replace('\n', "\r\n");
        std::fs::write(&path, legacy).unwrap();
        assert_eq!(read(&path).unwrap(), file);
        // Replacing the file, not just creating it, must work on Windows too.
        for _ in 0..2 {
            write_atomic(&path, &file).unwrap();
        }
        let bytes = std::fs::read(&path).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["takes"][0], "nested/a.session.json");
        assert_eq!(json["takes"][1], "../2026-10-16 11-00-00 B/b.session.json");
        for (i, take) in file.takes.iter().enumerate().skip(2) {
            assert_eq!(json["takes"][i], serde_json::to_value(take).unwrap());
        }
        let timeline = &json["timelines"][0];
        for take in [
            &timeline["clips"][0]["take"],
            &timeline["removed"][0]["take"],
            &timeline["recording"][0],
            &timeline["undo"][0]["clips"][0]["take"],
            &timeline["undo"][0]["removed"][0]["take"],
            &timeline["redo"][0]["clips"][0]["take"],
            &timeline["redo"][0]["removed"][0]["take"],
        ] {
            assert_eq!(take, "nested/a.session.json");
        }
        assert_eq!(timeline["clips"][0]["future"], "keep\\this");
        let back = read(&path).unwrap();
        assert_eq!(back, file);
        assert_eq!(
            resolve(&back.takes[0], copy),
            copy.join("nested/a.session.json")
        );
        #[cfg(unix)]
        {
            let mut expected = serde_json::to_vec_pretty(&file).unwrap();
            expected.push(b'\n');
            assert_eq!(bytes, expected);
            let mut literal = file.clone();
            literal.takes.push(r"literal\name.session.json".into());
            write_atomic(&path, &literal).unwrap();
            assert_eq!(read(&path).unwrap(), literal);
        }
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn timelines_match_by_uid_not_by_a_shared_id() {
        let t = |uid: Option<&str>, created: &str| Timeline {
            version: 1,
            id: "untitled".into(),
            uid: uid.map(str::to_string),
            name: "Untitled".into(),
            created_at: created.into(),
            clips: Vec::new(),
            removed: Vec::new(),
            recording: Vec::new(),
            next_clip: 1,
            revision: 0,
            undo: Vec::new(),
            redo: Vec::new(),
        };
        // Two Macs' `untitled`.
        assert!(!same_timeline(&t(Some("a"), "x"), &t(Some("b"), "x")));
        assert!(same_timeline(&t(Some("a"), "x"), &t(Some("a"), "y")));
        // Files from before uids: the id alone isn't enough.
        assert!(!same_timeline(
            &t(None, "2026-10-16T10:00:00"),
            &t(Some("b"), "2026-10-17T09:00:00")
        ));
        assert!(same_timeline(
            &t(None, "2026-10-16T10:00:00"),
            &t(Some("b"), "2026-10-16T10:00:00")
        ));
        assert!(!same_timeline(&t(None, ""), &t(None, "")));
        assert!(crate::timeline::valid_id("talk-2"));
        for bad in ["", "../x", "a/b", "A", "-x", "a.b"] {
            assert!(!crate::timeline::valid_id(bad), "{bad}");
        }
        let a = crate::timeline::new_uid();
        let b = crate::timeline::new_uid();
        assert_eq!(a.len(), 16);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn names_and_partials() {
        assert_eq!(file_name("My Talk"), "My Talk.isoprj");
        assert_eq!(file_name(""), "Untitled.isoprj");
        assert!(is_project_file(Path::new("/x/My Talk.isoprj")));
        assert!(!is_project_file(Path::new("/x/.My Talk.isoprj")));
        assert!(is_partial(".My Talk.isoprj.123-456.partial"));
        assert!(!is_partial("My Talk.isoprj"));
    }
}
