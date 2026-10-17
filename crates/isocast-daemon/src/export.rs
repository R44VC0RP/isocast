//! `timeline_export`: join a timeline's clips into one program file and
//! one file per ISO source every clip has, each cut to the clips' in and
//! out points, plus (when clips show other angles, C.3, or are cropped,
//! #87) an edited file with each clip's angle as its picture (cropped and
//! scaled to the program's size where the clip has a crop) and the
//! program's sound, and with `vertical` (#94) the same edited picture at
//! 9:16 (`<name> VERTICAL.mov`, the program's size on its side, every frame
//! encoded again), in the
//! background, reporting `export_progress` and `export_finished` events.
//! The video is copied as recorded, except the frames from a cut that
//! isn't on a keyframe to the next keyframe, angles in another format
//! than the program, and cropped clips, which are encoded again; the audio is decoded and
//! encoded once more as one continuous track per file, cross-faded at
//! each join (see `isocast-platform-macos`'s `join`), so it lines up with
//! the video at every join in any player or editor.
//!
//! Exports write next to the first clip's take (or to `folder`), named
//! after the timeline: `<name> PROGRAM.mov`, `<name> <SOURCE>.mov` and
//! `<name>.export.json`, numbered so nothing is overwritten. The takes'
//! files are only read. One export runs at a time. It never takes the
//! space a recording on the same disk keeps free, and stops if it would.
//! While it runs, `export.json` in the timelines folder names it, so if
//! isocastd is killed meanwhile, the next start marks it failed and removes
//! its unfinished file.

use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use chrono::Local;
use isocast_engine::{JoinPart, JoinSpec, Joined, OnJoinProgress};
use isocast_protocol::api::{Event, RpcError};
use isocast_protocol::manifest::{ManifestFile, SessionManifest};
use isocast_protocol::timeline::{
    Crop, EDITED, ExportClip, ExportFile, ExportManifest, ExportPart, ExportRunning, ExportSkipped,
    ExportStarted, ExportStatus, TimelineExportParams, VERTICAL, take_length,
};
use isocast_protocol::{Container, PROGRAM};

use crate::timeline::{failed, now_text, read_manifest, unfinished, wrong_state};
use crate::{Daemon, check_folder};

/// The export format version.
const EXPORT_VERSION: u32 = 1;
/// How often progress events go out, at most.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);
/// Free space an export leaves on its disk, beyond its files (and beyond
/// what a recording on the same disk keeps free).
const KEEP_FREE_BYTES: u64 = 512 << 20;
/// How often a running export checks the space left on its disk.
const SPACE_EVERY: Duration = Duration::from_secs(1);
/// The least a recording keeps free, whatever its setup says (the engine's
/// floor).
const RECORDING_FLOOR_BYTES: f64 = 64e6;
/// How long a running export is waited for when isocastd stops.
const STOP_WAIT: Duration = Duration::from_secs(10);
/// Names the running export, in the timelines folder (see the module docs).
const MARKER: &str = "export.json";
/// The longest file name macOS takes, in bytes.
const NAME_MAX: usize = 255;

/// The export running (or being planned, with an empty `export`).
pub struct Running {
    pub export: String,
    pub timeline: String,
    pub progress: f64,
    /// Why it should stop, once something asks it to.
    pub stop: Option<String>,
}

/// What the marker file holds.
#[derive(serde::Serialize, serde::Deserialize)]
struct Marker {
    manifest: PathBuf,
    /// Its files' unfinished names (`….partial.mov`).
    partials: Vec<PathBuf>,
}

/// Holds the one export slot while an export is planned; frees it if the
/// export doesn't start.
struct Slot<'a> {
    daemon: &'a Daemon,
    started: bool,
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        if !self.started {
            *self.daemon.export.lock() = None;
        }
    }
}

/// What one joined file is made of: its parts, the file whose format it
/// takes, and its picture size if not that file's.
type FileParts = (Vec<JoinPart>, Option<PathBuf>, Option<(u32, u32)>);

/// One clip, read from its take's manifest.
struct TakeFiles {
    clip: String,
    folder: PathBuf,
    manifest: SessionManifest,
    /// The clip's in point and length, in frames of its take's frame rate
    /// (the program's).
    in_frame: u64,
    frames: u64,
    fps: u32,
    /// The source the edited file shows for it.
    angle: String,
    /// The part of the angle's picture the edited file shows.
    crop: Option<Crop>,
}

impl TakeFiles {
    fn file(&self, source: &str) -> Option<&ManifestFile> {
        self.manifest.files.iter().find(|f| f.source == source)
    }

    fn in_seconds(&self) -> f64 {
        self.in_frame as f64 / f64::from(self.fps)
    }

    fn seconds(&self) -> f64 {
        self.frames as f64 / f64::from(self.fps)
    }

    /// What its file of `source` adds to that source's joined file: the
    /// clip's stretch of it, from its in point for its length (so every
    /// joined file switches clips at the same moments), and where the file
    /// starts within the take.
    fn part(&self, source: &str) -> JoinPart {
        let file = self.file(source).expect("planned");
        JoinPart {
            path: self.folder.join(&file.path),
            length: Some((self.frames, self.fps)),
            offset_seconds: file.start_offset_seconds.unwrap_or(0.0),
            in_seconds: self.in_seconds(),
            audio: None,
            fps: file.fps,
            crop: None,
        }
    }

    /// What it adds to the edited file: its angle's picture with the
    /// program's sound.
    fn edited_part(&self) -> JoinPart {
        let mut part = self.part(&self.angle);
        part.crop = self.crop;
        if self.angle != PROGRAM {
            let program = self.file(PROGRAM).expect("planned");
            part.audio = Some((
                self.folder.join(&program.path),
                program.start_offset_seconds.unwrap_or(0.0),
            ));
        }
        part
    }
}

/// Why the edited file can't be written, if it can't: every clip's angle
/// file must be there and finished.
fn edited_problem(takes: &[TakeFiles]) -> Option<String> {
    for take in takes {
        let Some(file) = take.file(&take.angle) else {
            return Some(format!(
                "clip {}'s angle {:?} isn't in its take",
                take.clip, take.angle
            ));
        };
        let path = take.folder.join(&file.path);
        if !path.exists() {
            return Some(format!(
                "clip {}'s angle file {} is missing",
                take.clip,
                path.display()
            ));
        }
        if file.duration_seconds.is_none() {
            return Some(format!(
                "clip {}'s angle file {} never finished",
                take.clip,
                path.display()
            ));
        }
    }
    None
}

/// The audio a file holds, as "audio tone (2 ch)", for messages.
fn audio_layout(file: &ManifestFile) -> String {
    match file.audio.is_empty() {
        true => "no audio".into(),
        false => format!(
            "audio {}",
            file.audio
                .iter()
                .map(|a| format!("{} ({} ch)", a.source, a.channels))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Which sources can be joined, and why the others can't: a source must be
/// in every clip, with the same size, frame rate and audio tracks.
fn plan(takes: &[TakeFiles]) -> (Vec<String>, Vec<ExportSkipped>) {
    // Every source in the order the takes list them, the program last.
    let mut sources: Vec<String> = Vec::new();
    for take in takes {
        for file in &take.manifest.files {
            if !sources.contains(&file.source) {
                sources.push(file.source.clone());
            }
        }
    }
    sources.sort_by_key(|source| source == PROGRAM);
    let mut joined = Vec::new();
    let mut skipped = Vec::new();
    for source in sources {
        let missing: Vec<&str> = takes
            .iter()
            .filter(|take| take.file(&source).is_none())
            .map(|take| take.clip.as_str())
            .collect();
        if !missing.is_empty() {
            skipped.push(ExportSkipped {
                source: source.clone(),
                reason: format!(
                    "not in clip{} {} (their takes didn't record it); {} of {} clips have it",
                    if missing.len() == 1 { "" } else { "s" },
                    missing.join(", "),
                    takes.len() - missing.len(),
                    takes.len()
                ),
            });
            continue;
        }
        let first = takes[0].file(&source).expect("checked");
        let mut reason = None;
        for take in takes {
            let file = take.file(&source).expect("checked");
            let path = take.folder.join(&file.path);
            if !path.exists() {
                reason = Some(format!(
                    "clip {}'s file {} is missing",
                    take.clip,
                    path.display()
                ));
            } else if (file.width, file.height, file.fps) != (first.width, first.height, first.fps)
            {
                reason = Some(format!(
                    "clip {} is {}x{} at {} fps and clip {} {}x{} at {} fps; files joined \
                     without re-encoding must match",
                    take.clip,
                    file.width,
                    file.height,
                    file.fps,
                    takes[0].clip,
                    first.width,
                    first.height,
                    first.fps
                ));
            } else if file.audio != first.audio {
                reason = Some(format!(
                    "clip {}'s file has {} and clip {}'s has {}; files joined must have the same \
                     audio tracks",
                    take.clip,
                    audio_layout(file),
                    takes[0].clip,
                    audio_layout(first)
                ));
            } else if file.duration_seconds.is_none() {
                reason = Some(format!(
                    "clip {}'s file {} never finished (its take was interrupted)",
                    take.clip,
                    path.display()
                ));
            }
            if reason.is_some() {
                break;
            }
        }
        match reason {
            Some(reason) => skipped.push(ExportSkipped { source, reason }),
            None => joined.push(source),
        }
    }
    (joined, skipped)
}

/// A name for files from a timeline's name: no slashes or colons.
pub(crate) fn file_stem(name: &str) -> String {
    let stem: String = name
        .chars()
        .map(|c| match c {
            '/' | ':' | '\\' => '-',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let stem = stem.trim().trim_start_matches('.').to_string();
    if stem.is_empty() {
        "Timeline".into()
    } else {
        stem
    }
}

/// `stem` cut (at a character) so every file name an export makes from it
/// fits in [`NAME_MAX`] bytes: `<stem> 99 <LABEL>.partial.<ext>` and
/// `<stem> 99.export.json.partial`.
fn fit_stem(stem: &str, files: &[(String, &str)]) -> String {
    let tail = files
        .iter()
        .map(|(label, extension)| format!(" 99 {label}.partial.{extension}").len())
        .chain([" 99.export.json.partial".len()])
        .max()
        .unwrap_or(0);
    let room = NAME_MAX.saturating_sub(tail).max(16);
    if stem.len() <= room {
        return stem.to_string();
    }
    let mut end = room;
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    stem[..end].trim_end().to_string()
}

/// An export id: the timeline's and the time, made unique among those this
/// isocastd made.
fn export_id(timeline: &str) -> String {
    static LAST: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let base = format!("{timeline}-{}", Local::now().format("%Y%m%d-%H%M%S"));
    let mut last = LAST.lock();
    let id = (1..)
        .map(|n| match n {
            1 => base.clone(),
            n => format!("{base}-{n}"),
        })
        .find(|id| !last.contains(id))
        .expect("a free id");
    last.push(id.clone());
    id
}

/// Paths for the files and manifest, numbered so none overwrites a file.
fn unique_paths(folder: &Path, stem: &str, files: &[(String, &str)]) -> (Vec<PathBuf>, PathBuf) {
    (1..)
        .map(|n| {
            let suffix = match n {
                1 => String::new(),
                n => format!(" {n}"),
            };
            let paths: Vec<PathBuf> = files
                .iter()
                .map(|(label, extension)| {
                    folder.join(format!("{stem}{suffix} {label}.{extension}"))
                })
                .collect();
            (paths, folder.join(format!("{stem}{suffix}.export.json")))
        })
        .find(|(paths, manifest)| {
            !manifest.exists()
                && paths.iter().all(|path| {
                    !path.exists() && {
                        // Nor a join's partial file left by a crash.
                        let partial = path.with_extension(format!(
                            "partial.{}",
                            path.extension().unwrap_or_default().to_string_lossy()
                        ));
                        !partial.exists()
                    }
                })
        })
        .expect("some numbered name is free")
}

/// `name.mov` → `name.partial.mov`, which a join writes until it is done
/// (as `isocast-platform-macos`'s join names it).
fn partial_path(path: &Path) -> PathBuf {
    path.with_extension(format!(
        "partial.{}",
        path.extension().unwrap_or_default().to_string_lossy()
    ))
}

/// An export isocastd was running when it was killed or crashed (the
/// marker in the timelines `folder` names it): its manifest says it
/// failed, its unfinished files are removed (files it had finished stay).
/// Returns what was done, for the log.
pub fn recover(folder: &Path) -> Vec<String> {
    let path = folder.join(MARKER);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let mut notes = Vec::new();
    if let Ok(marker) = serde_json::from_str::<Marker>(&text) {
        for partial in &marker.partials {
            // Only the export's own unfinished files, by their names.
            let unfinished = partial
                .file_name()
                .is_some_and(|name| name.to_string_lossy().contains(".partial."));
            if unfinished && partial.exists() && std::fs::remove_file(partial).is_ok() {
                notes.push(format!(
                    "removed {}, an export's unfinished file",
                    partial.display()
                ));
            }
        }
        let read = std::fs::read_to_string(&marker.manifest)
            .ok()
            .and_then(|text| serde_json::from_str::<ExportManifest>(&text).ok());
        if let Some(mut manifest) = read.filter(|m| m.status == ExportStatus::Exporting) {
            manifest.status = ExportStatus::Failed;
            let done: Vec<String> = manifest
                .files
                .iter()
                .filter(|file| file.path.exists())
                .map(|file| file.path.display().to_string())
                .collect();
            manifest.errors.push(format!(
                "isocastd stopped during the export (it was killed or crashed); export again{}",
                match done.is_empty() {
                    true => String::new(),
                    false => format!(
                        " (the files it had finished are complete: {})",
                        done.join(", ")
                    ),
                }
            ));
            match write_manifest(&marker.manifest, &manifest) {
                Ok(()) => notes.push(format!(
                    "export {} was interrupted; {} says so",
                    manifest.export,
                    marker.manifest.display()
                )),
                Err(problem) => notes.push(problem),
            }
        }
    }
    let _ = std::fs::remove_file(&path);
    notes
}

fn write_manifest(path: &Path, manifest: &ExportManifest) -> Result<(), String> {
    let partial = path.with_extension("json.partial");
    let json = serde_json::to_string_pretty(manifest).map_err(|err| err.to_string())?;
    std::fs::write(&partial, json + "\n")
        .and_then(|()| std::fs::rename(&partial, path))
        .map_err(|err| {
            let _ = std::fs::remove_file(&partial);
            format!("could not write {}: {err}", path.display())
        })
}

impl Daemon {
    /// The export running, as `status` shows it.
    pub(crate) fn export_running(&self) -> Option<ExportRunning> {
        let running = self.export.lock();
        running
            .as_ref()
            .filter(|running| !running.export.is_empty())
            .map(|running| ExportRunning {
                export: running.export.clone(),
                timeline: running.timeline.clone(),
                progress: running.progress,
            })
    }

    /// Stop the running export, if one is, for `reason`, and wait (a
    /// moment) for it to clean up: its unfinished file removed, its
    /// manifest saying why.
    pub(crate) fn stop_export(&self, reason: &str) {
        match self.export.lock().as_mut() {
            Some(running) => running.stop = Some(reason.to_string()),
            None => return,
        }
        let deadline = Instant::now() + STOP_WAIT;
        while self.export.lock().is_some() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// The least free space the recording running now keeps on the disk
    /// holding `folder`, if one is recording there: it stops when less is
    /// left (`recording.keep_free_gb`, or ten seconds of its data, as the
    /// engine works it out), and why, for messages.
    fn recording_floor(&self, folder: &Path) -> Option<(u64, String)> {
        let state = self.state.read();
        let session = state.session.as_ref()?;
        let files = session.files.lock().clone()?;
        let recording = files.first()?.parent()?.to_path_buf();
        let rate = session
            .live
            .lock()
            .as_ref()
            .and_then(|live| live.disk.as_ref())
            .map_or(0.0, |disk| disk.write_bytes_per_second);
        let keep = state
            .setup
            .as_ref()
            .map_or(0.0, |setup| setup.recording.keep_free_gb * 1e9);
        drop(state);
        use isocast_engine::Platform;
        if !crate::platform::NativePlatform
            .same_volume(folder, &recording)
            .ok()?
        {
            return None;
        }
        let floor = keep.max(rate * 10.0).max(RECORDING_FLOOR_BYTES);
        let why = format!(
            "the recording writing to {} on that disk stops when less than {} is left \
             (recording.keep_free_gb)",
            recording.display(),
            isocast_engine::space::human(floor)
        );
        Some((floor as u64, why))
    }

    pub(crate) fn timeline_export(
        self: &Arc<Self>,
        params: TimelineExportParams,
    ) -> Result<ExportStarted, RpcError> {
        check_folder(params.folder.as_deref())?;
        // Its clips' takes, where they are now.
        self.heal_timelines();
        // One at a time: the slot is taken before anything is planned.
        let mut slot = {
            let mut running = self.export.lock();
            if let Some(running) = running.as_ref() {
                return Err(wrong_state(match running.export.is_empty() {
                    true => "another export is starting; wait for its export_finished event \
                             (`isocast watch --kinds export_finished`)"
                        .to_string(),
                    false => format!(
                        "export {} of timeline {:?} is running ({:.0}%); wait for its \
                         export_finished event (`isocast watch --kinds export_finished`)",
                        running.export,
                        running.timeline,
                        running.progress * 100.0
                    ),
                }));
            }
            *running = Some(Running {
                export: String::new(),
                timeline: String::new(),
                progress: 0.0,
                stop: None,
            });
            Slot {
                daemon: self,
                started: false,
            }
        };
        let (id, timeline, markers) = {
            let timelines = self.timelines.lock();
            let id = timelines.resolve(params.timeline.as_deref())?;
            let timeline = timelines.get(&id).cloned().expect("resolved");
            (id, timeline, timelines.folder().join(MARKER))
        };
        if timeline.clips.is_empty() {
            return Err(wrong_state(format!(
                "timeline {id:?} has no clips to export yet; record a take while it is current"
            )));
        }
        let mut takes = Vec::new();
        for clip in &timeline.clips {
            let manifest = read_manifest(&clip.take)
                .map_err(|problem| failed(format!("clip {}: {problem}", clip.id)))?;
            if let Some(problem) = unfinished(&clip.id, &clip.take, &manifest) {
                return Err(failed(problem));
            }
            // The clip's stretch on its take's frame grid (edits keep its
            // points there; this only rounds what a hand edit left).
            let (take_frames, fps) = take_length(&manifest).unwrap_or((
                (manifest.duration_seconds.unwrap_or(0.0) * 60.0).round() as u64,
                60,
            ));
            let frame = |seconds: f64| (seconds * f64::from(fps)).round().max(0.0) as u64;
            let in_frame = frame(clip.in_seconds).min(take_frames);
            let out_frame = frame(clip.out_seconds).min(take_frames);
            if out_frame <= in_frame {
                return Err(failed(format!(
                    "clip {} covers no frames of its take ({:.3}–{:.3} s); trim it (`isocast \
                     timeline trim`, MCP trim_clip) or take it off",
                    clip.id, clip.in_seconds, clip.out_seconds
                )));
            }
            takes.push(TakeFiles {
                clip: clip.id.clone(),
                folder: clip.take.parent().unwrap_or(Path::new("/")).to_path_buf(),
                manifest,
                in_frame,
                frames: out_frame - in_frame,
                fps,
                angle: clip.angle.clone().unwrap_or_else(|| PROGRAM.to_string()),
                crop: clip.crop.filter(|crop| !crop.is_full()),
            });
        }
        let (sources, skipped) = plan(&takes);
        // The program is the one joined file the timeline is for: if the
        // takes recorded one, it must join, or nothing is exported.
        let recorded_program = takes.iter().any(|take| take.file(PROGRAM).is_some());
        if recorded_program && !sources.iter().any(|source| source == PROGRAM) {
            let reason = skipped
                .iter()
                .find(|s| s.source == PROGRAM)
                .map_or("it can't be joined", |s| s.reason.as_str());
            let fix = match reason.contains(" is missing") {
                true => "put the file back, or take that clip off",
                false => "take that clip off",
            };
            return Err(failed(format!(
                "program: the program can't be joined: {reason}. Nothing was exported; {fix} \
                 (`isocast timeline remove CLIP`, MCP remove_clip), then export again"
            )));
        }
        // The edited file, when a clip shows another angle than the program
        // or is cropped.
        let mut skipped = skipped;
        let angled = takes
            .iter()
            .any(|take| take.angle != PROGRAM || take.crop.is_some());
        let edited = angled && sources.iter().any(|source| source == PROGRAM);
        if angled && !edited {
            skipped.push(ExportSkipped {
                source: EDITED.into(),
                reason: "clips show other angles or crops, but the program can't be joined, \
                         and the edited file has the program's sound and format"
                    .into(),
            });
        }
        // A recording from before `edited` and `vertical` were reserved
        // source ids could have an ISO file by that name (#94).
        let taken = |label: &str| {
            sources.iter().any(|s| s == label).then(|| {
                format!(
                    "the takes have an ISO source called {label:?}, which would share the file's \
                     label; rename that source for new recordings"
                )
            })
        };
        let edited = edited
            && match edited_problem(&takes).or_else(|| taken(EDITED)) {
                None => true,
                Some(reason) => {
                    skipped.push(ExportSkipped {
                        source: EDITED.into(),
                        reason,
                    });
                    false
                }
            };
        // The vertical edited file (#94), when asked for: the program's
        // size on its side. A program already taller than wide (or square)
        // has nothing to turn on its side: its edited file is the vertical
        // one.
        let upright = sources.iter().any(|s| s == PROGRAM)
            && takes[0]
                .file(PROGRAM)
                .is_some_and(|program| program.height >= program.width);
        let vertical = params.vertical
            && match (
                sources.iter().any(|s| s == PROGRAM),
                edited_problem(&takes).or_else(|| taken(VERTICAL)),
            ) {
                (true, _) if upright => {
                    let program = takes[0].file(PROGRAM).expect("joined");
                    let shape = match program.height == program.width {
                        true => "square",
                        false => "already vertical",
                    };
                    skipped.push(ExportSkipped {
                        source: VERTICAL.into(),
                        reason: format!(
                            "the program is {shape} ({}x{}), so a 9:16 file of it would be the \
                             edited picture again; the program (and EDITED) file is the one to use",
                            program.width, program.height
                        ),
                    });
                    false
                }
                (false, _) => {
                    skipped.push(ExportSkipped {
                        source: VERTICAL.into(),
                        reason: "the program can't be joined, and the vertical file has the \
                                 program's sound and format"
                            .into(),
                    });
                    false
                }
                (true, Some(reason)) => {
                    skipped.push(ExportSkipped {
                        source: VERTICAL.into(),
                        reason,
                    });
                    false
                }
                (true, None) => true,
            };
        let vertical_size = vertical.then(|| {
            let program = takes[0].file(PROGRAM).expect("joined");
            // Each side even, as encoders take.
            (program.height / 2 * 2, program.width / 2 * 2)
        });
        if sources.is_empty() {
            let reasons: Vec<String> = skipped
                .iter()
                .map(|s| format!("{}: {}", s.source, s.reason))
                .collect();
            return Err(failed(format!(
                "nothing to export: no source can be joined across every clip ({})",
                reasons.join("; ")
            )));
        }
        let folder = params
            .folder
            .clone()
            .unwrap_or_else(|| takes[0].folder.clone());
        std::fs::create_dir_all(&folder).map_err(|err| {
            failed(format!(
                "folder: could not make {}: {err}",
                folder.display()
            ))
        })?;
        // Room for every file joined (their audio comes out about as big),
        // each clip's share of its file, and for a recording on the same
        // disk to carry on.
        let share = |take: &TakeFiles, source: &str| {
            let file = take.file(source).expect("planned");
            let size = std::fs::metadata(take.folder.join(&file.path)).map_or(0, |m| m.len());
            let length = file.duration_seconds.unwrap_or(0.0).max(1e-3);
            (size as f64 * (take.seconds() / length).min(1.0)) as u64
        };
        let mut needed: u64 = sources
            .iter()
            .flat_map(|source| takes.iter().map(move |take| share(take, source)))
            .sum();
        if vertical {
            // Every frame encoded again, scaled up: as an angle, allow 5.
            needed += takes
                .iter()
                .map(|take| share(take, &take.angle).max(share(take, PROGRAM)) * 5)
                .sum::<u64>();
        }
        if edited {
            // Angles encoded again (scaled up, at a higher quality) came
            // out up to 4.2 times the program's stretch: allow 5.
            needed += takes
                .iter()
                .map(|take| match take.angle == PROGRAM && take.crop.is_none() {
                    true => share(take, PROGRAM),
                    false => share(take, &take.angle).max(share(take, PROGRAM)) * 5,
                })
                .sum::<u64>();
        }
        let recording = self.recording_floor(&folder);
        if let Some(free) = free_bytes(&folder) {
            let human = |bytes: u64| isocast_engine::space::human(bytes as f64);
            if let Some((floor, why)) = &recording
                && free < needed + floor + KEEP_FREE_BYTES
            {
                return Err(failed(format!(
                    "folder: {} has {} free; the export needs about {}, and {why}, so exporting \
                     there now could stop it early. Export to another disk (`--folder`, the \
                     folder parameter) or once the recording has stopped",
                    folder.display(),
                    human(free),
                    human(needed)
                )));
            }
            if free < needed + KEEP_FREE_BYTES {
                return Err(failed(format!(
                    "folder: {} has {} free; the export needs about {} plus {} to spare",
                    folder.display(),
                    human(free),
                    human(needed),
                    human(KEEP_FREE_BYTES)
                )));
            }
        }
        let extension_of = |source: &str| {
            let file = takes[0].file(source).expect("planned");
            match file.path.rsplit_once('.') {
                Some((_, "mp4")) => "mp4",
                _ => "mov",
            }
        };
        let mut labels: Vec<(String, &str)> = sources
            .iter()
            .map(|source| (source.to_uppercase(), extension_of(source)))
            .collect();
        if edited {
            labels.push((EDITED.to_uppercase(), extension_of(PROGRAM)));
        }
        if vertical {
            labels.push((VERTICAL.to_uppercase(), extension_of(PROGRAM)));
        }
        let stem = fit_stem(&file_stem(&timeline.name), &labels);
        let (paths, manifest_path) = unique_paths(&folder, &stem, &labels);
        let mut outputs: Vec<String> = sources.clone();
        if edited {
            outputs.push(EDITED.into());
        }
        if vertical {
            outputs.push(VERTICAL.into());
        }
        let files: Vec<ExportFile> = outputs
            .iter()
            .zip(&paths)
            .map(|(source, path)| {
                // The edited file is in the program's format, with its sound.
                let shown = |take: &TakeFiles| match source.as_str() {
                    EDITED | VERTICAL => take.angle.clone(),
                    source => source.to_string(),
                };
                let format = match source.as_str() {
                    EDITED | VERTICAL => PROGRAM,
                    source => source,
                };
                let first = takes[0].file(format).expect("planned");
                let (width, height) = match (source.as_str(), vertical_size) {
                    (VERTICAL, Some(size)) => size,
                    _ => (first.width, first.height),
                };
                ExportFile {
                    path: path.clone(),
                    source: source.clone(),
                    width,
                    height,
                    fps: first.fps,
                    audio: first.audio.iter().map(|a| a.source.clone()).collect(),
                    parts: takes
                        .iter()
                        .map(|take| ExportPart {
                            clip: take.clip.clone(),
                            path: take
                                .folder
                                .join(&take.file(&shown(take)).expect("planned").path),
                            start_seconds: None,
                            duration_seconds: None,
                            frames: None,
                            frames_left_out: None,
                            reencoded_frames: None,
                        })
                        .collect(),
                    duration_seconds: None,
                    frames: None,
                }
            })
            .collect();
        let export = export_id(&id);
        let mut manifest = ExportManifest {
            version: EXPORT_VERSION,
            export: export.clone(),
            timeline: id.clone(),
            name: timeline.name.clone(),
            status: ExportStatus::Exporting,
            started_at: now_text(),
            clips: timeline
                .clips
                .iter()
                .map(|clip| ExportClip {
                    id: clip.id.clone(),
                    take: clip.take.clone(),
                    in_seconds: clip.in_seconds,
                    out_seconds: clip.out_seconds,
                    angle: clip.angle.clone(),
                    crop: clip.crop,
                })
                .collect(),
            files: files.clone(),
            skipped: skipped.clone(),
            notes: short_file_notes(&takes, &sources),
            errors: Vec::new(),
        };
        write_manifest(&manifest_path, &manifest).map_err(failed)?;
        let marker = Marker {
            manifest: manifest_path.clone(),
            partials: paths.iter().map(|path| partial_path(path)).collect(),
        };
        if let Ok(json) = serde_json::to_string_pretty(&marker) {
            let _ = std::fs::write(&markers, json + "\n");
        }
        if let Some(running) = self.export.lock().as_mut() {
            running.export = export.clone();
            running.timeline = id.clone();
        }
        slot.started = true;
        let duration_seconds: f64 = takes.iter().map(TakeFiles::seconds).sum();
        let manifest_notes = manifest.notes.clone();
        let mut parts: Vec<FileParts> = sources
            .iter()
            .map(|source| {
                (
                    takes.iter().map(|take| take.part(source)).collect(),
                    None,
                    None,
                )
            })
            .collect();
        if edited || vertical {
            let program = takes[0]
                .folder
                .join(&takes[0].file(PROGRAM).expect("planned").path);
            let edited_parts: Vec<JoinPart> = takes.iter().map(TakeFiles::edited_part).collect();
            if edited {
                parts.push((edited_parts.clone(), Some(program.clone()), None));
            }
            if vertical {
                parts.push((edited_parts, Some(program), vertical_size));
            }
        }
        let daemon = self.clone();
        let path = manifest_path.clone();
        let watched = folder.clone();
        thread::spawn(move || {
            daemon.run_export(&mut manifest, &parts, &path, &watched);
            let _ = std::fs::remove_file(&markers);
            *daemon.export.lock() = None;
            daemon.publish(Event::ExportFinished {
                export: manifest.export.clone(),
                timeline: manifest.timeline.clone(),
                manifest: path.to_path_buf(),
                files: manifest
                    .files
                    .iter()
                    .filter(|file| file.duration_seconds.is_some())
                    .cloned()
                    .collect(),
                error: manifest.errors.first().cloned(),
            });
        });
        Ok(ExportStarted {
            export,
            timeline: id,
            folder,
            manifest: manifest_path,
            files,
            skipped,
            duration_seconds,
            notes: manifest_notes,
        })
    }

    /// Write each file of `manifest` (from `parts`) in turn into `folder`,
    /// then the final manifest (its `errors` say why, if it failed). Stops
    /// when asked (`Running::stop`) or when a recording on the same disk
    /// would be left less than it keeps free.
    fn run_export(
        &self,
        manifest: &mut ExportManifest,
        parts: &[FileParts],
        path: &Path,
        folder: &Path,
    ) {
        let count = manifest.files.len() as f64;
        let mut last = Instant::now() - PROGRESS_EVERY;
        let mut checked = Instant::now();
        let mut stopped: Option<String> = None;
        let mut error = None;
        for (index, (parts, format, size)) in parts.iter().enumerate() {
            let file = &manifest.files[index];
            let spec = JoinSpec {
                parts: parts.clone(),
                output: file.path.clone(),
                container: match file.path.extension().and_then(|e| e.to_str()) {
                    Some("mp4") => Container::Mp4,
                    _ => Container::Mov,
                },
                format: format.clone(),
                size: *size,
            };
            let output = file.path.clone();
            let mut progress = |fraction: f64| {
                let overall = (index as f64 + fraction.clamp(0.0, 1.0)) / count;
                if let Some(running) = self.export.lock().as_mut() {
                    running.progress = overall;
                    if let Some(reason) = &running.stop {
                        stopped = Some(reason.clone());
                        return false;
                    }
                }
                if checked.elapsed() >= SPACE_EVERY {
                    checked = Instant::now();
                    if let (Some((floor, why)), Some(free)) =
                        (self.recording_floor(folder), free_bytes(folder))
                        && free < floor + KEEP_FREE_BYTES / 2
                    {
                        stopped = Some(format!(
                            "stopped with {} left on the disk, because {why}; export to another \
                             disk (`--folder`, the folder parameter) or once the recording has \
                             stopped",
                            isocast_engine::space::human(free as f64)
                        ));
                        return false;
                    }
                }
                if last.elapsed() >= PROGRESS_EVERY {
                    last = Instant::now();
                    self.publish(Event::ExportProgress {
                        export: manifest.export.clone(),
                        timeline: manifest.timeline.clone(),
                        progress: overall,
                        file: output.clone(),
                    });
                }
                true
            };
            match join_files(&spec, &mut progress) {
                Ok(joined) => {
                    record(&mut manifest.files[index], &joined);
                    let file = &manifest.files[index];
                    let mut notes = Vec::new();
                    for (part, written) in file.parts.iter().zip(&joined.parts) {
                        // An angle in the edited file is meant to be
                        // encoded again; say so only for a joined source.
                        if written.converted && file.source != EDITED {
                            notes.push(format!(
                                "{}: clip {}'s file {} isn't in the joined file's format ({}x{} \
                                 at {} fps, the codec of the first), so it was encoded again whole",
                                file.source,
                                part.clip,
                                part.path.display(),
                                file.width,
                                file.height,
                                file.fps
                            ));
                        }
                    }
                    manifest.notes.extend(notes);
                }
                Err(err) => {
                    error = Some(match stopped.take() {
                        Some(reason) => format!("{}: {reason}", file.path.display()),
                        None => format!("{}: {err:#}", file.path.display()),
                    });
                    break;
                }
            }
        }
        manifest.notes.extend(left_out_notes(manifest));
        manifest.status = match error {
            None => ExportStatus::Complete,
            Some(_) => ExportStatus::Failed,
        };
        manifest.errors.extend(error);
        if let Err(problem) = write_manifest(path, manifest) {
            manifest.errors.push(problem);
        }
    }
}

/// What a join wrote, into the export's file.
fn record(file: &mut ExportFile, joined: &Joined) {
    file.duration_seconds = Some(joined.duration_seconds);
    file.frames = Some(joined.frames);
    for (part, written) in file.parts.iter_mut().zip(&joined.parts) {
        part.start_seconds = Some(written.start_seconds);
        part.duration_seconds = Some(written.duration_seconds);
        part.frames = Some(written.frames);
        part.frames_left_out = (written.frames_left_out > 0).then_some(written.frames_left_out);
        part.reencoded_frames = (written.reencoded > 0).then_some(written.reencoded);
    }
}

/// Frames a join left out because they start past their clip's end.
fn left_out_notes(manifest: &ExportManifest) -> Vec<String> {
    let mut notes = Vec::new();
    for file in &manifest.files {
        for part in &file.parts {
            if let Some(count) = part.frames_left_out {
                notes.push(format!(
                    "{}: {count} frame{} of clip {}'s file start after the clip ends (the file ran \
                     longer than the program) and were left out",
                    file.source,
                    if count == 1 { "" } else { "s" },
                    part.clip
                ));
            }
        }
    }
    notes
}

/// Files that don't cover their whole clip (one started late or stopped
/// early): their picture holds and their audio is silent for the rest.
fn short_file_notes(takes: &[TakeFiles], sources: &[String]) -> Vec<String> {
    let mut notes = Vec::new();
    for source in sources {
        for take in takes {
            let file = take.file(source).expect("planned");
            let (clip_in, clip) = (take.in_seconds(), take.seconds());
            let file_in = file.start_offset_seconds.unwrap_or(0.0);
            let file_out = file_in + file.duration_seconds.unwrap_or(0.0);
            let covered = (file_out.min(clip_in + clip) - file_in.max(clip_in)).clamp(0.0, clip);
            let short = clip - covered;
            if short > 1.0 / f64::from(file.fps.max(1)) {
                notes.push(format!(
                    "{source}: clip {}'s file covers {covered:.3} s of the clip's {clip:.3} s; its \
                     picture holds and its audio is silent for the other {short:.3} s",
                    take.clip
                ));
            }
        }
    }
    notes
}

fn join_files(spec: &JoinSpec, progress: OnJoinProgress) -> anyhow::Result<Joined> {
    use isocast_engine::Platform;
    crate::platform::NativePlatform.join_files(spec, progress)
}

fn free_bytes(folder: &Path) -> Option<u64> {
    use isocast_engine::Platform;
    crate::platform::NativePlatform
        .volume(folder)
        .ok()
        .map(|v| v.free_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn take(clip: &str, files: serde_json::Value) -> TakeFiles {
        TakeFiles {
            clip: clip.into(),
            folder: std::env::temp_dir(),
            manifest: serde_json::from_value(serde_json::json!({
                "version": 1, "setup": "t", "status": "complete",
                "started_at": "2026-10-13T12:00:00.000-04:00", "host_time_ns": 0,
                "duration_seconds": 1.0, "files": files, "errors": []
            }))
            .unwrap(),
            in_frame: 0,
            frames: 30,
            fps: 30,
            angle: PROGRAM.into(),
            crop: None,
        }
    }

    fn file(source: &str, width: u32) -> serde_json::Value {
        // A file that exists, so only the differences matter.
        let path = std::env::temp_dir().join(format!("isocast-export-test-{source}-{width}.mov"));
        std::fs::write(&path, b"").unwrap();
        serde_json::json!({
            "path": path.file_name().unwrap().to_string_lossy(), "source": source,
            "kind": "camera", "width": width, "height": 720, "fps": 30,
            "audio": [{"source": "mic", "channels": 2}], "start_offset_seconds": 0.0,
            "duration_seconds": 1.0, "frames": 30, "dropped_frames": 0, "audio_dropouts": 0
        })
    }

    #[test]
    fn sources_every_clip_has_alike_are_joined_and_the_rest_say_why() {
        let takes = [
            take(
                "c1",
                serde_json::json!([
                    file("camera", 1280),
                    file("screen", 1280),
                    file("program", 1280)
                ]),
            ),
            take(
                "c3",
                serde_json::json!([file("camera", 1280), file("program", 1280)]),
            ),
            take(
                "c2",
                serde_json::json!([
                    file("camera", 1280),
                    file("screen", 640),
                    file("program", 1280)
                ]),
            ),
        ];
        let (joined, skipped) = plan(&takes);
        assert_eq!(joined, ["camera", "program"]);
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].source, "screen");
        assert!(
            skipped[0].reason.starts_with("not in clip c3"),
            "{}",
            skipped[0].reason
        );
        let (_, skipped) = plan(&[takes[0].clone_files(), takes[2].clone_files()]);
        assert!(
            skipped[0].reason.contains("1280x720 at 30 fps"),
            "{}",
            skipped[0].reason
        );
    }

    impl TakeFiles {
        fn clone_files(&self) -> Self {
            Self {
                clip: self.clip.clone(),
                folder: self.folder.clone(),
                manifest: self.manifest.clone(),
                in_frame: self.in_frame,
                frames: self.frames,
                fps: self.fps,
                angle: self.angle.clone(),
                crop: self.crop,
            }
        }
    }

    #[test]
    fn export_names_never_overwrite() {
        assert_eq!(file_stem("My/Video: take 2"), "My-Video- take 2");
        assert_eq!(file_stem("  "), "Timeline");
        let folder =
            std::env::temp_dir().join(format!("isocast-export-names-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let labels = [("PROGRAM".to_string(), "mov")];
        let (paths, manifest) = unique_paths(&folder, "Intro", &labels);
        assert_eq!(paths[0], folder.join("Intro PROGRAM.mov"));
        assert_eq!(manifest, folder.join("Intro.export.json"));
        std::fs::write(&paths[0], b"").unwrap();
        let (paths, _) = unique_paths(&folder, "Intro", &labels);
        assert_eq!(paths[0], folder.join("Intro 2 PROGRAM.mov"));
        let _ = std::fs::remove_dir_all(&folder);
    }
}
