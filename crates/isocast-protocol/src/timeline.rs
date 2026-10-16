//! Timelines (C.1): takes in order, to join into one file.
//!
//! A timeline is an ordered list of clips; each clip is one take (its
//! session manifest and every file it lists: the program and each ISO
//! file). While a timeline is current, every take that finishes (a stop or
//! a split) is added to its end. `retake` takes the last clip off and
//! records it again; clips can be removed and moved; `timeline_export`
//! joins the clips into one program file and one file per ISO source.
//!
//! Taking a clip off a timeline never touches the take's files: they stay
//! where they were recorded, and the timeline lists them under `removed`.
//! So does a take isocastd was recording when it was killed or crashed: it
//! never finished, so it is never a clip.
//! A clip covers part of its take, `in_seconds` to `out_seconds` on the
//! take's own clock (the whole take when it is added). The Edit view's edits
//! (C.3) only change clips: `timeline_trim` sets a clip's in and out points,
//! `timeline_split` cuts a clip in two, `timeline_delete_range` takes a span
//! of the timeline out (ripple), and `timeline_angle` sets which source the
//! edited video shows over a span (a clip's `angle`). Each edit is one
//! revision, which `timeline_undo` and `timeline_redo` step through. They
//! never touch a take's files.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::manifest::{ManifestFile, SessionManifest, SessionStatus};

/// The timeline file format version.
pub const TIMELINE_VERSION: u32 = 1;

/// A timeline as it is kept, one JSON file per timeline
/// (`<id>.timeline.json` in the timelines folder).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Timeline {
    /// Format version. Currently 1.
    pub version: u32,
    /// Lowercase letters, digits and dashes; stays the same when renamed.
    pub id: String,
    pub name: String,
    /// When it was made, local time, RFC 3339.
    pub created_at: String,
    pub clips: Vec<Clip>,
    /// Takes taken off this timeline (removed, retaken, dropped while
    /// recording, or interrupted), oldest first. Their files were left
    /// where they are.
    #[serde(default)]
    pub removed: Vec<RemovedTake>,
    /// Takes being recorded that began while this timeline was current
    /// (their manifests), until they finish (and are added to the timeline
    /// current then). One still here when isocastd starts again was
    /// interrupted, and moves to `removed`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recording: Vec<PathBuf>,
    /// The number the next clip's id gets (`c1`, `c2`, …), so ids are
    /// never reused.
    pub next_clip: u32,
    /// Counts every change to the clips (an edit, an undo, a take added),
    /// so a client can tell its copy is out of date.
    #[serde(default)]
    pub revision: u64,
    /// The clips (and removed takes) before each edit, oldest first, for
    /// `timeline_undo`; at most [`UNDO_LIMIT`]. A take added by recording
    /// clears it: undo goes back through the edits since the last take.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub undo: Vec<Revision>,
    /// What `timeline_undo` took back, newest last, for `timeline_redo`;
    /// any new edit clears it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redo: Vec<Revision>,
}

/// How many edits `timeline_undo` can take back.
pub const UNDO_LIMIT: usize = 100;

/// A timeline's clips as they were before (or after) an edit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Revision {
    /// The edit this undoes or redoes.
    pub action: TimelineAction,
    pub clips: Vec<Clip>,
    pub removed: Vec<RemovedTake>,
}

/// One take on a timeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Clip {
    /// `c1`, `c2`, …: unique within its timeline, never reused.
    pub id: String,
    /// The take's session manifest (`….session.json`), which lists its
    /// files next to it.
    pub take: PathBuf,
    /// The part of the take the clip covers, on the take's timeline in
    /// seconds: the whole take for now, 0 to its length on the program's
    /// frame grid ([`take_seconds`]).
    pub in_seconds: f64,
    pub out_seconds: f64,
    /// The recording project (its id) the take is in, so the clip is found
    /// again if the project's folder is renamed behind Isocast's back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// The source the edited video shows for this clip (an ISO source's
    /// id); the program if absent. Set with `timeline_angle`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub angle: Option<String>,
}

/// A take that was on a timeline (or recorded for it) and was taken off.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemovedTake {
    /// The take's session manifest; its files are next to it, untouched.
    pub take: PathBuf,
    /// The clip it was, if it was on the timeline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clip: Option<String>,
    pub reason: RemovedReason,
    /// When, local time, RFC 3339.
    pub at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemovedReason {
    /// `timeline_remove`.
    Removed,
    /// `retake` while not recording: the last clip, recorded again.
    Retaken,
    /// `retake` while recording: the take being recorded, never a clip.
    Dropped,
    /// isocastd stopped (killed or crashed) or the recording failed while
    /// recording it, so it never finished: its files hold what was written
    /// up to about 2 s before that, and their lengths aren't known.
    Interrupted,
}

/// Parameters of `timeline_new`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineNewParams {
    /// Its name; "Timeline N" if absent. Its id is made from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Which timeline: the current one if absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineTarget {
    /// A timeline's id (`timeline_list` has them); the current one if
    /// absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeline: Option<String>,
}

/// Parameters of `timeline_open`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineOpenParams {
    /// The id of the timeline to make current.
    pub timeline: String,
}

/// Parameters of `timeline_remove`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineRemoveParams {
    /// The clip's id, like `c2`.
    pub clip: String,
    /// The timeline's id; the current one if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeline: Option<String>,
}

/// Parameters of `timeline_move`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineMoveParams {
    /// The clip's id, like `c3`.
    pub clip: String,
    /// Where it goes, counting from 0 (the first clip) to the number of
    /// clips less one (the last).
    pub index: usize,
    /// The timeline's id; the current one if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeline: Option<String>,
}

/// Parameters of `timeline_trim`: a clip's new in and/or out point.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineTrimParams {
    /// The clip's id, like `c2`.
    pub clip: String,
    /// Where the clip starts, in seconds on its take's own clock (0 is the
    /// take's start); unchanged if absent. Rounded to the take's frame grid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_seconds: Option<f64>,
    /// Where the clip ends, on its take's clock, up to the take's length;
    /// unchanged if absent. Rounded to the take's frame grid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_seconds: Option<f64>,
    /// The timeline's id; the current one if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeline: Option<String>,
}

/// Parameters of `timeline_split`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineSplitParams {
    /// Where to cut, in seconds on the timeline (0 is its start), rounded
    /// to the frame grid of the clip there.
    pub at_seconds: f64,
    /// The timeline's id; the current one if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeline: Option<String>,
}

/// Parameters of `timeline_delete_range`: a span of the timeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineRangeParams {
    /// Where the span starts and ends, in seconds on the timeline, each
    /// rounded to the frame grid of the clip there.
    pub start_seconds: f64,
    pub end_seconds: f64,
    /// The timeline's id; the current one if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeline: Option<String>,
}

/// Parameters of `timeline_angle`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineAngleParams {
    /// Where the span starts and ends, in seconds on the timeline, each
    /// rounded to the frame grid of the clip there.
    pub start_seconds: f64,
    pub end_seconds: f64,
    /// What the edited video shows over the span: `program`, or an ISO
    /// source's id that every clip in the span recorded.
    pub source: String,
    /// The timeline's id; the current one if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeline: Option<String>,
}

/// Parameters of `retake`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RetakeParams {
    /// Write the new take here; else the folder of the take it replaces
    /// (else the setup's `recording.folder`). Not while recording, where
    /// the new take goes where the recording writes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<PathBuf>,
    /// Stop on its own after this many seconds (not while recording).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds: Option<f64>,
}

/// Parameters of `timeline_export`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineExportParams {
    /// The timeline's id; the current one if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeline: Option<String>,
    /// Where to write the joined files (made if missing); else the folder
    /// of the first clip's take.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<PathBuf>,
}

/// A timeline with what each clip holds, as `timeline_get` and the
/// methods that change one return it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TimelineInfo {
    pub id: String,
    pub name: String,
    /// It is the current timeline: finished takes are added to it.
    pub current: bool,
    pub created_at: String,
    /// The file it is kept in.
    pub path: PathBuf,
    /// The clips' durations added up.
    pub duration_seconds: f64,
    pub clips: Vec<ClipInfo>,
    /// Takes taken off it, whose files are where they were recorded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<RemovedTake>,
    /// What a change did that is worth knowing (where a removed take's
    /// files are).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// See [`Timeline::revision`].
    #[serde(default)]
    pub revision: u64,
    /// Edits `timeline_undo` can take back, and `timeline_redo` put back.
    #[serde(default)]
    pub undo: usize,
    #[serde(default)]
    pub redo: usize,
}

/// A clip and its take.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ClipInfo {
    pub id: String,
    /// Its place on the timeline, from 0.
    pub index: usize,
    /// The take's session manifest.
    pub take: PathBuf,
    /// Where the clip starts on the timeline.
    pub start_seconds: f64,
    pub duration_seconds: f64,
    pub in_seconds: f64,
    pub out_seconds: f64,
    /// When the take started, from its manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// The take's status, from its manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<SessionStatus>,
    /// Frames its files dropped, added up (each file's are in `files`).
    #[serde(default)]
    pub dropped_frames: u64,
    /// The take's files: the program and each ISO file.
    pub files: Vec<ClipFile>,
    /// Why the take can't be read (its manifest is missing or broken).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    /// The source the edited video shows (see [`Clip::angle`]); the
    /// program if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub angle: Option<String>,
    /// The take's frame rate, whose grid the clip's points sit on.
    #[serde(default)]
    pub fps: u32,
}

/// One of a take's files.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ClipFile {
    pub path: PathBuf,
    /// The video source in it (`program` for the program).
    pub source: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Audio sources, one track each.
    pub audio: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames: Option<u64>,
    /// Frames the recording dropped from this file, and gaps in its audio.
    #[serde(default)]
    pub dropped_frames: u64,
    #[serde(default)]
    pub audio_dropouts: u64,
    /// The file is where the manifest says.
    pub exists: bool,
    /// Where the file's first frame sits on its take's clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_offset_seconds: Option<f64>,
}

/// What `timeline_list` returns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TimelineList {
    /// The current timeline's id, if one is current.
    pub current: Option<String>,
    /// Where timelines are kept.
    pub folder: PathBuf,
    pub timelines: Vec<TimelineSummary>,
    /// Timeline files that couldn't be read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TimelineSummary {
    pub id: String,
    pub name: String,
    pub current: bool,
    pub created_at: String,
    pub clips: usize,
    /// The clips' durations added up.
    pub duration_seconds: f64,
}

/// What `timeline_close` returns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TimelineClosed {
    /// The timeline that was current, if one was.
    pub closed: Option<String>,
}

/// What `retake` returns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Retaken {
    /// The current timeline, which the new take is added to when it
    /// finishes (unless another is made current by then); a take dropped
    /// while recording is listed under its `removed`.
    pub timeline: String,
    /// The new take's files.
    pub files: Vec<PathBuf>,
    /// When the new take begins, while recording (local time, RFC 3339
    /// with milliseconds), and how long after this reply: what is recorded
    /// before then is in the dropped take.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub begins_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub begins_in_seconds: Option<f64>,
    /// The take that was taken off (its files stay where they are), if
    /// there was one.
    pub replaced: Option<RemovedTake>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// What `timeline_export` returns once the export has started; it runs
/// in the background, reporting `export_progress` and `export_finished`
/// events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ExportStarted {
    /// The export's id, which its events carry.
    pub export: String,
    pub timeline: String,
    pub folder: PathBuf,
    /// The export's manifest (`<name>.export.json`), written now and again
    /// when it finishes.
    pub manifest: PathBuf,
    /// The files it writes: the program and each ISO source every clip has,
    /// each cut to the clips' in and out points, and (when a clip shows
    /// another angle than the program) the edited file, source `edited`.
    pub files: Vec<ExportFile>,
    /// Sources it doesn't join, and why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<ExportSkipped>,
    /// The timeline's length: each file's will be its clips' added up.
    pub duration_seconds: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// The export running, as `status` shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ExportRunning {
    pub export: String,
    pub timeline: String,
    /// 0 to 1 across all its files.
    pub progress: f64,
}

/// A source left out of an export.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ExportSkipped {
    pub source: String,
    pub reason: String,
}

/// One joined file of an export.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ExportFile {
    pub path: PathBuf,
    /// The video source joined (`program` for the program, `edited` for
    /// the edited file).
    pub source: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Audio sources, one track each.
    pub audio: Vec<String>,
    /// Each clip's file joined into it, in order.
    pub parts: Vec<ExportPart>,
    /// Once written: its length and frames (each its parts' added up).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames: Option<u64>,
}

/// One clip's file in a joined file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ExportPart {
    pub clip: String,
    /// The take's file joined (in the edited file, the clip's angle's).
    pub path: PathBuf,
    /// Where it starts in the joined file and how long it is there, once
    /// written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames: Option<u64>,
    /// Frames of the take's file that start past the clip's end, left out
    /// (a file running longer than the program; normally none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames_left_out: Option<u64>,
    /// Frames decoded and encoded again (the partial keyframe intervals at
    /// a cut, or the whole part when its file's format differs from the
    /// joined file's); the rest were copied as recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reencoded_frames: Option<u64>,
}

/// An export's manifest, `<name>.export.json` next to its files.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ExportManifest {
    pub version: u32,
    pub export: String,
    pub timeline: String,
    pub name: String,
    pub status: ExportStatus,
    /// When it started, local time, RFC 3339.
    pub started_at: String,
    /// The clips, in order, with their takes.
    pub clips: Vec<ExportClip>,
    pub files: Vec<ExportFile>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<ExportSkipped>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ExportClip {
    pub id: String,
    pub take: PathBuf,
    pub in_seconds: f64,
    pub out_seconds: f64,
    /// The source the edited file shows for it, if not the program.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub angle: Option<String>,
}

/// The source label of an export's edited file: the program's sound with
/// each clip's angle (see [`Clip::angle`]) as its picture.
pub const EDITED: &str = "edited";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExportStatus {
    /// Being written, or isocastd stopped before it finished.
    Exporting,
    Complete,
    Failed,
}

/// What a change to a timeline was, in `timeline_changed` events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TimelineAction {
    Created,
    Opened,
    Closed,
    /// A finished take was added to the end.
    Appended,
    Removed,
    Moved,
    /// The last clip was taken off to be recorded again.
    Retaken,
    /// The take being recorded was dropped by `retake`.
    Dropped,
    /// A take being recorded for it never finished (the recording failed).
    Interrupted,
    /// A clip's in or out point moved (`timeline_trim`).
    Trimmed,
    /// A clip was cut in two (`timeline_split`).
    Split,
    /// A span was taken out (`timeline_delete_range`).
    Deleted,
    /// The edited video's source changed over a span (`timeline_angle`).
    Angle,
    /// An edit was taken back (`timeline_undo`) or put back (`timeline_redo`).
    Undone,
    Redone,
}

/// A take's length on the timeline, as whole frames at a frame rate: its
/// program file's (else its first file's), so the clip ends on the
/// program's frame grid. A take stopped between two frames has files that
/// each end on their own grid (a 30 fps file can run a little past a 60 fps
/// program); exports cut or hold every file to this length, so all of them
/// join at the same moments. `None` if no file finished.
pub fn take_length(manifest: &SessionManifest) -> Option<(u64, u32)> {
    let finished = |file: &&ManifestFile| file.duration_seconds.is_some() && file.fps > 0;
    let file = manifest
        .files
        .iter()
        .filter(finished)
        .find(|file| file.source == crate::PROGRAM)
        .or_else(|| manifest.files.iter().find(finished))?;
    let seconds = file.duration_seconds?;
    Some((
        (seconds * f64::from(file.fps)).round().max(0.0) as u64,
        file.fps,
    ))
}

/// [`take_length`] in seconds, or the take's own duration if no file
/// finished.
pub fn take_seconds(manifest: &SessionManifest) -> f64 {
    take_length(manifest).map_or_else(
        || manifest.duration_seconds.unwrap_or(0.0),
        |(frames, fps)| frames as f64 / f64::from(fps),
    )
}

/// The clips' combined length (0, not -0, for none).
pub fn duration(clips: &[Clip]) -> f64 {
    clips
        .iter()
        .map(Clip::duration_seconds)
        .fold(0.0, |a, b| a + b)
}

impl Clip {
    pub fn duration_seconds(&self) -> f64 {
        (self.out_seconds - self.in_seconds).max(0.0)
    }
}
