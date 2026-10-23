//! Project files for other editors (#104): a take as a project Final Cut
//! Pro, Premiere Pro or DaVinci Resolve opens (`take_export_nle`),
//! pointing at the take's own program and ISO files.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::manifest::{ManifestFile, SessionManifest};

/// A project format other editors open (#104), for
/// `take_export_nle`.
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
/// `take_export_nle`); FCPXML and OpenTimelineIO are the same for all.
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

/// Parameters of `take_export_nle`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TakeExportNleParams {
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
    /// The take: its manifest (`<date> <time>.session.json`, as `start`,
    /// `stop` and `take_finished` give it), or a project folder holding
    /// exactly one take; an absolute path.
    pub take: PathBuf,
    /// Write it in this folder (made if missing), named after the take and
    /// numbered so nothing is overwritten; else the take's own folder. Not
    /// with `path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<PathBuf>,
    /// Write exactly this file (an absolute path ending in the format's
    /// extension: .fcpxml, .xml or .otio). A file already there is replaced
    /// only if it is an earlier Isocast export, or with `replace`.
    /// Not with `folder`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// With `path`: replace whatever file is there.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub replace: bool,
}

/// What `take_export_nle` wrote.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NleExported {
    /// The take's manifest.
    pub take: PathBuf,
    pub format: NleFormat,
    pub app: NleApp,
    /// The project file. It points at the take's own program and ISO files,
    /// which are never changed or copied.
    pub path: PathBuf,
    /// The sequence: its length, frame rate (frames per second, as a
    /// fraction for rates like 59.94: `60000/1001`) and picture size.
    pub duration_seconds: f64,
    pub frame_rate: String,
    pub width: u32,
    pub height: u32,
    /// The video tracks from the bottom up: one per ISO source (off), then
    /// the program on top (on), all lined up on the take's clock.
    pub video_tracks: Vec<NleTrack>,
    /// The audio tracks: the program's (on), then each ISO file's (off).
    pub audio_tracks: Vec<NleTrack>,
    /// Media files it points at, and markers.
    pub media: usize,
    pub markers: usize,
    /// What the editor will show differently from the take, and why.
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

/// A take's length, as whole frames at a frame rate: its program file's
/// (else its first file's), so the sequence ends on the program's frame
/// grid. A take stopped between two frames has files that each end on their
/// own grid (a 30 fps file can run a little past a 60 fps program). `None`
/// if no file finished.
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
