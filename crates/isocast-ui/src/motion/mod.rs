//! The motion harness (`isocast-ui --motion SUITE.toml`): replays scripted
//! pointer and keyboard input into the real interface, offscreen, a frame
//! at a time, and checks every frame for lag behind the pointer, jumps,
//! stalls, blank frames and flicker. One scenario and variant per process
//! (Slint's platform is set once); `scripts/motion.sh` runs a suite
//! against a throwaway engine. The design is in docs/motion-testing.md.
//!
//! ```text
//! isocast-ui --motion SUITE.toml --list
//! isocast-ui --motion SUITE.toml --scenario NAME --variant idle|recording
//!            [--out DIR] [--clock lockstep|realtime] [--picture engine|test]
//!            [--size WxH]
//!            [--no-timing] [--frames] [--dump-frames N,M,…]
//! ```

mod measure;
mod runner;
mod scenario;
mod sheet;

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

const USAGE: &str = "usage: isocast-ui --motion SUITE.toml --list
       isocast-ui --motion SUITE.toml --scenario NAME --variant idle|recording
                  [--out DIR] [--clock lockstep|realtime] [--picture engine|test]
                  [--size WxH] [--no-timing] [--frames] [--dump-frames N,M,…]";

/// The run's folder under `out`, made ready: a plain child named after the
/// run (names are checked when the suite loads, and again here), not a
/// symlink, with only the files an earlier run of the harness wrote there
/// removed. Nothing outside it is touched.
fn run_folder(out: &std::path::Path, run: &scenario::Run) -> Result<PathBuf> {
    if !scenario::safe_name(&run.name) || !["idle", "recording"].contains(&run.variant.as_str()) {
        bail!("run {:?} isn't a plain name", run.id());
    }
    std::fs::create_dir_all(out).with_context(|| format!("can't make {}", out.display()))?;
    let out = out
        .canonicalize()
        .with_context(|| format!("can't resolve {}", out.display()))?;
    let dir = out.join(run.id());
    match std::fs::symlink_metadata(&dir) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            bail!(
                "{} exists and isn't a folder the harness made",
                dir.display()
            )
        }
        Ok(_) => {
            // Only what the harness writes in a run's folder.
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                let ours = matches!(name.as_str(), "result.json" | "frames.jsonl" | "sheet.png")
                    || (name.starts_with("frame-") && name.ends_with(".png"));
                let kind = entry.file_type()?;
                // A name the harness writes that isn't a plain file (a
                // symlink above all) would be written through: refuse.
                if (ours && !kind.is_file()) || (name == "frames" && !kind.is_dir()) {
                    bail!(
                        "{} isn't a file the harness wrote; refusing to write there",
                        entry.path().display()
                    );
                }
                if ours {
                    std::fs::remove_file(entry.path())?;
                } else if name == "frames" {
                    for frame in std::fs::read_dir(entry.path())? {
                        let frame = frame?;
                        let n = frame.file_name().to_string_lossy().into_owned();
                        let numbered = n.len() == 9
                            && n.ends_with(".png")
                            && n[..5].chars().all(|c| c.is_ascii_digit());
                        if numbered && !frame.file_type()?.is_file() {
                            bail!("{} isn't a file the harness wrote", frame.path().display());
                        }
                        if numbered {
                            std::fs::remove_file(frame.path())?;
                        }
                    }
                }
            }
        }
        Err(_) => {
            std::fs::create_dir(&dir).with_context(|| format!("can't make {}", dir.display()))?
        }
    }
    if dir.canonicalize()?.parent() != Some(out.as_path()) {
        bail!("{} isn't inside {}", dir.display(), out.display());
    }
    Ok(dir)
}

/// Whether the run passed (Err: it couldn't run).
pub fn main(args: Vec<String>) -> Result<bool> {
    let mut suite = None;
    let mut only = None;
    let mut variant = None;
    let mut out = PathBuf::from("target/scratch/motion");
    let mut list = false;
    let mut clock = None;
    let mut picture = None;
    let mut size = None;
    let mut timing = true;
    let mut frames = false;
    let mut dump: Vec<usize> = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next()
                .with_context(|| format!("{name} needs a value\n{USAGE}"))
        };
        match arg.as_str() {
            "--motion" => suite = Some(PathBuf::from(value("--motion")?)),
            "--scenario" => only = Some(value("--scenario")?),
            "--variant" => variant = Some(value("--variant")?),
            "--out" => out = value("--out")?.into(),
            "--clock" => clock = Some(value("--clock")?),
            "--picture" => picture = Some(value("--picture")?),
            "--size" => size = Some(value("--size")?),
            "--list" => list = true,
            "--no-timing" => timing = false,
            "--frames" => frames = true,
            "--dump-frames" => {
                dump = value("--dump-frames")?
                    .split(',')
                    .map(|n| n.trim().parse())
                    .collect::<Result<_, _>>()
                    .context("--dump-frames takes frame numbers, like 10,11,12")?
            }
            other => bail!("unknown option {other:?}\n{USAGE}"),
        }
    }
    let suite_path = suite.context(USAGE)?;
    // The harness drives a real engine (and can record): only one the
    // caller isolated, never the default socket and live setup.
    for var in ["ISOCAST_SOCKET", "ISOCAST_SETUP"] {
        if std::env::var_os(var).is_none_or(|v| v.is_empty()) && !list {
            bail!(
                "--motion needs {var} set to an isolated engine's (scripts/motion.sh sets it up); it never drives the default engine"
            );
        }
    }
    let mut loaded = scenario::load(&suite_path)?;
    if let Some(clock) = clock {
        loaded.defaults.clock = clock.clone();
        for s in &mut loaded.scenarios {
            s.clock = Some(clock.clone());
        }
    }
    for s in &mut loaded.scenarios {
        if let Some(picture) = &picture {
            s.picture = Some(picture.clone());
        }
        if let Some(size) = &size {
            s.size = Some(size.clone());
        }
    }
    let runs = scenario::runs(&loaded, only.as_deref(), variant.as_deref())?;
    if list {
        let mut stdout = std::io::stdout();
        for run in &runs {
            writeln!(stdout, "{} {}", run.name, run.variant)?;
        }
        return Ok(true);
    }
    let [run] = <[_; 1]>::try_from(runs).map_err(|runs: Vec<scenario::Run>| {
        let ids: Vec<String> = runs.iter().map(|r| r.id()).collect();
        anyhow::anyhow!(
            "one run at a time (scripts/motion.sh runs a suite); --scenario and --variant pick one of {}",
            ids.join(", ")
        )
    })?;
    let mut run = run;
    if !timing {
        run.thresholds.frame_ms_max = None;
        run.thresholds.late_frames = None;
    }
    let dir = run_folder(&out, &run)?;
    let axis = measure::Tracker::new(&run.target, run.engine_picture)?.axis();
    let recorded = runner::run(run.clone(), false, &dump, &dir)?;
    let mut lines = String::new();
    for f in &recorded.frames {
        lines.push_str(&serde_json::to_string(f)?);
        lines.push('\n');
    }
    std::fs::write(dir.join("frames.jsonl"), lines)?;
    if frames {
        sheet::write_frames(&dir.join("frames"), &recorded)?;
    }
    let result = measure::analyse(&run, &recorded, axis, &dir)?;
    std::fs::write(
        dir.join("result.json"),
        serde_json::to_string_pretty(&result)?,
    )?;
    println!("{}", serde_json::to_string(&result)?);
    // A control is judged on whether it came out as expected.
    Ok(result["as_expected"]
        .as_bool()
        .or(result["pass"].as_bool())
        .unwrap_or(false))
}
