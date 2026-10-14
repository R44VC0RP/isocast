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
    fn isocast_camera_manage(activate: bool) -> i32;
    fn isocast_camera_probe(capture: bool) -> i32;
}

pub fn activate() -> i32 {
    // SAFETY: called on the process's main thread, before starting the daemon.
    unsafe { isocast_camera_manage(true) }
}

pub fn deactivate() -> i32 {
    // SAFETY: the maintenance entry point runs on the process's main thread.
    unsafe { isocast_camera_manage(false) }
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
