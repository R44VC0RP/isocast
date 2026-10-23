//! `isocast project …`: recording projects (7.7b), a folder per
//! recording session, and their takes as projects for other editors. Each
//! command is one call on the method table (`list_projects`,
//! `rename_project`, `take_export_nle`).

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::Subcommand;
use isocast_protocol::api::Method;
use isocast_protocol::nle::{NleApp, NleExported, NleFormat, TakeExportNleParams};
use isocast_protocol::project::{
    ListProjectsParams, Project, ProjectRenamed, Projects, RenameProjectParams,
};

use crate::client::Client;

#[derive(Subcommand)]
pub enum ProjectCommand {
    /// List the recording projects (a folder per recording session),
    /// newest first, with their takes and files.
    List {
        /// At most this many (100 if absent).
        #[arg(long)]
        limit: Option<u32>,
        #[arg(long)]
        json: bool,
    },
    /// Rename a project (by id): its folder becomes `<date> <time> <NAME>`.
    /// Refused while a recording writes into it; manifests keep finding
    /// its files.
    Rename {
        project: String,
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Write a take as a project another editor opens, pointing at the
    /// take's own files (nothing is encoded or copied): fcpxml (Final Cut
    /// Pro), xmeml (Premiere Pro; DaVinci Resolve with --app resolve) or
    /// otio (OpenTimelineIO). Every ISO source on its own track with the
    /// program on top (the one on), lined up from the take's manifest.
    ExportNle {
        /// The take: its manifest (`….session.json`), or a project folder
        /// holding one take.
        take: PathBuf,
        #[arg(long, value_name = "FORMAT", value_parser = ["fcpxml", "xmeml", "otio"])]
        format: String,
        /// The editor it is for: final_cut (fcpxml's default), premiere
        /// (xmeml's) or resolve (with xmeml: Resolve's picture scale and a
        /// track per audio channel).
        #[arg(long, value_name = "APP", value_parser = ["final_cut", "premiere", "resolve"])]
        app: Option<String>,
        /// Write it in this folder (made if missing); else the take's.
        #[arg(long, conflicts_with = "output")]
        folder: Option<PathBuf>,
        /// Write exactly this file (ending in .fcpxml, .xml or .otio); one
        /// already there is replaced only if it is an earlier Isocast
        /// export, or with --replace.
        #[arg(long, value_name = "PATH")]
        output: Option<PathBuf>,
        /// With --output: replace whatever file is there.
        #[arg(long, requires = "output")]
        replace: bool,
        #[arg(long)]
        json: bool,
    },
}

pub fn run(command: ProjectCommand) -> Result<ExitCode> {
    let mut client = Client::connect_or_launch()?;
    match command {
        ProjectCommand::List { limit, json } => {
            let list: Projects =
                client.call(Method::ListProjects(Some(ListProjectsParams { limit })))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&list)?);
            } else if list.projects.is_empty() {
                println!(
                    "No projects yet in {}; each recording makes one.",
                    list.folders
                        .iter()
                        .map(|f| f.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            } else {
                for project in &list.projects {
                    print_project(project);
                }
            }
        }
        ProjectCommand::Rename {
            project,
            name,
            json,
        } => {
            let renamed: ProjectRenamed =
                client.call(Method::RenameProject(RenameProjectParams { project, name }))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&renamed)?);
            } else {
                println!(
                    "Renamed {:?} to {:?}: {}",
                    renamed.old_name,
                    renamed.project.name,
                    renamed.project.folder.display()
                );
                for note in &renamed.notes {
                    println!("  note: {note}");
                }
            }
        }
        ProjectCommand::ExportNle {
            take,
            format,
            app,
            folder,
            output,
            replace,
            json,
        } => {
            let params = TakeExportNleParams {
                format: NleFormat::parse(&format).expect("clap checked it"),
                app: app.as_deref().and_then(NleApp::parse),
                take: std::path::absolute(take)?,
                folder: folder.map(std::path::absolute).transpose()?,
                path: output.map(std::path::absolute).transpose()?,
                replace,
            };
            let exported: NleExported = client.call(Method::TakeExportNle(params))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&exported)?);
            } else {
                print_nle(&exported);
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn print_nle(exported: &NleExported) {
    println!(
        "Wrote {} for {} ({}): {:.3} s at {} fps, {}x{}, {} media file{}",
        exported.path.display(),
        exported.app.title(),
        exported.format.name(),
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
                "  {kind}{}  {}{}",
                n + 1,
                track.name,
                if track.enabled > 0 { "" } else { " (off)" }
            );
        }
    }
    if exported.markers > 0 {
        println!("  {} marker(s)", exported.markers);
    }
    for note in &exported.notes {
        println!("note: {note}");
    }
}

fn print_project(project: &Project) {
    println!(
        "{}  {:?}{}: {} take{}, {} file{}",
        project.id,
        project.name,
        if project.recording {
            " (recording)"
        } else {
            ""
        },
        project.takes.len(),
        if project.takes.len() == 1 { "" } else { "s" },
        project.files.len(),
        if project.files.len() == 1 { "" } else { "s" },
    );
    println!("    {}", project.folder.display());
}
