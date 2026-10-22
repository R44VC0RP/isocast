use super::*;
use isocast_protocol::roots::{FolderRoots, FolderRootsSetParams};

#[derive(clap::Subcommand)]
pub enum Command {
    /// Show the folders other computers can use: every path a client on
    /// another computer (or an obs-websocket client) gives must be inside
    /// one, symlinks resolved.
    Show {
        #[arg(long)]
        json: bool,
    },
    /// Choose the folders other computers can use, besides this machine's
    /// recordings folders and fonts (kept across restarts); --default goes
    /// back to Movies or Videos, Pictures, Music, Desktop and Downloads.
    /// Only on this machine.
    Set {
        /// Folders, absolute or ~/...
        #[arg(required_unless_present = "default", conflicts_with = "default")]
        folders: Vec<String>,
        #[arg(long)]
        default: bool,
        #[arg(long)]
        json: bool,
    },
}

pub fn run(command: Command) -> Result<ExitCode> {
    let (roots, json): (FolderRoots, bool) = match command {
        Command::Show { json } => (
            Client::connect_or_launch()?.call(Method::FolderRoots)?,
            json,
        ),
        Command::Set {
            folders,
            default,
            json,
        } => (
            Client::connect_or_launch()?.call(Method::FolderRootsSet(FolderRootsSetParams {
                folders: (!default).then_some(folders),
            }))?,
            json,
        ),
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&roots)?);
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "Folders other computers can use ({}):",
        if roots.custom {
            "chosen here"
        } else {
            "the defaults"
        }
    );
    for root in &roots.roots {
        println!(
            "  {} ({}{})",
            root.path.display(),
            root.why,
            if root.read_only { ", read only" } else { "" }
        );
    }
    Ok(ExitCode::SUCCESS)
}
