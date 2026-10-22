use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use isocast_client::{Client, remote};
use isocast_protocol::{api::Method, remote::*};

#[derive(Args)]
pub struct RemoteArgs {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Status {
        #[arg(long)]
        json: bool,
    },
    Enable {
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        bind: Option<String>,
        #[arg(long)]
        json: bool,
    },
    Disable {
        #[arg(long)]
        json: bool,
    },
    /// Show/create this client's public identity; its private key never leaves the OS store.
    Identity {
        #[arg(long)]
        json: bool,
    },
    /// Open a window locally (--show), or pair with HOST using a code read from stdin.
    Pair {
        host: Option<String>,
        #[arg(long, conflicts_with = "host")]
        show: bool,
        #[arg(long, conflicts_with = "show")]
        code_stdin: bool,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        json: bool,
    },
    Clients {
        #[arg(long)]
        json: bool,
    },
    Revoke {
        client_id: String,
        #[arg(long)]
        json: bool,
    },
    Forget {
        engine_id: String,
        #[arg(long)]
        json: bool,
    },
}

pub fn run(args: RemoteArgs) -> Result<()> {
    match args.command {
        Command::Status { json } => output(
            &Client::connect_or_launch()?.call::<RemoteStatus>(Method::RemoteStatus)?,
            json,
        ),
        Command::Enable { port, bind, json } => output(
            &Client::connect_or_launch()?.call::<RemoteStatus>(Method::RemoteEnable(Some(
                RemoteEnableParams { port, bind },
            )))?,
            json,
        ),
        Command::Disable { json } => output(
            &Client::connect_or_launch()?.call::<RemoteStatus>(Method::RemoteDisable)?,
            json,
        ),
        Command::Identity { json } => output(&remote::identity()?, json),
        Command::Clients { json } => output(
            &Client::connect_or_launch()?.call::<Vec<RemotePeer>>(Method::RemoteClients)?,
            json,
        ),
        Command::Revoke { client_id, json } => output(
            &Client::connect_or_launch()?
                .call::<Vec<RemotePeer>>(Method::RemoteRevoke(RemoteRevokeParams { client_id }))?,
            json,
        ),
        Command::Forget { engine_id, json } => {
            remote::forget(&engine_id)?;
            output(&serde_json::json!({"forgotten":engine_id}), json)
        }
        Command::Pair {
            host,
            show,
            code_stdin,
            name,
            json,
        } => {
            if show {
                let window: RemotePairWindow =
                    Client::connect_or_launch()?.call(Method::RemotePairBegin)?;
                return output(&window, json);
            }
            let host =
                host.context("give a host, or --show to open a pairing window on this engine")?;
            if !code_stdin {
                bail!(
                    "pairing codes are read from stdin, never command-line arguments; use --code-stdin"
                );
            }
            use std::io::{BufRead, Read};
            let mut code = String::new();
            std::io::stdin().lock().take(8).read_line(&mut code)?;
            let result = remote::pair(&host, code.trim(), name.as_deref());
            // No diagnostics contain the code; keep its lifetime within this call.
            code.clear();
            output(&result?, json)
        }
    }
}

fn output(value: &impl serde::Serialize, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(value)?);
    } else {
        println!("{}", serde_json::to_string_pretty(value)?);
    }
    Ok(())
}
