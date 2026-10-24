//! Linux desktop capture, Vulkan compositing and software encoding.
#![cfg(target_os = "linux")]

mod audio;
mod camera;
mod encoder;
pub mod frames;
pub mod network;
mod pipewire_audio;
mod pipewire_video;
mod platform;
mod playback;
mod portal;
mod pw_loop;
mod system;
mod writer;

pub use platform::LinuxPlatform;

// CI budget measurement (never merged).
