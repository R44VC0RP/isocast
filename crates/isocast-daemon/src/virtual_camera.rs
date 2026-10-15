use super::*;
use isocast_engine::record::{OnCameraChange, VirtualCamera};
use isocast_protocol::virtual_camera::{VirtualCameraState, VirtualCameraStatus};

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
    approval_pending: bool,
    /// Start the camera when that approval arrives (cleared by Stop).
    start_when_approved: bool,
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
            status.fps = setup.canvas.fps;
        }
        if status.transport.is_empty() {
            status.transport = "cmio-iosurface".into();
        }
        status
    }
}

impl Daemon {
    pub(super) fn virtual_camera_status(&self) -> VirtualCameraStatus {
        let state = self.state.lock().unwrap();
        state.camera.status_for(state.setup.as_ref())
    }

    pub(super) fn virtual_camera_start(self: &Arc<Self>) -> Result<VirtualCameraStatus, RpcError> {
        let result = self.start_camera();
        #[cfg(target_os = "macos")]
        if let Err(error) = &result
            && error
                .message
                .contains(isocast_platform_macos::virtual_camera::NOT_INSTALLED)
            && isocast_platform_macos::virtual_camera::embedded_extension().is_some()
        {
            return self.activate_camera();
        }
        result
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
            let mut state = self.state.lock().unwrap();
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
                    if self.state.lock().unwrap().camera.generation != generation {
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
                            generation = self.state.lock().unwrap().camera.generation;
                            thread::sleep(Duration::from_millis(250))
                        }
                        result => return result,
                    }
                }
                let state = self.state.lock().unwrap();
                Err(self.camera_failed(state, "virtual_camera: macOS activated Isocast Virtual Camera, but it isn't available yet; start it again in a few seconds".into()))
            }
            Activation::NeedsApproval(approval) => {
                let mut state = self.state.lock().unwrap();
                state.camera.approval_pending = true;
                state.camera.start_when_approved = state.camera.generation == generation;
                let daemon = Arc::downgrade(self);
                thread::spawn(move || {
                    let approved = approval.wait();
                    let Some(daemon) = daemon.upgrade() else {
                        return;
                    };
                    let start = {
                        let mut state = daemon.state.lock().unwrap();
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
                let state = self.state.lock().unwrap();
                Err(self.camera_failed(state, message))
            }
        }
    }

    /// Record and announce a failed start that never reached a worker.
    #[cfg(target_os = "macos")]
    fn camera_failed(
        &self,
        mut state: std::sync::MutexGuard<'_, State>,
        message: String,
    ) -> RpcError {
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
        self.publish(Event::VirtualCamera { camera: status });
        refused(message)
    }

    fn start_camera(self: &Arc<Self>) -> Result<VirtualCameraStatus, RpcError> {
        let (generation, previous) = {
            let mut state = self.state.lock().unwrap();
            if state.camera.active() {
                return Err(refused(
                    "virtual_camera: already running or starting; stop it first".into(),
                ));
            }
            let setup = state
                .setup
                .as_ref()
                .ok_or_else(|| state.no_setup("no virtual camera picture"))?;
            setup.virtual_camera.check_start(setup).map_err(refused)?;
            if state.tuning {
                return Err(refused("virtual_camera: wait for tune to finish".into()));
            }
            state.camera.starting = true;
            state.camera.generation += 1;
            (state.camera.generation, state.camera.running.take())
        };
        // Never join/destroy a camera or invoke native setup under state.
        if let Some(previous) = previous {
            previous.stop();
            if !previous.finished() {
                let mut state = self.state.lock().unwrap();
                state.camera.running = Some(previous);
                state.camera.starting = false;
                return Err(refused("virtual_camera: previous worker is still finishing; no additional worker was started".into()));
            }
        }
        let engine = self.ensure_engine();
        {
            let mut state = self.state.lock().unwrap();
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
                daemon.publish(Event::VirtualCamera { camera });
            }
        });
        match engine.virtual_camera(on_change) {
            Ok(camera) => {
                let camera = Arc::new(camera);
                {
                    let mut state = self.state.lock().unwrap();
                    if generation != state.camera.generation {
                        drop(state);
                        camera.stop();
                        let mut state = self.state.lock().unwrap();
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
                    self.publish(Event::VirtualCamera {
                        camera: status.clone(),
                    });
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
                    let mut state = self.state.lock().unwrap();
                    if generation == state.camera.generation {
                        state.camera.last = status.clone();
                    }
                    // A concurrent Stop cancels the generation but leaves
                    // admission reserved until this start actually finishes.
                    state.camera.starting = false;
                }
                self.publish(Event::VirtualCamera { camera: status });
                Err(failed(error))
            }
        }
    }

    pub(super) fn virtual_camera_stop(&self) -> VirtualCameraStatus {
        let (camera, generation) = {
            let mut state = self.state.lock().unwrap();
            state.camera.stopping = true;
            state.camera.start_when_approved = false;
            state.camera.generation += 1;
            (state.camera.running.clone(), state.camera.generation)
        };
        let status = camera
            .as_ref()
            .map_or_else(|| self.virtual_camera_status(), |camera| camera.stop());
        let retired = {
            let mut state = self.state.lock().unwrap();
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
