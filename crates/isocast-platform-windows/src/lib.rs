//! Windows native media. WGC delivers bounded, GPU-resident D3D11 frames
//! stamped on the system QPC clock, imported in place through shared handles
//! and GPU fences by wgpu/DX12. The daemon continues to select the stub
//! until the compositor, encoders and crash-safe writer are qualified.
#![cfg(windows)]

mod aac;
mod audio;
mod audio_apps;
mod broker;
mod camera;
mod capture;
mod clock;
mod convert;
mod credentials;
mod devices;
mod encoder;
mod frames;
mod gpu;
mod h264;
mod hevc;
mod media;
mod mf;
mod native;
pub mod network;
mod nvenc;
mod output;
mod p010;
mod permissions;
mod platform;
mod playback;
mod system;
mod writer;

pub use aac::{AacFinish, AacPacket, AacStats, MfAacEncoder};
pub use audio::{
    AudioCapture, AudioCaptureStats, AudioDevice, audio_devices, capture_loopback,
    capture_microphone, capture_system_audio, probe_loopback, probe_microphone,
};
pub use broker::{
    IsolatedCapture, IsolatedCaptureStats, capture_helper_main, isolated_capture_screen,
};
pub use camera::{
    CameraCapture, CameraCaptureStats, CameraDevice, CameraFormat, CameraPixelFormat,
    camera_devices, capture_camera,
};
pub use capture::{CaptureStats, CaptureTarget, DisplayCapture, capture_monitor, capture_screen};
pub use clock::{now, sleep_until};
pub use encoder::{EncoderBackend, RecordingQuality, VideoEncoding, WindowsVideoEncoder};
pub use frames::{Frame, GraphicsDevice, MAX_POOL_FRAMES, PixelFormat, TexturePool};
pub use gpu::Dx12Importer;
pub use mf::{MfEncoder, MfEncoderInfo, MfMode, mf_encoders, mf_encoders_for};
pub use native::{RecordingPowerGuard, audio_thread, video_thread};
pub use nvenc::{
    EncodeStats, NVENC_NATIVE_IN_FLIGHT, NVENC_PENDING_READY_LIMIT, NvencAccounted, NvencAdmission,
    NvencBudgetOutput, NvencBudgetPlan, NvencBudgetProfile, NvencClosing, NvencEncoder,
    NvencReservation,
};
pub use output::OutputPool;
pub use platform::{Capture, WindowsPlatform};
pub use system::process_started;
pub use writer::VideoWriter;
