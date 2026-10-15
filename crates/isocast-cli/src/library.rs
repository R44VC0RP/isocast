//! `isocast collection …` and `isocast profile …`: scene collections and
//! profiles, the two named parts the live setup is kept in (as in OBS).
//! Each command is one `library_*` call on the method table.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use isocast_client::library as files;
use isocast_protocol::api::Method;
use isocast_protocol::library::{
    Library, LibraryChange, LibraryCreateParams, LibraryDocument, LibraryEntry,
    LibraryImportObsParams, LibraryKind, LibraryRenameParams, LibrarySwitchParams, LibraryTarget,
};

use crate::client::Client;

#[derive(Subcommand)]
pub enum LibraryCommand {
    /// List them, marking the one in use.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Make a new one: a scene collection with one empty scene on the
    /// canvas in use, or a profile with starter settings (60 fps, HEVC
    /// program and ISO files to ~/Movies/Isocast).
    New {
        name: String,
        /// Its id (lowercase letters, digits and dashes); made from the
        /// name if absent.
        #[arg(long)]
        id: Option<String>,
        /// Switch to it (refused while recording or streaming).
        #[arg(long)]
        switch: bool,
        #[arg(long)]
        json: bool,
    },
    /// Copy one (by id) under a new name.
    Duplicate {
        id: String,
        name: String,
        /// The copy's id; made from the name if absent.
        #[arg(long)]
        new_id: Option<String>,
        #[arg(long)]
        switch: bool,
        #[arg(long)]
        json: bool,
    },
    /// Rename one (its id stays).
    Rename {
        id: String,
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Delete one that isn't in use; its file is kept in the library's
    /// `deleted` folder.
    Delete {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Use it: the live setup becomes it at once, and the live engine
    /// follows (a collection restores its last program/preview selection). Refused
    /// while recording or streaming.
    Switch {
        id: String,
        /// Also switch the other half at the same time: the profile (for a
        /// collection) or the collection (for a profile), by id.
        #[arg(long, value_name = "ID")]
        with: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Add one from a file: one exported with `export` (.toml or .json), or
    /// a whole setup file, which adds its collection and its profile. With
    /// --obs, from OBS Studio instead: a scene collection (its name or .json
    /// file) or a profile (its name, folder or basic.ini).
    Import {
        file: Option<PathBuf>,
        #[arg(long, value_name = "NAME_OR_FILE", conflicts_with = "file")]
        obs: Option<String>,
        /// For a scene collection from OBS: also import this OBS profile
        /// (name, folder or basic.ini), with Source Record's ISO settings.
        #[arg(long, value_name = "NAME_OR_FILE", requires = "obs")]
        obs_profile: Option<String>,
        /// The name to give it; its own if absent.
        #[arg(long)]
        name: Option<String>,
        /// For a scene collection from OBS: import it onto this canvas,
        /// WIDTHxHEIGHT, scaling positions and sizes.
        #[arg(long, value_name = "WxH", requires = "obs")]
        canvas: Option<String>,
        /// Switch to what was imported.
        #[arg(long)]
        switch: bool,
        /// Move the stream key OBS keeps in plain text in the OBS
        /// profile's service.json into the Keychain.
        #[arg(long, requires = "obs")]
        obs_stream_key: bool,
        #[arg(long)]
        json: bool,
    },
    /// Write one (the one in use if no id is given) to a file, .toml or
    /// .json, or print it as TOML; `import` adds it back, here or on
    /// another Mac.
    Export {
        id: Option<String>,
        #[arg(long, short)]
        output: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
}

pub fn run(kind: LibraryKind, command: LibraryCommand) -> Result<ExitCode> {
    let mut client = Client::connect_or_launch()?;
    match command {
        LibraryCommand::List { json } => {
            let library: Library = client.call(Method::Library)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&library)?);
            } else {
                print_list(kind, &library);
            }
            Ok(ExitCode::SUCCESS)
        }
        LibraryCommand::New {
            name,
            id,
            switch,
            json,
        } => {
            let change: LibraryChange =
                client.call(Method::LibraryCreate(Box::new(LibraryCreateParams {
                    kind,
                    name,
                    id,
                    copy_of: None,
                    switch,
                })))?;
            print_change(kind, "Made", &change, json)
        }
        LibraryCommand::Duplicate {
            id,
            name,
            new_id,
            switch,
            json,
        } => {
            let change: LibraryChange =
                client.call(Method::LibraryCreate(Box::new(LibraryCreateParams {
                    kind,
                    name,
                    id: new_id,
                    copy_of: Some(id),
                    switch,
                })))?;
            print_change(kind, "Made", &change, json)
        }
        LibraryCommand::Rename { id, name, json } => {
            let change: LibraryChange =
                client.call(Method::LibraryRename(Box::new(LibraryRenameParams {
                    kind,
                    id,
                    name,
                })))?;
            print_change(kind, "Renamed", &change, json)
        }
        LibraryCommand::Delete { id, json } => {
            let change: LibraryChange =
                client.call(Method::LibraryDelete(LibraryTarget { kind, id }))?;
            print_change(kind, "Deleted", &change, json)
        }
        LibraryCommand::Switch { id, with, json } => {
            let params = match kind {
                LibraryKind::Collection => LibrarySwitchParams {
                    collection: Some(id),
                    profile: with,
                },
                LibraryKind::Profile => LibrarySwitchParams {
                    collection: with,
                    profile: Some(id),
                },
            };
            let change: LibraryChange = client.call(Method::LibrarySwitch(params))?;
            print_change(kind, "Switched to", &change, json)
        }
        LibraryCommand::Import {
            file,
            obs,
            obs_profile,
            name,
            canvas,
            switch,
            obs_stream_key,
            json,
        } => {
            let change: LibraryChange = match (file, obs) {
                (Some(file), None) => {
                    let mut params = files::read_import(kind, &file)?;
                    params.name = name;
                    params.switch = switch;
                    client.call(Method::LibraryImport(Box::new(params)))?
                }
                (None, Some(obs)) => {
                    let (width, height) = match canvas.as_deref() {
                        Some(canvas) if kind == LibraryKind::Collection => {
                            let (w, h) = canvas
                                .split_once('x')
                                .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
                                .with_context(|| {
                                    format!("--canvas {canvas:?}: expected WIDTHxHEIGHT")
                                })?;
                            (Some(w), Some(h))
                        }
                        Some(_) => bail!("--canvas is for importing a scene collection"),
                        None => (None, None),
                    };
                    if obs_profile.is_some() && kind == LibraryKind::Profile {
                        bail!(
                            "--obs-profile is for `isocast collection import --obs`; here --obs \
                             names the profile"
                        );
                    }
                    let (collection, profile) = match kind {
                        LibraryKind::Collection => (Some(obs_path(obs)?), obs_profile),
                        LibraryKind::Profile => (None, Some(obs)),
                    };
                    let profile = profile.map(obs_path).transpose()?;
                    client.call(Method::LibraryImportObs(Box::new(LibraryImportObsParams {
                        collection,
                        profile,
                        name,
                        width,
                        height,
                        switch,
                        stream_key: obs_stream_key,
                    })))?
                }
                _ => bail!(
                    "give a file to import (exported from Isocast, or a whole setup), or --obs \
                     with an OBS {}",
                    kind.label()
                ),
            };
            print_change(kind, "Imported", &change, json)
        }
        LibraryCommand::Export { id, output, json } => {
            let id = match id {
                Some(id) => id,
                None => {
                    let library: Library = client.call(Method::Library)?;
                    let current = match kind {
                        LibraryKind::Collection => library.collection,
                        LibraryKind::Profile => library.profile,
                    };
                    current
                        .with_context(|| format!("no {} is in use yet; give an id", kind.label()))?
                }
            };
            let document: LibraryDocument =
                client.call(Method::LibraryGet(LibraryTarget { kind, id }))?;
            export(&document, output.as_deref(), json)
        }
    }
}

/// An OBS name or path for the daemon, which runs in another folder: a
/// file's absolute path, or a name as it is.
fn obs_path(given: String) -> Result<String> {
    let path_like = given.contains('/') && !given.starts_with('~');
    Ok(if Path::new(&given).exists() || path_like {
        std::path::absolute(&given)?.display().to_string()
    } else {
        given
    })
}

fn export(document: &LibraryDocument, output: Option<&Path>, json: bool) -> Result<ExitCode> {
    let entry = &document.entry;
    if json && output.is_none() {
        println!("{}", serde_json::to_string_pretty(document)?);
        return Ok(ExitCode::SUCCESS);
    }
    match output {
        Some(path) => {
            if path.exists() {
                bail!(
                    "{} already exists; choose another --output or remove it",
                    path.display()
                );
            }
            files::write_export(document, path)?;
            if json {
                println!("{}", serde_json::to_string_pretty(document)?);
            } else {
                println!(
                    "Exported {} {:?} ({}) to {}.",
                    entry.kind.label(),
                    entry.name,
                    entry.id,
                    path.display()
                );
            }
        }
        None => print!("{}", files::export_text(document, false)?),
    }
    Ok(ExitCode::SUCCESS)
}

fn print_list(kind: LibraryKind, library: &Library) {
    let entries = match kind {
        LibraryKind::Collection => &library.collections,
        LibraryKind::Profile => &library.profiles,
    };
    let title = match kind {
        LibraryKind::Collection => "Scene collections",
        LibraryKind::Profile => "Profiles",
    };
    println!(
        "{title} in {}:",
        library.folder.join(kind.folder()).display()
    );
    if entries.is_empty() {
        println!(
            "  none yet (one is made when a setup is first applied, or with `isocast {} new NAME`)",
            kind.command()
        );
    }
    let id_width = entries.iter().map(|e| e.id.len()).max().unwrap_or(0);
    let name_width = entries.iter().map(|e| e.name.len()).max().unwrap_or(0);
    for entry in entries {
        let mark = if entry.current { "*" } else { " " };
        let about = match (&entry.summary, &entry.problem) {
            (_, Some(problem)) => format!("can't be used: {problem}"),
            (Some(summary), None) => summary.clone(),
            (None, None) => String::new(),
        };
        println!(
            "{mark} {:id_width$}  {:name_width$}  {about}",
            entry.id, entry.name
        );
    }
    if entries.iter().any(|e| e.current) {
        println!("(* in use)");
    }
}

fn describe(entry: &LibraryEntry) -> String {
    format!("{} {:?} ({})", entry.kind.label(), entry.name, entry.id)
}

fn print_change(
    kind: LibraryKind,
    verb: &str,
    change: &LibraryChange,
    json: bool,
) -> Result<ExitCode> {
    if json {
        println!("{}", serde_json::to_string_pretty(change)?);
        return Ok(ExitCode::SUCCESS);
    }
    let mut done: Vec<String> = Vec::new();
    // The kind asked about first.
    let entries = match kind {
        LibraryKind::Collection => [&change.collection, &change.profile],
        LibraryKind::Profile => [&change.profile, &change.collection],
    };
    for entry in entries.into_iter().flatten() {
        done.push(describe(entry));
    }
    println!("{verb} {}.", done.join(" and "));
    if let Some(revision) = change.revision {
        println!("The live setup is now at revision {revision}.");
    }
    if let (Some(collection), Some(profile)) = (&change.current_collection, &change.current_profile)
    {
        println!("In use: scene collection {collection}, profile {profile}.");
    }
    if !change.unsupported.is_empty() {
        println!("Not imported:");
        for u in &change.unsupported {
            println!("  {} ({}): {}", u.obs, u.kind, u.reason);
        }
    }
    for note in &change.notes {
        println!("note: {note}");
    }
    for problem in &change.problems {
        println!(
            "problem with the other half in use (switching is refused until fixed): {}: {}",
            problem.path, problem.message
        );
    }
    Ok(ExitCode::SUCCESS)
}
