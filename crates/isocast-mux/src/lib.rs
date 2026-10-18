//! Encoder-neutral, crash-safe movie storage. Inputs are compressed access
//! units and codec descriptions, never GPU textures or raw pixels: NVENC/MF
//! can retain the GPU-resident capture/composite/encode path.
//!
//! This new writer is qualified separately from the existing Mac writer.
//! Its identifying brand and explicit per-track priming/fragment shifts keep
//! old Mac recovery from guessing Apple's AAC delay for Windows files.

pub mod file;
pub mod mp4;
mod packet;
#[cfg(windows)]
pub mod recovery;

pub use packet::{
    AudioConfig, Config, Framing, Progress, VideoCodec, VideoConfig, VideoPacket, Writer,
};
