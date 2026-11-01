use super::*;
use isocast_engine::record::{OnCameraChange, VirtualCamera};
use isocast_protocol::virtual_camera::{
    VirtualCameraRemoval, VirtualCameraState, VirtualCameraStatus,
};

fn refused(message: String) -> RpcError {
    RpcError {
        code: RpcError::WRONG_STATE,
        message,
    }
}

#[derive(Default)]
pub(super) struct CameraOutput {
    running: Option<Arc<VirtualCamera>>,
    starting: bool,
    stopping: bool,
    generation: u64,
    last: VirtualCameraStatus,
    /// An activation request is waiting for the user to allow the camera
    /// extension in System Settings (macOS release builds).
    #[cfg(target_os = "macos")]
    approval_pending: bool,
    /// Start the camera when that approval arrives (cleared by Stop).
    start_when_approved: bool,
    /// The embedded extension's Info.plist (its modification time) whose
    /// build macOS was found to have active, so Start asks only once per
    /// installed build (#69).
    #[cfg(target_os = "macos")]
    checked_extension: Option<std::time::SystemTime>,
    /// `virtual_camera_remove` waits for macOS; Start is refused meanwhile.
    removing: bool,
    /// Counts removals, so an approval wait they ended stays quiet.
    #[cfg(target_os = "macos")]
    removals: u64,
}

impl CameraOutput {
    pub(super) fn active(&self) -> bool {
        self.starting
            || self.stopping
            || self.running.as_ref().is_some_and(|camera| {
                matches!(
                    camera.status().state,
                    VirtualCameraState::Starting
                        | VirtualCameraState::Running
                        | VirtualCameraState::Stopping
                )
            })
    }
    pub(super) fn status(&self) -> VirtualCameraStatus {
        self.running
            .as_ref()
            .map_or_else(|| self.last.clone(), |camera| camera.status())
    }
    pub(super) fn status_for(&self, setup: Option<&Setup>) -> VirtualCameraStatus {
        let mut status = self.status();
        if !self.active()
            && let Some(setup) = setup
        {
            status.settings = setup.virtual_camera.clone();
            (status.width, status.height) = setup.virtual_camera.size(setup);
            status.fps = setup.virtual_camera.fps(setup);
        }
        if status.transport.is_empty() {
            status.transport = if cfg!(target_os = "macos") {
                "cmio-iosurface"
            } else if cfg!(target_os = "linux") {
                "v4l2loopback"
            } else {
                "unsupported"
            }
            .into();
        }
        status.removable = removable();
        status
    }
}

fn unavailable() -> Option<String> {
    #[cfg(windows)]
    return Some("Virtual camera not available on Windows yet".into());
    #[cfg(target_os = "linux")]
    return isocast_platform_linux::virtual_camera_unavailable();
    #[cfg(not(any(target_os = "linux", windows)))]
    None
}

/// Discovery may access sysfs; never call under the daemon state lock.
pub(super) fn availability(status: &mut VirtualCameraStatus) {
    if matches!(
        status.state,
        VirtualCameraState::Stopped | VirtualCameraState::Failed
    ) {
        status.unavailable_reason = unavailable();
        status.governor_fps = None;
    }
}

/// The engine carries the camera extension (the release app).
fn removable() -> bool {
    #[cfg(target_os = "macos")]
    {
        static CARRIES: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        // macOS only manages extensions of apps in /Applications.
        *CARRIES.get_or_init(|| {
            isocast_platform_macos::virtual_camera::embedded_extension()
                .is_some_and(|path| path.starts_with("/Applications/"))
        })
    }
    #[cfg(not(target_os = "macos"))]
    false
}

impl Daemon {
    /// Announce the camera's state; every status says whether it is
    /// removable (the engine worker's own statuses don't know).
    fn publish_camera(&self, mut camera: VirtualCameraStatus) {
        camera.removable = removable();
        self.publish(Event::VirtualCamera { camera });
    }

    pub(super) fn virtual_camera_status(&self) -> VirtualCameraStatus {
        let state = self.state.read();
        let mut status = state.camera.status_for(state.setup.as_ref());
        drop(state);
        availability(&mut status);
        status
    }

    pub(super) fn virtual_camera_start(self: &Arc<Self>) -> Result<VirtualCameraStatus, RpcError> {
        if let Some(reason) = unavailable() {
            return Err(refused(reason));
        }
        // Extension activation can retry here outside the dispatcher.
        let _gate = self.starting.read_recursive();
        self.cancel_calibration("Calibration deferred for the virtual camera")?;
        if self.state.read().camera.removing {
            return Err(refused(
                "virtual_camera: it is being removed from macOS; start it again once that finishes"
                    .into(),
            ));
        }
        let tag = |result: Result<VirtualCameraStatus, RpcError>| {
            result.map(|mut status| {
                status.removable = removable();
                status
            })
        };
        // A network caller only feeds a camera already installed and
        // current: installing or updating the system extension is done on
        // the machine itself (decision 25).
        if !crate::callers::current().is_local() {
            return tag(self.start_installed_camera());
        }
        #[cfg(target_os = "macos")]
        if let Some(result) = self.update_camera_extension() {
            return tag(result);
        }
        let result = self.start_camera();
        #[cfg(target_os = "macos")]
        if let Err(error) = &result
            && error
                .message
                .contains(isocast_platform_macos::virtual_camera::NOT_INSTALLED)
            && isocast_platform_macos::virtual_camera::embedded_extension().is_some()
        {
            return tag(self.activate_camera());
        }
        tag(result)
    }

    /// Start feeding the camera without installing or updating anything:
    /// refused when the extension isn't installed or isn't this build's.
    fn start_installed_camera(self: &Arc<Self>) -> Result<VirtualCameraStatus, RpcError> {
        #[cfg(target_os = "macos")]
        let not_here = |what: &str| RpcError {
            code: RpcError::NOT_PERMITTED,
            message: format!(
                "virtual_camera: {what}; installing or updating it is done on {} (start it \
                 there once)",
                crate::callers::machine_name()
            ),
        };
        #[cfg(target_os = "macos")]
        {
            use isocast_platform_macos::virtual_camera::{
                ExtensionState, embedded_extension, extension_state,
            };
            if embedded_extension().is_some() {
                match extension_state() {
                    Ok(ExtensionState::Current) => {}
                    Ok(ExtensionState::Outdated { .. }) => {
                        return Err(not_here("the installed camera is another build's"));
                    }
                    Ok(ExtensionState::Waiting) => {
                        return Err(not_here("the camera is waiting to be allowed"));
                    }
                    Ok(ExtensionState::Absent) => {
                        return Err(not_here("the camera isn't installed"));
                    }
                    Err(err) => {
                        return Err(not_here(&format!(
                            "whether the camera is installed couldn't be checked ({err:#})"
                        )));
                    }
                }
            }
        }
        let result = self.start_camera();
        #[cfg(target_os = "macos")]
        if let Err(error) = &result
            && error
                .message
                .contains(isocast_platform_macos::virtual_camera::NOT_INSTALLED)
        {
            return Err(not_here("the camera isn't installed"));
        }
        result
    }

    /// After an update (#69): Sparkle replaced the app and the extension it
    /// carries, but macOS keeps the build it activated. When another build
    /// is active, ask macOS to activate this one (it replaces the old one
    /// without asking the user again), then start. Checked once per
    /// installed build; a failed check never blocks a start.
    #[cfg(target_os = "macos")]
    fn update_camera_extension(self: &Arc<Self>) -> Option<Result<VirtualCameraStatus, RpcError>> {
        use isocast_platform_macos::virtual_camera::{
            ExtensionState, embedded_extension, extension_state,
        };
        let extension = embedded_extension()?;
        if !extension.starts_with("/Applications/") {
            return None;
        }
        let stamp = std::fs::metadata(extension.join("Contents/Info.plist"))
            .and_then(|m| m.modified())
            .ok()?;
        {
            let state = self.state.read();
            if state.camera.checked_extension == Some(stamp)
                || state.camera.active()
                || state.camera.approval_pending
            {
                return None;
            }
        }
        let checked = |daemon: &Self| {
            daemon.state.lock().camera.checked_extension = Some(stamp);
        };
        match extension_state() {
            Ok(ExtensionState::Outdated { active, embedded }) => {
                isocast_diagnostics::diagnostic!(
                    "virtual camera: macOS has camera extension build {active} active; this app carries {embedded}, so it asks macOS to replace it"
                );
                match self.activate_camera() {
                    Ok(status) => {
                        checked(self);
                        Some(Ok(status))
                    }
                    // macOS wants the user's approval after all: say so.
                    Err(error) if error.message.contains("Login Items & Extensions") => {
                        Some(Err(error))
                    }
                    // The old build may still work: start with it.
                    Err(error) => {
                        isocast_diagnostics::diagnostic!("warn"; "virtual camera: couldn't replace the active camera extension build ({}); starting with it", error.message);
                        None
                    }
                }
            }
            Ok(state) => {
                isocast_diagnostics::diagnostic!(
                    "virtual camera: camera extension build check: {state:?}"
                );
                checked(self);
                None
            }
            Err(error) => {
                isocast_diagnostics::diagnostic!("warn"; "virtual camera: couldn't read the camera extension's state ({error}); starting anyway");
                None
            }
        }
    }

    /// Take the camera extension out of macOS (#69), so deleting Isocast
    /// leaves nothing behind: stop the camera, then ask macOS to deactivate
    /// it, which asks for an administrator password. Blocks until macOS
    /// answers (the app calls it on its own connection).
    pub(super) fn virtual_camera_remove(&self) -> Result<VirtualCameraRemoval, RpcError> {
        #[cfg(not(target_os = "macos"))]
        {
            Err(refused(
                "virtual_camera: only macOS has a camera extension to remove".into(),
            ))
        }
        #[cfg(target_os = "macos")]
        {
            use isocast_platform_macos::virtual_camera::{Deactivation, request_deactivation};
            if !removable() {
                return Err(refused("virtual_camera: this engine doesn't carry the camera extension; remove a development install with scripts/vcam-uninstall.sh".into()));
            }
            {
                let mut state = self.state.lock();
                if state.camera.removing {
                    return Err(refused(
                        "virtual_camera: already being removed; answer macOS's password prompt"
                            .into(),
                    ));
                }
                state.camera.removing = true;
                state.camera.removals += 1;
                state.camera.start_when_approved = false;
                // Removal ends a pending activation request; the next Start
                // asks again rather than waiting on it.
                state.camera.approval_pending = false;
            }
            self.virtual_camera_stop();
            isocast_diagnostics::diagnostic!(
                "virtual camera: asking macOS to remove the camera extension"
            );
            let result = request_deactivation();
            let status = {
                let mut state = self.state.lock();
                state.camera.removing = false;
                state.camera.checked_extension = None;
                if !state.camera.active() {
                    // A failed start's reason (often "allow it in System
                    // Settings") no longer applies.
                    state.camera.last = VirtualCameraStatus::default();
                }
                state.camera.status_for(state.setup.as_ref())
            };
            self.publish_camera(status);
            let removal = match result {
                Deactivation::Removed => VirtualCameraRemoval {
                    restart_needed: false,
                    message: "Isocast Virtual Camera was removed from macOS. Starting it again installs it again.".into(),
                },
                Deactivation::Absent => VirtualCameraRemoval {
                    restart_needed: false,
                    message: "Isocast Virtual Camera isn't installed in macOS; nothing to remove."
                        .into(),
                },
                Deactivation::RestartNeeded => VirtualCameraRemoval {
                    restart_needed: true,
                    message: "Isocast Virtual Camera is removed when you restart your Mac.".into(),
                },
                Deactivation::Failed(message) => {
                    isocast_diagnostics::diagnostic!("warn"; "{message}");
                    return Err(refused(message));
                }
            };
            isocast_diagnostics::diagnostic!("virtual camera: {}", removal.message);
            Ok(removal)
        }
    }

    /// The first Start of a release build: its engine app carries the camera
    /// extension, so ask macOS to activate it. Already allowed: start again
    /// once it's published. Not yet: report the System Settings step and
    /// start by itself when the user allows it (unless Stop came first).
    #[cfg(target_os = "macos")]
    fn activate_camera(self: &Arc<Self>) -> Result<VirtualCameraStatus, RpcError> {
        use isocast_platform_macos::virtual_camera::{Activation, request_activation};
        const APPROVAL: &str = "virtual_camera: allow Isocast Virtual Camera in System Settings › General › Login Items & Extensions › Camera Extensions (macOS asks once); it starts as soon as you do";
        let mut generation = {
            let mut state = self.state.lock();
            if state.camera.approval_pending {
                state.camera.start_when_approved = true;
                return Err(self.camera_failed(state, APPROVAL.into()));
            }
            state.camera.generation
        };
        match request_activation() {
            Activation::Activated => {
                // sysextd has activated it; CoreMediaIO publishes the device
                // a moment later.
                for _ in 0..20 {
                    if self.state.lock().camera.generation != generation {
                        return Err(refused("virtual_camera: start was cancelled".into()));
                    }
                    match self.start_camera() {
                        Err(error)
                            if error.message.contains(
                                isocast_platform_macos::virtual_camera::NOT_INSTALLED,
                            ) =>
                        {
                            // Each attempt is a new generation; a Stop during
                            // the pause makes another one.
                            generation = self.state.lock().camera.generation;
                            thread::sleep(Duration::from_millis(250))
                        }
                        result => return result,
                    }
                }
                let state = self.state.lock();
                Err(self.camera_failed(state, "virtual_camera: macOS activated Isocast Virtual Camera, but it isn't available yet; start it again in a few seconds".into()))
            }
            Activation::NeedsApproval(approval) => {
                let mut state = self.state.lock();
                state.camera.approval_pending = true;
                state.camera.start_when_approved = state.camera.generation == generation;
                let removals = state.camera.removals;
                let daemon = Arc::downgrade(self);
                thread::spawn(move || {
                    let approved = approval.wait();
                    let Some(daemon) = daemon.upgrade() else {
                        return;
                    };
                    let start = {
                        let mut state = daemon.state.lock();
                        if state.camera.removing || state.camera.removals != removals {
                            // Remove Virtual Camera ended this request (and
                            // cleared its state; a later Start's is its own).
                            // A removal still running may have cleared it
                            // just before this request was recorded: no
                            // Start runs meanwhile, so clear it again.
                            if state.camera.removing {
                                state.camera.approval_pending = false;
                                state.camera.start_when_approved = false;
                            }
                            return;
                        }
                        state.camera.approval_pending = false;
                        let wanted = std::mem::take(&mut state.camera.start_when_approved);
                        if !approved && !state.camera.active() {
                            // Don't Allow, a restart pending, or 15 minutes:
                            // stop promising a start the app is waiting for.
                            let message = "virtual_camera: macOS didn't get your approval for Isocast Virtual Camera; start it again to ask once more".to_owned();
                            let _ = daemon.camera_failed(state, message);
                            return;
                        }
                        wanted && approved && !state.camera.active()
                    };
                    if start {
                        isocast_diagnostics::diagnostic!(
                            "virtual camera: allowed in System Settings; starting it"
                        );
                        if let Err(error) = daemon.virtual_camera_start() {
                            isocast_diagnostics::diagnostic!("warn"; "{}", error.message);
                        }
                    }
                });
                Err(self.camera_failed(state, APPROVAL.into()))
            }
            Activation::Failed(message) => {
                let state = self.state.lock();
                Err(self.camera_failed(state, message))
            }
        }
    }

    /// Record and announce a failed start that never reached a worker.
    #[cfg(target_os = "macos")]
    fn camera_failed(&self, mut state: Transaction<'_, State>, message: String) -> RpcError {
        let status = VirtualCameraStatus {
            state: VirtualCameraState::Failed,
            last_error: Some(message.clone()),
            ..Default::default()
        };
        // Status reads a finished worker before `last`: retire the failed
        // one so this reason is what Status reports.
        let mut retired = None;
        if !state.camera.active() {
            if state.camera.running.as_ref().is_some_and(|c| c.finished()) {
                retired = state.camera.running.take();
            }
            state.camera.last = status.clone();
        }
        drop(state);
        drop(retired); // Never dropped under state.
        self.publish_camera(status);
        refused(message)
    }

    fn start_camera(self: &Arc<Self>) -> Result<VirtualCameraStatus, RpcError> {
        let (generation, previous) = {
            let mut state = self.state.lock();
            if state.camera.active() {
                return Err(refused(
                    "virtual_camera: already running or starting; stop it first".into(),
                ));
            }
            let setup = state
                .setup
                .as_ref()
                .ok_or_else(|| state.no_setup("no virtual camera picture"))?;
            setup
                .virtual_camera
                .check_start(setup, crate::platform::NativePlatform::CAMERA_IS_MONITOR)
                .map_err(refused)?;
            if state.tuning {
                return Err(refused("virtual_camera: wait for tune to finish".into()));
            }
            if state.camera.removing {
                return Err(refused(
                    "virtual_camera: it is being removed from macOS; start it again once that finishes"
                        .into(),
                ));
            }
            state.camera.starting = true;
            state.camera.generation += 1;
            (state.camera.generation, state.camera.running.take())
        };
        // Never join/destroy a camera or invoke native setup under state.
        if let Some(previous) = previous {
            previous.stop();
            if !previous.finished() {
                let mut state = self.state.lock();
                state.camera.running = Some(previous);
                state.camera.starting = false;
                return Err(refused("virtual_camera: previous worker is still finishing; no additional worker was started".into()));
            }
        }
        let engine = self.ensure_engine();
        {
            let mut state = self.state.lock();
            if generation != state.camera.generation {
                state.camera.starting = false;
                return Err(refused("virtual_camera: start was cancelled".into()));
            }
            if engine.is_err() {
                state.camera.starting = false;
            }
        }
        let engine = engine?;
        let daemon = Arc::downgrade(self);
        let on_change: OnCameraChange = Arc::new(move |camera| {
            if let Some(daemon) = daemon.upgrade() {
                daemon.publish_camera(camera);
            }
        });
        match engine.virtual_camera(on_change) {
            Ok(camera) => {
                let camera = Arc::new(camera);
                {
                    let mut state = self.state.lock();
                    if generation != state.camera.generation {
                        drop(state);
                        camera.stop();
                        let mut state = self.state.lock();
                        if !camera.finished() {
                            // A cancelled, not-yet-published worker still owns
                            // source demand until it exits. Keep its admission
                            // slot so cancellation cannot accumulate workers.
                            state.camera.running = Some(camera.clone());
                        }
                        state.camera.starting = false;
                        return Err(refused("virtual_camera: start was cancelled".into()));
                    }
                    state.camera.running = Some(camera.clone());
                    state.camera.starting = false;
                }
                let status = camera.wait_ready();
                if status.state == VirtualCameraState::Running {
                    Ok(status)
                } else {
                    self.publish_camera(status.clone());
                    Err(refused(status.last_error.unwrap_or_else(|| {
                        "virtual_camera: start was cancelled".into()
                    })))
                }
            }
            Err(error) => {
                let status = VirtualCameraStatus {
                    state: VirtualCameraState::Failed,
                    last_error: Some(format!("{error:#}")),
                    ..Default::default()
                };
                {
                    let mut state = self.state.lock();
                    if generation == state.camera.generation {
                        state.camera.last = status.clone();
                    }
                    // A concurrent Stop cancels the generation but leaves
                    // admission reserved until this start actually finishes.
                    state.camera.starting = false;
                }
                self.publish_camera(status);
                Err(failed(error))
            }
        }
    }

    pub(super) fn virtual_camera_stop(&self) -> VirtualCameraStatus {
        let (camera, generation) = {
            let mut state = self.state.lock();
            state.camera.stopping = true;
            state.camera.start_when_approved = false;
            state.camera.generation += 1;
            (state.camera.running.clone(), state.camera.generation)
        };
        let mut status = camera
            .as_ref()
            .map_or_else(|| self.virtual_camera_status(), |camera| camera.stop());
        status.removable = removable();
        let retired = {
            let mut state = self.state.lock();
            if generation != state.camera.generation {
                return status;
            }
            state.camera.stopping = false;
            state.camera.last = status.clone();
            if camera.as_ref().is_none_or(|camera| camera.finished()) {
                state.camera.running.take()
            } else {
                None
            }
        };
        drop(retired); // The last Arc (and its Drop) is never released under state.
        status
    }
}
