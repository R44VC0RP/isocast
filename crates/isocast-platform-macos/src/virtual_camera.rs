//! macOS camera output. The optional development probe uses the same frames
//! and native sample validation, but never pretends to be an installed camera.
use super::PixelBuffer;
use anyhow::{Result, bail};
use isocast_engine::{Time, record::CameraSink};
use std::ffi::c_void;
mod process;
pub use process::run_child;

unsafe extern "C" {
    fn isocast_camera_open(
        width: u32,
        height: u32,
        fps: u32,
        error: *mut i32,
        stage: *mut i32,
    ) -> *mut c_void;
    fn isocast_camera_send(handle: *mut c_void, pixels: *const c_void, pts: i64) -> i32;
    fn isocast_camera_close(handle: *mut c_void);
    #[cfg(feature = "vcam-diagnostics")]
    fn isocast_camera_validate(pixels: *const c_void, pts: i64, fps: u32) -> i32;
    fn isocast_camera_auth_parent(fd: i32) -> bool;
    fn isocast_camera_manage(activate: bool, wait: bool) -> i32;
    fn isocast_camera_probe(capture: bool) -> i32;
    fn isocast_camera_state() -> i32;
}

/// `wait`: after macOS asks for approval, print `needs-approval` and keep
/// the request alive until the user allows it (the engine's Start, below).
pub fn activate(wait: bool) -> i32 {
    // SAFETY: called on the process's main thread, before starting the daemon.
    unsafe { isocast_camera_manage(true, wait) }
}

pub fn deactivate() -> i32 {
    // SAFETY: the maintenance entry point runs on the process's main thread.
    unsafe { isocast_camera_manage(false, false) }
}

/// `isocastd --camera-extension-state`: print what sysextd knows about the
/// embedded extension as one JSON line (see [`extension_state`]).
pub fn print_state() -> i32 {
    // SAFETY: the maintenance entry point runs on the process's main thread.
    unsafe { isocast_camera_state() }
}

/// What macOS has activated of the camera extension this engine carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionState {
    /// This engine's build is enabled.
    Current,
    /// Another build is enabled (an update replaced the app, not the
    /// extension): `active` is its CFBundleVersion, `embedded` this one's.
    Outdated { active: String, embedded: String },
    /// macOS waits for the user to allow it.
    Waiting,
    /// Not activated.
    Absent,
}

/// Ask sysextd (from a child with its own run loop, as for activation)
/// which builds of the embedded camera extension it has activated.
pub fn extension_state() -> Result<ExtensionState, String> {
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let output = run_with_deadline(
        std::process::Command::new(exe).arg("--camera-extension-state"),
        std::time::Duration::from_secs(20),
    )?;
    let line: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("unreadable extension state ({error})"))?;
    if let Some(error) = line["error"].as_str() {
        return Err(error.to_owned());
    }
    Ok(match line["state"].as_str() {
        Some("current") => ExtensionState::Current,
        Some("waiting") => ExtensionState::Waiting,
        Some("absent") => ExtensionState::Absent,
        Some("outdated") => ExtensionState::Outdated {
            active: line["builds"]
                .as_array()
                .and_then(|builds| {
                    builds
                        .iter()
                        .find(|b| b["enabled"] == true && b["uninstalling"] != true)
                })
                .and_then(|b| b["version"].as_str())
                .unwrap_or("?")
                .to_owned(),
            embedded: line["embedded_version"].as_str().unwrap_or("?").to_owned(),
        },
        other => return Err(format!("unknown extension state {other:?}")),
    })
}

/// How a removal ended (see [`request_deactivation`]).
pub enum Deactivation {
    /// macOS removed it.
    Removed,
    /// Nothing was active to remove.
    Absent,
    /// macOS finishes removing it at the next restart.
    RestartNeeded,
    Failed(String),
}

/// Ask macOS to deactivate the embedded camera extension (it asks for an
/// administrator password), from a child of this executable; waits for the
/// answer, at most a little over the child's own 180 s for the password
/// (plus a replacement of an older active build first, see virtual_camera.m).
pub fn request_deactivation() -> Deactivation {
    if embedded_extension().is_none() {
        return Deactivation::Failed(
            "virtual_camera: this engine doesn't carry the camera extension; remove a development install with scripts/vcam-uninstall.sh".into(),
        );
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => return Deactivation::Failed(format!("virtual_camera: {error}")),
    };
    let output = match run_with_deadline(
        std::process::Command::new(exe).arg("--uninstall-camera-extension"),
        std::time::Duration::from_secs(260),
    ) {
        Ok(output) => output,
        Err(error) => {
            return Deactivation::Failed(format!(
                "virtual_camera: macOS didn't remove Isocast Virtual Camera ({error}; no administrator password?)"
            ));
        }
    };
    let said = String::from_utf8_lossy(&output.stderr);
    let said = said.lines().last().unwrap_or("").trim();
    let said = said
        .strip_prefix("Isocast camera deactivation failed: ")
        .unwrap_or(said);
    match output.status.code() {
        Some(0) if said.contains("already absent") => Deactivation::Absent,
        Some(0) => Deactivation::Removed,
        Some(2) if said.starts_with("Restart macOS") => Deactivation::RestartNeeded,
        Some(2) => Deactivation::Failed(
            "virtual_camera: macOS didn't remove Isocast Virtual Camera (no administrator password in 3 minutes); try again".into(),
        ),
        _ => Deactivation::Failed(format!(
            "virtual_camera: macOS couldn't remove Isocast Virtual Camera: {said}"
        )),
    }
}

/// Run a maintenance child to completion, killing it after `deadline`.
fn run_with_deadline(
    command: &mut std::process::Command,
    deadline: std::time::Duration,
) -> Result<std::process::Output, String> {
    use std::io::Read;
    use std::process::Stdio;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("couldn't ask macOS: {error}"))?;
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let out = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let err = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes);
        bytes
    });
    let end = std::time::Instant::now() + deadline;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < end => {
                std::thread::sleep(std::time::Duration::from_millis(50))
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "macOS didn't answer in {} s; try again",
                    deadline.as_secs()
                ));
            }
            Err(error) => return Err(error.to_string()),
        }
    };
    Ok(std::process::Output {
        status,
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    })
}

/// Part of the error a Start returns when no Isocast camera is published
/// (process.rs); the daemon matches it to offer activation.
pub const NOT_INSTALLED: &str = "Isocast Virtual Camera is not installed/approved";
/// The extension's bundle id, embedded under the engine app's
/// Contents/Library/SystemExtensions by release builds (scripts/release.sh).
pub const EXTENSION_ID: &str = "dev.isocast.isocastd.camera";

/// The camera extension this engine's own app carries, if any. Development
/// engines (no extension) are installed with scripts/vcam-install.sh instead.
pub fn embedded_extension() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // .../X.app/Contents/MacOS/isocastd -> .../X.app/Contents
    let contents = exe.parent()?.parent()?;
    let extension = contents
        .join("Library/SystemExtensions")
        .join(format!("{EXTENSION_ID}.systemextension"));
    extension
        .join("Contents/Info.plist")
        .is_file()
        .then_some(extension)
}

/// An activation request waiting for the user's approval in System Settings.
pub struct ApprovalWait(std::process::Child);

impl ApprovalWait {
    /// Blocks until the user allows the extension (true), or the request
    /// fails or gives up after 15 minutes (false).
    pub fn wait(mut self) -> bool {
        let ok = self.0.wait().is_ok_and(|status| status.success());
        if !ok {
            isocast_diagnostics::diagnostic!("warn"; "virtual camera: the activation request ended without approval");
        }
        ok
    }
}

pub enum Activation {
    /// macOS activated it (it was already allowed): Start can retry now.
    Activated,
    /// macOS asked the user to allow it; `wait` reports when they do.
    NeedsApproval(ApprovalWait),
    Failed(String),
}

/// Ask macOS to activate the embedded camera extension, from a child of
/// this same executable (its own main run loop, as OSSystemExtensionRequest
/// needs). Returns once macOS has activated it, asked for approval, or
/// refused; never waits more than 45 s.
pub fn request_activation() -> Activation {
    use std::io::{BufRead, BufReader, Read};
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::time::Duration;
    let Some(extension) = embedded_extension() else {
        return Activation::Failed(format!(
            "virtual_camera: {NOT_INSTALLED}, and this engine doesn't carry it; run scripts/vcam-install.sh"
        ));
    };
    // sysextd only activates extensions of apps in /Applications.
    if !extension.starts_with("/Applications/") {
        return Activation::Failed(
            "virtual_camera: move Isocast into the Applications folder, then start the virtual camera again (macOS only activates camera extensions from there)".into(),
        );
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => return Activation::Failed(format!("virtual_camera: {error}")),
    };
    let mut child = match Command::new(exe)
        .args(["--install-camera-extension", "--wait-for-approval"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return Activation::Failed(format!(
                "virtual_camera: couldn't ask macOS to activate Isocast Virtual Camera: {error}"
            ));
        }
    };
    let stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line == "needs-approval" {
                let _ = tx.send(());
            }
        }
    });
    let stderr = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(45);
    loop {
        if rx.try_recv().is_ok() {
            isocast_diagnostics::diagnostic!(
                "virtual camera: macOS asked the user to allow the extension"
            );
            return Activation::NeedsApproval(ApprovalWait(child));
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let said = stderr.join().unwrap_or_default();
                let said = said.lines().last().unwrap_or("").trim();
                let said = said
                    .strip_prefix("Isocast camera activation failed: ")
                    .unwrap_or(said)
                    .to_owned();
                return match status.code() {
                    Some(0) => Activation::Activated,
                    Some(2) => Activation::Failed(format!(
                        "virtual_camera: restart your Mac to finish installing Isocast Virtual Camera ({said})"
                    )),
                    _ => Activation::Failed(format!(
                        "virtual_camera: macOS couldn't activate Isocast Virtual Camera: {said}"
                    )),
                };
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Activation::Failed(
                    "virtual_camera: macOS didn't answer the activation request in 45 s; try again"
                        .into(),
                );
            }
        }
    }
}

pub fn camera_probe(capture: bool) -> i32 {
    // SAFETY: standalone maintenance process; native code owns its capture
    // session and stops it before returning. Only Isocast's UID is opened.
    unsafe { isocast_camera_probe(capture) }
}

struct Camera(*mut c_void);
// SAFETY: the native queue is owned and used by one camera worker, then closed.
unsafe impl Send for Camera {}
impl Drop for Camera {
    fn drop(&mut self) {
        unsafe { isocast_camera_close(self.0) }
    }
}
impl CameraSink<PixelBuffer> for Camera {
    fn send(&mut self, frame: &PixelBuffer, pts: Time) -> Result<bool> {
        // SAFETY: handle and retained frame live for this call. Accepted samples retain it.
        match unsafe {
            isocast_camera_send(
                self.0,
                (&*frame.0 as *const objc2_core_video::CVPixelBuffer).cast(),
                pts.as_nanos(),
            )
        } {
            0 => Ok(true),
            1 => Ok(false),
            error => bail!(
                "virtual_camera: CoreMediaIO sink failed ({error}); stop/start the camera after checking its extension"
            ),
        }
    }
}

pub fn open(width: u32, height: u32, fps: u32) -> Result<Box<dyn CameraSink<PixelBuffer>>> {
    #[cfg(not(feature = "vcam-diagnostics"))]
    if std::env::var_os("ISOCAST_VCAM_PROBE").is_some()
        || std::env::var_os("ISOCAST_VCAM_CHILD_TEST").is_some()
    {
        bail!(
            "virtual_camera: diagnostic transports require the vcam-diagnostics build feature; unset ISOCAST_VCAM_PROBE/ISOCAST_VCAM_CHILD_TEST for normal use"
        );
    }
    #[cfg(feature = "vcam-diagnostics")]
    if let Some(path) = std::env::var_os("ISOCAST_VCAM_PROBE") {
        return Ok(Box::new(probe::Probe::connect(
            std::path::Path::new(&path),
            fps,
        )?));
    }
    process::open(width, height, fps)
}

#[cfg(feature = "vcam-diagnostics")]
pub mod probe;
