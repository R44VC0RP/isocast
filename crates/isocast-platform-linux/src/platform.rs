use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};
use isocast_engine::gpu::Gpu;
use isocast_engine::{
    AudioRender, AudioSink, BrowserControl, BrowserPage, CameraRequest, CaptureFormat,
    EncoderFrameFormat, LiveAudioSpec, LiveVideoSpec, MediaInfo, MediaPart, MediaReader, Platform,
    Probe, ScreenTarget, Time, VideoSink, Volume, WriterSpec,
};
use isocast_protocol::api::Machine;
use isocast_protocol::devices::{Devices, PermissionKind, PermissionsReport};
use isocast_protocol::{Canvas, SourceKind};

use crate::encoder::Encoder;
use crate::frames::{Frame, FramePool, Importer};
use crate::system;
use crate::writer::Writer;

#[derive(Clone, Copy, Default)]
pub struct LinuxPlatform;

pub enum Unsupported {}

fn unsupported(what: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "{what}: not supported by the headless Linux bootstrap (synthetic sources and software H.264 recording only)"
    )
}

impl MediaReader for Unsupported {
    type Frame = Frame;
    fn next_video(&mut self) -> Result<Option<(Frame, std::time::Duration)>> {
        match *self {}
    }
    fn next_audio(&mut self) -> Result<Option<(Vec<f32>, std::time::Duration)>> {
        match *self {}
    }
}

impl Platform for LinuxPlatform {
    type Frame = Frame;
    type FramePool = FramePool;
    type Writer = Writer;
    type Capture = Unsupported;
    type Importer = Importer;
    type Media = Unsupported;
    type Playback = Unsupported;
    type VideoEncoder = Encoder;
    type AudioEncoder = isocast_platform_stub::Unsupported;
    type SharedSurface = isocast_surfaces::Surface;

    fn now(&self) -> Time {
        system::now()
    }
    fn machine(&self) -> Machine {
        system::machine()
    }
    fn video_encoders(&self) -> isocast_engine::VideoEncoders {
        isocast_engine::VideoEncoders {
            hardware: Vec::new(),
            software: vec!["OpenH264 (software)".into()],
        }
    }
    fn volume(&self, path: &Path) -> Result<Volume> {
        system::volume(path)
    }
    fn same_volume(&self, left: &Path, right: &Path) -> Result<bool> {
        system::same_volume(left, right)
    }
    fn process_cpu_seconds(&self) -> Option<f64> {
        system::process_cpu_seconds()
    }

    fn devices(&self) -> Result<Devices> {
        Err(unsupported("listing capture devices"))
    }
    fn permissions(&self) -> Result<PermissionsReport> {
        Err(unsupported("capture permissions"))
    }
    fn request_permissions(&self, _only: Option<PermissionKind>) -> Result<PermissionsReport> {
        Err(unsupported("requesting capture permissions"))
    }
    fn open_permission_settings(&self, _permission: PermissionKind) -> Result<String> {
        Err(unsupported("opening capture permission settings"))
    }
    fn media_info(&self, _path: &Path) -> Result<MediaInfo> {
        Err(unsupported("media file decoding"))
    }
    fn open_media(&self, _path: &Path, _part: MediaPart) -> Result<Unsupported> {
        Err(unsupported("media file decoding"))
    }
    fn probe(&self, source: &SourceKind, _canvas: &Canvas) -> Result<Option<Probe>> {
        match source {
            SourceKind::TestPattern(_)
            | SourceKind::Tone(_)
            | SourceKind::Color(_)
            | SourceKind::Image(_)
            | SourceKind::Slideshow(_)
            | SourceKind::Text(_) => Ok(None),
            other => Err(unsupported(other.label())),
        }
    }

    fn frame_pool(&self, width: u32, height: u32) -> Result<FramePool> {
        FramePool::new(width, height)
    }
    fn frame_importer(&self, gpu: &Gpu) -> Result<Importer> {
        Ok(Importer(gpu.clone()))
    }
    fn encoder_frame_format(&self) -> EncoderFrameFormat {
        EncoderFrameFormat::Bgra
    }
    fn recording_needs_program_frame(&self) -> bool {
        true
    }
    fn ycbcr_frame_pool(&self, _width: u32, _height: u32) -> Result<FramePool> {
        Err(unsupported("YCbCr frame pools"))
    }
    fn frame_size(&self, frame: &Frame) -> (u32, u32) {
        (frame.width(), frame.height())
    }
    fn frame_is_ycbcr(&self, _frame: &Frame) -> bool {
        false
    }
    fn shared_surface(
        &self,
        _gpu: &Gpu,
        _width: u32,
        _height: u32,
    ) -> Result<(Self::SharedSurface, wgpu::Texture)> {
        Err(unsupported("shared GPU surfaces"))
    }
    fn encode_jpeg(
        &self,
        frame: &Frame,
        max_width: Option<u32>,
        quality: f32,
    ) -> Result<(Vec<u8>, u32, u32)> {
        let mut bgra = frame.read_bgra()?;
        for pixel in bgra.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
        }
        let image = image::RgbaImage::from_raw(frame.width(), frame.height(), bgra)
            .ok_or_else(|| anyhow::anyhow!("invalid Linux BGRA frame dimensions"))?;
        let mut image = image::DynamicImage::ImageRgba8(image);
        if let Some(width) = max_width.filter(|width| *width > 0 && *width < frame.width()) {
            image = image.resize(width, u32::MAX, image::imageops::FilterType::Triangle);
        }
        let image = image.to_rgb8();
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(
            &mut bytes,
            (quality.clamp(0.0, 1.0) * 100.0) as u8,
        )
        .encode(
            image.as_raw(),
            image.width(),
            image.height(),
            image::ExtendedColorType::Rgb8,
        )?;
        Ok((bytes, image.width(), image.height()))
    }
    fn create_writer(&self, spec: &WriterSpec) -> Result<Writer> {
        Writer::new(spec)
    }
    fn recover_file(
        &self,
        folder: &Path,
        name: &std::ffi::OsStr,
    ) -> Result<Option<isocast_engine::RecoveredFile>> {
        let _ = (folder, name);
        // Startup recovery holds one pinned directory lease through media
        // repair and manifest publication. This path-only legacy hook must
        // not acquire a second, conflicting lease or reopen a changed path.
        bail!("recover_file: use handle-anchored engine recovery")
    }
    fn create_video_encoder(&self, spec: &LiveVideoSpec) -> Result<Encoder> {
        Encoder::new(spec)
    }
    fn create_audio_encoder(&self, _spec: &LiveAudioSpec) -> Result<Self::AudioEncoder> {
        bail!("AAC streaming is not supported by the Linux bootstrap")
    }

    fn capture_screen(
        &self,
        _target: ScreenTarget<'_>,
        _fps: u32,
        _ten_bit: bool,
        _cursor: bool,
        _sink: VideoSink<Frame>,
    ) -> Result<(Unsupported, CaptureFormat)> {
        Err(unsupported("screen capture (PipeWire portal)"))
    }
    fn capture_camera(
        &self,
        _device: &str,
        _request: CameraRequest,
        _canvas: &Canvas,
        _sink: VideoSink<Frame>,
    ) -> Result<(Unsupported, CaptureFormat)> {
        Err(unsupported("camera capture (V4L2)"))
    }
    fn capture_microphone(
        &self,
        _device: &str,
        _channels: Option<&[u32]>,
        _sink: AudioSink,
    ) -> Result<(Unsupported, u32)> {
        Err(unsupported("microphone capture (PipeWire)"))
    }
    fn capture_system_audio(
        &self,
        _exclude: &[String],
        _sink: AudioSink,
        _exclude_own: bool,
    ) -> Result<(Unsupported, u32)> {
        Err(unsupported("system audio capture (PipeWire)"))
    }
    fn capture_application_audio(
        &self,
        _app: &str,
        _sink: AudioSink,
    ) -> Result<(Unsupported, u32)> {
        Err(unsupported("application audio capture (PipeWire)"))
    }
    fn play_audio(&self, _device: &str, _render: AudioRender) -> Result<(Unsupported, String)> {
        Err(unsupported("audio playback"))
    }
    fn capture_browser(
        &self,
        _page: &BrowserPage<'_>,
        _video: VideoSink<Frame>,
        _audio: Option<AudioSink>,
    ) -> Result<(Unsupported, CaptureFormat, Arc<dyn BrowserControl>)> {
        Err(unsupported("browser capture"))
    }

    fn has_secret(&self, service: &str, account: &str) -> Result<bool> {
        isocast_platform_credentials::has_secret(service, account)
    }
    fn secret(&self, service: &str, account: &str) -> Result<Option<String>> {
        isocast_platform_credentials::secret(service, account)
    }
    fn set_secret(&self, service: &str, account: &str, secret: &str) -> Result<()> {
        isocast_platform_credentials::set_secret(service, account, secret)
    }
    fn delete_secret(&self, service: &str, account: &str) -> Result<bool> {
        isocast_platform_credentials::delete_secret(service, account)
    }
}
