//! Native Windows backend. The daemon is not switched until capture lifetime
//! and the remaining integration gates pass.
//! Unsupported services fail closed; they never become synthetic successes.
//!
//! Camera frames arrive in the format their delivery path produces: native
//! Media Foundation modes as GPU NV12, or BGRA8 uploaded from shared Frame
//! Server MJPEG when another app controls the camera. Nothing here converts
//! one into the other.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use isocast_engine::gpu::Gpu;
use isocast_engine::{
    AudioRender, AudioSink, BrowserControl, BrowserPage, CameraMetadataReader, CameraRequest,
    CaptureFormat, EncoderFrameFormat, LiveAudioSpec, LiveVideoSpec, MediaInfo, MediaPart,
    MediaReader, Platform, Probe, ScreenTarget, Time, VideoSink, Volume, WriterSpec,
};
use isocast_protocol::api::Machine;
use isocast_protocol::devices::{Devices, PermissionKind, PermissionsReport};
use isocast_protocol::{Canvas, SourceKind};
use parking_lot::Mutex;

use crate::camera::{CameraCapture, capture_camera};
use crate::{
    AudioCapture, Dx12Importer, Frame, GraphicsDevice, IsolatedCapture, MfAacEncoder, OutputPool,
    PixelFormat, VideoWriter, WindowsVideoEncoder, isolated_capture_screen, now, system,
};

/// A cheap shared handle. Machine/disk/clock queries do not initialize a GPU;
/// pools, captures and encoders share the same lazily opened D3D11 device.
#[derive(Clone, Default)]
pub struct WindowsPlatform {
    device: Arc<Mutex<Option<GraphicsDevice>>>,
}

/// Dropping any native guard stops and joins its capture worker.
pub enum Capture {
    Screen(IsolatedCapture),
    Audio(AudioCapture),
    /// Owns only this process's reader. A shared Frame Server attachment
    /// never stops or reconfigures the app that controls the camera.
    Camera(CameraCapture),
}

impl WindowsPlatform {
    fn graphics(&self) -> Result<GraphicsDevice> {
        let mut current = self.device.lock();
        if let Some(device) = &*current {
            return Ok(device.clone());
        }
        let device = GraphicsDevice::new()?;
        *current = Some(device.clone());
        Ok(device)
    }
}

/// Services not implemented by the native backend yet cannot hand out a
/// functioning-looking resource. This type deliberately has no values.
#[derive(Debug, Clone)]
pub enum Unsupported {}

impl MediaReader for Unsupported {
    type Frame = Frame;

    fn next_video(&mut self) -> Result<Option<(Frame, Duration)>> {
        match *self {}
    }

    fn next_audio(&mut self) -> Result<Option<(Vec<f32>, Duration)>> {
        match *self {}
    }
}

fn unsupported(what: &str) -> anyhow::Error {
    anyhow!("{what}: not implemented by the native Windows backend yet")
}

impl Platform for WindowsPlatform {
    type Frame = Frame;
    type FramePool = OutputPool;
    type Writer = VideoWriter;
    type Capture = Capture;
    type Importer = Dx12Importer;
    type Media = Unsupported;
    type Playback = Unsupported;
    type VideoEncoder = WindowsVideoEncoder;
    type AudioEncoder = MfAacEncoder;
    /// The portable surface type, so the daemon's bound holds; creating one
    /// still fails on Windows until native preview sharing lands.
    type SharedSurface = isocast_surfaces::Surface;

    fn now(&self) -> Time {
        now()
    }

    fn devices(&self) -> Result<Devices> {
        crate::devices::devices()
    }

    fn permissions(&self) -> Result<PermissionsReport> {
        Ok(crate::permissions::report())
    }

    fn request_permissions(&self, _only: Option<PermissionKind>) -> Result<PermissionsReport> {
        // Unpackaged desktop apps have no per-app permission prompt to request.
        // Return honest unknown states and guidance, without opening settings.
        self.permissions()
    }

    fn open_permission_settings(&self, permission: PermissionKind) -> Result<String> {
        crate::permissions::open(permission)
    }

    fn media_info(&self, _path: &Path) -> Result<MediaInfo> {
        Err(unsupported("reading media file information"))
    }

    fn open_media(&self, _path: &Path, _part: MediaPart) -> Result<Unsupported> {
        Err(unsupported("decoding media files"))
    }

    fn frame_importer(&self, gpu: &Gpu) -> Result<Dx12Importer> {
        Dx12Importer::new(gpu)
    }

    fn machine(&self) -> Machine {
        system::machine()
    }

    fn probe(&self, source: &SourceKind, canvas: &Canvas) -> Result<Option<Probe>> {
        match source {
            SourceKind::Display(display) => {
                crate::devices::screen_probe(ScreenTarget::Display(&display.display), canvas.fps)
                    .map(Some)
            }
            SourceKind::Window(window) => crate::devices::screen_probe(
                ScreenTarget::Window {
                    app: &window.app,
                    title: window.title.as_deref(),
                },
                canvas.fps,
            )
            .map(Some),
            SourceKind::Camera(camera) => crate::devices::camera_probe(&camera.device).map(Some),
            SourceKind::TestPattern(_)
            | SourceKind::Tone(_)
            | SourceKind::Color(_)
            | SourceKind::Image(_)
            | SourceKind::Slideshow(_)
            | SourceKind::Text(_) => Ok(None),
            other => Err(unsupported(&format!("probing {} sources", other.label()))),
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

    fn thread_cpu_time(&self) -> Option<Duration> {
        system::thread_cpu_time()
    }

    fn real_time_thread(&self) {
        system::real_time_thread();
    }

    fn frame_pool(&self, width: u32, height: u32) -> Result<OutputPool> {
        OutputPool::new(self.graphics()?, width, height)
    }

    fn ycbcr_frame_pool(&self, _width: u32, _height: u32) -> Result<OutputPool> {
        bail!("frame_pool: Windows compositor output is BGRA; Y'CbCr targets are not supported")
    }

    fn encoder_frame_format(&self) -> EncoderFrameFormat {
        EncoderFrameFormat::Bgra
    }

    fn frame_size(&self, frame: &Frame) -> (u32, u32) {
        (frame.width(), frame.height())
    }

    fn frame_is_ycbcr(&self, frame: &Frame) -> bool {
        // Native camera modes deliver NV12; outputs and screens are BGRA.
        frame.pixel_format() == PixelFormat::Nv12
    }

    fn shared_surface(
        &self,
        _gpu: &Gpu,
        _width: u32,
        _height: u32,
    ) -> Result<(isocast_surfaces::Surface, wgpu::Texture)> {
        Err(unsupported("shared preview surfaces"))
    }

    fn encode_jpeg(
        &self,
        _frame: &Frame,
        _max_width: Option<u32>,
        _quality: f32,
    ) -> Result<(Vec<u8>, u32, u32)> {
        Err(unsupported("JPEG previews"))
    }

    fn create_writer(&self, spec: &WriterSpec) -> Result<VideoWriter> {
        // Composed outputs and staged ISOs are BGRA8. Raw ISOs, whose
        // frames may be NV12 from a native MF camera, come through
        // `create_writer_for_frame`; the encoder rejects a mismatched frame
        // rather than converting. Output preference is not permission to
        // convert camera ISOs to RGB.
        let encoder =
            WindowsVideoEncoder::recording(&self.graphics()?, &spec.video, PixelFormat::Bgra8)?;
        VideoWriter::from_encoder(spec, encoder)
    }

    fn create_writer_for_frame(&self, spec: &WriterSpec, frame: &Frame) -> Result<VideoWriter> {
        // The encoder takes the source's frames as captured: NV12 from a
        // native camera mode, BGRA8 from screens, windows and shared cameras.
        let encoder =
            WindowsVideoEncoder::recording(&self.graphics()?, &spec.video, frame.pixel_format())?;
        VideoWriter::from_encoder(spec, encoder)
    }

    fn recover_file(
        &self,
        _folder: &Path,
        _name: &std::ffi::OsStr,
    ) -> Result<Option<isocast_engine::RecoveredFile>> {
        // Windows engine startup already repairs through isocast-mux's one
        // pinned directory lease. This legacy path-only hook cannot receive
        // that lease; reopening a folder path here could follow a retargeted
        // junction and must not masquerade as safe recovery.
        bail!("recover_file: use Windows handle-anchored engine recovery")
    }

    fn capture_screen(
        &self,
        target: ScreenTarget<'_>,
        fps: u32,
        ten_bit: bool,
        cursor: bool,
        sink: VideoSink<Frame>,
    ) -> Result<(Capture, CaptureFormat)> {
        if ten_bit {
            bail!("screen.bit_depth: Windows capture currently supports 8-bit only");
        }
        let target = crate::devices::screen_target(target)?;
        let helper = std::env::current_exe()?.with_file_name("isocast-capture.exe");
        isolated_capture_screen(&self.graphics()?, &helper, target, fps, cursor, sink)
            .map(|(capture, format)| (Capture::Screen(capture), format))
    }

    fn capture_camera(
        &self,
        device: &str,
        request: CameraRequest,
        canvas: &Canvas,
        sink: VideoSink<Frame>,
    ) -> Result<(Capture, CaptureFormat)> {
        capture_camera(
            &self.graphics()?,
            device,
            request,
            canvas,
            sink,
            system::real_time_thread,
        )
        .map(|(capture, format)| (Capture::Camera(capture), format))
    }

    fn camera_metadata(&self, capture: &Capture) -> Option<CameraMetadataReader> {
        match capture {
            Capture::Camera(camera) => Some(camera.metadata_reader()),
            Capture::Screen(_) | Capture::Audio(_) => None,
        }
    }

    fn capture_microphone(
        &self,
        device: &str,
        channels: Option<&[u32]>,
        sink: AudioSink,
    ) -> Result<(Capture, u32)> {
        crate::capture_microphone(device, channels, sink, system::real_time_thread)
            .map(|(capture, channels)| (Capture::Audio(capture), channels))
    }

    fn capture_system_audio(
        &self,
        exclude: &[String],
        sink: AudioSink,
        exclude_own: bool,
    ) -> Result<(Capture, u32)> {
        crate::capture_system_audio(exclude, sink, exclude_own, system::real_time_thread)
            .map(|(capture, channels)| (Capture::Audio(capture), channels))
    }

    fn play_audio(&self, _device: &str, _render: AudioRender) -> Result<(Unsupported, String)> {
        Err(unsupported("audio playback"))
    }

    fn capture_browser(
        &self,
        _page: &BrowserPage<'_>,
        _video: VideoSink<Frame>,
        _audio: Option<AudioSink>,
    ) -> Result<(Capture, CaptureFormat, Arc<dyn BrowserControl>)> {
        Err(unsupported("browser sources"))
    }

    fn capture_application_audio(&self, _app: &str, _sink: AudioSink) -> Result<(Capture, u32)> {
        Err(unsupported("application audio capture"))
    }

    fn create_video_encoder(&self, spec: &LiveVideoSpec) -> Result<WindowsVideoEncoder> {
        // Live FLV/frame-stream callers require H.264. HEVC is confined to
        // the file writer's explicitly checked codec/configuration boundary.
        WindowsVideoEncoder::new(&self.graphics()?, spec)
    }

    fn create_audio_encoder(&self, spec: &LiveAudioSpec) -> Result<MfAacEncoder> {
        MfAacEncoder::with_thread_start(spec, system::real_time_thread)
    }

    fn has_secret(&self, _service: &str, _account: &str) -> Result<bool> {
        Err(unsupported("the Windows credential store"))
    }

    fn secret(&self, _service: &str, _account: &str) -> Result<Option<String>> {
        Err(unsupported("the Windows credential store"))
    }

    fn set_secret(&self, _service: &str, _account: &str, _secret: &str) -> Result<()> {
        Err(unsupported("the Windows credential store"))
    }

    fn delete_secret(&self, _service: &str, _account: &str) -> Result<bool> {
        Err(unsupported("the Windows credential store"))
    }
}
