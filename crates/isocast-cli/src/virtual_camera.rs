use super::*;
use isocast_protocol::virtual_camera::{VirtualCameraRemoval, VirtualCameraStatus};

#[derive(clap::Subcommand)]
pub enum Command {
    /// Start the live setup's virtual_camera selection. Never installs anything.
    Start {
        #[arg(long)]
        json: bool,
    },
    /// Stop feeding the camera, restoring the Isocast waiting card.
    Stop {
        #[arg(long)]
        json: bool,
    },
    /// Output state, format, counters, enqueue latency and last error.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Remove the camera from macOS before deleting Isocast (release app):
    /// stops it, then macOS asks for an administrator password.
    Remove {
        #[arg(long)]
        json: bool,
    },
}

pub fn run(command: Command) -> Result<ExitCode> {
    if let Command::Remove { json } = command {
        if !json {
            println!(
                "Removing Isocast Virtual Camera: enter an administrator password when macOS asks…"
            );
        }
        let removal: VirtualCameraRemoval =
            Client::connect_or_launch()?.call(Method::VirtualCameraRemove)?;
        if json {
            println!("{}", serde_json::to_string_pretty(&removal)?);
        } else {
            println!("{}", removal.message);
        }
        return Ok(ExitCode::SUCCESS);
    }
    let (method, json) = match command {
        Command::Start { json } => (Method::VirtualCameraStart, json),
        Command::Stop { json } => (Method::VirtualCameraStop, json),
        Command::Status { json } => (Method::VirtualCameraStatus, json),
        Command::Remove { .. } => unreachable!("handled above"),
    };
    let status: VirtualCameraStatus = Client::connect_or_launch()?.call(method)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        print(&status);
    }
    Ok(ExitCode::SUCCESS)
}

pub fn print(status: &VirtualCameraStatus) {
    if status.fps == 0 {
        println!(
            "Virtual camera {:?}: no live setup ({})",
            status.state, status.transport
        );
        if let Some(error) = &status.last_error {
            println!("{error}");
        }
        return;
    }
    println!(
        "Virtual camera {:?}: {}x{} at {} fps ({}) — {} sent, {} dropped",
        status.state,
        status.width,
        status.height,
        status.fps,
        status.transport,
        status.frames_sent,
        status.frames_dropped
    );
    if let Some(error) = &status.last_error {
        println!("{error}");
    }
}
