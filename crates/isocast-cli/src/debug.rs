use super::*;
use isocast_protocol::developer::{DebugSetParams, DeveloperStatus};

#[derive(clap::Subcommand)]
pub enum Command {
    /// Turn developer mode on in the current profile: every API call and
    /// debug messages logged, and frame timing traced each second while
    /// recording, in their own log segments (`isocast logs`). It turns
    /// itself off after 24 hours unless --auto-off-hours says otherwise.
    On {
        /// Hours until it turns itself off; 0 keeps it on.
        #[arg(long)]
        auto_off_hours: Option<u32>,
        #[arg(long)]
        json: bool,
    },
    /// Turn developer mode off.
    Off {
        #[arg(long)]
        json: bool,
    },
    /// Whether developer mode is on, until when, its logs and the latest
    /// trace.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Advertise a test engine on the local network until Ctrl-C, so
    /// `isocast engines --discover` on another computer has something to
    /// find. Development only: nothing listens on the port, and it runs
    /// in this process, not the engine.
    Advertise {
        /// The instance name to advertise.
        #[arg(long)]
        name: String,
        /// The TCP port to advertise.
        #[arg(long)]
        port: u16,
        /// The `id` in the TXT record.
        #[arg(long, default_value = "debug")]
        id: String,
        /// Stop by itself after this many seconds, as Ctrl-C would.
        #[arg(long)]
        seconds: Option<u64>,
        #[arg(long)]
        json: bool,
    },
}

pub fn run(command: Command) -> Result<ExitCode> {
    let (method, json) = match command {
        Command::On {
            auto_off_hours,
            json,
        } => (
            Method::DebugSet(DebugSetParams {
                enabled: Some(true),
                auto_off_hours,
            }),
            json,
        ),
        Command::Off { json } => (
            Method::DebugSet(DebugSetParams {
                enabled: Some(false),
                auto_off_hours: None,
            }),
            json,
        ),
        Command::Status { json } => (Method::DebugStatus, json),
        Command::Advertise {
            name,
            port,
            id,
            seconds,
            json,
        } => return crate::engines::advertise(&name, port, &id, seconds, json),
    };
    let status: DeveloperStatus = Client::connect_or_launch()?.call(method)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        print(&status);
    }
    Ok(ExitCode::SUCCESS)
}

fn print(status: &DeveloperStatus) {
    let state = match (status.enabled, status.active) {
        (true, true) => "on",
        (false, false) => "off",
        (true, false) => "on (starting)",
        (false, true) => "off (stopping)",
    };
    println!(
        "Developer mode {state}{}",
        status
            .profile
            .as_deref()
            .map(|p| format!(" (profile {p})"))
            .unwrap_or_default()
    );
    if let Some(since) = &status.since {
        println!("  on since    {since}");
    }
    match (&status.auto_off_at, status.auto_off_hours) {
        (Some(at), _) => println!("  turns off   {at}"),
        (None, 0) => println!("  turns off   never (auto-off 0)"),
        (None, hours) => println!("  auto-off    {hours} h after it is turned on"),
    }
    println!(
        "  logs        {} ({:.1} of {} MiB in developer segments)",
        status.logs_folder.display(),
        status.log_bytes as f64 / (1u64 << 20) as f64,
        status.log_budget_bytes >> 20
    );
    if let Some(memory) = status.memory_bytes {
        println!("  isocastd    {:.0} MB", memory as f64 / 1e6);
    }
    if let Some(trace) = &status.trace {
        for output in trace["outputs"].as_array().into_iter().flatten() {
            println!(
                "  {:<12} {} frames/s, {} dropped, {} late, submit {} µs (max {}), age {} µs, queue {}",
                output["source"].as_str().unwrap_or("?"),
                output["frames"],
                output["dropped"],
                output["late"],
                output["submit_us"]["mean"],
                output["submit_us"]["max"],
                output["age_us"]["mean"],
                output["writer_queue_max"]
            );
        }
    }
}
