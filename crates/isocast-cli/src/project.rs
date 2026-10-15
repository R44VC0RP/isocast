//! `isocast project …`: recording projects (7.7b), a folder per
//! recording session. Each command is one call on the method table
//! (`list_projects`, `rename_project`).

use std::process::ExitCode;

use anyhow::Result;
use clap::Subcommand;
use isocast_protocol::api::Method;
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
    /// Refused while a recording or an export writes into it; manifests
    /// and timelines keep finding its files.
    Rename {
        project: String,
        name: String,
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
