//! Timelines (C.1, `isocast_protocol::timeline`): takes in order, kept as
//! one JSON file each in the timelines folder (`timelines_path`), with
//! `current.json` naming the current one. Every change is written at once
//! (to a temporary file, then renamed), so a crash or a killed isocastd
//! leaves the last change, never half of one. Clips point at their takes'
//! manifests; taking a clip off never touches the take's files.

use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use chrono::{Local, SecondsFormat};
use isocast_engine::record::{SplitTake, SplitTiming};
use isocast_protocol::api::{Event, RpcError, StartParams, TakeCause, timelines_path};
use isocast_protocol::library::slug;
use isocast_protocol::manifest::{SessionManifest, SessionStatus};
use isocast_protocol::timeline::{
    self, Clip, ClipFile, ClipInfo, RemovedReason, RemovedTake, RetakeParams, Retaken,
    TIMELINE_VERSION, Timeline, TimelineAction, TimelineClosed, TimelineInfo, TimelineList,
    TimelineMoveParams, TimelineNewParams, TimelineOpenParams, TimelineRemoveParams,
    TimelineSummary, TimelineTarget,
};

use crate::Daemon;

const CURRENT: &str = "current.json";
const EXTENSION: &str = ".timeline.json";

pub fn invalid(message: impl Into<String>) -> RpcError {
    RpcError {
        code: RpcError::INVALID_PARAMS,
        message: message.into(),
    }
}

pub fn wrong_state(message: impl Into<String>) -> RpcError {
    RpcError {
        code: RpcError::WRONG_STATE,
        message: message.into(),
    }
}

pub fn failed(message: impl Into<String>) -> RpcError {
    RpcError {
        code: RpcError::FAILED,
        message: message.into(),
    }
}

/// Now, local time, as timelines record it.
pub fn now_text() -> String {
    Local::now().to_rfc3339_opts(SecondsFormat::Secs, false)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CurrentFile {
    timeline: Option<String>,
}

/// Every timeline, in memory and on disk.
pub struct Timelines {
    folder: PathBuf,
    current: Option<String>,
    timelines: BTreeMap<String, Timeline>,
    /// Files that couldn't be read when loading.
    problems: Vec<String>,
    /// Why no timeline is current although `current.json` names one (its
    /// file couldn't be read), until another is made current.
    current_problem: Option<String>,
}

impl Timelines {
    /// The timelines kept in `folder` (none if it doesn't exist yet).
    pub fn load(folder: PathBuf) -> Self {
        let mut timelines = BTreeMap::new();
        let mut problems = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&folder) {
            let mut paths: Vec<PathBuf> = entries
                .filter_map(|entry| Some(entry.ok()?.path()))
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.ends_with(EXTENSION) && !name.starts_with('.'))
                })
                .collect();
            paths.sort();
            for path in paths {
                match std::fs::read_to_string(&path)
                    .map_err(|err| err.to_string())
                    .and_then(|text| {
                        serde_json::from_str::<Timeline>(&text).map_err(|err| err.to_string())
                    }) {
                    Ok(timeline) => {
                        timelines.insert(timeline.id.clone(), timeline);
                    }
                    Err(err) => problems.push(format!("{}: {err}", path.display())),
                }
            }
        }
        let named = std::fs::read_to_string(folder.join(CURRENT))
            .ok()
            .and_then(|text| serde_json::from_str::<CurrentFile>(&text).ok())
            .and_then(|file| file.timeline);
        let current = named.clone().filter(|id| timelines.contains_key(id));
        let current_problem = match (&named, &current) {
            (Some(id), None) => Some(format!(
                "timeline {id:?} was current, but {} couldn't be read ({}), so no timeline is \
                 current and finished takes aren't added to any; fix the file and run \
                 `isocast timeline open {id}` (MCP open_timeline), or make another current",
                folder.join(format!("{id}{EXTENSION}")).display(),
                problems
                    .iter()
                    .find(|problem| problem.contains(&format!("{id}{EXTENSION}")))
                    .map_or("it is missing", String::as_str)
            )),
            _ => None,
        };
        Self {
            folder,
            current,
            timelines,
            problems,
            current_problem,
        }
    }

    pub fn folder(&self) -> &Path {
        &self.folder
    }

    /// See [`Timelines::current_problem`].
    pub fn current_problem(&self) -> Option<&str> {
        self.current_problem.as_deref()
    }

    /// Takes that were being recorded when isocastd last stopped (killed or
    /// crashed): moved to `removed` as interrupted. Returns what was moved,
    /// for the log.
    pub fn recover_interrupted(&mut self) -> Vec<String> {
        let held: Vec<String> = self
            .timelines
            .values()
            .filter(|timeline| !timeline.recording.is_empty())
            .map(|timeline| timeline.id.clone())
            .collect();
        let mut moved = Vec::new();
        for id in held {
            let result = self.change(&id, |timeline| {
                for take in std::mem::take(&mut timeline.recording) {
                    moved.push(format!("{} (timeline {id:?})", take.display()));
                    timeline.removed.push(RemovedTake {
                        take,
                        clip: None,
                        reason: RemovedReason::Interrupted,
                        at: now_text(),
                    });
                }
                Ok(())
            });
            if let Err(err) = result {
                moved.push(format!("timeline {id:?}: {}", err.message));
            }
        }
        moved
    }

    /// Read timeline `id`'s file again if it isn't loaded (it couldn't be
    /// read when isocastd started, and has been fixed since). An error says
    /// why it still can't be read.
    fn reload(&mut self, id: &str) -> Result<(), RpcError> {
        let slug_like = !id.is_empty()
            && id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if self.timelines.contains_key(id) || !slug_like {
            return Ok(());
        }
        let path = self.path(id);
        if !path.exists() {
            return Ok(());
        }
        let timeline = std::fs::read_to_string(&path)
            .map_err(|err| err.to_string())
            .and_then(|text| serde_json::from_str::<Timeline>(&text).map_err(|err| err.to_string()))
            .map_err(|err| {
                invalid(format!(
                    "timeline: {} couldn't be read: {err}; fix it, or open another ({})",
                    path.display(),
                    self.listing()
                ))
            })?;
        if timeline.id != id {
            return Ok(());
        }
        let name = path.display().to_string();
        self.problems.retain(|problem| !problem.starts_with(&name));
        self.timelines.insert(timeline.id.clone(), timeline);
        Ok(())
    }

    /// The timelines that list `take` as being recorded.
    fn holding(&self, take: &Path) -> Vec<String> {
        self.timelines
            .values()
            .filter(|timeline| timeline.recording.iter().any(|t| t == take))
            .map(|timeline| timeline.id.clone())
            .collect()
    }

    pub fn current(&self) -> Option<&str> {
        self.current.as_deref()
    }

    /// Every timeline.
    pub fn all(&self) -> impl Iterator<Item = &Timeline> {
        self.timelines.values()
    }

    pub fn get(&self, id: &str) -> Option<&Timeline> {
        self.timelines.get(id)
    }

    fn path(&self, id: &str) -> PathBuf {
        self.folder.join(format!("{id}{EXTENSION}"))
    }

    /// "a (Intro), b (Outro)", or how to make one.
    fn listing(&self) -> String {
        if self.timelines.is_empty() {
            return "there are no timelines yet (`isocast timeline new`, or MCP's \
                    new_timeline, makes one)"
                .into();
        }
        let ids: Vec<String> = self
            .timelines
            .values()
            .map(|t| format!("{} ({})", t.id, t.name))
            .collect();
        format!("there are: {}", ids.join(", "))
    }

    /// The timeline `target` names, or the current one; the error names the
    /// `timeline` field and lists the timelines there are.
    pub fn resolve(&self, target: Option<&str>) -> Result<String, RpcError> {
        match target {
            Some(id) if self.timelines.contains_key(id) => Ok(id.to_string()),
            Some(id) => Err(invalid(format!(
                "timeline: no timeline {id:?}; {}",
                self.listing()
            ))),
            None => self.current.clone().ok_or_else(|| {
                wrong_state(format!(
                    "no timeline is current; open one (`isocast timeline open ID`, MCP's \
                     open_timeline) or make one (`isocast timeline new`, new_timeline); {}",
                    self.listing()
                ))
            }),
        }
    }

    pub fn summary(&self, id: &str) -> Option<TimelineSummary> {
        let timeline = self.timelines.get(id)?;
        Some(TimelineSummary {
            id: timeline.id.clone(),
            name: timeline.name.clone(),
            current: self.current.as_deref() == Some(id),
            created_at: timeline.created_at.clone(),
            clips: timeline.clips.len(),
            duration_seconds: timeline::duration(&timeline.clips),
        })
    }

    pub fn list(&self) -> TimelineList {
        TimelineList {
            current: self.current.clone(),
            folder: self.folder.clone(),
            timelines: self
                .timelines
                .keys()
                .filter_map(|id| self.summary(id))
                .collect(),
            problems: self.problems.clone(),
        }
    }

    /// A timeline with what each clip's take holds (read from the takes'
    /// manifests).
    pub fn info(&self, id: &str, notes: Vec<String>) -> Option<TimelineInfo> {
        let timeline = self.timelines.get(id)?;
        let mut start = 0.0;
        let clips = timeline
            .clips
            .iter()
            .enumerate()
            .map(|(index, clip)| {
                let info = clip_info(clip, index, start);
                start += clip.duration_seconds();
                info
            })
            .collect();
        Some(TimelineInfo {
            id: timeline.id.clone(),
            name: timeline.name.clone(),
            current: self.current.as_deref() == Some(id),
            created_at: timeline.created_at.clone(),
            path: self.path(id),
            duration_seconds: timeline::duration(&timeline.clips),
            clips,
            removed: timeline.removed.clone(),
            notes,
            revision: timeline.revision,
            undo: timeline.undo.len(),
            redo: timeline.redo.len(),
        })
    }

    /// Make a timeline named `name` (or "Timeline N") and make it current.
    pub fn create(&mut self, name: Option<String>) -> Result<String, RpcError> {
        let name = match name {
            Some(name) => {
                let name = name.trim().to_string();
                if name.is_empty() || name.chars().count() > 120 || name.contains(['\n', '\r']) {
                    return Err(invalid(
                        "name: a timeline's name must be one line of 1 to 120 characters",
                    ));
                }
                name
            }
            None => (1..)
                .map(|n| format!("Timeline {n}"))
                .find(|name| !self.timelines.values().any(|t| &t.name == name))
                .expect("a free name"),
        };
        let base = slug(&name, "timeline");
        let id = (1..)
            .map(|n| match n {
                1 => base.clone(),
                n => format!("{base}-{n}"),
            })
            .find(|id| !self.timelines.contains_key(id) && !self.path(id).exists())
            .expect("a free id");
        let timeline = Timeline {
            version: TIMELINE_VERSION,
            id: id.clone(),
            name,
            created_at: now_text(),
            clips: Vec::new(),
            removed: Vec::new(),
            recording: Vec::new(),
            next_clip: 1,
            revision: 0,
            undo: Vec::new(),
            redo: Vec::new(),
        };
        self.write(&timeline)?;
        self.timelines.insert(id.clone(), timeline);
        self.set_current(Some(id.clone()))?;
        Ok(id)
    }

    /// Make `id` (or no timeline) current. Takes being recorded go with
    /// it (their `recording` entries): when they finish, they are added to
    /// the timeline current then.
    pub fn set_current(&mut self, id: Option<String>) -> Result<(), RpcError> {
        std::fs::create_dir_all(&self.folder).map_err(|err| {
            failed(format!(
                "could not make the timelines folder {}: {err}",
                self.folder.display()
            ))
        })?;
        let json = serde_json::to_string_pretty(&CurrentFile {
            timeline: id.clone(),
        })
        .map_err(|err| failed(err.to_string()))?;
        write_atomic(&self.folder.join(CURRENT), &json)?;
        let before = std::mem::replace(&mut self.current, id.clone());
        self.current_problem = None;
        if let Some(before) = before.filter(|before| Some(before) != id.as_ref()) {
            let moving = self
                .timelines
                .get(&before)
                .map(|t| t.recording.clone())
                .unwrap_or_default();
            if !moving.is_empty() {
                // Best effort: these only matter after a crash.
                let _ = self.change(&before, |t| {
                    t.recording.clear();
                    Ok(())
                });
                if let Some(id) = &id {
                    let _ = self.change(id, |t| {
                        t.recording.extend(moving);
                        Ok(())
                    });
                }
            }
        }
        Ok(())
    }

    /// A recording project's folder moved from `from` to `to`: every
    /// timeline naming takes inside it names them there. All or nothing:
    /// if one can't be written, those already written are put back.
    /// Clips moved without a project id get `project` (#56). Returns the
    /// timelines changed.
    pub fn move_folder(
        &mut self,
        from: &Path,
        to: &Path,
        project: Option<&str>,
    ) -> Result<Vec<String>, RpcError> {
        let fix = |path: &mut PathBuf| match isocast_protocol::project::moved(path, from, to) {
            Some(new) => {
                *path = new;
                true
            }
            None => false,
        };
        let mut changed: Vec<(String, Timeline)> = Vec::new();
        for (id, timeline) in &self.timelines {
            let mut moved = timeline.clone();
            let mut any = false;
            for clip in &mut moved.clips {
                if fix(&mut clip.take) {
                    any = true;
                    if clip.project.is_none() {
                        clip.project = project.map(str::to_string);
                    }
                }
            }
            for removed in &mut moved.removed {
                any |= fix(&mut removed.take);
            }
            for take in &mut moved.recording {
                any |= fix(take);
            }
            if any {
                changed.push((id.clone(), moved));
            }
        }
        for (i, (_, moved)) in changed.iter().enumerate() {
            if let Err(err) = self.write(moved) {
                for (id, _) in &changed[..i] {
                    if let Some(old) = self.timelines.get(id) {
                        let _ = self.write(old);
                    }
                }
                return Err(err);
            }
        }
        let ids = changed.iter().map(|(id, _)| id.clone()).collect();
        for (id, moved) in changed {
            self.timelines.insert(id, moved);
        }
        Ok(ids)
    }

    /// Change timeline `id` and write it; nothing changes if it can't be
    /// written.
    pub fn change<T>(
        &mut self,
        id: &str,
        change: impl FnOnce(&mut Timeline) -> Result<T, RpcError>,
    ) -> Result<T, RpcError> {
        let mut timeline =
            self.timelines.get(id).cloned().ok_or_else(|| {
                invalid(format!("timeline: no timeline {id:?}; {}", self.listing()))
            })?;
        let result = change(&mut timeline)?;
        self.write(&timeline)?;
        self.timelines.insert(id.to_string(), timeline);
        Ok(result)
    }

    fn write(&self, timeline: &Timeline) -> Result<(), RpcError> {
        std::fs::create_dir_all(&self.folder).map_err(|err| {
            failed(format!(
                "could not make the timelines folder {}: {err}",
                self.folder.display()
            ))
        })?;
        let json = serde_json::to_string_pretty(timeline).map_err(|err| failed(err.to_string()))?;
        write_atomic(&self.path(&timeline.id), &json)
    }
}

/// Write `text` to `path` so a reader (or a crash) never sees half of it.
fn write_atomic(path: &Path, text: &str) -> Result<(), RpcError> {
    let partial = path.with_extension("json.partial");
    std::fs::write(&partial, format!("{text}\n"))
        .and_then(|()| std::fs::rename(&partial, path))
        .map_err(|err| {
            let _ = std::fs::remove_file(&partial);
            failed(format!("could not write {}: {err}", path.display()))
        })
}

/// Add a finished take to the end of `timeline`; returns the clip's id.
pub fn append(timeline: &mut Timeline, take: &Path, manifest: &SessionManifest) -> String {
    let id = format!("c{}", timeline.next_clip);
    timeline.next_clip += 1;
    timeline.clips.push(Clip {
        id: id.clone(),
        take: take.to_path_buf(),
        in_seconds: 0.0,
        out_seconds: timeline::take_seconds(manifest),
        project: take
            .parent()
            .and_then(isocast_protocol::project::read)
            .map(|file| file.id),
        angle: None,
    });
    crate::edit::recorded(timeline);
    id
}

/// The index of clip `clip`, or an error naming the `clip` field and the
/// clips there are.
pub fn find(timeline: &Timeline, clip: &str) -> Result<usize, RpcError> {
    timeline
        .clips
        .iter()
        .position(|c| c.id == clip)
        .ok_or_else(|| {
            let ids: Vec<&str> = timeline.clips.iter().map(|c| c.id.as_str()).collect();
            invalid(match ids.is_empty() {
                true => format!(
                    "clip: no clip {clip:?}; timeline {:?} has no clips yet",
                    timeline.id
                ),
                false => format!(
                    "clip: no clip {clip:?} in timeline {:?}; its clips are {}",
                    timeline.id,
                    ids.join(", ")
                ),
            })
        })
}

/// Take clip `index` off `timeline`, keeping it under `removed`.
pub fn remove(timeline: &mut Timeline, index: usize, reason: RemovedReason) -> RemovedTake {
    let clip = timeline.clips.remove(index);
    let removed = RemovedTake {
        take: clip.take,
        clip: Some(clip.id),
        reason,
        at: now_text(),
    };
    timeline.removed.push(removed.clone());
    removed
}

/// Move clip `from` to `to`; the error names the `index` field and the
/// valid range.
pub fn move_clip(timeline: &mut Timeline, from: usize, to: usize) -> Result<(), RpcError> {
    let last = timeline.clips.len() - 1;
    if to > last {
        return Err(invalid(format!(
            "index: {to} is past the end; timeline {:?} has {} clips, so use 0 to {last}",
            timeline.id,
            timeline.clips.len()
        )));
    }
    let clip = timeline.clips.remove(from);
    timeline.clips.insert(to, clip);
    Ok(())
}

/// Where a take's files are, for messages: its manifest's folder.
pub fn where_files_are(take: &Path) -> String {
    let stem = take
        .file_name()
        .map(|name| name.to_string_lossy().replace(".session.json", ""))
        .unwrap_or_default();
    format!(
        "its files (\"{stem}\" and the ISO files listed in {}) stay in {}",
        take.display(),
        take.parent().unwrap_or(Path::new("/")).display()
    )
}

/// A clip with what its take's manifest says.
fn clip_info(clip: &Clip, index: usize, start: f64) -> ClipInfo {
    let mut info = ClipInfo {
        id: clip.id.clone(),
        index,
        take: clip.take.clone(),
        start_seconds: start,
        duration_seconds: clip.duration_seconds(),
        in_seconds: clip.in_seconds,
        out_seconds: clip.out_seconds,
        started_at: None,
        status: None,
        dropped_frames: 0,
        files: Vec::new(),
        problem: None,
        angle: clip.angle.clone(),
        fps: 0,
    };
    match read_manifest(&clip.take) {
        Ok(manifest) => {
            let folder = clip.take.parent().unwrap_or(Path::new(""));
            info.started_at = Some(manifest.started_at.clone());
            info.status = Some(manifest.status);
            info.problem = unfinished(&clip.id, &clip.take, &manifest);
            info.fps = timeline::take_length(&manifest).map_or(0, |(_, fps)| fps);
            info.files = manifest
                .files
                .iter()
                .map(|file| {
                    let path = folder.join(&file.path);
                    ClipFile {
                        exists: path.exists(),
                        path,
                        source: file.source.clone(),
                        width: file.width,
                        height: file.height,
                        fps: file.fps,
                        audio: file.audio.iter().map(|a| a.source.clone()).collect(),
                        duration_seconds: file.duration_seconds,
                        frames: file.frames,
                        dropped_frames: file.dropped_frames.unwrap_or(0),
                        audio_dropouts: file.audio_dropouts.unwrap_or(0),
                        start_offset_seconds: file.start_offset_seconds,
                    }
                })
                .collect();
            info.dropped_frames = info.files.iter().map(|f| f.dropped_frames).sum();
        }
        Err(problem) => info.problem = Some(problem),
    }
    info
}

/// Why clip `clip`'s take (manifest `take`) can't be joined as it is: its
/// manifest doesn't say how long its files are. A clip is a take that
/// finished, so this is a final manifest that couldn't be written (the
/// folder became read-only, or the disk was full, when it stopped). (One
/// file of a finished take without a length is the export's to skip.)
pub fn unfinished(clip: &str, take: &Path, manifest: &SessionManifest) -> Option<String> {
    let lengths_unknown = manifest
        .files
        .iter()
        .all(|file| file.duration_seconds.is_none());
    (manifest.status == SessionStatus::Recording || lengths_unknown).then(|| {
        format!(
            "clip {clip}'s take manifest {} doesn't say how long its files are (status {:?}): \
             its final manifest couldn't be written when the take stopped. Its files are there, \
             but they can't be joined without their lengths; take the clip off (`isocast \
             timeline remove {clip}`, MCP remove_clip) to export the others",
            take.display(),
            serde_json::to_value(manifest.status)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default()
        )
    })
}

pub fn read_manifest(path: &Path) -> Result<SessionManifest, String> {
    let text = std::fs::read_to_string(path).map_err(|err| {
        format!(
            "could not read the take's manifest {}: {err}",
            path.display()
        )
    })?;
    serde_json::from_str(&text).map_err(|err| {
        format!(
            "the take's manifest {} is unreadable: {err}",
            path.display()
        )
    })
}

impl Default for Timelines {
    /// None, until [`Timelines::load`] reads them.
    fn default() -> Self {
        Self {
            folder: PathBuf::new(),
            current: None,
            timelines: BTreeMap::new(),
            problems: Vec::new(),
            current_problem: None,
        }
    }
}

/// The takes a recording session is making, for its timeline.
#[derive(Default)]
pub struct SessionTakes {
    /// The take being recorded, or about to begin after a split.
    pub current: Mutex<TakeNow>,
    /// Takes `retake` dropped while recording (by number, from 1), and the
    /// timeline it acted on, which lists them as dropped when they finish.
    pub dropped: Mutex<Vec<(u32, String)>>,
    /// Takes begun (number, manifest) that haven't finished.
    begun: Mutex<Vec<(u32, PathBuf)>>,
    /// The last take of the session added to a timeline (its manifest).
    appended: Mutex<Option<PathBuf>>,
}

/// The take a session records now (or is about to begin).
#[derive(Debug, Clone, Default)]
pub struct TakeNow {
    /// From 1; 0 before the recording has started.
    pub number: u32,
    pub files: Vec<PathBuf>,
    /// When a split's take begins (it records into the take before until
    /// then), as an instant and as text for replies.
    pub begins: Option<Instant>,
    pub begins_at: Option<String>,
}

impl TakeNow {
    /// A split's take that hasn't begun yet.
    fn pending(&self) -> bool {
        self.begins.is_some_and(|begins| begins > Instant::now())
    }

    fn begins_in_seconds(&self) -> Option<f64> {
        self.begins.map(|begins| {
            begins
                .saturating_duration_since(Instant::now())
                .as_secs_f64()
        })
    }
}

impl SessionTakes {
    /// Drop take `number` (the one `retake` replaces) on `timeline`;
    /// false if it already was.
    fn drop_take(&self, number: u32, timeline: &str) -> bool {
        let mut dropped = self.dropped.lock();
        if dropped.iter().any(|(n, _)| *n == number) {
            return false;
        }
        dropped.push((number, timeline.to_string()));
        true
    }
}

/// What an idle `retake` takes off the timeline before recording again.
enum TakeOff {
    /// The last clip, whichever it is.
    Last,
    /// The last clip only if it is this take (the one a stop that was
    /// finishing just added), else nothing (that take was dropped).
    Just(Option<PathBuf>),
}

impl Daemon {
    pub(crate) fn load_timelines(&self) {
        let mut loaded = Timelines::load(timelines_path());
        for problem in &loaded.problems {
            eprintln!("isocastd: a timeline couldn't be read: {problem}");
        }
        if let Some(problem) = loaded.current_problem() {
            eprintln!("isocastd: {problem}");
        }
        for take in loaded.recover_interrupted() {
            eprintln!(
                "isocastd: a take was being recorded when isocastd last stopped; its timeline \
                 lists it as interrupted: {take}"
            );
        }
        for note in crate::export::recover(loaded.folder()) {
            eprintln!("isocastd: {note}");
        }
        *self.timelines.lock() = loaded;
    }

    pub(crate) fn timeline_changed(&self, id: &str, action: TimelineAction, clip: Option<String>) {
        let timelines = self.timelines.lock();
        let clips = timelines.get(id).map_or(0, |t| t.clips.len());
        let current = timelines.current().map(str::to_string);
        drop(timelines);
        self.publish(Event::TimelineChanged {
            timeline: id.to_string(),
            action,
            clip,
            clips,
            current,
        });
    }

    /// Takes on timelines whose files aren't where the timeline says (their
    /// project's folder was renamed in the Finder, or isocastd stopped
    /// between renaming one and updating the timelines): find their
    /// project by id (or, for clips from before ids, the project folder
    /// holding a manifest of that name, if only one does) and point every
    /// timeline there.
    pub(crate) fn heal_timelines(&self) {
        let missing: Vec<(PathBuf, Option<String>)> = {
            let timelines = self.timelines.lock();
            timelines
                .all()
                .flat_map(|timeline| {
                    timeline
                        .clips
                        .iter()
                        .map(|clip| (clip.take.clone(), clip.project.clone()))
                        .chain(timeline.removed.iter().map(|r| (r.take.clone(), None)))
                })
                .filter(|(take, _)| take.symlink_metadata().is_err())
                .collect()
        };
        if missing.is_empty() {
            return;
        }
        let projects: Vec<_> = self
            .recording_roots()
            .iter()
            .flat_map(|root| isocast_protocol::project::scan(root))
            .collect();
        let mut moves: Vec<(PathBuf, PathBuf, String)> = Vec::new();
        for (take, project) in missing {
            let (Some(old), Some(name)) = (take.parent(), take.file_name()) else {
                continue;
            };
            if moves.iter().any(|(from, _, _)| from == old) {
                continue;
            }
            let by_id = project.and_then(|id| projects.iter().find(|(_, file)| file.id == id));
            let found = match by_id {
                Some((folder, file)) => Some((folder.clone(), file.id.clone())),
                None => {
                    let holding: Vec<_> = projects
                        .iter()
                        .filter(|(folder, _)| folder.join(name).is_file())
                        .collect();
                    match holding.as_slice() {
                        [(folder, file)] => Some((folder.clone(), file.id.clone())),
                        _ => None,
                    }
                }
            };
            if let Some((new, id)) = found.filter(|(new, _)| new.join(name).is_file()) {
                moves.push((old.to_path_buf(), new, id));
            }
        }
        for (from, to, id) in moves {
            let moved = self.timelines.lock().move_folder(&from, &to, Some(&id));
            for message in crate::projects::heal_exports(&to) {
                self.publish(Event::Warning { message });
            }
            if let Err(err) = moved {
                self.publish(Event::Warning {
                    message: format!(
                        "timelines name takes in {}, which moved to {}, but couldn't be \
                         updated: {}",
                        from.display(),
                        to.display(),
                        err.message
                    ),
                });
            }
        }
    }

    pub(crate) fn timeline_info(
        &self,
        id: &str,
        notes: Vec<String>,
    ) -> Result<TimelineInfo, RpcError> {
        self.heal_timelines();
        self.timelines
            .lock()
            .info(id, notes)
            .ok_or_else(|| invalid(format!("timeline: no timeline {id:?}")))
    }

    pub(crate) fn timeline_new(&self, params: TimelineNewParams) -> Result<TimelineInfo, RpcError> {
        let id = self.timelines.lock().create(params.name)?;
        self.timeline_changed(&id, TimelineAction::Created, None);
        let recording = self.state.lock().session.is_some();
        let notes = match recording {
            true => vec![
                "a recording is running: its take is added to this timeline when it finishes"
                    .into(),
            ],
            false => Vec::new(),
        };
        self.timeline_info(&id, notes)
    }

    pub(crate) fn timeline_get(&self, target: TimelineTarget) -> Result<TimelineInfo, RpcError> {
        let id = self.timelines.lock().resolve(target.timeline.as_deref())?;
        self.timeline_info(&id, Vec::new())
    }

    pub(crate) fn timeline_list(&self) -> TimelineList {
        self.timelines.lock().list()
    }

    pub(crate) fn timeline_open(
        &self,
        params: TimelineOpenParams,
    ) -> Result<TimelineInfo, RpcError> {
        let id = {
            let mut timelines = self.timelines.lock();
            timelines.reload(&params.timeline)?;
            let id = timelines.resolve(Some(&params.timeline))?;
            timelines.set_current(Some(id.clone()))?;
            id
        };
        self.timeline_changed(&id, TimelineAction::Opened, None);
        self.timeline_info(&id, Vec::new())
    }

    pub(crate) fn timeline_close(&self) -> Result<TimelineClosed, RpcError> {
        let closed = {
            let mut timelines = self.timelines.lock();
            let closed = timelines.current().map(str::to_string);
            timelines.set_current(None)?;
            closed
        };
        if let Some(id) = &closed {
            self.timeline_changed(id, TimelineAction::Closed, None);
        }
        Ok(TimelineClosed { closed })
    }

    pub(crate) fn timeline_remove(
        &self,
        params: TimelineRemoveParams,
    ) -> Result<TimelineInfo, RpcError> {
        let (id, removed) = {
            let mut timelines = self.timelines.lock();
            let id = timelines.resolve(params.timeline.as_deref())?;
            let removed = timelines.change(&id, |timeline| {
                let at = find(timeline, &params.clip)?;
                crate::edit::revise(timeline, TimelineAction::Removed, |timeline| {
                    Ok(remove(timeline, at, RemovedReason::Removed))
                })
                .map_err(invalid)
            })?;
            (id, removed)
        };
        self.timeline_changed(&id, TimelineAction::Removed, Some(params.clip.clone()));
        let note = format!(
            "clip {} is off the timeline; {}",
            params.clip,
            where_files_are(&removed.take)
        );
        self.timeline_info(&id, vec![note])
    }

    pub(crate) fn timeline_move(
        &self,
        params: TimelineMoveParams,
    ) -> Result<TimelineInfo, RpcError> {
        let id = {
            let mut timelines = self.timelines.lock();
            let id = timelines.resolve(params.timeline.as_deref())?;
            timelines.change(&id, |timeline| {
                let at = find(timeline, &params.clip)?;
                crate::edit::revise(timeline, TimelineAction::Moved, |timeline| {
                    move_clip(timeline, at, params.index).map_err(|err| err.message)
                })
                .map_err(invalid)
            })?;
            id
        };
        self.timeline_changed(&id, TimelineAction::Moved, Some(params.clip.clone()));
        self.timeline_info(&id, Vec::new())
    }

    /// A take of the running session began (the first at the start, or a
    /// split's): the current timeline lists it as being recorded until it
    /// finishes, so a crash meanwhile leaves a trace there.
    pub(crate) fn take_began(&self, takes: &SessionTakes, number: u32, manifest: &Path) {
        takes.begun.lock().push((number, manifest.to_path_buf()));
        let mut timelines = self.timelines.lock();
        let Some(id) = timelines.current().map(str::to_string) else {
            return;
        };
        let noted = timelines.change(&id, |timeline| {
            timeline.recording.push(manifest.to_path_buf());
            Ok(())
        });
        drop(timelines);
        if let Err(err) = noted {
            self.publish(Event::Warning {
                message: format!(
                    "timeline {id:?} couldn't note the take being recorded ({}): {}",
                    manifest.display(),
                    err.message
                ),
            });
        }
    }

    /// A take of the running session finished: add it to the current
    /// timeline, or (if `retake` dropped it) list it as dropped on the
    /// timeline `retake` acted on.
    pub(crate) fn take_finished(
        &self,
        takes: &SessionTakes,
        number: u32,
        path: &Path,
        manifest: &SessionManifest,
    ) {
        takes.begun.lock().retain(|(n, _)| *n != number);
        let dropped = {
            let mut dropped = takes.dropped.lock();
            dropped
                .iter()
                .position(|(n, _)| *n == number)
                .map(|i| dropped.remove(i).1)
        };
        let mut timelines = self.timelines.lock();
        for id in timelines.holding(path) {
            let _ = timelines.change(&id, |timeline| {
                timeline.recording.retain(|take| take != path);
                Ok(())
            });
        }
        let (id, action) = match dropped {
            Some(id) => (id, TimelineAction::Dropped),
            None => match timelines.current() {
                Some(id) => (id.to_string(), TimelineAction::Appended),
                None => return,
            },
        };
        let changed = timelines.change(&id, |timeline| {
            Ok(match action {
                TimelineAction::Dropped => {
                    timeline.removed.push(RemovedTake {
                        take: path.to_path_buf(),
                        clip: None,
                        reason: RemovedReason::Dropped,
                        at: now_text(),
                    });
                    None
                }
                _ => Some(append(timeline, path, manifest)),
            })
        });
        drop(timelines);
        match changed {
            Ok(clip) => {
                if clip.is_some() {
                    *takes.appended.lock() = Some(path.to_path_buf());
                }
                self.timeline_changed(&id, action, clip);
            }
            Err(err) => self.publish(Event::Warning {
                message: format!(
                    "the take {} couldn't be added to timeline {id:?}: {}",
                    path.display(),
                    err.message
                ),
            }),
        }
    }

    /// The session ended: takes it began that never finished (the
    /// recording failed) are listed as interrupted where they were noted.
    pub(crate) fn session_over(&self, takes: &SessionTakes) {
        let unfinished = std::mem::take(&mut *takes.begun.lock());
        let mut changed = Vec::new();
        let mut timelines = self.timelines.lock();
        for (_, take) in unfinished {
            for id in timelines.holding(&take) {
                let moved = timelines.change(&id, |timeline| {
                    timeline.recording.retain(|t| *t != take);
                    timeline.removed.push(RemovedTake {
                        take: take.clone(),
                        clip: None,
                        reason: RemovedReason::Interrupted,
                        at: now_text(),
                    });
                    Ok(())
                });
                if moved.is_ok() {
                    changed.push(id);
                }
            }
        }
        drop(timelines);
        for id in changed {
            self.timeline_changed(&id, TimelineAction::Interrupted, None);
        }
    }

    /// Wait for session `session` to end (a stop finishing, or a recording
    /// at its end).
    fn wait_for_end(&self, session: u64) -> Result<(), RpcError> {
        let mut state = self.state.lock();
        let waited = state.wait_while_for(&self.ended, crate::STOP_TIMEOUT, |s| {
            s.ended_session < session
        });
        match waited.timed_out() {
            true => Err(failed(
                "the recording did not stop in time; retake once `isocast status` says it has",
            )),
            false => Ok(()),
        }
    }

    /// See [`Method::Retake`](isocast_protocol::api::Method::Retake).
    pub(crate) fn retake(self: &Arc<Self>, params: RetakeParams) -> Result<Retaken, RpcError> {
        // One at a time: a second retake waits for the first, then finds
        // its new take not begun yet (or begun, and drops that).
        let _one = self.retaking.lock();
        let id = self.timelines.lock().resolve(None)?;
        let running = {
            let state = self.state.read();
            state.session.as_ref().map(|session| {
                (
                    session.id,
                    session.stop.load(Ordering::SeqCst),
                    session.takes.clone(),
                )
            })
        };
        let Some((session, stopping, takes)) = running else {
            return self.retake_idle(id, params, Vec::new(), TakeOff::Last);
        };
        // A stop (or the recording's end) is finishing: once it has, its
        // take is retaken as when not recording.
        let after_end = |reason: &str| {
            self.wait_for_end(session)?;
            let appended = takes.appended.lock().clone();
            let note = match appended {
                Some(_) => {
                    format!("{reason}, so its take finished first; that is the clip retaken")
                }
                None => format!(
                    "{reason}, so its take finished first; it wasn't added to the timeline \
                     (it was dropped), so nothing more was taken off"
                ),
            };
            self.retake_idle(
                id.clone(),
                params.clone(),
                vec![note],
                TakeOff::Just(appended),
            )
        };
        if stopping {
            return after_end("the recording was stopping");
        }
        if params.folder.is_some() || params.seconds.is_some() {
            return Err(invalid(
                "folder, seconds: not while recording; the new take goes where the recording \
                 writes (stop it first to record elsewhere or for a set time)",
            ));
        }
        let now = takes.current.lock().clone();
        if now.number == 0 {
            return Err(wrong_state(
                "the recording is still starting; retake once it has started (`isocast status` \
                 says when)",
            ));
        }
        let folder_of = |files: &[PathBuf]| {
            files
                .first()
                .and_then(|path| path.parent())
                .map(|folder| folder.display().to_string())
                .unwrap_or_default()
        };
        // A split or retake just now whose take hasn't begun: the take
        // being recorded is the one before it, and there is nothing more
        // to split.
        if now.pending() {
            let previous = now.number - 1;
            let note = match takes.drop_take(previous, &id) {
                true => format!(
                    "the take being recorded is dropped from timeline {id:?} when the next take \
                     begins at {}; its files stay in {}",
                    now.begins_at.as_deref().unwrap_or("its start"),
                    folder_of(&now.files)
                ),
                false => format!(
                    "the new take hasn't begun yet (it begins at {}) and the take before it is \
                     already dropped: nothing more to drop",
                    now.begins_at.as_deref().unwrap_or("its start")
                ),
            };
            return Ok(Retaken {
                timeline: id,
                begins_in_seconds: now.begins_in_seconds(),
                begins_at: now.begins_at,
                files: now.files,
                replaced: None,
                notes: vec![note],
            });
        }
        match self.split_take(SplitTiming::Soonest, TakeCause::Retake) {
            Ok(split) => {
                // The take split from ends when the new one begins (at least
                // a quarter second from now) and finishes after that, so it
                // is marked well before it finishes.
                takes.drop_take(split.take - 1, &id);
                let begins_at = split_text(&split);
                let names: Vec<String> = now
                    .files
                    .iter()
                    .filter_map(|path| path.file_name())
                    .map(|name| name.to_string_lossy().into_owned())
                    .collect();
                Ok(Retaken {
                    timeline: id.clone(),
                    begins_in_seconds: Some(split.begins_in.as_secs_f64()),
                    notes: vec![format!(
                        "the take being recorded is dropped from timeline {id:?} when the new take \
                         begins at {begins_at} (in {:.2} s); its files ({}) stay in {}",
                        split.begins_in.as_secs_f64(),
                        names.join(", "),
                        folder_of(&now.files)
                    )],
                    begins_at: Some(begins_at),
                    files: split.files,
                    replaced: None,
                })
            }
            Err(split) if split.ending => after_end("the recording was ending"),
            Err(split) => Err(split.error),
        }
    }

    /// `retake` while not recording: take the last clip off (as `take_off`
    /// says), then record again; if the recording can't start, the clip
    /// goes back where it was.
    fn retake_idle(
        self: &Arc<Self>,
        id: String,
        params: RetakeParams,
        mut notes: Vec<String>,
        take_off: TakeOff,
    ) -> Result<Retaken, RpcError> {
        // The retake goes into its take's folder: find it if it moved.
        self.heal_timelines();
        let taken = self.timelines.lock().change(&id, |timeline| {
            let off = match (&take_off, timeline.clips.last()) {
                (_, None) => false,
                (TakeOff::Last, Some(_)) => true,
                (TakeOff::Just(take), Some(clip)) => take.as_ref() == Some(&clip.take),
            };
            if !off {
                return Ok(None);
            }
            let clip = timeline.clips.pop().expect("checked");
            crate::edit::recorded(timeline);
            let removed = RemovedTake {
                take: clip.take.clone(),
                clip: Some(clip.id.clone()),
                reason: RemovedReason::Retaken,
                at: now_text(),
            };
            timeline.removed.push(removed.clone());
            Ok(Some((clip, removed)))
        })?;
        let folder = params.folder.clone().or_else(|| {
            taken
                .as_ref()
                .and_then(|(clip, _)| clip.take.parent().map(Path::to_path_buf))
        });
        let started = self.start_as(
            StartParams {
                setup: None,
                folder,
                seconds: params.seconds,
            },
            TakeCause::Retake,
        );
        match started {
            Ok(started) => {
                notes.extend(started.notes);
                match &taken {
                    Some((clip, removed)) => {
                        self.timeline_changed(&id, TimelineAction::Retaken, Some(clip.id.clone()));
                        notes.push(format!(
                            "clip {} is off the timeline; {}",
                            clip.id,
                            where_files_are(&removed.take)
                        ));
                    }
                    None if matches!(take_off, TakeOff::Last) => notes.push(
                        "the timeline had no clips, so nothing was taken off; recording its \
                         first take"
                            .into(),
                    ),
                    None => {}
                }
                Ok(Retaken {
                    timeline: id,
                    files: started.files,
                    begins_at: None,
                    begins_in_seconds: None,
                    replaced: taken.map(|(_, removed)| removed),
                    notes,
                })
            }
            Err(err) => {
                // Nothing was recorded: the clip goes back where it was.
                if let Some((clip, removed)) = taken {
                    let _ = self.timelines.lock().change(&id, |timeline| {
                        if let Some(at) = timeline.removed.iter().rposition(|r| *r == removed) {
                            timeline.removed.remove(at);
                        }
                        timeline.clips.push(clip);
                        Ok(())
                    });
                }
                Err(err)
            }
        }
    }
}

/// When a split's take begins, as replies say it.
pub fn split_text(split: &SplitTake) -> String {
    split
        .begins_at
        .to_rfc3339_opts(SecondsFormat::Millis, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(seconds: f64) -> SessionManifest {
        serde_json::from_value(serde_json::json!({
            "version": 1, "setup": "t", "status": "complete",
            "started_at": "2026-10-13T12:00:00.000-04:00", "host_time_ns": 0,
            "duration_seconds": seconds, "files": [], "errors": []
        }))
        .unwrap()
    }

    #[test]
    fn clips_are_added_removed_and_moved_by_id() {
        let folder = std::env::temp_dir().join(format!("isocast-timelines-{}", std::process::id()));
        let mut timelines = Timelines::load(folder.clone());
        let id = timelines.create(Some("My video".into())).unwrap();
        assert_eq!(id, "my-video");
        assert_eq!(timelines.current(), Some("my-video"));
        for n in 1..=4 {
            let take = PathBuf::from(format!("/r/take{n}.session.json"));
            timelines
                .change(&id, |t| Ok(append(t, &take, &manifest(n as f64))))
                .unwrap();
        }
        let ids = |t: &Timelines| -> Vec<String> {
            t.get(&id)
                .unwrap()
                .clips
                .iter()
                .map(|c| c.id.clone())
                .collect()
        };
        assert_eq!(ids(&timelines), ["c1", "c2", "c3", "c4"]);
        timelines
            .change(&id, |t| {
                let at = find(t, "c2")?;
                Ok(remove(t, at, RemovedReason::Removed))
            })
            .unwrap();
        timelines
            .change(&id, |t| {
                let at = find(t, "c3")?;
                move_clip(t, at, 0)
            })
            .unwrap();
        assert_eq!(ids(&timelines), ["c3", "c1", "c4"]);
        assert_eq!(
            timelines.summary(&id).unwrap().duration_seconds,
            3.0 + 1.0 + 4.0
        );
        // The next clip never reuses an id.
        let take = PathBuf::from("/r/take5.session.json");
        let new = timelines
            .change(&id, |t| Ok(append(t, &take, &manifest(5.0))))
            .unwrap();
        assert_eq!(new, "c5");
        // Errors name the field and list what there is.
        let err = timelines.change(&id, |t| find(t, "c9")).unwrap_err();
        assert!(
            err.message.starts_with("clip: no clip \"c9\""),
            "{}",
            err.message
        );
        assert!(err.message.contains("c3, c1, c4, c5"), "{}", err.message);
        let err = timelines.change(&id, |t| move_clip(t, 0, 4)).unwrap_err();
        assert!(err.message.contains("use 0 to 3"), "{}", err.message);
        // It all comes back from disk.
        let again = Timelines::load(folder.clone());
        assert_eq!(again.current(), Some("my-video"));
        assert_eq!(ids(&again), ["c3", "c1", "c4", "c5"]);
        assert_eq!(again.get(&id).unwrap().removed.len(), 1);
        let _ = std::fs::remove_dir_all(&folder);
    }
}
