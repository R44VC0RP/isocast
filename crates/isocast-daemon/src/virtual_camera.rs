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
