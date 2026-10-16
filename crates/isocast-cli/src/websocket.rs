use super::*;
use isocast_protocol::websocket::{WebsocketEnableParams, WebsocketStatus};

#[derive(clap::Subcommand)]
pub enum Command {
    /// Show whether the obs-websocket server is on and listening, where,
    /// whether it asks for a password, and how many clients are connected.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Turn the server on (kept across restarts). The first time, a
    /// password is generated and shown once: enter it, with the address,
    /// in the OBS tool's WebSocket settings.
    Enable {
        /// The TCP port (OBS's is 4455).
        #[arg(long)]
        port: Option<u16>,
        /// The address to listen on: 127.0.0.1 (this Mac only, the
        /// default), an address of this Mac, or 0.0.0.0 (every network).
        #[arg(long)]
        bind: Option<String>,
        /// Set a new password: read from stdin, or asked for without echo.
        #[arg(long, conflicts_with = "no_password")]
        password: bool,
        /// Let clients connect without a password (any program on this
        /// Mac, but not web pages); only with a loopback --bind.
        #[arg(long)]
        no_password: bool,
        #[arg(long)]
        json: bool,
    },
    /// Turn the server off, disconnecting its clients.
    Disable {
        #[arg(long)]
        json: bool,
    },
}

pub fn run(command: Command) -> Result<ExitCode> {
    match command {
        Command::Status { json } => {
            let status: WebsocketStatus =
                Client::connect_or_launch()?.call(Method::WebsocketStatus)?;
            print_status(&status, json)
        }
        Command::Enable {
            port,
            bind,
            password,
            no_password,
            json,
        } => {
            let password = if password {
                let password = read_secret("obs-websocket password", "obs-websocket password")?;
                if password.is_empty() {
                    bail!("the password is empty; give one, or use --no-password");
                }
                Some(password)
            } else {
                None
            };
            let status: WebsocketStatus = Client::connect_or_launch()?.call(
                Method::WebsocketEnable(Some(WebsocketEnableParams {
                    port,
                    bind,
                    password,
                    no_password,
                })),
            )?;
            print_status(&status, json)
        }
        Command::Disable { json } => {
            let Some(mut client) = Client::connect()? else {
                bail!("isocastd isn't running; the obs-websocket server is off while it isn't");
            };
            let status: WebsocketStatus = client.call(Method::WebsocketDisable)?;
            print_status(&status, json)
        }
    }
}

fn print_status(status: &WebsocketStatus, json: bool) -> Result<ExitCode> {
    if json {
        println!("{}", serde_json::to_string_pretty(status)?);
    } else {
        print(status);
    }
    Ok(if status.enabled && !status.running {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn print(status: &WebsocketStatus) {
    let state = match (status.enabled, status.running) {
        (false, _) => "off",
        (true, true) => "listening",
        (true, false) => "on, but not listening",
    };
    println!(
        "obs-websocket {state}: {} ({}, obs-websocket {} rpcVersion {})",
        status.url,
        if status.authentication {
            "password required"
        } else {
            "no password"
        },
        status.obs_websocket_version,
        status.rpc_version
    );
    if status.running {
        println!("  {} client(s) connected", status.clients);
    }
    if let Some(password) = &status.generated_password {
        println!("  password (shown once; enter it in the OBS tool): {password}");
    }
    if let Some(error) = &status.last_error {
        println!("  {error}");
    }
}
