use super::*;
use isocast_protocol::replay::{ReplayParams, ReplaySaved, ReplayState, ReplayStatus};

#[derive(clap::Subcommand)]
pub enum Command {
    /// Start keeping the last `recording.replay.seconds` of the program
    /// (video and the program mix) in memory, starting the live engine if
    /// needed. Recording and streaming start and stop without touching it.
    /// With --canvas vertical, the vertical canvas's program, in a buffer
    /// of its own with the same settings.
    Start {
        #[command(flatten)]
        canvas: CanvasArg,
        #[arg(long, help = "Print JSON")]
        json: bool,
    },
    /// Stop the replay buffer, discarding what it holds.
    Stop {
        #[command(flatten)]
        canvas: CanvasArg,
        #[arg(long, help = "Print JSON")]
        json: bool,
    },
    /// Save what the buffer holds, up to now, to a new file in the
    /// recordings folder (the running recording's project folder, if one
    /// records). Prints the file's path.
    Save {
        #[command(flatten)]
        canvas: CanvasArg,
        #[arg(long, help = "Print JSON")]
        json: bool,
    },
    /// Show whether it runs, what it holds, its memory, losses and last save.
    Status {
        #[command(flatten)]
        canvas: CanvasArg,
        #[arg(long, help = "Print JSON")]
        json: bool,
    },
}

#[derive(clap::Args)]
pub struct CanvasArg {
    /// Which canvas's buffer: main, or vertical (the setup's [vertical]
    /// canvas; each canvas has its own buffer).
    #[arg(long, value_parser = parse_canvas, default_value = "main")]
    canvas: CanvasId,
}

impl CanvasArg {
    fn params(&self) -> Option<ReplayParams> {
        ReplayParams::of(self.canvas)
    }
}

/// How messages name `canvas`'s buffer.
fn what(canvas: CanvasId) -> String {
    match canvas {
        CanvasId::Main => "the replay buffer".into(),
        CanvasId::Vertical => "the vertical replay buffer".into(),
        other => format!("canvas {other:?}'s replay buffer"),
    }
}

pub fn run(command: Command) -> Result<ExitCode> {
    match command {
        Command::Start { canvas, json } => {
            let status: ReplayStatus =
                Client::connect_or_launch()?.call(Method::ReplayStart(canvas.params()))?;
            print_status(&status, json)
        }
        Command::Stop { canvas, json } => {
            let Some(mut client) = Client::connect()? else {
                bail!(
                    "{} isn't running: isocastd isn't running",
                    what(canvas.canvas)
                );
            };
            let status: ReplayStatus = client.call(Method::ReplayStop(canvas.params()))?;
            print_status(&status, json)
        }
        Command::Save { canvas, json } => {
            let Some(mut client) = Client::connect()? else {
                bail!(
                    "{} isn't running: isocastd isn't running",
                    what(canvas.canvas)
                );
            };
            let saved: ReplaySaved = client.call(Method::ReplaySave(canvas.params()))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&saved)?);
            } else {
                print_saved(&saved);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Status { canvas, json } => {
            let Some(mut client) = Client::connect()? else {
                if json {
                    println!("null");
                } else {
                    println!(
                        "{}Replay buffer off: isocastd isn't running",
                        label(canvas.canvas)
                    );
                }
                return Ok(ExitCode::SUCCESS);
            };
            let status: ReplayStatus = client.call(Method::ReplayStatus(canvas.params()))?;
            print_status(&status, json)
        }
    }
}

fn print_status(status: &ReplayStatus, json: bool) -> Result<ExitCode> {
    if json {
        println!("{}", serde_json::to_string_pretty(status)?);
    } else {
        print(status);
    }
    Ok(ExitCode::SUCCESS)
}

pub fn print(status: &ReplayStatus) {
    let state = match status.state {
        ReplayState::Off => "off",
        ReplayState::Running => "running",
        ReplayState::Failed => "failed",
    };
    println!(
        "{}Replay buffer {state}: keeps {} s",
        label(status.canvas),
        status.seconds
    );
    if status.state == ReplayState::Running {
        println!(
            "  {}×{} at {} fps, {} kb/s H.264{}: {:.1} s held, {:.0} of {:.0} MB",
            status.width,
            status.height,
            status.fps,
            status.video_kbps,
            if status.shared_encoder {
                ", sharing the stream's encoder"
            } else {
                ""
            },
            status.buffered_seconds,
            status.memory_bytes as f64 / 1e6,
            status.max_memory_bytes as f64 / 1e6,
        );
        if status.memory_limited {
            println!(
                "  the memory limit keeps it short of {} s (recording.replay.max_memory_mb)",
                status.seconds
            );
        }
    }
    if status.dropped_frames > 0 || status.audio_dropouts > 0 {
        println!(
            "  missed {} frame(s) and {} audio chunk(s)",
            status.dropped_frames, status.audio_dropouts
        );
    }
    if let Some(saved) = &status.last_saved {
        print!("  last save ({} so far): ", status.saves);
        print_saved(saved);
    }
    if let Some(error) = &status.last_error {
        println!("  {error}");
    }
}

/// "Vertical " before the vertical canvas's buffer's lines.
fn label(canvas: CanvasId) -> String {
    match canvas {
        CanvasId::Main => String::new(),
        CanvasId::Vertical => "Vertical ".into(),
        other => format!("Canvas {other:?} "),
    }
}

fn print_saved(saved: &ReplaySaved) {
    println!(
        "{} ({:.1} s, {} frames{}, {:.1} MB){}",
        saved.path,
        saved.seconds,
        saved.frames,
        if saved.dropped_frames > 0 {
            format!(", {} missed", saved.dropped_frames)
        } else {
            String::new()
        },
        saved.bytes as f64 / 1e6,
        if saved.short {
            "; shorter than asked: the buffer hadn't filled yet"
        } else {
            ""
        }
    );
}
