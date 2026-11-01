//! Idle-only calibration in a killable helper: native calls cannot enforce
//! a deadline themselves. No capture or network code runs in the helper.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use isocast_engine::capacity;
use isocast_protocol::api::RpcError;
use isocast_protocol::machine::MachineStatus;

use crate::Daemon;

const LIMIT: Duration = Duration::from_secs(29);

#[derive(Default)]
pub(super) struct Calibration {
    child: Option<Child>,
    started: Option<Instant>,
    scratch: Option<PathBuf>,
    path: PathBuf,
    status: MachineStatus,
    automatic: bool,
    /// The running helper was scheduled automatically, rather than requested.
    background: bool,
}

impl Calibration {
    fn failure_path(&self) -> PathBuf {
        self.path.with_extension("failed-build")
    }

    fn failed_this_build(&self) -> bool {
        use std::io::Read;
        let mut build = String::new();
        fs::File::open(self.failure_path())
            .and_then(|file| file.take(128).read_to_string(&mut build))
            .is_ok()
            && build == crate::daemon_build().build
    }

    fn remember_failure(&mut self) {
        self.automatic = false;
        let temporary = self
            .path
            .with_extension(format!("failed-build.{}.tmp", std::process::id()));
        let result = (|| -> std::io::Result<()> {
            if let Some(parent) = self.path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(crate::daemon_build().build.as_bytes())?;
            file.sync_all()?;
            isocast_protocol::fs::replace(&temporary, self.failure_path())
        })();
        if let Err(error) = result {
            self.status
                .summary
                .push_str(&format!("; could not remember the failed build: {error}"));
        }
        let _ = fs::remove_file(temporary);
    }

    fn stop(&mut self, reason: &str) -> anyhow::Result<()> {
        if let Some(child) = self.child.as_mut() {
            if child.try_wait()?.is_none() {
                child.kill()?;
            }
            // Reap before permitting recording: an orphan encoder would
            // compete with the very output which cancelled calibration.
            child.wait()?;
        }
        self.child = None;
        self.started = None;
        self.status.running = false;
        self.status.summary = reason.into();
        if let Some(scratch) = self.scratch.take() {
            let _ = fs::remove_dir_all(scratch);
        }
        Ok(())
    }

    fn status(&self, full: bool) -> MachineStatus {
        let mut status = self.status.clone();
        status.elapsed_seconds = self.started.map_or(0.0, |at| at.elapsed().as_secs_f64());
        if !status.running
            && let Some(profile) = &status.profile
        {
            let measured = profile.summary();
            if status.summary != measured {
                status.summary = format!("{measured}. {}", status.summary);
            }
        }
        if !full {
            status.profile = None;
        }
        status
    }
}

impl Drop for Calibration {
    fn drop(&mut self) {
        let _ = self.stop("Calibration stopped");
    }
}

fn failed(error: impl std::fmt::Display) -> RpcError {
    RpcError {
        code: RpcError::FAILED,
        message: format!("machine calibration: {error}"),
    }
}

fn busy() -> RpcError {
    RpcError {
        code: RpcError::WRONG_STATE,
        message: "machine calibration: stop recording, streaming, replay, virtual camera, tuning and exports first".into(),
    }
}

/// A local name is not proof of ownership: retain malformed names, links,
/// and every directory whose owner might still be alive (including EPERM).
fn sweep_stale(parent: &Path) {
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some((pid, stamp)) = name
            .to_str()
            .and_then(|name| name.strip_prefix(".calibration-"))
            .and_then(|name| name.split_once('-'))
        else {
            continue;
        };
        let Ok(pid) = pid.parse::<i32>() else {
            continue;
        };
        if pid <= 0
            || stamp.is_empty()
            || !stamp.bytes().all(|b| b.is_ascii_digit())
            || !entry.file_type().is_ok_and(|kind| kind.is_dir())
        {
            continue;
        }
        #[cfg(unix)]
        // SAFETY: signal 0 checks existence; it does not signal the process.
        let dead = unsafe { libc::kill(pid, 0) } != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        #[cfg(not(unix))]
        let dead = false;
        if dead {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

impl Daemon {
    pub(super) fn load_machine(self: &Arc<Self>) {
        let path = capacity::profile_path(&capacity::memory_path(self.state.read().file.legacy()));
        if let Some(parent) = path.parent() {
            sweep_stale(parent);
        }
        let mut calibration = self.machine.lock();
        calibration.path = path;
        if let Some(machine) = crate::this_machine() {
            calibration.status.profile =
                capacity::load_profile(&calibration.path, machine, &crate::daemon_build().build);
        }
        if let Some(profile) = &calibration.status.profile {
            calibration.status.summary = profile.summary();
        }
        // The disposable measurement worker is implemented only on macOS.
        // An unavailable backend is not a failed calibration; do not launch
        // it or write a failed-build marker on other platforms.
        if !cfg!(target_os = "macos") {
            if calibration.status.profile.is_none() {
                calibration.status.summary =
                    "Calibration is not available on this platform yet; using the capacity table"
                        .into();
            }
            return;
        }
        // Other daemons cannot see our outputs. An isolated snapshot/test
        // engine must never spend hardware encoder capacity automatically.
        let allowed = match std::env::var("ISOCAST_CALIBRATION").as_deref() {
            Ok("off") => false,
            Ok("on") => true,
            _ => {
                std::env::var_os("ISOCAST_SOCKET").is_none()
                    && std::env::var_os("ISOCAST_SETUP").is_none()
            }
        };
        let failed = calibration.failed_this_build();
        calibration.automatic = calibration.status.profile.is_none() && allowed && !failed;
        if failed && calibration.status.profile.is_none() {
            calibration.status.summary = "Calibration failed on this build; using the per-chip table. Retry with `isocast machine calibrate`".into();
        }
        drop(calibration);
        let daemon = self.clone();
        std::thread::spawn(move || {
            let mut next = Instant::now() + Duration::from_secs(5);
            while !daemon.shutting_down.load(Ordering::SeqCst) {
                daemon.poll_calibration();
                let automatic = daemon.machine.lock().automatic;
                if automatic && Instant::now() >= next {
                    if let Err(error) = daemon.begin_calibration(true)
                        && error.code != RpcError::WRONG_STATE
                    {
                        let mut calibration = daemon.machine.lock();
                        calibration.status.summary = error.message;
                        calibration.remember_failure();
                    }
                    next = Instant::now() + Duration::from_secs(60);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = daemon.cancel_calibration("Calibration cancelled by shutdown");
        });
    }

    pub(super) fn machine_status(&self, full: bool) -> MachineStatus {
        self.machine.lock().status(full)
    }

    pub(super) fn cancel_calibration(&self, reason: &str) -> Result<MachineStatus, RpcError> {
        let mut calibration = self.machine.lock();
        if calibration.child.is_some() {
            calibration.stop(reason).map_err(failed)?;
        }
        Ok(calibration.status(true))
    }

    pub(super) fn machine_cancel(&self) -> Result<MachineStatus, RpcError> {
        let result = self.cancel_calibration("Calibration cancelled; previous profile retained")?;
        self.machine.lock().automatic = false;
        Ok(result)
    }

    pub(super) fn cancel_background_calibration(&self) -> Result<(), RpcError> {
        let mut calibration = self.machine.lock();
        if calibration.child.is_some() && calibration.background {
            calibration
                .stop("Automatic calibration deferred while the live engine previews")
                .map_err(failed)?;
        }
        Ok(())
    }

    pub(super) fn machine_calibrate(&self) -> Result<MachineStatus, RpcError> {
        self.begin_calibration(false)
    }

    fn begin_calibration(&self, automatic: bool) -> Result<MachineStatus, RpcError> {
        // Start calls hold the read side until output state is published.
        // Taking the write side makes idle-check + helper launch atomic.
        let _gate = self.starting.try_write().ok_or_else(busy)?;
        if self.shutting_down.load(Ordering::SeqCst) || self.busy().is_some() {
            return Err(busy());
        }
        if !cfg!(target_os = "macos") || crate::this_machine().is_none() {
            return Err(failed("this platform has no calibration backend yet"));
        }
        let previewing = {
            let state = self.state.read();
            state.engine.is_some() || state.starting || state.hold
        };
        let mut calibration = self.machine.lock();
        if calibration.child.is_some() {
            return Ok(calibration.status(true));
        }
        if automatic && previewing {
            calibration.status.summary = "Automatic calibration deferred while the live engine previews; using the per-chip table".into();
            return Err(busy());
        }
        let parent = calibration
            .path
            .parent()
            .ok_or_else(|| failed("no profile folder"))?;
        fs::create_dir_all(parent).map_err(failed)?;
        // Never reuse or remove another invocation's directory.
        let scratch = parent.join(format!(
            ".calibration-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir(&scratch).map_err(failed)?;
        let errors = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(scratch.join("errors.txt"))
        {
            Ok(file) => file,
            Err(error) => {
                let _ = fs::remove_dir_all(&scratch);
                return Err(failed(error));
            }
        };
        let started = Instant::now();
        let result = Command::new(std::env::current_exe().map_err(failed)?)
            .arg("--machine-calibration-worker")
            .arg(&scratch)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(errors))
            .spawn();
        let child = match result {
            Ok(child) => child,
            Err(error) => {
                let _ = fs::remove_dir_all(&scratch);
                return Err(failed(error));
            }
        };
        calibration.child = Some(child);
        calibration.background = automatic;
        calibration.started = Some(started);
        calibration.scratch = Some(scratch);
        calibration.status.running = true;
        calibration.status.summary =
            "Calibrating quietly (at most 30 seconds); starting an output cancels it".into();
        Ok(calibration.status(true))
    }

    fn poll_calibration(&self) {
        let mut calibration = self.machine.lock();
        let Some(child) = calibration.child.as_mut() else {
            return;
        };
        let result = child.try_wait();
        let done = match result {
            Ok(Some(status)) => Some(status.success()),
            Ok(None) => None,
            Err(_) => Some(false),
        };
        if let Some(success) = done {
            let profile = if success {
                calibration.scratch.as_ref().and_then(|scratch| {
                    capacity::load_profile(
                        &scratch.join("profile.json"),
                        crate::this_machine()?,
                        &crate::daemon_build().build,
                    )
                })
            } else {
                None
            };
            let saved = profile.as_ref().map(|profile| -> anyhow::Result<()> {
                let temporary = calibration.scratch.as_ref().unwrap().join("saved.json");
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temporary)?;
                file.write_all(&serde_json::to_vec_pretty(profile)?)?;
                file.sync_all()?;
                isocast_protocol::fs::replace(&temporary, &calibration.path)?;
                Ok(())
            });
            let failed = !matches!(saved, Some(Ok(())));
            let summary = match saved {
                Some(Ok(())) => {
                    let _ = fs::remove_file(calibration.failure_path());
                    calibration.status.profile = profile;
                    calibration.status.profile.as_ref().unwrap().summary()
                }
                Some(Err(error)) => {
                    format!("Calibration could not be saved: {error}; previous profile retained")
                }
                None => {
                    use std::io::Read;
                    let mut error = String::new();
                    if let Some(scratch) = &calibration.scratch
                        && let Ok(file) = fs::File::open(scratch.join("errors.txt"))
                    {
                        let _ = file.take(2048).read_to_string(&mut error);
                    }
                    format!(
                        "Calibration did not produce valid measurements; previous profile retained. {}",
                        error.trim()
                    )
                }
            };
            let _ = calibration.stop(&summary);
            if failed {
                calibration.remember_failure();
            }
            // Retry a failed build only explicitly, not on every launch.
            calibration.automatic = false;
        } else if calibration.started.is_some_and(|at| at.elapsed() >= LIMIT) {
            let _ = calibration
                .stop("Calibration reached its 30-second limit; previous profile retained");
            calibration.remember_failure();
        }
    }
}

/// Called before normal daemon initialization and permission preflight.
pub(super) fn worker() -> Option<std::process::ExitCode> {
    if std::env::args().nth(1).as_deref() != Some("--machine-calibration-worker") {
        return None;
    }
    // Also bound a helper whose parent is killed. Native calls may never
    // return, so this watchdog exits the disposable process, not a thread.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(28));
        std::process::exit(2);
    });
    #[cfg(target_os = "macos")]
    let result = (|| -> anyhow::Result<()> {
        let scratch = std::env::args_os()
            .nth(2)
            .map(PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("missing scratch folder"))?;
        let mut profile =
            isocast_engine::machine_profile::measure(crate::platform::native(), &scratch)?;
        profile.app_build = crate::daemon_build().build;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(scratch.join("profile.json"))?;
        file.write_all(&serde_json::to_vec(&profile)?)?;
        Ok(())
    })();
    #[cfg(not(target_os = "macos"))]
    let result: anyhow::Result<()> = Err(anyhow::anyhow!("no calibration backend"));
    Some(match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("machine calibration: {error:#}");
            std::process::ExitCode::FAILURE
        }
    })
}
