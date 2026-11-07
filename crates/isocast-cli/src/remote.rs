use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use isocast_client::{Client, remote};
use isocast_protocol::{api::Method, hello::EngineOs, remote::*};

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
    #[cfg_attr(
        target_os = "linux",
        command(
            about = "Show/create this client's public identity; its private key stays in the desktop keyring (an owner-only 0600 file without one)."
        )
    )]
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
        Command::Status { json } => status(Method::RemoteStatus, json),
        Command::Enable { port, bind, json } => status(
            Method::RemoteEnable(Some(RemoteEnableParams { port, bind })),
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

/// Status and enable print the engine's state; on a Linux engine the human
/// form adds one line saying where its key lives (`--json` is unchanged).
fn status(method: Method, json: bool) -> Result<()> {
    let mut client = Client::connect_or_launch()?;
    let status: RemoteStatus = client.call(method)?;
    output(&status, json)?;
    let os = client.hello().map_or(EngineOs::current(), |hello| hello.os);
    if let Some(line) = key_note(os, &status, json) {
        println!("{line}");
    }
    Ok(())
}

fn key_note(os: EngineOs, status: &RemoteStatus, json: bool) -> Option<&'static str> {
    (!json && os == EngineOs::Linux && status.identity.is_some()).then_some(
        "note: this Linux engine's remote key is stored in its desktop keyring, or, on a \
         computer without one, in a plaintext 0600 file in $XDG_CONFIG_HOME/isocast/credentials \
         (default ~/.config/isocast/credentials)",
    )
}

fn output(value: &impl serde::Serialize, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(value)?);
    } else {
        println!("{}", serde_json::to_string_pretty(value)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(identity: bool) -> RemoteStatus {
        RemoteStatus {
            enabled: true,
            running: true,
            identity: identity.then(|| RemoteIdentity {
                id: "id".into(),
                name: "engine".into(),
                spki: "spki".into(),
            }),
            address: None,
            pairing_open: false,
            error: None,
        }
    }

    #[test]
    fn linux_key_note_is_human_only_and_needs_a_key() {
        let note = key_note(EngineOs::Linux, &sample(true), false).unwrap();
        assert!(note.contains("keyring") && note.contains("0600 file") && !note.contains('\n'));
        assert!(key_note(EngineOs::Linux, &sample(true), true).is_none());
        assert!(key_note(EngineOs::Linux, &sample(false), false).is_none());
        assert!(key_note(EngineOs::Macos, &sample(true), false).is_none());
        assert!(key_note(EngineOs::Windows, &sample(true), false).is_none());
    }
}
