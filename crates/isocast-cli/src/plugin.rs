//! `isocast plugin …`: installing, listing, removing and turning plugins
//! on and off. Each command is one `plugin_*` call on the method table.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Subcommand;
use isocast_protocol::api::Method;
use isocast_protocol::plugin::{
    KindClass, Manifest, PluginAddParams, PluginChange, PluginEnableParams, PluginInfo,
    PluginState, PluginTarget, Plugins, ProcessState,
};

use crate::client::Client;

#[derive(Subcommand)]
pub enum PluginCommand {
    /// Install a plugin from its folder (one with an isocast-plugin.toml):
    /// the folder is copied into the plugins folder and its process, if it
    /// has one, starts at once. Without --allow it only lists the
    /// permissions the plugin asks for.
    Add {
        path: PathBuf,
        /// Grant the permissions it asks for (they are printed without
        /// this).
        #[arg(long)]
        allow: bool,
        /// Link to the folder instead of copying it, for developing a
        /// plugin: it is read again each time the plugin starts.
        #[arg(long)]
        link: bool,
        /// Install it even if the same or a newer version is installed.
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// List the installed plugins: version, what they provide, state,
    /// process and what they were granted.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Stop a plugin's process and remove it (its data folder is kept if
    /// it wrote anything there). Refused while the live setup uses its
    /// kinds, unless --force.
    Remove {
        id: String,
        /// Remove it even though the live setup uses its kinds: they stay
        /// in the setup, bypassed, until the plugin is added again.
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// Turn a plugin on again (also after it crashed too often).
    Enable {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Turn a plugin off: its process stops until it is enabled.
    Disable {
        id: String,
        #[arg(long)]
        json: bool,
    },
}

pub fn run(command: PluginCommand) -> Result<ExitCode> {
    let mut client = Client::connect_or_launch()?;
    let (method, json) = match command {
        PluginCommand::List { json } => {
            let plugins: Plugins = client.call(Method::PluginList)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&plugins)?);
            } else {
                print_list(&plugins);
            }
            return Ok(ExitCode::SUCCESS);
        }
        PluginCommand::Add {
            path,
            allow,
            link,
            force,
            json,
        } => {
            // isocastd doesn't run in this folder.
            let path = std::path::absolute(&path)
                .with_context(|| format!("can't make {} absolute", path.display()))?;
            // Granting is giving back exactly what it asks for; if the
            // manifest can't be read here, isocastd says why.
            let grant = if allow {
                Manifest::read(&path)
                    .ok()
                    .map(|manifest| manifest.permissions)
            } else {
                None
            };
            let method = Method::PluginAdd(Box::new(PluginAddParams {
                path,
                link,
                grant,
                force,
            }));
            (method, json)
        }
        PluginCommand::Remove { id, force, json } => {
            (Method::PluginRemove(PluginTarget { id, force }), json)
        }
        PluginCommand::Enable { id, json } => (
            Method::PluginSetEnabled(PluginEnableParams { id, enabled: true }),
            json,
        ),
        PluginCommand::Disable { id, json } => (
            Method::PluginSetEnabled(PluginEnableParams { id, enabled: false }),
            json,
        ),
    };
    let change: PluginChange = client.call(method)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&change)?);
    } else {
        let verb = serde_json::to_value(change.action)?
            .as_str()
            .unwrap_or_default()
            .to_string();
        let verb = format!("{}{}", verb[..1].to_uppercase(), &verb[1..]);
        println!("{verb} {} {}", change.plugin.id, change.plugin.version);
        print_plugin(&change.plugin);
        for note in &change.notes {
            println!("  note: {note}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn print_list(plugins: &Plugins) {
    if plugins.plugins.is_empty() {
        println!(
            "No plugins installed in {} (`isocast plugin add FOLDER` installs one).",
            plugins.folder.display()
        );
        return;
    }
    for (i, plugin) in plugins.plugins.iter().enumerate() {
        if i > 0 {
            println!();
        }
        println!("{} {}  {}", plugin.id, plugin.version, plugin.name);
        print_plugin(plugin);
    }
}

fn print_plugin(plugin: &PluginInfo) {
    let state = match plugin.state {
        PluginState::Ok => "ok".to_string(),
        PluginState::Disabled => "disabled".to_string(),
        PluginState::Incompatible => "incompatible".to_string(),
        PluginState::Failed => "failed".to_string(),
    };
    match &plugin.reason {
        Some(reason) => println!("  state: {state}: {reason}"),
        None => println!("  state: {state}"),
    }
    if !plugin.description.is_empty() {
        println!("  {}", plugin.description);
    }
    let provides = provides(plugin);
    if !provides.is_empty() {
        println!("  provides: {provides}");
    }
    if let Some(process) = &plugin.process {
        let mut text = match process.state {
            ProcessState::Running => format!(
                "running{}",
                process
                    .pid
                    .map(|pid| format!(" (pid {pid})"))
                    .unwrap_or_default()
            ),
            ProcessState::Restarting => "restarting".into(),
            ProcessState::Stopped => "stopped".into(),
        };
        if process.restarts > 0 {
            text.push_str(&format!(", {} restart(s)", process.restarts));
        }
        if let Some(exit) = &process.last_exit {
            text.push_str(&format!(", last {exit}"));
        }
        if !process.sandboxed {
            text.push_str(", not sandboxed");
        }
        println!("  process: {} ({text})", process.command.join(" "));
    }
    if let Some(log) = &plugin.log {
        println!("  log: {}", log.display());
    }
    for line in plugin.granted.describe() {
        println!("  granted {line}");
    }
    println!(
        "  files: {}{}",
        plugin.path.display(),
        if plugin.linked { " (linked)" } else { "" }
    );
}

/// What a plugin provides, in words.
fn provides(info: &PluginInfo) -> String {
    let mut parts: Vec<String> = Vec::new();
    if info.process.is_some() {
        parts.push("a process".into());
    }
    for class in [
        KindClass::Feed,
        KindClass::Node,
        KindClass::Filter,
        KindClass::Transition,
        KindClass::AudioFilter,
        KindClass::Source,
    ] {
        let kinds: Vec<&str> = info
            .kinds
            .iter()
            .filter(|kind| kind.what == class)
            .map(|kind| kind.kind.as_str())
            .collect();
        if !kinds.is_empty() {
            parts.push(format!("{}: {}", class.section(), kinds.join(", ")));
        }
    }
    parts.join("; ")
}
