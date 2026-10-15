//! `isocast setup discover-obs`, `isocast stream speed-test` and `isocast
//! optimize` (#46): one call each on the method table (`obs_discover`,
//! `speed_test`, `optimize`).

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use isocast_protocol::api::Method;
use isocast_protocol::obs::Discovery;
use isocast_protocol::optimize::{
    Goal, ObsDiscoverParams, OptimizeParams, Optimized, SpeedTest, SpeedTestParams, mbps,
};
use isocast_protocol::stream::StreamService;

use crate::client::Client;

pub fn parse_goal(text: &str) -> Result<Goal, String> {
    match text.trim().to_ascii_lowercase().as_str() {
        "recording" => Ok(Goal::Recording),
        "streaming" => Ok(Goal::Streaming),
        "both" => Ok(Goal::Both),
        other => Err(format!(
            "unknown goal {other:?}; goals: recording, streaming, both"
        )),
    }
}

pub fn discover(folder: Option<PathBuf>, json: bool) -> Result<ExitCode> {
    let folder = folder
        .map(std::path::absolute)
        .transpose()?
        .map(|f| f.display().to_string());
    let found: Discovery = Client::connect_or_launch()?
        .call(Method::ObsDiscover(Some(ObsDiscoverParams { folder })))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&found)?);
        return Ok(ExitCode::SUCCESS);
    }
    if !found.found {
        println!("No OBS settings in {}.", found.folder);
        return Ok(ExitCode::SUCCESS);
    }
    println!("OBS settings in {}", found.folder);
    println!("Scene collections ({}):", found.collections.len());
    for c in &found.collections {
        let profile = found
            .profiles
            .iter()
            .find(|p| Some(&p.path) == c.profile.as_ref())
            .map(|p| format!(", profile {:?}", p.name))
            .unwrap_or_default();
        println!(
            "  {}{}: {} scene(s), {} source(s){profile}",
            c.name,
            if c.current { " (current)" } else { "" },
            c.scenes,
            c.sources
        );
    }
    println!("Profiles ({}):", found.profiles.len());
    for p in &found.profiles {
        println!(
            "  {}{}: {}{}",
            p.name,
            if p.current { " (current)" } else { "" },
            p.summary,
            if p.stream.as_ref().is_some_and(|s| s.key_in_file) {
                " (stream key in plain text: --obs-stream-key moves it to the Keychain)"
            } else {
                ""
            }
        );
    }
    for e in &found.errors {
        println!("couldn't read {}: {}", e.file, e.message);
    }
    if let Some(c) = found.collections.first() {
        println!(
            "Import with: isocast collection import --obs {:?} --switch",
            c.name
        );
    }
    Ok(ExitCode::SUCCESS)
}

pub fn speed_test(
    service: Option<StreamService>,
    server: Option<String>,
    seconds: Option<u32>,
    json: bool,
) -> Result<ExitCode> {
    if !json {
        isocast_diagnostics::diagnostic!("Testing the upload…");
    }
    let test: SpeedTest =
        Client::connect_or_launch()?.call(Method::SpeedTest(Box::new(SpeedTestParams {
            service,
            server,
            seconds,
        })))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&test)?);
        return Ok(ExitCode::SUCCESS);
    }
    match test.upload_kbps {
        Some(kbps) => println!(
            "Upload to {}: {}{} ({:.1} s, {} bytes, by {})",
            test.server.as_deref().unwrap_or(test.service.id()),
            if test.capped { "at least " } else { "" },
            mbps(kbps),
            test.seconds,
            test.bytes,
            test.method.as_deref().unwrap_or("?")
        ),
        None => println!(
            "Not measured: {}",
            test.reason.as_deref().unwrap_or("no reason")
        ),
    }
    println!("Stream bitrate: {}", mbps(test.recommended_kbps));
    Ok(ExitCode::SUCCESS)
}

pub fn optimize(params: OptimizeParams, json: bool) -> Result<ExitCode> {
    if !json {
        isocast_diagnostics::diagnostic!(
            "Optimizing for {}; benchmarking this Mac takes a minute or two…",
            params.goal.id()
        );
    }
    let apply = params.apply;
    let done: Optimized = Client::connect_or_launch()?.call(Method::Optimize(Box::new(params)))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&done)?);
        return Ok(ExitCode::SUCCESS);
    }
    println!("{}", done.summary);
    if let Some(speed) = &done.speed
        && let Some(kbps) = speed.upload_kbps
    {
        println!("Upload: {}", mbps(kbps));
    }
    for d in &done.differences {
        let show = |v: &Option<serde_json::Value>| {
            v.as_ref()
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into())
        };
        println!("  {}: {} → {}", d.path, show(&d.from), show(&d.to));
    }
    for note in &done.notes {
        println!("note: {note}");
    }
    match (&done.applied, apply, done.differences.is_empty()) {
        (_, _, true) => println!("Nothing to change."),
        (Some(applied), _, _) => println!("Applied (revision {}).", applied.revision),
        (None, false, _) => println!("Make these changes with --apply."),
        (None, true, _) => {}
    }
    Ok(ExitCode::SUCCESS)
}
