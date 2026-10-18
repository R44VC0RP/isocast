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
//! never touch a take's files. `timeline_crop` (#87) sets the part of the
//! picture the edited video shows over a span (a clip's `crop`, a
//! rectangle in fractions of its angle's picture), and `timeline_roll`
//! moves the cut between two clips without changing the timeline's length.

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
    /// Unique across Macs (random), so two Macs' timelines with the same
    /// id (`untitled`) are never taken for each other when a project file
    /// is opened (#88). Absent in files from before it; given on load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
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

/// A new [`Timeline::uid`]: 16 random hex digits (the time and process
/// if the system's random source can't be read).
pub fn new_uid() -> String {
    #[cfg(unix)]
    use std::io::Read;
    let mut bytes = [0u8; 8];
    #[cfg(unix)]
    let random = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_ok();
    #[cfg(not(unix))]
    let random = getrandom::fill(&mut bytes).is_ok();
    if !random {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        bytes = (nanos ^ (u64::from(std::process::id()) << 40)).to_be_bytes();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether `id` can be a timeline's id (and so a file name): 1 to 120
/// lowercase letters, digits and dashes, not starting with a dash.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 120
        && !id.starts_with('-')
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
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
    /// The part of its angle's picture the edited video shows, scaled to
    /// fit the program's size; the whole picture if absent. Set with
    /// `timeline_crop`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crop: Option<Crop>,
    /// Fields of the clip this version doesn't know (a newer Isocast's),
    /// kept as they are so a project file read and written again loses
    /// none of its edits (#88).
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// A crop: the part of a picture shown, as fractions of the picture's
/// width and height from its top-left corner (0 to 1). Kept on the
/// picture's 2-pixel grid, at least 32 pixels each way.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Crop {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Crop {
    /// The whole picture.
    pub const FULL: Crop = Crop {
        x: 0.0,
        y: 0.0,
        width: 1.0,
        height: 1.0,
    };

    /// It covers the whole picture (no crop).
    pub fn is_full(&self) -> bool {
        self.x <= 1e-6 && self.y <= 1e-6 && self.width >= 1.0 - 1e-6 && self.height >= 1.0 - 1e-6
    }
}

/// A crop's shape: the largest rectangle of that shape (in pixels) that
/// fits, centred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum CropPreset {
    /// Landscape, as most programs are.
    #[serde(rename = "16:9")]
    Wide,
    /// Vertical video (Shorts, Reels, TikTok).
    #[serde(rename = "9:16")]
    Vertical,
    /// Square.
    #[serde(rename = "1:1")]
    Square,
    /// Portrait feed posts.
    #[serde(rename = "4:5")]
    Portrait,
}

impl CropPreset {
    pub const ALL: [CropPreset; 4] = [Self::Wide, Self::Vertical, Self::Square, Self::Portrait];

    /// Width over height.
    pub fn aspect(self) -> f64 {
        match self {
            Self::Wide => 16.0 / 9.0,
            Self::Vertical => 9.0 / 16.0,
            Self::Square => 1.0,
            Self::Portrait => 4.0 / 5.0,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Wide => "16:9",
            Self::Vertical => "9:16",
            Self::Square => "1:1",
            Self::Portrait => "4:5",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.name() == text.trim())
    }
}

/// The largest rectangle of `aspect` (width over height, in pixels) that
/// fits inside `within` on a `width`×`height` picture, centred in it.
pub fn fit_aspect(within: Crop, aspect: f64, (width, height): (u32, u32)) -> Crop {
    let (w, h) = (f64::from(width.max(1)), f64::from(height.max(1)));
    let (room_w, room_h) = (within.width * w, within.height * h);
    let (out_w, out_h) = match room_w / room_h > aspect {
        true => (room_h * aspect, room_h),
        false => (room_w, room_w / aspect),
    };
    Crop {
        x: within.x + (within.width - out_w / w) / 2.0,
        y: within.y + (within.height - out_h / h) / 2.0,
        width: out_w / w,
        height: out_h / h,
    }
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

/// Parameters of `timeline_relink`: where an offline clip's take is now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineRelinkParams {
    /// The clip's id, like `c2`.
    pub clip: String,
    /// The take's manifest (`….session.json`), or the folder holding a
    /// manifest of the same name. Given a folder, every take of every
    /// timeline that was in the clip's old folder is looked for there too.
    pub path: PathBuf,
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

/// Parameters of `timeline_crop`: what part of the picture the edited
/// video shows over a span.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineCropParams {
    /// Where the span starts and ends, in seconds on the timeline, each
    /// rounded to the frame grid of the clip there.
    pub start_seconds: f64,
    pub end_seconds: f64,
    /// The part of each clip's picture to show (fractions of its width and
    /// height, from the top-left); the whole picture if absent and a
    /// `preset` is given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crop: Option<Crop>,
    /// Make it this shape: the largest rectangle of it inside `crop` (or
    /// the picture), centred: `16:9`, `9:16`, `1:1` or `4:5`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<CropPreset>,
    /// Show the whole picture again over the span (no `crop` or `preset`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear: bool,
    /// The timeline's id; the current one if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeline: Option<String>,
}

/// Parameters of `timeline_roll`: move the cut between a clip and the next
/// (the clip's out point and the next one's in point together), so the
/// timeline keeps its length.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineRollParams {
    /// The clip before the cut, like `c2`.
    pub clip: String,
    /// How far to move the cut, in seconds (negative: earlier), rounded to
    /// the clips' frame grid. Both clips must keep a frame and stay within
    /// their takes.
    pub by_seconds: f64,
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
    /// Also write a vertical (9:16) edited file, `<name> VERTICAL.<ext>`:
    /// each clip's angle (or its crop) fitted into the program's size
    /// turned on its side (1920x1080 → 1080x1920), so a 9:16 crop fills
    /// the frame; the program's sound. Every frame is encoded again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub vertical: bool,
}

/// A project format other editors open (#104), for
/// `timeline_export_nle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NleFormat {
    /// FCPXML 1.10 (`.fcpxml`): Final Cut Pro 10.6 or later. (Resolve
    /// opens it but plays each clip's sound with its picture: use xmeml.)
    Fcpxml,
    /// Final Cut Pro 7 XML, xmeml version 5 (`.xml`): Premiere Pro, and
    /// DaVinci Resolve (with app `resolve`).
    Xmeml,
    /// OpenTimelineIO (`.otio`): DaVinci Resolve 18.5 or later, and
    /// anything with OpenTimelineIO's adapters.
    Otio,
}

impl NleFormat {
    pub const ALL: [NleFormat; 3] = [Self::Fcpxml, Self::Xmeml, Self::Otio];

    pub fn name(self) -> &'static str {
        match self {
            Self::Fcpxml => "fcpxml",
            Self::Xmeml => "xmeml",
            Self::Otio => "otio",
        }
    }

    /// The file's extension.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Fcpxml => "fcpxml",
            Self::Xmeml => "xml",
            Self::Otio => "otio",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.name() == text.trim())
    }
}

/// The editor a project file is for. In xmeml, Premiere and Resolve count
/// a picture's scale differently and read audio tracks differently (see
/// `timeline_export_nle`); FCPXML and OpenTimelineIO are the same for all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NleApp {
    /// Final Cut Pro (FCPXML's own units).
    FinalCut,
    /// Premiere Pro (xmeml's own units).
    Premiere,
    /// DaVinci Resolve.
    Resolve,
}

impl NleApp {
    pub const ALL: [NleApp; 3] = [Self::FinalCut, Self::Premiere, Self::Resolve];

    pub fn name(self) -> &'static str {
        match self {
            Self::FinalCut => "final_cut",
            Self::Premiere => "premiere",
            Self::Resolve => "resolve",
        }
    }

    /// As people call it, for file names and messages.
    pub fn title(self) -> &'static str {
        match self {
            Self::FinalCut => "Final Cut Pro",
            Self::Premiere => "Premiere Pro",
            Self::Resolve => "DaVinci Resolve",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.name() == text.trim())
    }
}

/// Parameters of `timeline_export_nle`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineExportNleParams {
    /// `fcpxml` (Final Cut Pro), `xmeml` (Premiere Pro, or Resolve with app
    /// `resolve`) or `otio` (OpenTimelineIO).
    pub format: NleFormat,
    /// The editor it is for: `final_cut`, `premiere` or `resolve`. With
    /// xmeml, `resolve` writes the scale of a picture of another size than
    /// the sequence from Resolve's own fit, and each audio channel on its
    /// own track (Resolve reads xmeml audio a channel at a time). Absent:
    /// `final_cut` for fcpxml, `premiere` for xmeml, `resolve` for otio (the
    /// same file for any editor). Resolve's best format
    /// is xmeml: it ignores FCPXML's split of picture and sound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<NleApp>,
    /// The timeline's id; the current one if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeline: Option<String>,
    /// Write it in this folder (made if missing), named after the timeline
    /// and numbered so nothing is overwritten; else the first clip's take's
    /// folder. Not with `path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<PathBuf>,
    /// Write exactly this file (an absolute path ending in the format's
    /// extension: .fcpxml, .xml or .otio). A file already there is replaced
    /// only if it is an earlier Isocast timeline export, or with `replace`.
    /// Not with `folder`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// With `path`: replace whatever file is there.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub replace: bool,
}

/// What `timeline_export_nle` wrote.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NleExported {
    pub timeline: String,
    pub format: NleFormat,
    pub app: NleApp,
    /// The project file. It points at the takes' own program and ISO files,
    /// which are never changed or copied.
    pub path: PathBuf,
    /// The sequence: its length, frame rate (frames per second, as a
    /// fraction for rates like 59.94: `60000/1001`) and picture size.
    pub duration_seconds: f64,
    pub frame_rate: String,
    pub width: u32,
    pub height: u32,
    /// The video tracks from the bottom up: one per ISO source, then the
    /// program on top. At each clip only its angle's track is on.
    pub video_tracks: Vec<NleTrack>,
    /// The audio tracks: the program's (on), then each ISO file's (off, as
    /// Isocast's edited file has the program's sound).
    pub audio_tracks: Vec<NleTrack>,
    /// Media files it points at, and markers.
    pub media: usize,
    pub markers: usize,
    /// What isn't as in Isocast's own export, and why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// One track of a project file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NleTrack {
    pub name: String,
    /// The video source whose files it holds (`program` for the program).
    pub source: String,
    /// An audio track: which audio source of those files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<String>,
    /// Clips on it, and how many of them are on (shown or heard).
    pub clips: usize,
    pub enabled: usize,
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
    /// See [`Clip::crop`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crop: Option<Crop>,
    /// Markers added while the take was recorded, on its own clock (those
    /// inside the clip show at `start_seconds + time - in_seconds`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub markers: Vec<crate::manifest::Marker>,
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

/// Where the next recording goes (#114): a new project with its own
/// timeline (the default), or another take on a timeline the user chose.
/// `status` (`record_target`) and `record_target` give it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RecordTarget {
    /// The timeline the next recording's takes are added to; absent: each
    /// recording starts a new project folder with a timeline of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeline: Option<String>,
    /// That timeline's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The take the next recording becomes on it (its clips, plus one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub take: Option<usize>,
    /// The project folder its take is written into: the folder of the
    /// timeline's last take (absent: a new project folder).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<PathBuf>,
    /// Why the choice is what it is (a timeline that was current before
    /// 0.0.8, or a chosen timeline that is gone).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// The timeline that was current before 0.0.8 and took every
    /// recording, until a choice is made: no longer used unless chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migrated: Option<String>,
}

/// Parameters of `record_target`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecordTargetParams {
    /// `new` (each recording starts a new project with its own timeline)
    /// or a timeline's id (`timeline_list` has them): its next take. Absent:
    /// the choice is only read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

/// The timeline a recording's takes go to (#114), in `status`
/// (`recording_into`) while it records and in its `stopped` event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RecordingInto {
    pub timeline: String,
    pub name: String,
    /// The recording started a new project, and this is its timeline.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub new_project: bool,
    /// The recording's takes added to the timeline so far.
    pub takes: usize,
    /// The timeline's clips now (its last `takes` are this recording's).
    pub clips: usize,
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
    /// The part of its picture the edited file shows, if not the whole.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crop: Option<Crop>,
}

/// The source label of an export's edited file: the program's sound with
/// each clip's angle (see [`Clip::angle`]) as its picture.
pub const EDITED: &str = "edited";

/// The source label of an export's vertical (9:16) edited file
/// ([`TimelineExportParams::vertical`]).
pub const VERTICAL: &str = "vertical";

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
    /// The edited video's crop changed over a span (`timeline_crop`).
    Cropped,
    /// The cut between two clips moved (`timeline_roll`).
    Rolled,
    /// An offline clip was pointed at where its take is now
    /// (`timeline_relink`), or a project opened from elsewhere was taken
    /// in (`project_open`).
    Relinked,
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
