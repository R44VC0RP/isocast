//! A private daemon whose lifetime belongs to one client, not the user's
//! background engine. The pipe carries no protocol: dropping its sole writer
//! asks the daemon to finalize and exit, even when the parent dies abruptly.

use std::io::{self, PipeReader, PipeWriter};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::{Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use isocast_protocol::api::{Method, Status, setup_path};

use crate::{Client, SPAWN_TIMEOUT, UnixStream, daemon_path};

const EXIT_TIMEOUT: Duration = Duration::from_secs(30);

/// Owns an isolated daemon. Call [`Self::close`] after stopping the recording
/// to wait for actual process exit and remove its private runtime files.
/// Dropping it instead requests graceful shutdown without waiting, retaining
/// diagnostics. Output recordings are outside this runtime directory.
pub struct IsolatedDaemon {
    workspace: PathBuf,
    owner: Option<PipeWriter>,
    exit: Mutex<Exit>,
}

struct Exit {
    receiver: mpsc::Receiver<io::Result<ExitStatus>>,
    status: Option<ExitStatus>,
}

pub(super) struct SpawnOptions {
    pub input: PipeReader,
    pub env: [(&'static str, PathBuf); 4],
}

impl IsolatedDaemon {
    /// Launch and verify a parent-owned daemon without changing this process's
    /// environment or consulting/starting the default daemon.
    pub fn launch() -> Result<Self> {
        let daemon = daemon_path()?;
        let mut directory = tempfile::Builder::new();
        directory.prefix("isocast-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            directory.permissions(std::fs::Permissions::from_mode(0o700));
        }
        // macOS's per-user TMPDIR is too long for a Unix-domain socket.
        #[cfg(target_os = "macos")]
        let directory = directory.tempdir_in("/tmp")?;
        #[cfg(not(target_os = "macos"))]
        let directory = directory.tempdir()?;
        // No TempDir destructor may remove a workspace while the child uses it.
        // Only a successful close, after the reaper confirms exit, removes it.
        let workspace = directory.keep();
        Self::launch_in(&daemon, &workspace).with_context(|| {
            format!(
                "could not launch isolated daemon {}; diagnostics retained in {}",
                daemon.display(),
                workspace.display()
            )
        })
    }

    fn launch_in(daemon: &Path, workspace: &Path) -> Result<Self> {
        let socket = workspace.join("daemon.sock");
        let log = socket.with_extension("log");
        let (input, owner) = io::pipe()?;
        let options = SpawnOptions {
            input,
            env: [
                ("ISOCAST_SOCKET", socket),
                ("ISOCAST_SETUP", workspace.join("setup.toml")),
                ("ISOCAST_PLUGINS", workspace.join("plugins")),
                // Read-only: estimates use the main engine's measurements,
                // while setup, tune memory and calibration stay private.
                ("ISOCAST_MACHINE_PROFILE", main_machine_profile()),
            ],
        };
        let (started_tx, started_rx) = mpsc::channel();
        let (exit_tx, exit_rx) = mpsc::channel();
        let program = daemon.to_owned();
        // Create the reaper BEFORE the child. If thread creation fails there is
        // no detached child to lose. It never owns a copy of the pipe writer.
        thread::Builder::new()
            .name("isolated-daemon".into())
            .spawn(move || match spawn(&program, &log, options) {
                Ok(mut child) => {
                    let _ = started_tx.send(Ok(()));
                    // Keep waiting even if the owner dropped its receiver.
                    let _ = exit_tx.send(child.wait());
                }
                Err(error) => {
                    let _ = started_tx.send(Err(error));
                }
            })?;
        started_rx
            .recv()
            .context("lost isolated daemon launcher")??;
        let isolated = Self {
            workspace: workspace.to_owned(),
            owner: Some(owner),
            exit: Mutex::new(Exit {
                receiver: exit_rx,
                status: None,
            }),
        };
        let mut client = isolated.connect()?;
        // A signed development app can predate this client and ignore unknown
        // flags. Require proof that it installed the stdin guardian before any
        // recording can start; do not trust successful spawn or a live socket.
        client.writer.set_write_timeout(Some(SPAWN_TIMEOUT))?;
        client
            .reader
            .get_ref()
            .set_read_timeout(Some(SPAWN_TIMEOUT))?;
        let status: Result<Status> = client.call(Method::Status);
        match status {
            Ok(status)
                if status
                    .daemon
                    .as_ref()
                    .is_some_and(|daemon| daemon.parent_owned) => {}
            status => {
                let reason = match status {
                    Ok(_) => "daemon is too old or has no stdin guardian".to_owned(),
                    Err(error) => format!("status failed: {error:#}"),
                };
                // This is always our private socket, never the default engine.
                // Also try this after a malformed/failed status from an old app.
                let shutdown = client.call::<serde_json::Value>(Method::ShutdownIfIdle);
                drop(isolated);
                bail!(
                    "{} did not confirm parent-owned shutdown ({reason}); rebuild/sign the daemon before recording (idle shutdown: {shutdown:?}); log {}",
                    daemon.display(),
                    workspace.join("daemon.log").display()
                );
            }
        }
        Ok(isolated)
    }

    /// Connect only to this daemon, retrying startup for at most five seconds.
    /// This never falls back to another socket or launches another process.
    pub fn connect(&self) -> Result<Client> {
        let socket = self.workspace.join("daemon.sock");
        let deadline = Instant::now() + SPAWN_TIMEOUT;
        loop {
            if let Some(status) = self.exit_status()? {
                bail!(
                    "isolated daemon exited ({status}); log {}",
                    socket.with_extension("log").display()
                );
            }
            let error = match UnixStream::connect(&socket) {
                Ok(stream) => {
                    return Client::new(stream).with_context(|| {
                        format!(
                            "could not connect to isolated daemon; log {}",
                            socket.with_extension("log").display()
                        )
                    });
                }
                Err(error) => error,
            };
            if Instant::now() >= deadline {
                bail!(
                    "isolated daemon did not listen on {} within {} s ({error}); log {}",
                    socket.display(),
                    SPAWN_TIMEOUT.as_secs(),
                    socket.with_extension("log").display()
                );
            }
            // This is a private child we just launched, not a busy background
            // engine. A 50 ms retry was a measurable startup latency floor.
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Close the ownership pipe and wait up to 30 seconds for graceful exit.
    /// On any failure, retain diagnostics and never kill the child or delete
    /// files it might still use. Recordings are never part of this cleanup.
    pub fn close(self) -> Result<()> {
        self.finish(false)
    }

    /// Finish after an operation failed, keeping the child's diagnostic files.
    pub fn close_preserving(self) -> Result<()> {
        self.finish(true)
    }

    fn finish(mut self, preserve: bool) -> Result<()> {
        drop(self.owner.take());
        let deadline = Instant::now() + EXIT_TIMEOUT;
        loop {
            if let Some(status) = self.exit_status()? {
                if !status.success() {
                    bail!(
                        "isolated daemon exited ({status}); diagnostics retained in {}",
                        self.workspace.display()
                    );
                }
                if preserve {
                    isocast_diagnostics::diagnostic!("warn";
                        "isolated daemon exited; diagnostics retained in {}", self.workspace.display());
                    return Ok(());
                }
                return std::fs::remove_dir_all(&self.workspace).with_context(|| {
                    format!(
                        "isolated daemon exited but could not clean {}",
                        self.workspace.display()
                    )
                });
            }
            if Instant::now() >= deadline {
                bail!(
                    "isolated daemon has not exited after {} s; left it to finalize, diagnostics retained in {}",
                    EXIT_TIMEOUT.as_secs(),
                    self.workspace.display()
                );
            }
            let mut exit = self.exit.lock().unwrap_or_else(|error| error.into_inner());
            match exit
                .receiver
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(status) => {
                    exit.status = Some(status.with_context(|| {
                        format!(
                            "could not wait for isolated daemon; diagnostics retained in {}",
                            self.workspace.display()
                        )
                    })?);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => bail!(
                    "lost isolated daemon exit notification; diagnostics retained in {}",
                    self.workspace.display()
                ),
            }
        }
    }

    fn exit_status(&self) -> Result<Option<ExitStatus>> {
        let mut exit = self.exit.lock().unwrap_or_else(|error| error.into_inner());
        if exit.status.is_none() {
            match exit.receiver.try_recv() {
                Ok(status) => {
                    exit.status = Some(status.with_context(|| {
                        format!(
                            "could not wait for isolated daemon; diagnostics retained in {}",
                            self.workspace.display()
                        )
                    })?);
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => bail!(
                    "lost isolated daemon exit notification; diagnostics retained in {}",
                    self.workspace.display()
                ),
            }
        }
        Ok(exit.status)
    }
}

impl Drop for IsolatedDaemon {
    fn drop(&mut self) {
        if self.owner.take().is_some() {
            isocast_diagnostics::diagnostic!("warn";
                "isolated daemon asked to finalize; diagnostics retained in {}",
                self.workspace.display()
            );
        }
    }
}

/// The machine profile the engine this client would otherwise use reads:
/// its own inherited one, else the daemon's `capacity::profile_path` for
/// [`setup_path`] (`setup.toml` -> `setup.machine.json`).
fn main_machine_profile() -> PathBuf {
    std::env::var_os("ISOCAST_MACHINE_PROFILE")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| setup_path().with_extension("machine.json"))
}

#[cfg(target_os = "macos")]
struct Child(libc::pid_t);

#[cfg(target_os = "macos")]
impl Child {
    fn wait(&mut self) -> io::Result<ExitStatus> {
        use std::os::unix::process::ExitStatusExt;
        loop {
            let mut status = 0;
            // This reaper is the only waiter for this exact child, not a pid
            // inferred from a lock file. waitpid consumes its zombie on exit.
            if unsafe { libc::waitpid(self.0, &mut status, 0) } == self.0 {
                return Ok(ExitStatus::from_raw(status));
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn spawn(daemon: &Path, log: &Path, options: SpawnOptions) -> io::Result<Child> {
    crate::spawn_daemon_with(daemon, log, Some(&options)).map(Child)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn spawn(daemon: &Path, log: &Path, options: SpawnOptions) -> io::Result<std::process::Child> {
    use std::os::unix::process::CommandExt;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)?;
    std::process::Command::new(daemon)
        .arg("--exit-on-stdin-close")
        .envs(options.env)
        .stdin(options.input)
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0)
        .spawn()
}

#[cfg(windows)]
fn spawn(daemon: &Path, log: &Path, options: SpawnOptions) -> io::Result<crate::windows::Child> {
    crate::windows::spawn_daemon_with(daemon, log, Some(&options))
}
