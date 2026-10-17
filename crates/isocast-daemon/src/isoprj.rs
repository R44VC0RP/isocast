//! Project files (#88, the format is `isocast_protocol::isoprj`): one
//! background writer keeps `<Project name>.isoprj` in each project folder
//! up to date; `project_open` opens one (taking in timelines copied from
//! elsewhere), `recent_projects` lists those opened lately, and
//! `timeline_relink` points an offline clip at its take.
//!
//! Never in a recording's way: timelines tell the writer which folders
//! changed with a channel send (the recording's callbacks write only the
//! timelines folder, as before), the writer leaves the folder a recording
//! writes into alone until it stops, and every write is atomic (a hidden
//! temporary file, synced, then renamed) and only ever warns if it fails.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use parking_lot::Mutex;

use isocast_protocol::api::{Event, RpcError};
use isocast_protocol::isoprj::{
    self, IsoprjFile, ProjectOpenParams, ProjectOpened, RecentProject, RecentProjects,
    RecentProjectsParams,
};
use isocast_protocol::manifest::SessionStatus;
use isocast_protocol::project::{self, ProjectFile};
use isocast_protocol::timeline::{
    Timeline, TimelineAction, TimelineInfo, TimelineRelinkParams, TimelineSummary,
};

use crate::Daemon;
use crate::projects::same_folder;
use crate::timeline::{self, failed, invalid, now_text, wrong_state};

/// How long the writer gathers changes before writing (a burst of edits,
/// or a stop's several timeline writes, is one write per folder).
const SETTLE: Duration = Duration::from_millis(300);

/// How often folders waiting for a recording to stop are looked at again.
const RECHECK: Duration = Duration::from_millis(500);

/// How long a graceful shutdown waits for the writer to finish.
const FLUSH_WAIT: Duration = Duration::from_secs(3);

/// How long the start-up pass waits for a folder's disk to answer.
const PROBE: Duration = Duration::from_secs(2);

/// Whether `folder` exists and its disk answers within `wait`; a stalled
/// look is left to finish on its own thread.
fn answers_within(folder: &Path, wait: Duration) -> bool {
    let (tx, rx) = mpsc::channel();
    let probe = folder.to_path_buf();
    let spawned = std::thread::Builder::new()
        .name("isoprj probe".into())
        .spawn(move || {
            let _ = tx.send(probe.is_dir());
        });
    if spawned.is_err() {
        return true;
    }
    match rx.recv_timeout(wait) {
        Ok(exists) => exists,
        Err(_) => {
            isocast_diagnostics::diagnostic!(
                "isocastd: {} didn't answer within {}s; its project file is updated when it \
                 changes next",
                folder.display(),
                wait.as_secs()
            );
            false
        }
    }
}

/// The writer's end of the channel, once started.
#[derive(Default)]
pub(crate) struct Writer {
    tx: Mutex<Option<mpsc::Sender<PathBuf>>>,
    /// Told when the writer has written everything asked for before a
    /// [`Writer::flush`] (an empty path on the channel asks for it).
    flushed: Mutex<Option<mpsc::Sender<()>>>,
}

impl Writer {
    /// Ask for `folder`'s project file to be written again (if it is a
    /// project's). Never blocks.
    pub(crate) fn dirty(&self, folder: &Path) {
        if let Some(tx) = self.tx.lock().as_ref() {
            let _ = tx.send(folder.to_path_buf());
        }
    }

    /// Write every project file asked for so far now, without the usual
    /// moment's wait, and wait (a few seconds at most) until it's done: a
    /// quit right after a stop leaves no stale `.isoprj` (#102). Folders
    /// still being recorded into are left alone, as always.
    pub(crate) fn flush(&self) {
        let (done, wait) = mpsc::channel();
        *self.flushed.lock() = Some(done);
        let sent = self
            .tx
            .lock()
            .as_ref()
            .is_some_and(|tx| tx.send(PathBuf::new()).is_ok());
        if sent && wait.recv_timeout(FLUSH_WAIT).is_err() {
            isocast_diagnostics::diagnostic!(
                "isocastd: the project file writer didn't finish within {}s of the shutdown",
                FLUSH_WAIT.as_secs()
            );
        }
        self.flushed.lock().take();
    }
}

/// What [`Daemon::write_project_file`] did.
pub(crate) enum Written {
    /// The folder isn't a project's: nothing to write.
    NotAProject,
    /// A recording started writing into the folder: left for later.
    Busy,
    /// The file is up to date; `kept_newer` names the timelines whose
    /// edits in the file are newer than the engine's (so the engine's
    /// weren't written).
    Done {
        file: PathBuf,
        kept_newer: Vec<String>,
    },
}

/// Start the writer, give the timelines its channel, and (a moment later,
/// off the start-up path) ask for every project file that should exist:
/// those of folders timelines use, and of every project folder in the
/// recordings folders that has none yet.
pub(crate) fn start(daemon: &Arc<Daemon>) {
    let (tx, rx) = mpsc::channel::<PathBuf>();
    daemon.timelines.lock().set_writer(tx.clone());
    *daemon.isoprj.tx.lock() = Some(tx);
    let writer = daemon.clone();
    std::thread::Builder::new()
        .name("isoprj writer".into())
        .spawn(move || writer.contain("project file writer", || writer.write_loop(rx)))
        .ok();
    let first = daemon.clone();
    std::thread::Builder::new()
        .name("isoprj first".into())
        .spawn(move || {
            std::thread::sleep(Duration::from_secs(2));
            first.contain("project files at start", || {
                let mut folders = first.timelines.lock().all_take_folders();
                for root in first.recording_roots() {
                    folders.extend(
                        project::scan(&root)
                            .into_iter()
                            .map(|(folder, _)| folder)
                            .filter(|folder| isoprj::find_in(folder).is_empty()),
                    );
                }
                for folder in folders {
                    // A folder on a sleeping or gone network disk would
                    // hold up every folder behind it in the writer (#102).
                    if answers_within(&folder, PROBE) {
                        first.isoprj.dirty(&folder);
                    }
                }
            })
        })
        .ok();
}

impl Daemon {
    /// The writer: gathers asked-for folders for a moment, then writes
    /// each project file; folders being recorded into wait until the
    /// recording stops.
    fn write_loop(self: &Arc<Self>, rx: mpsc::Receiver<PathBuf>) {
        let mut pending: BTreeSet<PathBuf> = BTreeSet::new();
        let mut warned: HashSet<PathBuf> = HashSet::new();
        let mut kept_warned: HashSet<PathBuf> = HashSet::new();
        loop {
            let next = match pending.is_empty() {
                true => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
                false => rx.recv_timeout(RECHECK),
            };
            // An empty path is a flush: write now, then say so.
            let mut flush = false;
            match next {
                Ok(folder) => {
                    flush = folder.as_os_str().is_empty();
                    pending.insert(folder);
                    if !flush {
                        std::thread::sleep(SETTLE);
                    }
                    pending.extend(rx.try_iter());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
            flush |= pending.remove(Path::new(""));
            let ready: Vec<PathBuf> = pending
                .iter()
                .filter(|folder| !self.folder_busy(folder))
                .cloned()
                .collect();
            for folder in ready {
                pending.remove(&folder);
                match self.write_project_file(&folder) {
                    Ok(Written::Busy) => {
                        pending.insert(folder);
                    }
                    Ok(Written::NotAProject) => {
                        warned.remove(&folder);
                    }
                    Ok(Written::Done { file, kept_newer }) => {
                        warned.remove(&folder);
                        if kept_newer.is_empty() {
                            kept_warned.remove(&folder);
                        } else if kept_warned.insert(folder.clone()) {
                            // Once per folder until the file and the
                            // engine agree again (#102).
                            let message = format!(
                                "the project file {} has newer edits to timeline{} {} (made \
                                 elsewhere and copied back) than this Mac's, so this Mac's \
                                 edits to {} aren't written there; open the project (`isocast \
                                 project open \"{}\"`) to take the file's edits in",
                                file.display(),
                                if kept_newer.len() == 1 { "" } else { "s" },
                                kept_newer.join(", "),
                                if kept_newer.len() == 1 { "it" } else { "them" },
                                file.display()
                            );
                            isocast_diagnostics::diagnostic!("isocastd: {message}");
                            self.publish(Event::Warning { message });
                        }
                    }
                    Err(message) => {
                        isocast_diagnostics::diagnostic!("isocastd: {message}");
                        // Once per folder until it works again.
                        if warned.insert(folder.clone()) {
                            self.publish(Event::Warning { message });
                        }
                    }
                }
            }
            if flush && let Some(done) = self.isoprj.flushed.lock().take() {
                let _ = done.send(());
            }
        }
    }

    /// Whether a recording writes into `folder` (this engine's, or a
    /// standalone `isocast record`'s): the writer leaves it alone until it
    /// stops.
    fn folder_busy(&self, folder: &Path) -> bool {
        self.recording_project()
            .is_some_and(|rec| same_folder(&rec.folder, folder))
            || being_recorded(folder)
    }

    /// Write `folder`'s project file from the timelines that use it and
    /// its takes, keeping timelines in the file the engine doesn't have
    /// (copied from another Mac and not opened yet) and edits there newer
    /// than the engine's. Returns the file, if `folder` is a project.
    pub(crate) fn write_project_file(&self, folder: &Path) -> Result<Written, String> {
        let Some(identity) = project::read(folder) else {
            return Ok(Written::NotAProject);
        };
        remove_stale_partials(folder);
        let name = project::name_of(folder, &identity);
        let target = folder.join(isoprj::file_name(&name));
        let (existing_path, existing) = match own_file(folder, &identity, &target) {
            Own::None => (None, None),
            Own::Found(path, file) => (Some(path), Some(*file)),
            // Never overwrite a file this Isocast can't read (a newer
            // one's, or damaged): it may hold edits.
            Own::Unreadable(path, why) => {
                return Err(format!(
                    "the project file {} {why}, so it isn't updated",
                    path.display()
                ));
            }
        };
        let central = self.timelines.lock().using_folder(folder);
        let mut timelines: Vec<Timeline> = existing
            .as_ref()
            .map(|file| resolved(file, folder))
            .unwrap_or_default();
        let mut kept_newer = Vec::new();
        for timeline in central {
            match timelines
                .iter_mut()
                .find(|kept| isoprj::same_timeline(kept, &timeline))
            {
                // The file's edits are newer (made elsewhere, then copied
                // back): keep them for `project_open` to take in, and say
                // so (#102).
                Some(kept) if kept.revision > timeline.revision => {
                    kept_newer.push(format!(
                        "{:?} (revision {} in the file, {} here)",
                        timeline.name, kept.revision, timeline.revision
                    ));
                }
                Some(kept) => *kept = timeline,
                None => timelines.push(timeline),
            }
        }
        for timeline in &mut timelines {
            isoprj::map_paths(timeline, |path| isoprj::relative_to(path, folder));
        }
        let takes = project::describe(folder, &identity, false)
            .takes
            .iter()
            .map(|take| isoprj::relative_to(take, folder))
            .collect();
        let mut file = IsoprjFile {
            format: isoprj::FORMAT.into(),
            version: isoprj::ISOPRJ_VERSION,
            project: identity.id.clone(),
            name,
            prefix: identity.prefix.clone(),
            created_at: identity.created_at.clone(),
            saved_at: String::new(),
            takes,
            timelines,
        };
        // A recording may have started writing into the folder since the
        // writer looked (#102): never write beside it.
        if self.folder_busy(folder) {
            return Ok(Written::Busy);
        }
        // Renamed (by `rename_project`, or in the Finder): the file follows.
        if let Some(old) = existing_path.as_ref().filter(|old| **old != target)
            && !target.exists()
        {
            crate::projects::rename_exclusive(old, &target).map_err(|err| {
                format!(
                    "could not rename the project file {} to {}: {err}",
                    old.display(),
                    target.display()
                )
            })?;
        }
        if let Some(existing) = &existing {
            file.saved_at = existing.saved_at.clone();
            if *existing == file && target.exists() {
                return Ok(Written::Done {
                    file: target,
                    kept_newer,
                });
            }
        }
        file.saved_at = now_text();
        isoprj::write_atomic(&target, &file).map_err(|err| {
            format!(
                "could not write the project file {}: {err}",
                target.display()
            )
        })?;
        Ok(Written::Done {
            file: target,
            kept_newer,
        })
    }

    /// See [`Method::ProjectOpen`](isocast_protocol::api::Method::ProjectOpen).
    pub(crate) fn project_open(
        self: &Arc<Self>,
        params: ProjectOpenParams,
    ) -> Result<ProjectOpened, RpcError> {
        let path = crate::expand_home(&params.path.to_string_lossy());
        if path.is_relative() {
            return Err(invalid(format!(
                "path: {} is relative; give the project file's (or folder's) full path",
                path.display()
            )));
        }
        let path = isoprj::normalize(&path);
        let mut notes = Vec::new();
        let (folder, file_path) = if path.is_dir() {
            let identity = project::read(&path);
            let files = isoprj::find_in(&path);
            let own = files
                .iter()
                .find(|file| {
                    isoprj::read(file)
                        .is_ok_and(|f| identity.as_ref().is_none_or(|id| id.id == f.project))
                })
                .or(files.first())
                .cloned();
            (path.clone(), own)
        } else if isoprj::is_project_file(&path) && path.is_file() {
            let folder = path.parent().unwrap_or(Path::new("/")).to_path_buf();
            (folder, Some(path.clone()))
        } else if path.exists() {
            return Err(invalid(format!(
                "path: {} is neither a project file (.{}) nor a project folder",
                path.display(),
                isoprj::EXTENSION
            )));
        } else {
            return Err(invalid(format!("path: {} doesn't exist", path.display())));
        };
        let parsed = match &file_path {
            None => None,
            Some(file) => match isoprj::read(file) {
                Ok(parsed) => Some(parsed),
                // A newer Isocast's file: never touched, and not opened.
                Err(why) if why.contains("newer Isocast") => {
                    return Err(failed(format!("path: {} {why}", file.display())));
                }
                // Not a project's folder: nothing to open, and nothing
                // there is touched.
                Err(why) if project::read(&folder).is_none() => {
                    return Err(invalid(format!("path: {} {why}", file.display())));
                }
                Err(why) => {
                    // Kept for whoever wants it (an earlier one too); a new
                    // one is written.
                    let aside = (1..100)
                        .map(|n| match n {
                            1 => file.with_extension(format!("{}.damaged", isoprj::EXTENSION)),
                            n => file.with_extension(format!("{}.damaged-{n}", isoprj::EXTENSION)),
                        })
                        .find(|p| p.symlink_metadata().is_err())
                        .unwrap_or_else(|| file.with_extension("isoprj.damaged-last"));
                    let moved = crate::projects::rename_exclusive(file, &aside);
                    notes.push(format!(
                        "the project file {} {why}; {}, and the project was opened from its \
                         takes and the engine's timelines",
                        file.display(),
                        match moved {
                            Ok(()) => format!("it was moved to {}", aside.display()),
                            Err(err) => format!("it couldn't be moved aside ({err})"),
                        }
                    ));
                    None
                }
            },
        };
        // Whether `folder` is this project's: its own identity says so, or
        // (copied without the hidden file) it is named like the project and
        // holds one of the file's takes. Anything else (a loose file in
        // Downloads, another project's folder) opens read-only: the
        // timelines are taken in, nothing is written into the folder, and
        // it never becomes a project (#88 review, follow-up 1).
        let folder_identity = project::read(&folder);
        let read_only = match (&folder_identity, &parsed) {
            (Some(identity), Some(file)) => identity.id != file.project,
            (Some(_), None) => false,
            (None, Some(file)) => !looks_like_project(&folder, file),
            (None, None) => false,
        };
        let identity = match (folder_identity, &parsed) {
            (_, Some(parsed)) if read_only => {
                notes.push(format!(
                    "opened read-only: {} isn't project {:?}'s folder (it isn't named like it \
                     and holds none of its takes), so its timelines were taken in and nothing \
                     was written there; to keep the project there, copy its whole folder",
                    folder.display(),
                    parsed.name
                ));
                ProjectFile {
                    version: project::PROJECT_VERSION,
                    id: parsed.project.clone(),
                    prefix: parsed.prefix.clone(),
                    created_at: parsed.created_at.clone(),
                }
            }
            (Some(identity), _) => identity,
            // The hidden file didn't come along (copied without it): the
            // project file says who it is.
            (None, Some(parsed)) => {
                let identity = ProjectFile {
                    version: project::PROJECT_VERSION,
                    id: parsed.project.clone(),
                    prefix: parsed.prefix.clone(),
                    created_at: parsed.created_at.clone(),
                };
                match restore_identity(&folder, &identity) {
                    Ok(()) => notes.push(format!(
                        "{} was missing; it was made again from the project file",
                        project::PROJECT_FILE
                    )),
                    Err(err) => notes.push(format!(
                        "{} is missing and couldn't be made again ({err}); the project opens, \
                         but recordings won't add to it",
                        project::PROJECT_FILE
                    )),
                }
                identity
            }
            (None, None) => {
                return Err(invalid(format!(
                    "path: {} isn't an Isocast project (it has no .{} file and no {})",
                    folder.display(),
                    isoprj::EXTENSION,
                    project::PROJECT_FILE
                )));
            }
        };
        let name = match (read_only, &parsed) {
            (true, Some(file)) => file.name.clone(),
            _ => project::name_of(&folder, &identity),
        };
        let mut from_file: Vec<Timeline> = parsed
            .as_ref()
            .map(|file| resolved(file, &folder))
            .unwrap_or_default();
        // A take is a manifest: a timeline naming anything else (a crafted
        // file) isn't taken in.
        from_file.retain(|timeline| {
            let bad = isoprj::takes_of(timeline).any(|take| {
                !take
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().ends_with(".session.json"))
            });
            if bad {
                notes.push(format!(
                    "timeline {:?} in the project file names takes that aren't take manifests \
                     (….session.json), so it wasn't taken in",
                    timeline.name
                ));
            }
            !bad
        });

        let mut changed: Vec<(String, TimelineAction)> = Vec::new();
        // Read before the timelines' lock is taken.
        let recording = self.recording_project().map(|p| p.folder);
        let chosen = {
            let mut timelines = self.timelines.lock();
            let mut candidates: Vec<String> = Vec::new();
            for theirs in from_file {
                let ours = timelines
                    .all()
                    .find(|t| isoprj::same_timeline(t, &theirs) && isoprj::uses_folder(t, &folder))
                    .map(|t| t.id.clone())
                    .or_else(|| moved_here(&timelines, &theirs, &folder, &identity.id));
                let id = match ours {
                    Some(id) => {
                        // Moved here (its folder renamed or moved away):
                        // point it at this folder.
                        let olds = old_folders(timelines.get(&id), &folder, &identity.id);
                        for old in olds {
                            timelines.move_folder(&old, &folder, Some(&identity.id))?;
                            changed.push((id.clone(), TimelineAction::Relinked));
                            notes.push(format!(
                                "timeline {id:?} named its takes in {}, which is gone; they are \
                                 now found here",
                                old.display()
                            ));
                        }
                        let newer = timelines
                            .get(&id)
                            .is_some_and(|t| theirs.revision > t.revision);
                        if newer {
                            timelines.change(&id, |t| {
                                let keep = t.id.clone();
                                *t = theirs.clone();
                                t.id = keep;
                                t.recording.clear();
                                Ok(())
                            })?;
                            changed.push((id.clone(), TimelineAction::Relinked));
                            notes.push(format!(
                                "timeline {id:?} took the project file's newer edits (revision \
                                 {})",
                                theirs.revision
                            ));
                        }
                        id
                    }
                    None => {
                        let was = theirs.id.clone();
                        let id = timelines.import(theirs)?;
                        changed.push((id.clone(), TimelineAction::Created));
                        notes.push(match id == was {
                            true => format!("imported timeline {id:?} from the project file"),
                            false => format!(
                                "imported timeline {was:?} from the project file as {id:?} \
                                 ({was:?} is another timeline here)"
                            ),
                        });
                        id
                    }
                };
                if !candidates.contains(&id) {
                    candidates.push(id);
                }
            }
            // Timelines using the folder the file doesn't have yet.
            for timeline in timelines.using_folder(&folder) {
                if !candidates.contains(&timeline.id) {
                    candidates.push(timeline.id);
                }
            }
            if candidates.is_empty() && read_only {
                return Err(wrong_state(format!(
                    "path: the project file has no timelines to open, and {} isn't the \
                     project's folder; open the project's own folder instead",
                    folder.display()
                )));
            }
            if candidates.is_empty() {
                // No timeline yet: one of the project's takes, in order.
                let takes = project::describe(&folder, &identity, false).takes;
                if takes.is_empty() {
                    return Err(wrong_state(format!(
                        "project {:?} in {} has no takes yet; record into it first",
                        name,
                        folder.display()
                    )));
                }
                let id = timelines.create(Some(name.clone()))?;
                let mut skipped = Vec::new();
                let mut recording_now = Vec::new();
                timelines.change(&id, |t| {
                    for take in &takes {
                        match timeline::read_manifest(take) {
                            Ok(manifest)
                                if manifest.status != SessionStatus::Recording
                                    && timeline::unfinished("", take, &manifest).is_none() =>
                            {
                                timeline::append(t, take, &manifest);
                            }
                            // Being recorded now (this engine's take): it
                            // joins the timeline when it stops (#102).
                            Ok(manifest)
                                if manifest.status == SessionStatus::Recording
                                    && recording
                                        .as_ref()
                                        .is_some_and(|rec| same_folder(rec, &folder)) =>
                            {
                                recording_now.push(take.display().to_string());
                            }
                            _ => skipped.push(take.display().to_string()),
                        }
                    }
                    Ok(())
                })?;
                let skipped_all = skipped.len() + recording_now.len();
                changed.push((id.clone(), TimelineAction::Created));
                notes.push(format!(
                    "made timeline {id:?} of the project's {} take{}",
                    takes.len() - skipped_all,
                    if takes.len() - skipped_all == 1 {
                        ""
                    } else {
                        "s"
                    }
                ));
                if !recording_now.is_empty() {
                    notes.push(format!(
                        "the take being recorded now joins the timeline when it stops: {}",
                        recording_now.join(", ")
                    ));
                }
                if !skipped.is_empty() {
                    notes.push(format!(
                        "takes that never finished aren't on it: {}",
                        skipped.join(", ")
                    ));
                }
                candidates.push(id);
            }
            let chosen = match params.timeline.as_deref() {
                Some(id) if candidates.iter().any(|c| c == id) => id.to_string(),
                Some(id) => {
                    return Err(invalid(format!(
                        "timeline: {id:?} isn't one of project {name:?}'s timelines; they are: {}",
                        candidates.join(", ")
                    )));
                }
                None => match timelines
                    .current()
                    .filter(|c| candidates.iter().any(|x| x == c))
                {
                    Some(current) => current.to_string(),
                    None => candidates
                        .iter()
                        .max_by_key(|id| {
                            timelines
                                .get(id)
                                .map(|t| t.created_at.clone())
                                .unwrap_or_default()
                        })
                        .cloned()
                        .unwrap_or_default(),
                },
            };
            if timelines.current() != Some(chosen.as_str()) {
                timelines.set_current(Some(chosen.clone()))?;
            }
            (chosen, candidates)
        };
        let (chosen, candidates) = chosen;
        for (id, action) in changed {
            self.timeline_changed(&id, action, None);
        }
        self.timeline_changed(&chosen, TimelineAction::Opened, None);
        let info: TimelineInfo = self.timeline_info(&chosen, Vec::new())?;
        let (summaries, offline) = {
            let timelines = self.timelines.lock();
            let summaries: Vec<TimelineSummary> = candidates
                .iter()
                .filter_map(|id| timelines.summary(id))
                .collect();
            let mut offline: Vec<PathBuf> = candidates
                .iter()
                .filter_map(|id| timelines.get(id))
                .flat_map(|t| t.clips.iter().map(|c| c.take.clone()))
                .filter(|take| take.symlink_metadata().is_err())
                .collect();
            offline.sort();
            offline.dedup();
            (summaries, offline)
        };
        if !offline.is_empty() {
            notes.push(format!(
                "{} take{} offline (not where the timeline says); relink with `isocast \
                 timeline relink CLIP PATH` (MCP relink_clip)",
                offline.len(),
                if offline.len() == 1 { " is" } else { "s are" }
            ));
        }
        // Named for the project (the folder's name after its date): the
        // writer renames a copy's file to match, as it does after a rename.
        let file = match (read_only, &file_path) {
            (true, Some(given)) => given.clone(),
            _ => folder.join(isoprj::file_name(&name)),
        };
        remember_recent(&file, &name);
        if !read_only {
            self.isoprj.dirty(&folder);
        }
        let editor = params.editor.unwrap_or(true);
        self.publish(Event::ProjectOpened {
            path: file.clone(),
            folder: folder.clone(),
            project: identity.id.clone(),
            timeline: chosen,
            editor,
        });
        Ok(ProjectOpened {
            path: file,
            folder,
            project: identity.id,
            name,
            timeline: info,
            timelines: summaries,
            offline,
            notes,
        })
    }

    /// See [`Method::RecentProjects`](isocast_protocol::api::Method::RecentProjects).
    pub(crate) fn recent_projects(&self, params: RecentProjectsParams) -> RecentProjects {
        if params.clear {
            let _ = write_recent(&[]);
        }
        let mut list = read_recent();
        for project in &mut list.projects {
            // Renamed since (with its project, or in the Finder): the
            // folder's own project file.
            if !project.path.is_file()
                && let Some(folder) = project.path.parent()
                && let Some(identity) = project::read(folder)
            {
                let now = folder.join(isoprj::file_name(&project::name_of(folder, &identity)));
                if now.is_file() {
                    project.path = now;
                }
            }
            project.exists = project.path.is_file();
        }
        list
    }

    /// See [`Method::TimelineRelink`](isocast_protocol::api::Method::TimelineRelink).
    pub(crate) fn timeline_relink(
        &self,
        params: TimelineRelinkParams,
    ) -> Result<TimelineInfo, RpcError> {
        let target_path = crate::expand_home(&params.path.to_string_lossy());
        if target_path.is_relative() {
            return Err(invalid(format!(
                "path: {} is relative; give the take's manifest (or its folder) by full path",
                target_path.display()
            )));
        }
        let (id, note) = {
            let mut timelines = self.timelines.lock();
            let id = timelines.resolve(params.timeline.as_deref())?;
            let take = {
                let t = timelines
                    .get(&id)
                    .ok_or_else(|| invalid(format!("timeline: no timeline {id:?}")))?;
                let at = timeline::find(t, &params.clip)?;
                t.clips[at].take.clone()
            };
            let name = take.file_name().unwrap_or_default().to_os_string();
            let target = match target_path.is_dir() {
                true => target_path.join(&name),
                false => target_path.clone(),
            };
            if !target.is_file() {
                return Err(invalid(format!(
                    "path: {} has no {}; give the folder holding clip {}'s take, or its manifest",
                    target_path.display(),
                    name.to_string_lossy(),
                    params.clip
                )));
            }
            timeline::read_manifest(&target).map_err(|why| invalid(format!("path: {why}")))?;
            let old = take.parent().unwrap_or(Path::new("/")).to_path_buf();
            let new = target.parent().unwrap_or(Path::new("/")).to_path_buf();
            // Relinking an offline clip to its folder: the folder moved, so
            // every offline take of every timeline that was in it and is in
            // the new one too is found there. A clip whose take is still
            // where it was is relinked alone (#102): other timelines' takes
            // there are fine as they are.
            let offline = take.symlink_metadata().is_err();
            let note = if offline && target.file_name() == Some(name.as_os_str()) {
                let project = project::read(&new).map(|f| f.id);
                let moved = timelines.remap(
                    |path| {
                        let name = path.file_name()?;
                        (path.parent() == Some(old.as_path())
                            && path.symlink_metadata().is_err()
                            && new.join(name).is_file())
                        .then(|| new.join(name))
                    },
                    project.as_deref(),
                )?;
                format!(
                    "takes in {} that are in {} are now found there ({} timeline{})",
                    old.display(),
                    new.display(),
                    moved.len(),
                    if moved.len() == 1 { "" } else { "s" }
                )
            } else {
                timelines.change(&id, |t| {
                    let project = project::read(&new).map(|f| f.id);
                    isoprj::map_paths(t, |path| match path == take {
                        true => target.clone(),
                        false => path.to_path_buf(),
                    });
                    for clip in &mut t.clips {
                        if clip.take == target {
                            clip.project = project.clone();
                        }
                    }
                    Ok(())
                })?;
                format!(
                    "clip {}'s take is now {} (it was {})",
                    params.clip,
                    target.display(),
                    take.display()
                )
            };
            (id, note)
        };
        self.timeline_changed(&id, TimelineAction::Relinked, Some(params.clip.clone()));
        self.timeline_info(&id, vec![note])
    }
}

/// A project file in `folder`, as the writer finds it.
enum Own {
    None,
    Found(PathBuf, Box<IsoprjFile>),
    Unreadable(PathBuf, String),
}

/// `folder`'s own project file: `target` if it is there, else the one
/// other with the project's id (renamed since). Files of other projects
/// (copied in by hand) are never touched.
fn own_file(folder: &Path, identity: &ProjectFile, target: &Path) -> Own {
    if target.exists() {
        return match isoprj::read(target) {
            Ok(file) if file.project == identity.id => {
                Own::Found(target.to_path_buf(), Box::new(file))
            }
            Ok(file) => Own::Unreadable(
                target.to_path_buf(),
                format!("is project {:?}'s, not this one's", file.project),
            ),
            Err(why) => Own::Unreadable(target.to_path_buf(), why),
        };
    }
    let own: Vec<(PathBuf, IsoprjFile)> = isoprj::find_in(folder)
        .into_iter()
        .filter_map(|path| isoprj::read(&path).ok().map(|file| (path, file)))
        .filter(|(_, file)| file.project == identity.id)
        .collect();
    match <[_; 1]>::try_from(own) {
        Ok([(path, file)]) => Own::Found(path, Box::new(file)),
        _ => Own::None,
    }
}

/// Whether `folder`, which has no project identity, is the folder `file`
/// was written in (copied without its hidden file): named like the
/// project (its date-and-time prefix) and holding one of its takes.
fn looks_like_project(folder: &Path, file: &IsoprjFile) -> bool {
    let named = folder
        .file_name()
        .is_some_and(|n| !file.prefix.is_empty() && n.to_string_lossy().starts_with(&file.prefix));
    let holds_a_take = file.takes.iter().any(|take| {
        take.components().count() == 1
            && take.to_string_lossy().ends_with(".session.json")
            && folder.join(take).is_file()
    });
    named && holds_a_take
}

/// A project file's timelines with their paths made absolute.
fn resolved(file: &IsoprjFile, folder: &Path) -> Vec<Timeline> {
    file.timelines
        .iter()
        .cloned()
        .map(|mut timeline| {
            isoprj::map_paths(&mut timeline, |path| isoprj::resolve(path, folder));
            timeline
        })
        .collect()
}

/// The engine's timeline that is `theirs` but whose takes from this
/// project are gone from where it says (the folder was moved), if one is.
fn moved_here(
    timelines: &timeline::Timelines,
    theirs: &Timeline,
    folder: &Path,
    project: &str,
) -> Option<String> {
    timelines
        .all()
        .filter(|t| isoprj::same_timeline(t, theirs))
        .find(|t| !old_folders(Some(t), folder, project).is_empty())
        .map(|t| t.id.clone())
}

/// The folders `timeline` names takes of project `project` in that are
/// gone, whose takes are in `folder` now.
fn old_folders(timeline: Option<&Timeline>, folder: &Path, project: &str) -> Vec<PathBuf> {
    let Some(timeline) = timeline else {
        return Vec::new();
    };
    let mut olds: Vec<PathBuf> = timeline
        .clips
        .iter()
        .filter(|clip| clip.project.as_deref().is_none_or(|p| p == project))
        .map(|clip| clip.take.as_path())
        .chain(timeline.removed.iter().map(|r| r.take.as_path()))
        .filter(|take| take.symlink_metadata().is_err())
        .filter(|take| take.file_name().is_some_and(|n| folder.join(n).is_file()))
        .filter_map(Path::parent)
        .filter(|old| *old != folder && old.symlink_metadata().is_err())
        .map(Path::to_path_buf)
        .collect();
    olds.sort();
    olds.dedup();
    olds
}

/// Whether a recording (this engine's, or standalone `isocast record`'s)
/// writes in `folder`: it holds the folder's lock shared while it does.
/// Checked with a moment's exclusive try, never kept, and the lock file is
/// never made here.
fn being_recorded(folder: &Path) -> bool {
    let Ok(file) = std::fs::OpenOptions::new()
        .write(true)
        .open(folder.join(".isocast.lock"))
    else {
        return false;
    };
    match file.try_lock() {
        Ok(()) => {
            let _ = file.unlock();
            false
        }
        Err(_) => true,
    }
}

/// Write `.isocast-project.json` again from a project file.
fn restore_identity(folder: &Path, identity: &ProjectFile) -> std::io::Result<()> {
    let path = folder.join(project::PROJECT_FILE);
    let partial = folder.join(format!("{}.partial", project::PROJECT_FILE));
    let json = serde_json::to_vec_pretty(identity).map_err(std::io::Error::other)?;
    std::fs::write(&partial, json)
        .and_then(|()| std::fs::rename(&partial, &path))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&partial);
        })
}

/// Temporary files a killed writer left in `folder` (older than a minute,
/// so another engine's write in progress is left alone).
fn remove_stale_partials(folder: &Path) {
    for entry in std::fs::read_dir(folder).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !isoprj::is_partial(&name) {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(60));
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Where the recent projects are kept: beside the setup file.
fn recent_path() -> PathBuf {
    isocast_protocol::api::setup_path().with_extension("recent-projects.json")
}

/// The recent list serializes these (`exists` is worked out when asked).
static RECENT: Mutex<()> = Mutex::new(());

fn read_recent() -> RecentProjects {
    std::fs::read(recent_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn write_recent(projects: &[RecentProject]) -> std::io::Result<()> {
    let _one = RECENT.lock();
    let path = recent_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let partial = path.with_extension("json.partial");
    let json = serde_json::to_vec_pretty(&RecentProjects {
        projects: projects.to_vec(),
    })
    .map_err(std::io::Error::other)?;
    std::fs::write(&partial, json).and_then(|()| std::fs::rename(&partial, &path))
}

/// Put `file` first in the recent list.
fn remember_recent(file: &Path, name: &str) {
    let mut list = read_recent().projects;
    list.retain(|p| p.path != file);
    list.insert(
        0,
        RecentProject {
            path: file.to_path_buf(),
            name: name.to_string(),
            opened_at: now_text(),
            exists: true,
        },
    );
    list.truncate(isoprj::RECENT_LIMIT);
    if let Err(err) = write_recent(&list) {
        isocast_diagnostics::diagnostic!(
            "isocastd: could not remember {} as a recent project: {err}",
            file.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use isocast_protocol::timeline::{Clip, Crop, TIMELINE_VERSION};

    fn timeline(id: &str, takes: &[&Path], revision: u64) -> Timeline {
        Timeline {
            version: TIMELINE_VERSION,
            id: id.into(),
            uid: Some(format!("uid-{id}")),
            name: "Talk".into(),
            created_at: "2026-10-16T10:00:00-04:00".into(),
            clips: takes
                .iter()
                .enumerate()
                .map(|(i, take)| Clip {
                    id: format!("c{}", i + 1),
                    take: take.to_path_buf(),
                    in_seconds: 0.0,
                    out_seconds: 1.0,
                    project: Some("p".into()),
                    angle: Some("camera".into()),
                    crop: Some(Crop {
                        x: 0.25,
                        y: 0.0,
                        width: 0.5,
                        height: 1.0,
                    }),
                    extra: serde_json::Map::from_iter([(
                        "future".to_string(),
                        serde_json::json!({"left": 0.1}),
                    )]),
                })
                .collect(),
            removed: Vec::new(),
            recording: Vec::new(),
            next_clip: takes.len() as u32 + 1,
            revision,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    #[test]
    fn a_project_file_round_trips_with_relative_paths_and_unknown_fields() {
        let root = std::env::temp_dir();
        let folder = root.join("Isocast/2026-10-16 10-00-00 Talk");
        let mut t = timeline("talk", &[&folder.join("a.session.json")], 3);
        isoprj::map_paths(&mut t, |p| isoprj::relative_to(p, &folder));
        assert_eq!(t.clips[0].take, Path::new("a.session.json"));
        let file = IsoprjFile {
            format: isoprj::FORMAT.into(),
            version: 1,
            project: "p".into(),
            name: "Talk".into(),
            prefix: "2026-10-16 10-00-00".into(),
            created_at: String::new(),
            saved_at: String::new(),
            takes: vec!["a.session.json".into()],
            timelines: vec![t],
        };
        let json = serde_json::to_string(&file).unwrap();
        assert!(json.contains("\"crop\""), "{json}");
        assert!(json.contains("\"future\""), "{json}");
        let back: IsoprjFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back, file);
        let copy = root.join("Other/Talk");
        let opened = resolved(&back, &copy);
        assert_eq!(opened[0].clips[0].take, copy.join("a.session.json"));
        assert_eq!(opened[0].clips[0].crop.unwrap().width, 0.5);
        assert_eq!(opened[0].clips[0].angle.as_deref(), Some("camera"));
        assert_eq!(opened[0].clips[0].extra["future"]["left"], 0.1);
    }

    #[test]
    fn writes_are_atomic_and_never_leave_a_partial() {
        let dir = std::env::temp_dir().join(format!("isoprj-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Talk.isoprj");
        let mut file = IsoprjFile {
            format: isoprj::FORMAT.into(),
            version: 1,
            project: "p".into(),
            name: "Talk".into(),
            prefix: String::new(),
            created_at: String::new(),
            saved_at: String::new(),
            takes: Vec::new(),
            timelines: vec![timeline("talk", &[Path::new("a.session.json")], 1)],
        };
        isoprj::write_atomic(&path, &file).unwrap();
        file.timelines[0].revision = 2;
        isoprj::write_atomic(&path, &file).unwrap();
        assert_eq!(isoprj::read(&path).unwrap().timelines[0].revision, 2);
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["Talk.isoprj"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
