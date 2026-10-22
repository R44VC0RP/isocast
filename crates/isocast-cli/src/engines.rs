//! Paired identities alongside untrusted DNS-SD discovery hints.

use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use isocast_discovery::{Event, Found, Txt};
use serde_json::{Value, json};

pub fn run(_discover: bool, timeout: f64, json: bool) -> Result<ExitCode> {
    let paired = isocast_client::remote::engines()?;
    if !(0.1..=60.0).contains(&timeout) {
        bail!("--timeout: {timeout} s; use 0.1 to 60 seconds");
    }
    let found = isocast_discovery::browse(Duration::from_secs_f64(timeout))?;
    if json {
        let discovered: Vec<Value> = found.iter().map(to_json).collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "paired": paired, "discovered": discovered }))?
        );
        return Ok(ExitCode::SUCCESS);
    }
    if !paired.is_empty() {
        println!("{} paired engine(s), pinned identities:", paired.len());
        for peer in &paired {
            println!(
                "  {}  {}  {}",
                peer.name,
                peer.id,
                peer.address.as_deref().unwrap_or("no saved address")
            );
        }
    }
    if found.is_empty() {
        println!("No engines found on the local network within {timeout} s.");
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "{} found on the local network (advertised, not verified):",
        match found.len() {
            1 => "1 engine".to_owned(),
            n => format!("{n} engines"),
        }
    );
    for engine in &found {
        let addrs: Vec<String> = engine.addrs.iter().map(address).collect();
        println!("  {}", engine.name);
        println!("    host     {}:{}", engine.host, engine.port);
        println!("    address  {}", addrs.join(", "));
        if !engine.txt.id.is_empty() {
            println!("    id       {}", engine.txt.id);
        }
        if !engine.txt.name.is_empty() && engine.txt.name != engine.name {
            println!("    shows as {}", engine.txt.name);
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `--discover --watch`: print engines as they appear and go away.
pub fn watch(discover: bool, json: bool) -> Result<ExitCode> {
    if !discover {
        bail!("--watch: only with --discover");
    }
    let (stop, stopped) = std::sync::mpsc::channel();
    ctrlc::set_handler(move || {
        let _ = stop.send(());
    })
    .context("could not catch Ctrl-C")?;
    let watching = isocast_discovery::watch(Box::new(move |event| match (event, json) {
        (Event::Found(engine), true) => {
            let mut line = to_json(&engine);
            line["event"] = json!("found");
            println!("{line}");
        }
        (Event::Lost { name }, true) => println!("{}", json!({ "event": "lost", "name": name })),
        (Event::Found(engine), false) => {
            let addrs: Vec<String> = engine.addrs.iter().map(address).collect();
            println!(
                "found  {}  {}:{}  {}",
                engine.name,
                engine.host,
                engine.port,
                addrs.join(", ")
            );
        }
        (Event::Lost { name }, false) => println!("lost   {name}"),
        (Event::Error(reason), true) => {
            println!("{}", json!({ "event": "error", "reason": reason }))
        }
        (Event::Error(reason), false) => println!("error  {reason}"),
    }))?;
    if !json {
        println!("Watching for engines on the local network; Ctrl-C to stop.");
    }
    let _ = stopped.recv();
    drop(watching);
    Ok(ExitCode::SUCCESS)
}

/// An address without its port; link-local IPv6 keeps its scope.
fn address(addr: &SocketAddr) -> String {
    match addr {
        SocketAddr::V4(v4) => v4.ip().to_string(),
        SocketAddr::V6(v6) if v6.scope_id() != 0 => format!("{}%{}", v6.ip(), v6.scope_id()),
        SocketAddr::V6(v6) => v6.ip().to_string(),
    }
}

fn to_json(engine: &Found) -> Value {
    json!({
        "name": engine.name,
        "host": engine.host,
        "port": engine.port,
        "addrs": engine.addrs.iter().map(address).collect::<Vec<_>>(),
        "txt": {
            "id": engine.txt.id,
            "proto": engine.txt.proto,
            "name": engine.txt.name,
        },
    })
}

/// `isocast debug advertise`: hold a test advertisement until Ctrl-C.
pub fn advertise(
    name: &str,
    port: u16,
    id: &str,
    seconds: Option<u64>,
    json: bool,
) -> Result<ExitCode> {
    let (stop, stopped) = std::sync::mpsc::channel();
    ctrlc::set_handler(move || {
        let _ = stop.send(());
    })
    .context("could not catch Ctrl-C")?;
    let txt = Txt {
        id: id.to_owned(),
        proto: "1".to_owned(),
        name: name.to_owned(),
    };
    let registration = isocast_discovery::register(name, port, &txt)?;
    if json {
        println!(
            "{}",
            json!({
                "advertising": registration.name(),
                "service": isocast_discovery::SERVICE_TYPE,
                "port": port,
            })
        );
    } else {
        println!(
            "Advertising {:?} ({}) on port {port}; Ctrl-C to stop.",
            registration.name(),
            isocast_discovery::SERVICE_TYPE
        );
    }
    match seconds {
        Some(seconds) => {
            let _ = stopped.recv_timeout(Duration::from_secs(seconds));
        }
        None => {
            let _ = stopped.recv();
        }
    }
    drop(registration);
    if !json {
        println!("Stopped advertising.");
    }
    Ok(ExitCode::SUCCESS)
}
