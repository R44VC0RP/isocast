//! Thin client for the profile's hotkey methods.

use crate::client::Client;
use anyhow::{Context, Result, bail};
use clap::Subcommand;
use isocast_protocol::api::Method;
use isocast_protocol::hotkeys::{Binding, Catalogue, Chord, Hotkeys, Phase, SetParams};
use std::process::ExitCode;

#[derive(Subcommand)]
pub enum HotkeysCommand {
    /// Current profile bindings and native registration warnings.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Bind an id to a chord and action (replace that binding only).
    Set {
        id: String,
        keys: String,
        action: String,
        /// Typed action arguments as JSON; see `hotkeys actions --json`.
        #[arg(long, default_value = "{}")]
        args: String,
        #[arg(long)]
        json: bool,
    },
    /// Remove a binding by id.
    Unset {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Stable action catalogue, schemas, key names and suggested defaults.
    Actions {
        #[arg(long)]
        json: bool,
    },
    /// Add defaults without replacing custom ids or occupied chords.
    Defaults {
        /// Explicitly replace the whole set with defaults.
        #[arg(long)]
        replace: bool,
        #[arg(long)]
        json: bool,
    },
    /// Forward focused fallback input. A one-shot press pulses; --hold-ms
    /// keeps the connection open and sends a release after the hold.
    Input {
        keys: String,
        #[arg(long, default_value = "press", value_parser = ["press", "release"])]
        phase: String,
        #[arg(long, value_parser = clap::value_parser!(u64).range(0..=60_000))]
        hold_ms: Option<u64>,
        #[arg(long)]
        json: bool,
    },
}

pub fn run(command: HotkeysCommand) -> Result<ExitCode> {
    let mut client = Client::connect_or_launch()?;
    let (method, json) = match command {
        HotkeysCommand::Input {
            keys,
            phase,
            hold_ms,
            json,
        } => {
            if phase == "release" && hold_ms.is_some() {
                bail!("hold_ms: only valid with phase press");
            }
            let phase = if phase == "press" {
                Phase::Press
            } else {
                Phase::Release
            };
            let mut result = client.hotkey_input(keys.clone(), phase)?;
            if let Some(ms) = hold_ms {
                std::thread::sleep(std::time::Duration::from_millis(ms));
                result = serde_json::json!({"press":result,"release":client.hotkey_input(keys, Phase::Release)?});
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("{result}");
            }
            return Ok(ExitCode::SUCCESS);
        }
        HotkeysCommand::Actions { json } => {
            let catalogue: Catalogue = client.call(Method::HotkeysActions)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&catalogue)?);
            } else {
                for action in &catalogue.actions {
                    println!("{}  [{}] {}", action.id, action.group, action.label);
                }
                println!("\nArgument schemas and keys: --json. Syntax: docs/hotkeys.md.");
            }
            return Ok(ExitCode::SUCCESS);
        }
        HotkeysCommand::List { json } => (Method::HotkeysGet, json),
        HotkeysCommand::Set {
            id,
            keys,
            action,
            args,
            json,
        } => {
            let args = serde_json::from_str(&args).context("args: expected a JSON object")?;
            (
                Method::HotkeysSet(Box::new(SetParams {
                    patch: Some([(id, Some(Binding { keys, action, args }))].into()),
                    ..Default::default()
                })),
                json,
            )
        }
        HotkeysCommand::Unset { id, json } => (
            Method::HotkeysSet(Box::new(SetParams {
                patch: Some([(id, None)].into()),
                ..Default::default()
            })),
            json,
        ),
        HotkeysCommand::Defaults { replace, json } => {
            let catalogue: Catalogue = client.call(Method::HotkeysActions)?;
            if !replace {
                let current: Hotkeys = client.call(Method::HotkeysGet)?;
                let configured = current
                    .configured
                    .value
                    .as_ref()
                    .and_then(serde_json::Value::as_object);
                let occupied: Vec<_> = current
                    .bindings
                    .values()
                    .filter_map(|b| Chord::parse(&b.keys).ok())
                    .collect();
                let patch = catalogue
                    .defaults
                    .into_iter()
                    .filter(|(id, binding)| {
                        !configured.is_some_and(|c| c.contains_key(id))
                            && !current.bindings.contains_key(id)
                            && !Chord::parse(&binding.keys).is_ok_and(|key| occupied.contains(&key))
                    })
                    .map(|(id, binding)| (id, Some(binding)))
                    .collect();
                (
                    Method::HotkeysSet(Box::new(SetParams {
                        patch: Some(patch),
                        revision: Some(current.revision),
                        ..Default::default()
                    })),
                    json,
                )
            } else {
                (
                    Method::HotkeysSet(Box::new(SetParams {
                        bindings: Some(catalogue.defaults),
                        ..Default::default()
                    })),
                    json,
                )
            }
        }
    };
    let hotkeys: Hotkeys = client.call(method)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&hotkeys)?);
    } else {
        println!(
            "Profile: {} (revision {})",
            hotkeys.profile.as_deref().unwrap_or("none"),
            hotkeys.revision
        );
        for (id, binding) in hotkeys.bindings {
            println!(
                "{id}: {} → {} {}",
                binding.keys, binding.action, binding.args
            );
        }
        for registration in hotkeys.registrations {
            if let Some(warning) = registration.warning {
                println!("{}: {warning}", registration.binding);
            }
        }
        for warning in hotkeys.warnings {
            println!("warning: {warning}");
        }
    }
    Ok(ExitCode::SUCCESS)
}
