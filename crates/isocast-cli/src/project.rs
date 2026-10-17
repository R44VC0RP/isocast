//! `isocast project …`: recording projects (7.7b), a folder per
//! recording session. Each command is one call on the method table
//! (`list_projects`, `rename_project`, `project_open`,
//! `recent_projects`); `auto-open` reads and changes the setup's
//! `recording.open_editor` with `get` and `apply`.

use std::process::ExitCode;

use anyhow::Result;
use clap::Subcommand;
use isocast_protocol::api::Method;
use isocast_protocol::project::{
    ListProjectsParams, Project, ProjectRenamed, Projects, RenameProjectParams,
};

use isocast_protocol::api::{Applied, ApplyParams, GetParams, Live};
use isocast_protocol::change::Change;
use isocast_protocol::isoprj::{
    ProjectOpenParams, ProjectOpened, RecentProjects, RecentProjectsParams,
};
use std::path::PathBuf;

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
    /// Refused while a recording or an export writes into it; manifests
    /// and timelines keep finding its files.
    Rename {
        project: String,
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Open a project file (`.isoprj`) or project folder: its timelines
    /// join the engine's (imported if it was copied from elsewhere), one
    /// becomes current, and the app (if it is running) shows the editor
    /// on it. Missing takes are listed as offline (`timeline relink`).
    Open {
        /// The `.isoprj` file, or the project's folder.
        path: PathBuf,
        /// Which of its timelines (an id); else the current one if it is
        /// one of them, else the newest.
        #[arg(long)]
        timeline: Option<String>,
        /// Don't ask the app to show the editor.
        #[arg(long)]
        no_editor: bool,
        #[arg(long)]
        json: bool,
    },
    /// The projects opened lately (File › Open Recent), newest first.
    Recent {
        /// Forget them.
        #[arg(long)]
        clear: bool,
        #[arg(long)]
        json: bool,
    },
    /// Whether the app opens the editor when a recording stops (the
    /// setup's `recording.open_editor`): `on` or `off` changes it, nothing
    /// says it.
    AutoOpen {
        #[arg(value_parser = ["on", "off"])]
        state: Option<String>,
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
        ProjectCommand::Open {
            path,
            timeline,
            no_editor,
            json,
        } => {
            let opened: ProjectOpened = client.call(Method::ProjectOpen(ProjectOpenParams {
                path: crate::timeline::absolute(path)?,
                timeline,
                editor: Some(!no_editor),
            }))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&opened)?);
            } else {
                println!(
                    "Opened {:?} ({}): {}",
                    opened.name,
                    opened.project,
                    opened.path.display()
                );
                for note in &opened.notes {
                    println!("note: {note}");
                }
                crate::timeline::print_timeline(&opened.timeline);
                if opened.timelines.len() > 1 {
                    let others: Vec<&str> = opened
                        .timelines
                        .iter()
                        .filter(|t| t.id != opened.timeline.id)
                        .map(|t| t.id.as_str())
                        .collect();
                    println!("Its other timelines: {}", others.join(", "));
                }
                for take in &opened.offline {
                    println!("offline: {}", take.display());
                }
            }
        }
        ProjectCommand::Recent { clear, json } => {
            let recent: RecentProjects =
                client.call(Method::RecentProjects(Some(RecentProjectsParams { clear })))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&recent)?);
            } else if recent.projects.is_empty() {
                println!("No projects opened lately (`isocast project open PATH` opens one).");
            } else {
                for project in &recent.projects {
                    println!(
                        "{:?}  {}{}",
                        project.name,
                        project.path.display(),
                        if project.exists { "" } else { "  (missing)" }
                    );
                }
            }
        }
        ProjectCommand::AutoOpen { state, json } => {
            let live: Live = client.call(Method::Get(Some(GetParams::default())))?;
            let Some(setup) = live.setup else {
                anyhow::bail!("there is no setup yet (`isocast setup init` makes one)");
            };
            let mut on = setup.recording.open_editor;
            if let Some(state) = state {
                let want = state == "on";
                if want != on {
                    let _: Applied = client.call(Method::Apply(Box::new(ApplyParams {
                        revision: live.revision,
                        change: Change::Patch(
                            serde_json::json!({"recording": {"open_editor": want}}),
                        ),
                        live: false,
                    })))?;
                }
                on = want;
            }
            if json {
                println!("{}", serde_json::json!({ "open_editor": on }));
            } else {
                println!(
                    "The app {} the editor when a recording stops (recording.open_editor = {on}).",
                    if on { "opens" } else { "doesn't open" }
                );
            }
        }
    }
    Ok(ExitCode::SUCCESS)
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
