//! Headless Linux server. Capture and hardware encoders are deliberately
//! unsupported; synthetic sources use Vulkan and software encoding.
#![cfg(target_os = "linux")]

mod audio;
mod encoder;
pub mod frames;
pub mod network;
mod platform;
mod system;
mod writer;

pub use platform::LinuxPlatform;
