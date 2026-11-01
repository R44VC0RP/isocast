//! Reclaim only identified, dead, day-old Windows record workspaces, after
//! recovery. Old unmarked workspaces cannot prove ownership and are retained.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use isocast_platform_windows::process_started;
use isocast_protocol::api::{Event, setup_path, socket_path};
use serde::{Deserialize, Serialize};

const OWNER: &str = "record-owner.json";
const DAY: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Owner {
    pid: u32,
    created: u64,
}

/// Keep the marker locked for the entire daemon lifetime. The process identity
/// also protects a live owner when another process inspects an unlocked copy.
pub(crate) fn register() -> Result<Option<File>> {
    if !std::env::args_os().any(|arg| arg == "--exit-on-stdin-close") {
        return Ok(None);
    }
    let setup = setup_path();
    let workspace = setup.parent().context("isolated setup has no parent")?;
    if !candidate(workspace) || socket_path().parent() != Some(workspace) {
        bail!("isolated workspace must be an isocast- directory directly in temp");
    }
    let pid = std::process::id();
    let created = process_started(pid)?.context("could not identify isolated daemon")?;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(workspace.join(OWNER))?;
    file.lock()?;
    serde_json::to_writer(&mut file, &Owner { pid, created })?;
    file.flush()?;
    file.sync_all()?;
    Ok(Some(file))
}

fn plain(path: &Path) -> Result<std::fs::Metadata> {
    let metadata = path.symlink_metadata()?;
    // Includes junctions and other reparse points, not just symbolic links.
    if metadata.file_attributes() & 0x400 != 0 {
        bail!("reparse point: {}", path.display());
    }
    Ok(metadata)
}

fn candidate(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with("isocast-"))
        && path.parent().and_then(|p| p.canonicalize().ok())
            == std::env::temp_dir().canonicalize().ok()
        && plain(path).is_ok_and(|m| m.is_dir())
}

pub(crate) fn sweep(daemon: &crate::Daemon) {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let workspace = entry.path();
        if !candidate(&workspace) || !workspace.join(OWNER).exists() {
            continue;
        }
        if let Err(error) = reclaim(daemon, &workspace) {
            isocast_diagnostics::diagnostic!("warn"; "retained isolated workspace {}: {error:#}", workspace.display());
        }
    }
}

fn reclaim(daemon: &crate::Daemon, workspace: &Path) -> Result<()> {
    let path = workspace.join(OWNER);
    let metadata = plain(&path)?;
    if !metadata.is_file()
        || metadata.len() > 4096
        || SystemTime::now()
            .duration_since(metadata.modified()?)
            .unwrap_or_default()
            <= DAY
    {
        return Ok(());
    }
    let mut marker = OpenOptions::new().read(true).write(true).open(&path)?;
    if marker.try_lock().is_err() {
        return Ok(());
    }
    let owner: Owner = serde_json::from_reader((&mut marker).take(4097))?;
    if owner.pid == 0 || owner.created == 0 {
        bail!("invalid process identity");
    }
    if process_started(owner.pid)? == Some(owner.created) {
        return Ok(());
    }
    // Never remove recordings, unknown files or links, even if someone placed
    // them inside the otherwise-private runtime tree. This also bounds the walk.
    runtime_only(workspace, &mut 1024)?;
    let registry = workspace.join("setup.recording-folders.json");
    let roots: Vec<PathBuf> = if registry.exists() {
        let metadata = plain(&registry)?;
        if !metadata.is_file() || metadata.len() > 64 * 1024 {
            bail!("invalid recording registry");
        }
        serde_json::from_reader(File::open(&registry)?.take(64 * 1024 + 1))?
    } else if workspace.join("setup.toml").exists() || workspace.join("setup").exists() {
        bail!("setup without a recording registry; cannot establish recovery roots");
    } else {
        Vec::new()
    };
    if roots.len() > 20 {
        bail!("recording registry exceeds 20 roots");
    }
    for root in roots {
        if !root.is_absolute() || !plain(&root)?.is_dir() {
            bail!("invalid recording root");
        }
        let mut folders = vec![root.clone()];
        for (n, entry) in std::fs::read_dir(&root)?.enumerate() {
            if n >= 4096 {
                bail!("recording root exceeds cleanup scan limit");
            }
            let path = entry?.path();
            // The root itself and its recording projects, as in startup
            // recovery. Never create recovery lock files in unrelated folders.
            if plain(&path)?.is_dir()
                && (path.join(isocast_protocol::project::PROJECT_FILE).exists()
                    || path.file_name().is_some_and(|name| {
                        isocast_protocol::project::named_like_project(&name.to_string_lossy())
                    }))
            {
                if folders.len() > 50 {
                    bail!("recording root exceeds project scan limit");
                }
                folders.push(path);
            }
        }
        for folder in folders {
            // #118's pinned-directory lease verifies media and persists repaired
            // manifests BEFORE deletion. Failed/partial recovery retains this
            // workspace and its registry for the next launch.
            let recovered = isocast_engine::record::recover_directory(&folder)?;
            if recovered.incomplete || !recovered.warnings.is_empty() {
                bail!("recording recovery incomplete in {}", folder.display());
            }
            let mut interrupted = daemon.interrupted.lock();
            let mut notices = Vec::new();
            for take in recovered.sessions {
                // Ordinary startup recovery ran first. Do not replace its
                // newly_detected=true with this verification pass's false.
                if !interrupted.iter().any(|old| old.manifest == take.manifest) {
                    if take.newly_detected {
                        notices.push(take.manifest.clone());
                    }
                    interrupted.push(take);
                }
            }
            interrupted.sort_by(|a, b| b.recording.started_at.cmp(&a.recording.started_at));
            interrupted.truncate(20);
            drop(interrupted);
            for manifest in notices {
                daemon.interrupted_unnoted.lock().push(manifest.clone());
                daemon.publish(Event::Warning {
                    message: format!("recording recovery inspected {}; see the manifest and status.interrupted for each file's verified media, unknown counts and any persistence errors", manifest.display()),
                });
            }
        }
        // Keep discoverability when only the isolated registry knew this root.
        isocast_protocol::project::remember_root(&root)?;
    }
    // The marker lock serializes sweepers. An exited identity cannot become live
    // again; a reused PID has a different creation time. The normal close path
    // still belongs to the client after successful child exit.
    std::fs::remove_dir_all(workspace)?;
    isocast_diagnostics::diagnostic!(
        "removed stale isolated workspace {} after recovery",
        workspace.display()
    );
    Ok(())
}

fn runtime_only(path: &Path, remaining: &mut usize) -> Result<()> {
    for entry in std::fs::read_dir(path)? {
        *remaining = remaining
            .checked_sub(1)
            .context("workspace exceeds cleanup scan limit")?;
        let path = entry?.path();
        let metadata = plain(&path)?;
        if metadata.is_dir() {
            runtime_only(&path, remaining)?;
        } else {
            let name = path
                .file_name()
                .context("file without name")?
                .to_string_lossy();
            if !metadata.is_file()
                || name.contains(".session.json")
                || name == ".isocast-project.json"
                || !matches!(
                    path.extension().and_then(|s| s.to_str()),
                    Some("json" | "toml" | "log" | "jsonl" | "run" | "bak" | "lock" | "sock")
                )
            {
                bail!("workspace contains non-runtime file {}", path.display());
            }
        }
    }
    Ok(())
}
