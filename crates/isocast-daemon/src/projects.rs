//! Recording projects (7.7b): a folder per recording session, and
//! `list_projects` / `rename_project`. The folder format is in
//! `isocast_protocol::project`.

use std::path::{Path, PathBuf};

use isocast_protocol::api::{Event, RpcError};
use isocast_protocol::project::{
    self, ListProjectsParams, ProjectRenamed, Projects, RenameProjectParams,
};

use crate::Daemon;
use crate::timeline::{failed, invalid, wrong_state};

/// The project a recording writes into.
#[derive(Debug, Clone)]
pub(crate) struct ProjectRef {
    pub id: String,
    pub folder: PathBuf,
    /// The recording made the folder (rather than adding to a project).
    pub created: bool,
}

/// How many projects `list_projects` gives without a `limit`.
const DEFAULT_LIMIT: u32 = 100;

/// The recordings folder a recording asked to write into `folder` uses:
/// the folder holding it, if it is a project's; else `folder` itself.
pub(crate) fn root_of(folder: &Path) -> PathBuf {
    match project::read(folder) {
        Some(_) => folder.parent().unwrap_or(folder).to_path_buf(),
        None => folder.to_path_buf(),
    }
}

/// The project for a recording asked to write into `folder`: `folder`
/// itself if it is a project's (a retake adds to its take's project),
/// else a new project folder inside it.
pub(crate) fn project_for(folder: &Path) -> Result<ProjectRef, RpcError> {
    let (made, file, created) = isocast_engine::project::begin(folder).map_err(|err| {
        failed(format!(
            "could not make a project folder in {}: {err}; record somewhere else with --folder",
            folder.display()
        ))
    })?;
    Ok(ProjectRef {
        id: file.id,
        folder: made,
        created,
    })
}

/// Rename `from` to `to` unless `to` exists, atomically.
fn rename_exclusive(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let from_c = std::ffi::CString::new(from.as_os_str().as_bytes())?;
        let to_c = std::ffi::CString::new(to.as_os_str().as_bytes())?;
        // SAFETY: both are valid NUL-terminated paths.
        let result = unsafe { libc::renamex_np(from_c.as_ptr(), to_c.as_ptr(), libc::RENAME_EXCL) };
        if result == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        // exFAT, FAT and some network shares don't do RENAME_EXCL: check,
        // then rename. Callers hold the projects lock, so only something
        // outside Isocast could make the name between the two, and
        // rename(2) won't replace a folder that isn't empty.
        if err.raw_os_error() == Some(libc::ENOTSUP) || err.raw_os_error() == Some(libc::EINVAL) {
            return plain_rename(from, to);
        }
        Err(err)
    }
    #[cfg(not(target_os = "macos"))]
    plain_rename(from, to)
}

/// Rename `from` to `to` if nothing is at `to`.
fn plain_rename(from: &Path, to: &Path) -> std::io::Result<()> {
    if to.symlink_metadata().is_ok() {
        return Err(std::io::ErrorKind::AlreadyExists.into());
    }
    std::fs::rename(from, to)
}

impl Daemon {
    /// The recordings folders: the live setup's `recording.folder` and
    /// every folder recordings were made in lately.
    pub(crate) fn recording_roots(&self) -> Vec<PathBuf> {
        let mut roots = self.remembered_recording_folders();
        if let Some(setup) = self.state.lock().unwrap().setup.as_ref() {
            roots.push(crate::expand_home(&setup.recording.folder));
        }
        roots.sort();
        roots.dedup();
        roots
    }

    /// The project the running recording writes into, if any.
    pub(crate) fn recording_project(&self) -> Option<ProjectRef> {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.session.as_ref().and_then(|s| s.project.clone())
    }

    /// See [`Method::ListProjects`](isocast_protocol::api::Method::ListProjects).
    pub(crate) fn list_projects(&self, params: ListProjectsParams) -> Projects {
        let folders = self.recording_roots();
        let recording = self.recording_project().map(|p| p.folder);
        let mut found: Vec<_> = folders
            .iter()
            .flat_map(|root| project::scan(root))
            .collect();
        found.sort_by(|a, b| (&b.1.created_at, &b.0).cmp(&(&a.1.created_at, &a.0)));
        found.dedup_by(|a, b| a.0 == b.0);
        found.truncate(params.limit.unwrap_or(DEFAULT_LIMIT) as usize);
        for (folder, _) in &found {
            for message in heal_exports(folder) {
                self.publish(Event::Warning { message });
            }
        }
        Projects {
            projects: found
                .iter()
                .map(|(folder, file)| {
                    project::describe(folder, file, recording.as_ref() == Some(folder))
                })
                .collect(),
            folders,
        }
    }

    /// The project with id `id`: its folder and file.
    fn find_project(&self, id: &str) -> Result<(PathBuf, project::ProjectFile), RpcError> {
        let roots = self.recording_roots();
        let mut all: Vec<_> = roots.iter().flat_map(|root| project::scan(root)).collect();
        if let Some(at) = all.iter().position(|(_, file)| file.id == id) {
            return Ok(all.swap_remove(at));
        }
        all.sort_by(|a, b| b.1.created_at.cmp(&a.1.created_at));
        let ids: Vec<String> = all.iter().take(10).map(|(_, f)| f.id.clone()).collect();
        Err(invalid(format!(
            "project: no project {id:?} in {}; the newest are: {}",
            roots
                .iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
            if ids.is_empty() {
                "none".to_string()
            } else {
                ids.join(", ")
            }
        )))
    }

    /// See [`Method::RenameProject`](isocast_protocol::api::Method::RenameProject).
    pub(crate) fn rename_project(
        &self,
        params: RenameProjectParams,
    ) -> Result<ProjectRenamed, RpcError> {
        let name = project::check_name(&params.name).map_err(|err| invalid(err.to_string()))?;
        // A retake picks its take's folder before it starts recording into
        // it: wait for one running. Then the lock starting a recording holds
        // while it picks its project, so neither sees the other half done.
        let _retaking = self.retaking.lock().unwrap();
        let _projects = self.projects.lock().unwrap();
        let (folder, file) = self.find_project(&params.project)?;
        if self
            .recording_project()
            .is_some_and(|p| same_folder(&p.folder, &folder))
        {
            return Err(wrong_state(format!(
                "project {:?} is being recorded into; rename it once the recording has stopped",
                file.id
            )));
        }
        if let Some(running) = self.export.lock().unwrap().as_ref() {
            return Err(wrong_state(format!(
                "export {} is running and may be reading or writing this project's files; \
                 rename it once the export has finished",
                running.export
            )));
        }
        let old_name = project::name_of(&folder, &file);
        let mut notes = Vec::new();
        if old_name == name {
            return Ok(ProjectRenamed {
                project: project::describe(&folder, &file, false),
                old_name,
                old_folder: folder,
                notes: vec!["it already has that name".into()],
            });
        }
        let root = folder.parent().unwrap_or(Path::new("/")).to_path_buf();
        let mut renamed = None;
        for n in 1..100u32 {
            let target = match n {
                1 => root.join(format!("{} {name}", file.prefix)),
                n => root.join(format!("{} {name} {n}", file.prefix)),
            };
            match rename_exclusive(&folder, &target) {
                Ok(()) => {
                    if n > 1 {
                        notes.push(format!(
                            "a folder named {:?} already exists, so this one is named {:?}",
                            format!("{} {name}", file.prefix),
                            target.file_name().unwrap_or_default().to_string_lossy()
                        ));
                    }
                    renamed = Some(target);
                    break;
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    return Err(failed(format!(
                        "could not rename {} to {}: {err}",
                        folder.display(),
                        target.display()
                    )));
                }
            }
        }
        let Some(target) = renamed else {
            return Err(failed(format!(
                "every name like {:?} is taken in {}; choose another name",
                format!("{} {name}", file.prefix),
                root.display()
            )));
        };
        // Timelines name their takes' manifests: move them too, or undo.
        let moved = self
            .timelines
            .lock()
            .unwrap()
            .move_folder(&folder, &target, Some(&file.id));
        if let Err(err) = moved {
            let undone = rename_exclusive(&target, &folder);
            return Err(failed(format!(
                "the timelines couldn't be updated ({}), so the folder {}",
                err.message,
                match undone {
                    Ok(()) => "was renamed back".to_string(),
                    Err(undo) => format!(
                        "stays renamed to {} (renaming it back failed: {undo})",
                        target.display()
                    ),
                }
            )));
        }
        notes.extend(rewrite_exports(&target, &folder));
        self.moved_last_report(&folder, &target);
        let project = project::describe(&target, &file, false);
        self.publish(Event::ProjectRenamed {
            project: project.clone(),
            old_name: old_name.clone(),
            old_folder: folder.clone(),
        });
        Ok(ProjectRenamed {
            project,
            old_name,
            old_folder: folder,
            notes,
        })
    }

    /// The last recording's report (`status.last`) after its project moved.
    fn moved_last_report(&self, from: &Path, to: &Path) {
        let mut state = self.state.lock().unwrap();
        let Some(last) = state.last.as_mut() else {
            return;
        };
        let fix = |path: &mut PathBuf| {
            if let Some(new) = project::moved(path, from, to) {
                *path = new;
            }
        };
        fix(&mut last.folder);
        fix(&mut last.manifest);
        last.takes.iter_mut().for_each(fix);
        last.files.iter_mut().for_each(|f| fix(&mut f.path));
    }
}

/// Whether two folders are the same (one may be spelled differently).
fn same_folder(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(a), Ok(b)) if a == b
        )
}

/// Export manifests in a renamed project name its files and takes by
/// path: point those inside it at the new folder. Returns notes for any
/// that couldn't be updated.
fn rewrite_exports(folder: &Path, old: &Path) -> Vec<String> {
    export_manifests(folder)
        .filter_map(|(path, json)| rewrite_export(&path, json, old, folder))
        .collect()
}

/// Export manifests in `folder` that name the folder they were written in
/// by a path that's gone (the folder was renamed in the Finder, so
/// `rename_project` never rewrote them): point them at `folder` (#56).
/// Returns notes for any that couldn't be updated.
pub(crate) fn heal_exports(folder: &Path) -> Vec<String> {
    let mut notes = Vec::new();
    for (path, json) in export_manifests(folder) {
        // Still being written by the export that made it: leave it.
        if json.get("status").and_then(|s| s.as_str()) == Some("exporting") {
            continue;
        }
        // Its own files are written beside it: their folder is where it was.
        let Some(file) = json
            .pointer("/files/0/path")
            .and_then(|p| p.as_str())
            .map(PathBuf::from)
        else {
            continue;
        };
        let (Some(old), Some(name)) = (file.parent(), file.file_name()) else {
            continue;
        };
        if same_folder(old, folder)
            || old.symlink_metadata().is_ok()
            || folder.join(name).symlink_metadata().is_err()
        {
            continue;
        }
        let old = old.to_path_buf();
        notes.extend(rewrite_export(&path, json, &old, folder));
    }
    notes
}

/// The export manifests (`*.export.json`) in `folder` that can be read.
fn export_manifests(folder: &Path) -> impl Iterator<Item = (PathBuf, serde_json::Value)> {
    std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .ends_with(".export.json")
        })
        .filter_map(|entry| {
            let path = entry.path();
            let json = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
            Some((path, json))
        })
}

/// Rewrite the export manifest at `path` (`json`) with its paths inside
/// `old` moved to `new`, if any are; a note if it couldn't be written.
fn rewrite_export(
    path: &Path,
    mut json: serde_json::Value,
    old: &Path,
    new: &Path,
) -> Option<String> {
    if !move_paths(&mut json, old, new) {
        return None;
    }
    let partial = path.with_extension("json.partial");
    let written = serde_json::to_vec_pretty(&json)
        .map_err(std::io::Error::other)
        .and_then(|bytes| std::fs::write(&partial, bytes))
        .and_then(|()| std::fs::rename(&partial, path));
    let err = written.err()?;
    let _ = std::fs::remove_file(&partial);
    Some(format!(
        "the export manifest {} still names files in the old folder: {err}",
        path.display()
    ))
}

/// Rewrite every string in `json` that is a path inside `from` to the
/// same path inside `to`; true if any changed.
fn move_paths(json: &mut serde_json::Value, from: &Path, to: &Path) -> bool {
    match json {
        serde_json::Value::String(text) => match project::moved(Path::new(text), from, to) {
            Some(new) => {
                *text = new.to_string_lossy().into_owned();
                true
            }
            None => false,
        },
        // Every item, not stopping at the first that changed.
        serde_json::Value::Array(items) => {
            let mut any = false;
            for item in items {
                any |= move_paths(item, from, to);
            }
            any
        }
        serde_json::Value::Object(map) => {
            let mut any = false;
            for item in map.values_mut() {
                any |= move_paths(item, from, to);
            }
            any
        }
        _ => false,
    }
}
