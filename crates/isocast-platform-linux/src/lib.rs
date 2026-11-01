//! Linux desktop capture, Vulkan compositing and H.264 encoding.
#![cfg(target_os = "linux")]

mod audio;
mod browser;
mod camera;
mod encoder;
pub mod frames;
mod media;
pub mod network;
mod nvenc;
mod pipewire_audio;
mod pipewire_video;
mod platform;
mod playback;
mod portal;
mod pw_loop;
mod system;
mod video_encoder;
mod writer;

pub use platform::LinuxPlatform;

/// Check discovery/permissions without becoming a producer. Regular status
/// polls share one short-lived result; start always probes the current device.
pub fn virtual_camera_unavailable() -> Option<String> {
    use std::{
        sync::{Mutex, OnceLock},
        time::{Duration, Instant},
    };
    type Probe = Option<(Instant, Option<String>)>;
    static CACHE: OnceLock<Mutex<Probe>> = OnceLock::new();
    let mut cached = CACHE
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if let Some((at, reason)) = &*cached
        && at.elapsed() < Duration::from_secs(1)
    {
        return reason.clone();
    }
    let reason = camera::output::device().err().map(|e| e.to_string());
    *cached = Some((Instant::now(), reason.clone()));
    reason
}
