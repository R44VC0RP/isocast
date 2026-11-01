use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};
use isocast_engine::gpu::Gpu;
use isocast_engine::{
    AudioRender, AudioSink, BrowserControl, BrowserPage, CameraRequest, CaptureFormat,
    EncoderFrameFormat, LiveAudioSpec, LiveVideoSpec, MediaInfo, MediaPart, Platform, Probe,
    ScreenTarget, Time, VideoSink, Volume, WriterSpec,
};
use isocast_protocol::api::Machine;
use isocast_protocol::devices::{
    Devices, Permission, PermissionAction, PermissionDetail, PermissionKind, Permissions,
    PermissionsReport,
};
use isocast_protocol::{Canvas, SourceKind};

use crate::frames::{Frame, FramePool, Importer};
use crate::system;
use crate::video_encoder::Encoder;
use crate::writer::Writer;
use crate::{browser, camera, pipewire_audio, portal};

#[derive(Clone, Copy, Default)]
pub struct LinuxPlatform;

// Each native backend owns its shutdown; no capture thread is detached.
pub enum Capture {
    Screen(portal::Capture),
    Browser(browser::Capture),
    Audio(pipewire_audio::Capture),
    Camera(camera::Capture),
}

fn unsupported(what: &str) -> anyhow::Error {
    anyhow::anyhow!("{what}: not supported by the Linux backend")
}

impl Platform for LinuxPlatform {
    type Frame = Frame;
    type FramePool = FramePool;
    type Writer = Writer;
    type Capture = Capture;
    type Importer = Importer;
    type Media = crate::media::Reader;
    type Playback = crate::playback::Playback;
    type VideoEncoder = Encoder;
    type AudioEncoder = crate::audio::StreamEncoder;
    type SharedSurface = isocast_surfaces::Surface;

    const CAMERA_IS_MONITOR: bool = true;

    fn virtual_camera_sink(
        &self,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<Box<dyn isocast_engine::record::CameraSink<Frame>>> {
        camera::output::start(width, height, fps)
    }

    fn now(&self) -> Time {
        system::now()
    }
    fn machine(&self) -> Machine {
        system::machine()
    }
    fn missing_codecs(&self) -> Vec<isocast_engine::MissingCodec> {
        match isocast_openh264::check() {
            Ok(_) => Vec::new(),
            Err(error) => vec![isocast_engine::MissingCodec {
                name: "OpenH264".into(),
                reason: format!(
                    "{}: software H.264 encoding and live H.264 previews are unavailable",
                    error.reason
                ),
                fix: error.fix,
            }],
        }
    }
    fn browser_warning(&self) -> Option<isocast_engine::DoctorWarning> {
        browser::doctor_warning()
    }
    fn video_encoders(&self) -> isocast_engine::VideoEncoders {
        let availability = crate::nvenc::availability();
        isocast_engine::VideoEncoders {
            hardware: if availability.is_ok() {
                vec!["NVIDIA NVENC (H.264)".into()]
            } else {
                Vec::new()
            },
            software: vec!["OpenH264 (software)".into()],
            unavailable: availability
                .as_ref()
                .err()
                .map(|reason| format!("NVIDIA NVENC (H.264): {reason}"))
                .into_iter()
                .collect(),
        }
    }
    fn volume(&self, path: &Path) -> Result<Volume> {
        system::volume(path)
    }
    fn process_cpu_seconds(&self) -> Option<f64> {
        system::process_cpu_seconds()
    }

    fn devices(&self) -> Result<Devices> {
        let mut devices = Devices {
            displays: portal::displays()?,
            cameras: camera::devices()?,
            permissions: self.permissions()?.permissions,
            ..Default::default()
        };
        match pipewire_audio::devices() {
            Ok((microphones, outputs, apps)) => {
                devices.microphones = microphones;
                devices.audio_outputs = outputs;
                devices.audio_apps = apps;
            }
            Err(error) => devices
                .notes
                .push(format!("PipeWire audio devices: {error:#}")),
        }
        devices.notes.push("Screen selection happens in the desktop portal when capture opens. Sizes are unknown until selected; saved sizes describe the last grant. Restore tokens avoid repeated prompts only on portals that support persistent grants. Desktop audio uses the default output's monitor through a system_audio source.".into());
        Ok(devices)
    }
    fn permissions(&self) -> Result<PermissionsReport> {
        let camera = match camera::access() {
            Some(true) => Permission::Granted,
            Some(false) => Permission::Denied,
            None => Permission::Unknown,
        };
        let details = (camera == Permission::Denied)
            .then(|| PermissionDetail {
                permission: PermissionKind::Camera,
                state: camera,
                needed_for: "cameras and capture cards".into(),
                action: PermissionAction::AskAdministrator,
                settings_url: String::new(),
                next_step: Some(
                    "your user can't open any /dev/video* device: log in at this computer's \
                     own screen (the desktop session is given its cameras), or add your user \
                     to the `video` group (`sudo usermod -aG video $USER`) and log in again"
                        .into(),
                ),
                restart_after_granting: true,
                app: String::new(),
                app_path: None,
                denied_without_prompt: false,
            })
            .into_iter()
            .collect();
        Ok(PermissionsReport {
            permissions: Permissions {
                screen_recording: Permission::Unknown,
                camera,
                microphone: Permission::Unknown,
                system_audio: Permission::Unknown,
            },
            app: "Isocast".into(),
            details,
        })
    }
    fn source_open_timeout(&self, source: &SourceKind) -> std::time::Duration {
        std::time::Duration::from_secs(match source {
            SourceKind::Display(_) => 120,
            // PipeWire allows 5s enumeration then 5s for the first samples.
            SourceKind::Microphone(_)
            | SourceKind::SystemAudio(_)
            | SourceKind::ApplicationAudio(_) => 12,
            _ => 8,
        })
    }
    fn request_permissions(&self, only: Option<PermissionKind>) -> Result<PermissionsReport> {
        if only.is_none() || only == Some(PermissionKind::ScreenRecording) {
            // Consent belongs to a selection, not a global permission bit.
            // Permit a failed source's next retry to show the portal again.
            portal::allow_prompt();
        }
        self.permissions()
    }
    fn open_permission_settings(&self, _permission: PermissionKind) -> Result<String> {
        Err(unsupported("opening capture permission settings"))
    }
    fn media_info(&self, path: &Path) -> Result<MediaInfo> {
        crate::media::media_info(path)
    }
    fn open_media(&self, path: &Path, part: MediaPart) -> Result<crate::media::Reader> {
        crate::media::open_media(path, part)
    }
    fn doctor_checks(&self) -> Vec<isocast_protocol::api::Check> {
        let [server, capture] = pipewire_audio::scheduling_checks();
        use isocast_protocol::api::{Check, CheckStatus};
        let (status, detail, fix) = match camera::output::device() {
            Ok(node) => (
                CheckStatus::Ok,
                format!("v4l2loopback: {}", node.display()),
                None,
            ),
            Err(e) => (
                CheckStatus::Warning,
                "Optional camera output unavailable; recording is unaffected".into(),
                Some(e.to_string()),
            ),
        };
        vec![
            crate::media::decoders_check(),
            portal::doctor_check(),
            server,
            capture,
            Check {
                name: "virtual camera".into(),
                status,
                detail,
                fix,
            },
        ]
    }
    fn probe(&self, source: &SourceKind, canvas: &Canvas) -> Result<Option<Probe>> {
        match source {
            SourceKind::Camera(source) => camera::probe_capture(
                &source.device,
                CameraRequest {
                    size: source.width.zip(source.height),
                    fps: source.fps,
                },
                canvas,
            )
            .map(Some),
            SourceKind::Display(display) => {
                let saved = portal::displays()?
                    .into_iter()
                    .find(|d| d.id == display.display);
                Ok(Some(Probe {
                    device: display.display.clone(),
                    video: saved
                        .filter(|d| d.width > 0 && d.height > 0)
                        .map(|d| CaptureFormat {
                            width: d.width,
                            height: d.height,
                            fps: canvas.fps,
                        }),
                    channels: None,
                }))
            }
            SourceKind::Microphone(mic) => {
                let (devices, _, _) = pipewire_audio::devices()?;
                let device = devices.iter().find(|d| d.id == mic.device || d.name == mic.device)
                    .or_else(|| (mic.device == "default" || mic.device == "main")
                        .then(|| devices.iter().find(|device| device.default).or_else(|| devices.first())).flatten())
                    .ok_or_else(|| anyhow::anyhow!("microphone.device {:?}: not found; run devices for available PipeWire inputs", mic.device))?;
                Ok(Some(Probe {
                    device: device.id.clone(),
                    video: None,
                    channels: Some(
                        mic.channels
                            .as_ref()
                            .map_or(device.channels.min(2), |c| c.len() as u32),
                    ),
                }))
            }
            SourceKind::SystemAudio(_) => Ok(Some(Probe {
                device: "default output monitor".into(),
                video: None,
                channels: Some(2),
            })),
            SourceKind::ApplicationAudio(app) => Ok(Some(Probe {
                device: pipewire_audio::probe_application(&app.app)?,
                video: None,
                channels: Some(2),
            })),
            SourceKind::TestPattern(_)
            | SourceKind::Tone(_)
            | SourceKind::Color(_)
            | SourceKind::Image(_)
            | SourceKind::Slideshow(_)
            | SourceKind::Text(_)
            | SourceKind::Media(_)
            | SourceKind::Browser(_) => Ok(None),
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
    /// A memfd surface for a local client, and a GPU texture the frame is
    /// drawn into first; the engine reads it back into the surface.
    fn shared_surface(
        &self,
        gpu: &Gpu,
        width: u32,
        height: u32,
    ) -> Result<(Self::SharedSurface, wgpu::Texture)> {
        let surface = isocast_surfaces::Surface::rgba(width, height)?;
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("shared frame"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        Ok((surface, texture))
    }
    fn shared_surface_in_memory(&self) -> bool {
        true
    }
    fn write_shared_surface(
        &self,
        surface: &Self::SharedSurface,
        rows: &[u8],
        stride: usize,
        bgra: bool,
    ) -> Result<()> {
        surface.write(rows, stride, bgra)
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
    fn create_audio_encoder(&self, spec: &LiveAudioSpec) -> Result<Self::AudioEncoder> {
        crate::audio::StreamEncoder::new(spec)
    }

    fn capture_screen(
        &self,
        target: ScreenTarget<'_>,
        fps: u32,
        ten_bit: bool,
        cursor: bool,
        sink: VideoSink<Frame>,
    ) -> Result<(Capture, CaptureFormat)> {
        let (capture, format) = portal::capture(target, fps, ten_bit, cursor, sink)?;
        Ok((Capture::Screen(capture), format))
    }
    fn capture_camera(
        &self,
        device: &str,
        request: CameraRequest,
        canvas: &Canvas,
        sink: VideoSink<Frame>,
    ) -> Result<(Capture, CaptureFormat)> {
        let (capture, format) = camera::capture(device, request, canvas, sink)?;
        Ok((Capture::Camera(capture), format))
    }
    fn capture_microphone(
        &self,
        device: &str,
        channels: Option<&[u32]>,
        sink: AudioSink,
    ) -> Result<(Capture, u32)> {
        let (capture, channels) = pipewire_audio::microphone(device, channels, sink)?;
        Ok((Capture::Audio(capture), channels))
    }
    fn capture_system_audio(
        &self,
        exclude: &[String],
        sink: AudioSink,
        exclude_own: bool,
    ) -> Result<(Capture, u32)> {
        let (capture, channels) = pipewire_audio::desktop(exclude, sink, exclude_own)?;
        Ok((Capture::Audio(capture), channels))
    }
    fn capture_application_audio(&self, app: &str, sink: AudioSink) -> Result<(Capture, u32)> {
        let (capture, channels) = pipewire_audio::application(app, sink)?;
        Ok((Capture::Audio(capture), channels))
    }
    fn play_audio(
        &self,
        device: &str,
        render: AudioRender,
    ) -> Result<(crate::playback::Playback, String)> {
        crate::playback::play(device, render)
    }
    fn capture_browser(
        &self,
        page: &BrowserPage<'_>,
        video: VideoSink<Frame>,
        audio: Option<AudioSink>,
    ) -> Result<(Capture, CaptureFormat, Arc<dyn BrowserControl>)> {
        let (capture, format, control) = browser::capture_browser(page, video, audio)?;
        Ok((Capture::Browser(capture), format, control))
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
