//! `isocast timeline …` and `isocast retake`: timelines (C.1), takes in
//! order to join into one file. Each command is one call on the method
//! table (`timeline_*`, `retake`); `export` also watches the export's
//! events until it finishes.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, bail};
use clap::Subcommand;
use isocast_protocol::api::{Event, EventKind, Method, WatchParams};
use isocast_protocol::timeline::TimelineRelinkParams;
use isocast_protocol::timeline::{
    Crop, CropPreset, TimelineAngleParams, TimelineCropParams, TimelineRangeParams,
    TimelineRollParams, TimelineSplitParams, TimelineTrimParams,
};
use isocast_protocol::timeline::{
    ExportFile, ExportStarted, RemovedReason, RemovedTake, RetakeParams, Retaken, TimelineClosed,
    TimelineExportParams, TimelineInfo, TimelineList, TimelineMoveParams, TimelineNewParams,
    TimelineOpenParams, TimelineRemoveParams, TimelineTarget,
};
use isocast_protocol::timeline::{NleApp, NleExported, NleFormat, TimelineExportNleParams};
use isocast_protocol::timeline::{RecordTarget, RecordTargetParams};
use serde::Serialize;

use crate::client::Client;

#[derive(Subcommand)]
pub enum TimelineCommand {
    /// Make a timeline, make it current and choose it for the next
    /// recording: from then on every recording adds its takes to its end
    /// (`isocast record-target new` starts new projects again).
    New {
        /// Its name ("Timeline N" if absent); its id is made from it.
        name: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// List the timelines, and which is current.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Make a timeline current (by id): the one the editor shows and
    /// timeline commands act on. Recordings go where `isocast
    /// record-target` says.
    Open {
        timeline: String,
        #[arg(long)]
        json: bool,
    },
    /// Make no timeline current. If it was chosen for the next recording,
    /// recordings start new projects again.
    Close {
        #[arg(long)]
        json: bool,
    },
    /// Show a timeline (the current one unless an id is given): its clips
    /// in order, each with its take and files, and the takes taken off it.
    Show {
        timeline: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Take a clip off the timeline (by id, like c2). Its take's files stay
    /// where they are.
    Remove {
        clip: String,
        /// The timeline's id; the current one if absent.
        #[arg(long)]
        timeline: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Move a clip (by id) to POSITION, counting from 1 (the first).
    Move {
        clip: String,
        position: usize,
        /// The timeline's id; the current one if absent.
        #[arg(long)]
        timeline: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Join the clips, in order, into one program file and one file per ISO
    /// source every clip has, each cut to the clips' in and out points (the
    /// video as recorded except the frames from a cut to the next
    /// keyframe), an EDITED file when clips show other angles, and an
    /// .export.json manifest; waits for it unless --background.
    Export {
        /// The timeline's id; the current one if absent.
        #[arg(long)]
        timeline: Option<String>,
        /// Write the files here (made if missing); else next to the first
        /// clip's take.
        #[arg(long)]
        folder: Option<PathBuf>,
        /// Also write a vertical (9:16) edited file: each clip's picture or
        /// crop fitted into the program's size on its side (1080x1920 for
        /// 1080p), so a 9:16 crop fills it.
        #[arg(long)]
        vertical: bool,
        /// Return once it has started; `isocast watch --kinds
        /// export_progress,export_finished` follows it.
        #[arg(long)]
        background: bool,
        /// Instead, write a project another editor opens, pointing at the
        /// takes' own files (nothing is encoded or copied): fcpxml (Final
        /// Cut Pro), xmeml (Premiere Pro; DaVinci Resolve with --app
        /// resolve) or otio (OpenTimelineIO). Every ISO source on its own
        /// track with the program on top, each clip's angle the one on.
        #[arg(long, value_name = "FORMAT", value_parser = ["fcpxml", "xmeml", "otio"],
              conflicts_with_all = ["vertical", "background"])]
        nle: Option<String>,
        /// With --nle: the editor it is for: final_cut (fcpxml's default),
        /// premiere (xmeml's) or resolve (with xmeml: Resolve's picture
        /// scale and a track per audio channel).
        #[arg(long, value_name = "APP", value_parser = ["final_cut", "premiere", "resolve"],
              requires = "nle")]
        app: Option<String>,
        /// With --nle: write exactly this file (ending in .fcpxml, .xml or
        /// .otio); one already there is replaced only if it is an earlier
        /// Isocast timeline export, or with --replace.
        #[arg(long, value_name = "PATH", requires = "nle", conflicts_with = "folder")]
        output: Option<PathBuf>,
        /// With --output: replace whatever file is there.
        #[arg(long, requires = "output")]
        replace: bool,
        #[arg(long)]
        json: bool,
    },
    /// Move a clip's in and/or out point, in seconds on its take's own
    /// clock (rounded to its frames). The take's files are untouched.
    Trim {
        clip: String,
        /// The new in point (0 is the take's start).
        #[arg(long = "in", value_name = "SECONDS")]
        in_seconds: Option<f64>,
        /// The new out point (up to the take's length).
        #[arg(long = "out", value_name = "SECONDS")]
        out_seconds: Option<f64>,
        #[arg(long)]
        timeline: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Cut the clip at SECONDS on the timeline in two.
    Split {
        #[arg(value_name = "SECONDS")]
        at: f64,
        #[arg(long)]
        timeline: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Take START to END (seconds on the timeline) out, closing the gap.
    Delete {
        start: f64,
        end: f64,
        #[arg(long)]
        timeline: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Show SOURCE (`program` or an ISO source's id) in the edited video
    /// from START to END (seconds on the timeline).
    Angle {
        source: String,
        start: f64,
        end: f64,
        #[arg(long)]
        timeline: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Crop the edited video from START to END (seconds on the timeline):
    /// show part of each clip's picture, scaled to the program's size.
    /// Give --rect, --preset (or both: that shape inside the rectangle),
    /// or --clear.
    Crop {
        start: f64,
        end: f64,
        /// The part shown, as fractions of the picture: X,Y,WIDTH,HEIGHT
        /// from the top-left (0 to 1), like 0.25,0,0.5,1.
        #[arg(long, value_name = "X,Y,W,H", value_parser = parse_rect)]
        rect: Option<Crop>,
        /// A shape: the largest one that fits, centred: 16:9, 9:16, 1:1 or
        /// 4:5.
        #[arg(long, value_parser = parse_preset)]
        preset: Option<CropPreset>,
        /// Show the whole picture again.
        #[arg(long, conflicts_with_all = ["rect", "preset"])]
        clear: bool,
        #[arg(long)]
        timeline: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Move the cut after CLIP by SECONDS (negative: earlier), trimming
    /// CLIP's end and the next clip's start together (a roll edit): the
    /// timeline keeps its length.
    Roll {
        clip: String,
        #[arg(value_name = "SECONDS", allow_hyphen_values = true)]
        by: f64,
        #[arg(long)]
        timeline: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Take back the last edit (trim, split, delete, angle, crop, roll,
    /// remove, move).
    Undo {
        #[arg(long)]
        timeline: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Put back the last edit undone.
    Redo {
        #[arg(long)]
        timeline: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Point an offline clip at where its take is now: its manifest
    /// (`….session.json`), or the folder holding it (then every take that
    /// was in the clip's old folder is looked for there too). The take's
    /// files are never touched.
    Relink {
        /// The clip, like `c2`.
        clip: String,
        /// The take's manifest, or its folder.
        path: PathBuf,
        #[arg(long)]
        timeline: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

fn parse_rect(text: &str) -> Result<Crop, String> {
    let values: Vec<f64> = text
        .split(',')
        .map(|v| v.trim().parse::<f64>())
        .collect::<Result<_, _>>()
        .map_err(|_| format!("{text:?}: give four numbers X,Y,WIDTH,HEIGHT, like 0.25,0,0.5,1"))?;
    match values[..] {
        [x, y, width, height] => Ok(Crop {
            x,
            y,
            width,
            height,
        }),
        _ => Err(format!(
            "{text:?}: give four numbers X,Y,WIDTH,HEIGHT, like 0.25,0,0.5,1"
        )),
    }
}

fn parse_preset(text: &str) -> Result<CropPreset, String> {
    CropPreset::parse(text).ok_or_else(|| {
        let names: Vec<&str> = CropPreset::ALL.iter().map(|p| p.name()).collect();
        format!("{text:?}: choose one of {}", names.join(", "))
    })
}

/// Print an edit's result: the timeline, as JSON or text.
fn edited(info: &TimelineInfo, json: bool) -> Result<()> {
    if json {
        return print(info);
    }
    print_notes(&info.notes);
    print_timeline(info);
    Ok(())
}

fn print<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

pub fn run(command: TimelineCommand) -> Result<ExitCode> {
    let mut client = Client::connect_or_launch()?;
    match command {
        TimelineCommand::New { name, json } => {
            let info: TimelineInfo =
                client.call(Method::TimelineNew(Some(TimelineNewParams { name })))?;
            if json {
                print(&info)?;
            } else {
                println!(
                    "Made timeline {:?} ({}); it is current, and the next recording adds its \
                     takes to it.",
                    info.name, info.id
                );
                print_notes(&info.notes);
            }
        }
        TimelineCommand::List { json } => {
            let list: TimelineList = client.call(Method::TimelineList)?;
            if json {
                print(&list)?;
            } else {
                print_list(&list);
            }
        }
        TimelineCommand::Open { timeline, json } => {
            let info: TimelineInfo =
                client.call(Method::TimelineOpen(TimelineOpenParams { timeline }))?;
            if json {
                print(&info)?;
            } else {
                println!("Timeline {:?} ({}) is current.", info.name, info.id);
                print_timeline(&info);
            }
        }
        TimelineCommand::Close { json } => {
            let closed: TimelineClosed = client.call(Method::TimelineClose)?;
            if json {
                print(&closed)?;
            } else {
                match closed.closed {
                    Some(id) => println!(
                        "Closed timeline {id} (`isocast timeline open {id}` shows it again)."
                    ),
                    None => println!("No timeline was current."),
                }
            }
        }
        TimelineCommand::Show { timeline, json } => {
            let info: TimelineInfo =
                client.call(Method::TimelineGet(Some(TimelineTarget { timeline })))?;
            if json {
                print(&info)?;
            } else {
                print_timeline(&info);
            }
        }
        TimelineCommand::Remove {
            clip,
            timeline,
            json,
        } => {
            let info: TimelineInfo = client.call(Method::TimelineRemove(TimelineRemoveParams {
                clip,
                timeline,
            }))?;
            if json {
                print(&info)?;
            } else {
                print_notes(&info.notes);
                print_timeline(&info);
            }
        }
        TimelineCommand::Move {
            clip,
            position,
            timeline,
            json,
        } => {
            // Positions count from 1 here; the method's index from 0.
            let current: TimelineInfo = client.call(Method::TimelineGet(Some(TimelineTarget {
                timeline: timeline.clone(),
            })))?;
            let count = current.clips.len();
            if position == 0 || position > count {
                bail!(
                    "position: {position} is not a place on timeline {:?}, which has {count} \
                     clip{}; use 1 (the first) to {count}",
                    current.id,
                    if count == 1 { "" } else { "s" }
                );
            }
            let info: TimelineInfo = client.call(Method::TimelineMove(TimelineMoveParams {
                clip,
                index: position - 1,
                timeline,
            }))?;
            if json {
                print(&info)?;
            } else {
                print_timeline(&info);
            }
        }
        TimelineCommand::Export {
            timeline,
            folder,
            nle: Some(format),
            app,
            output,
            replace,
            json,
            ..
        } => {
            let params = TimelineExportNleParams {
                format: NleFormat::parse(&format).expect("clap checked it"),
                app: app.as_deref().and_then(NleApp::parse),
                timeline,
                folder: folder.map(std::path::absolute).transpose()?,
                path: output.map(std::path::absolute).transpose()?,
                replace,
            };
            let exported: NleExported = client.call(Method::TimelineExportNle(params))?;
            if json {
                print(&exported)?;
            } else {
                print_nle(&exported);
            }
        }
        TimelineCommand::Export {
            timeline,
            folder,
            vertical,
            background,
            json,
            ..
        } => return export(client, timeline, folder, vertical, background, json),
        TimelineCommand::Trim {
            clip,
            in_seconds,
            out_seconds,
            timeline,
            json,
        } => {
            let info: TimelineInfo = client.call(Method::TimelineTrim(TimelineTrimParams {
                clip,
                in_seconds,
                out_seconds,
                timeline,
            }))?;
            edited(&info, json)?;
        }
        TimelineCommand::Split { at, timeline, json } => {
            let info: TimelineInfo = client.call(Method::TimelineSplit(TimelineSplitParams {
                at_seconds: at,
                timeline,
            }))?;
            edited(&info, json)?;
        }
        TimelineCommand::Delete {
            start,
            end,
            timeline,
            json,
        } => {
            let info: TimelineInfo =
                client.call(Method::TimelineDeleteRange(TimelineRangeParams {
                    start_seconds: start,
                    end_seconds: end,
                    timeline,
                }))?;
            edited(&info, json)?;
        }
        TimelineCommand::Angle {
            source,
            start,
            end,
            timeline,
            json,
        } => {
            let info: TimelineInfo = client.call(Method::TimelineAngle(TimelineAngleParams {
                start_seconds: start,
                end_seconds: end,
                source,
                timeline,
            }))?;
            edited(&info, json)?;
        }
        TimelineCommand::Crop {
            start,
            end,
            rect,
            preset,
            clear,
            timeline,
            json,
        } => {
            let info: TimelineInfo = client.call(Method::TimelineCrop(TimelineCropParams {
                start_seconds: start,
                end_seconds: end,
                crop: rect,
                preset,
                clear,
                timeline,
            }))?;
            edited(&info, json)?;
        }
        TimelineCommand::Roll {
            clip,
            by,
            timeline,
            json,
        } => {
            let info: TimelineInfo = client.call(Method::TimelineRoll(TimelineRollParams {
                clip,
                by_seconds: by,
                timeline,
            }))?;
            edited(&info, json)?;
        }
        TimelineCommand::Undo { timeline, json } => {
            let info: TimelineInfo =
                client.call(Method::TimelineUndo(Some(TimelineTarget { timeline })))?;
            edited(&info, json)?;
        }
        TimelineCommand::Redo { timeline, json } => {
            let info: TimelineInfo =
                client.call(Method::TimelineRedo(Some(TimelineTarget { timeline })))?;
            edited(&info, json)?;
        }
        TimelineCommand::Relink {
            clip,
            path,
            timeline,
            json,
        } => {
            let info: TimelineInfo = client.call(Method::TimelineRelink(TimelineRelinkParams {
                clip,
                path: absolute(path)?,
                timeline,
            }))?;
            edited(&info, json)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `path` made absolute against the working folder (the engine has its
/// own).
pub(crate) fn absolute(path: PathBuf) -> Result<PathBuf> {
    Ok(match path.is_relative() {
        true => std::env::current_dir()?.join(path),
        false => path,
    })
}

fn print_nle(exported: &NleExported) {
    println!(
        "Wrote {} for {} ({}): timeline {}, {:.3} s at {} fps, {}x{}, {} media file{}",
        exported.path.display(),
        exported.app.title(),
        exported.format.name(),
        exported.timeline,
        exported.duration_seconds,
        exported.frame_rate,
        exported.width,
        exported.height,
        exported.media,
        if exported.media == 1 { "" } else { "s" }
    );
    for (kind, tracks) in [("V", &exported.video_tracks), ("A", &exported.audio_tracks)] {
        for (n, track) in tracks.iter().enumerate() {
            println!(
                "  {kind}{}  {}: {} clip{}, {} on",
                n + 1,
                track.name,
                track.clips,
                if track.clips == 1 { "" } else { "s" },
                track.enabled
            );
        }
    }
    if exported.markers > 0 {
        println!("  {} marker(s)", exported.markers);
    }
    print_notes(&exported.notes);
}

fn print_notes(notes: &[String]) {
    for note in notes {
        println!("note: {note}");
    }
}

fn print_list(list: &TimelineList) {
    if list.timelines.is_empty() {
        println!("No timelines yet; `isocast timeline new NAME` makes one.");
    }
    for timeline in &list.timelines {
        println!(
            "{} {:<20} {:?}, {} clip{}, {:.2} s",
            if timeline.current { "*" } else { " " },
            timeline.id,
            timeline.name,
            timeline.clips,
            if timeline.clips == 1 { "" } else { "s" },
            timeline.duration_seconds
        );
    }
    match &list.current {
        Some(id) => println!("Current: {id} (finished takes are added to it)."),
        None => println!("No timeline is current: takes are only recorded."),
    }
    for problem in &list.problems {
        println!("warning: {problem}");
    }
    println!("Kept in {}", list.folder.display());
}

pub(crate) fn print_timeline(info: &TimelineInfo) {
    println!(
        "Timeline {:?} ({}){}: {} clip{}, {:.2} s",
        info.name,
        info.id,
        if info.current { ", current" } else { "" },
        info.clips.len(),
        if info.clips.len() == 1 { "" } else { "s" },
        info.duration_seconds
    );
    for clip in &info.clips {
        let take = clip
            .take
            .file_name()
            .map(|name| name.to_string_lossy().replace(".session.json", ""))
            .unwrap_or_default();
        let status = clip
            .status
            .map(|status| format!(", {}", serde_json::to_value(status).unwrap_or_default()))
            .unwrap_or_default()
            .replace('"', "");
        let dropped = match clip.dropped_frames {
            0 => String::new(),
            1 => ", 1 dropped frame".into(),
            n => format!(", {n} dropped frames"),
        };
        let angle = clip
            .angle
            .as_ref()
            .map(|angle| format!(", angle {angle}"))
            .unwrap_or_default();
        let angle = match clip.crop {
            Some(c) => format!(
                "{angle}, crop {:.3},{:.3} {:.3}×{:.3}",
                c.x, c.y, c.width, c.height
            ),
            None => angle,
        };
        println!(
            "  {:>2}. {:<4} at {:>8.2} s  {:>7.2} s  take \"{take}\" {:.2}–{:.2} s{angle}{status}{dropped}",
            clip.index + 1,
            clip.id,
            clip.start_seconds,
            clip.duration_seconds,
            clip.in_seconds,
            clip.out_seconds
        );
        if let Some(problem) = &clip.problem {
            println!("        problem: {problem}");
        }
        for file in &clip.files {
            let dropped = match (file.dropped_frames, file.audio_dropouts) {
                (0, 0) => String::new(),
                (frames, gaps) => format!("  ({frames} dropped frames, {gaps} audio gaps)"),
            };
            println!(
                "        {:<8} {}{}{dropped}",
                file.source,
                file.path.display(),
                if file.exists { "" } else { "  (missing)" }
            );
        }
    }
    if !info.removed.is_empty() {
        println!("Taken off (files kept where they are):");
        for removed in &info.removed {
            print_removed(removed);
        }
    }
}

fn print_removed(removed: &RemovedTake) {
    let what = match removed.reason {
        RemovedReason::Removed => "removed",
        RemovedReason::Retaken => "retaken",
        RemovedReason::Dropped => "dropped while recording",
        RemovedReason::Interrupted => {
            "interrupted (never finished; its files play up to about 2 s before isocastd stopped)"
        }
    };
    println!(
        "  {} {what} {}: {}",
        removed.clip.as_deref().unwrap_or("take"),
        removed.at,
        removed.take.display()
    );
}

fn export(
    client: Client,
    timeline: Option<String>,
    folder: Option<PathBuf>,
    vertical: bool,
    background: bool,
    json: bool,
) -> Result<ExitCode> {
    // The daemon may run in another folder; give it an absolute one.
    let folder = folder.map(std::path::absolute).transpose()?;
    // Watching first, so no event of this export is missed.
    let events = match background {
        true => None,
        false => {
            let watcher = Client::connect_or_launch()?;
            let (_, events) = watcher.watch_some(Some(WatchParams {
                kinds: Some(vec![EventKind::ExportProgress, EventKind::ExportFinished]),
                except: Vec::new(),
            }))?;
            Some(events)
        }
    };
    let mut client = client;
    let started: ExportStarted =
        client.call(Method::TimelineExport(Some(TimelineExportParams {
            timeline,
            folder,
            vertical,
        })))?;
    if !json {
        println!(
            "Exporting timeline {} ({:.2} s) to {}:",
            started.timeline,
            started.duration_seconds,
            started.folder.display()
        );
        for file in &started.files {
            println!("  {}", file.path.display());
        }
        for skipped in &started.skipped {
            println!("  not joined: {}: {}", skipped.source, skipped.reason);
        }
    }
    let Some(events) = events else {
        if json {
            print(&started)?;
        } else {
            println!(
                "It runs in the background; its manifest is {}.",
                started.manifest.display()
            );
        }
        return Ok(ExitCode::SUCCESS);
    };
    let mut shown = -1i64;
    for event in events {
        match event? {
            Event::ExportProgress {
                export, progress, ..
            } if export == started.export => {
                let percent = (progress * 100.0).floor() as i64;
                if !json && percent >= shown + 10 {
                    shown = percent - percent % 10;
                    eprintln!("  {percent}%");
                }
            }
            Event::ExportFinished {
                export,
                manifest,
                files,
                error,
                ..
            } if export == started.export => {
                if json {
                    print(&serde_json::json!({
                        "export": export,
                        "timeline": started.timeline,
                        "manifest": manifest,
                        "files": files,
                        "skipped": started.skipped,
                        "error": error,
                    }))?;
                } else {
                    print_files(&files);
                    println!("Manifest: {}", manifest.display());
                }
                return match error {
                    None => Ok(ExitCode::SUCCESS),
                    Some(error) => {
                        eprintln!("error: the export failed: {error}");
                        Ok(ExitCode::FAILURE)
                    }
                };
            }
            _ => {}
        }
    }
    bail!("isocastd went away before the export finished")
}

fn print_files(files: &[ExportFile]) {
    for file in files {
        println!(
            "Wrote {}: {:.3} s, {} frames, from {} clip{}",
            file.path.display(),
            file.duration_seconds.unwrap_or(0.0),
            file.frames.unwrap_or(0),
            file.parts.len(),
            if file.parts.len() == 1 { "" } else { "s" }
        );
    }
}

/// `isocast retake`.
pub fn retake(folder: Option<PathBuf>, seconds: Option<f64>, json: bool) -> Result<ExitCode> {
    let folder = folder.map(std::path::absolute).transpose()?;
    let mut client = Client::connect_or_launch()?;
    let retaken: Retaken = client.call(Method::Retake(Some(RetakeParams { folder, seconds })))?;
    // While recording, the new take begins a moment later: return once it
    // has, so whoever runs this can start again straight away.
    if let Some(wait) = retaken.begins_in_seconds.filter(|s| *s > 0.0) {
        std::thread::sleep(std::time::Duration::from_secs_f64(wait.min(5.0)));
    }
    if json {
        print(&retaken)?;
        return Ok(ExitCode::SUCCESS);
    }
    match (&retaken.replaced, &retaken.begins_at) {
        (Some(removed), _) => println!(
            "Retaking {} on timeline {}; recording the new take:",
            removed.clip.as_deref().unwrap_or("the last clip"),
            retaken.timeline
        ),
        (None, Some(at)) => println!(
            "Recording the new take for timeline {} from {at} (now):",
            retaken.timeline
        ),
        (None, None) => println!("Recording a new take for timeline {}:", retaken.timeline),
    }
    for file in &retaken.files {
        println!("  {}", file.display());
    }
    print_notes(&retaken.notes);
    Ok(ExitCode::SUCCESS)
}

/// The words for where the next recording goes (#114), as `record-target`
/// and `status` say it.
pub fn target_words(target: &RecordTarget) -> String {
    match (&target.timeline, &target.name) {
        (Some(id), Some(name)) => format!(
            "take {} of {name:?} ({id}){}",
            target.take.unwrap_or(1),
            target
                .folder
                .as_ref()
                .map(|folder| format!(", in {}", folder.display()))
                .unwrap_or_default()
        ),
        _ => "a new project, with its own timeline".into(),
    }
}

/// `isocast record-target [new|TIMELINE]`.
pub fn record_target(target: Option<String>, json: bool) -> Result<ExitCode> {
    let mut client = Client::connect_or_launch()?;
    let chose = target.is_some();
    let target: RecordTarget =
        client.call(Method::RecordTarget(Some(RecordTargetParams { target })))?;
    if json {
        print(&target)?;
    } else {
        println!("Next recording: {}.", target_words(&target));
        if !chose {
            println!(
                "Choose with `isocast record-target new` or `isocast record-target ID` (`isocast \
                 timeline list` has the ids)."
            );
        }
        print_notes(&target.notes);
    }
    Ok(ExitCode::SUCCESS)
}
