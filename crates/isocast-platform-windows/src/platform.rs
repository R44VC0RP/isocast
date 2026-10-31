//! Native Windows backend. The daemon is not switched until capture lifetime
//! and the remaining integration gates pass.
//! Unsupported services fail closed; they never become synthetic successes.
//!
//! Camera frames arrive in the format their delivery path produces: native
//! Media Foundation modes as GPU NV12, or BGRA8 uploaded from shared Frame
//! Server MJPEG when another app controls the camera. Nothing here converts
//! one into the other.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use isocast_engine::gpu::Gpu;
use isocast_engine::{
    AudioRender, AudioSink, BrowserControl, BrowserPage, CameraMetadataReader, CameraRequest,
    CaptureFormat, EncoderAdmission, EncoderAdmissionOutput, EncoderFrameFormat, FileWriter,
    LiveAudioSpec, LiveVideoSpec, MediaInfo, MediaPart, Platform, Probe, ScreenTarget, Time,
    VideoSink, Volume, WriterRequest, WriterSpec, encoder_calibration,
};
use isocast_protocol::api::Machine;
use isocast_protocol::devices::{Devices, PermissionKind, PermissionsReport};
use isocast_protocol::{Canvas, SourceKind};
use parking_lot::Mutex;

use crate::camera::{CameraCapture, capture_camera};
use crate::encoder::NvencOwner;
use crate::nvenc::{NVENC_NATIVE_IN_FLIGHT, NVENC_PENDING_READY_LIMIT, NvencAdmission};
use crate::{
    AudioCapture, Dx12Importer, Frame, GraphicsDevice, IsolatedCapture, MfAacEncoder,
    NvencBudgetOutput, NvencBudgetProfile, OutputPool, PixelFormat, RecordingQuality, VideoWriter,
    WindowsVideoEncoder, isolated_capture_screen, now, system,
};

/// A cheap shared handle. Machine/disk/clock queries do not initialize a GPU;
/// pools, captures and encoders share the same lazily opened D3D11 device.
#[derive(Clone, Default)]
pub struct WindowsPlatform {
    device: Arc<Mutex<Option<GraphicsDevice>>>,
    outputs: Arc<crate::playback::Outputs>,
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

/// The per-engine NVENC preset calibration, beside the machine calibration
/// (`setup.toml` -> `setup.nvenc-budget.json`), so `$ISOCAST_SETUP` and
/// `$ISOCAST_SOCKET` isolate it the same way. Only ever read here.
fn budget_cache_path() -> PathBuf {
    isocast_protocol::api::setup_path().with_extension("nvenc-budget.json")
}

/// The only qualification label whose figures may be applied.
const ACCEPTED_QUALIFICATION: &str = "accepted-workload-specific";
const CALIBRATION_UNITS: &str = "completed 1080p-equivalent frames/s";

/// The recording policy this encoder runs (see `NvencEncoder` and
/// `WindowsVideoEncoder::recording_policy`): LL single pass, constant QP,
/// spatial AQ8, no temporal AQ/B-frames/lookahead, two-second GOP, and this
/// build's admission queue. Every structured field must match exactly;
/// descriptive text is never interpreted.
fn check_calibration_policy(cache: &encoder_calibration::PresetCalibration) -> Result<()> {
    if cache.qualification != ACCEPTED_QUALIFICATION {
        bail!(
            "qualification is {:?}, not {ACCEPTED_QUALIFICATION:?}",
            cache.qualification
        );
    }
    if cache.units != CALIBRATION_UNITS {
        bail!("units are {:?}, not {CALIBRATION_UNITS:?}", cache.units);
    }
    if cache.preset_order != [1, 3, 5] {
        bail!("preset_order is {:?}, not [1, 3, 5]", cache.preset_order);
    }
    let policy = &cache.policy;
    let expected = [
        ("tuning", policy.tuning == "LL"),
        ("rate_control", policy.rate_control == "CQP"),
        ("spatial_aq", policy.spatial_aq == 8),
        ("temporal_aq", !policy.temporal_aq),
        ("b_frames", policy.b_frames == 0),
        ("lookahead", !policy.lookahead),
        ("multipass", !policy.multipass),
        ("input", policy.input == "D3D11 BGRA8"),
        ("output", policy.output == "8-bit 4:2:0"),
        (
            "gop_frames",
            policy.nominal_fps > 0 && Some(policy.gop_frames) == policy.nominal_fps.checked_mul(2),
        ),
        (
            "native_in_flight",
            policy.native_in_flight as usize == NVENC_NATIVE_IN_FLIGHT,
        ),
        (
            "pending_ready_limit",
            policy.pending_ready_limit as usize == NVENC_PENDING_READY_LIMIT,
        ),
    ];
    if let Some((field, _)) = expected.iter().find(|(_, ok)| !ok) {
        bail!(
            "policy.{field} differs from this encoder's (LL/CQP/AQ8, no temporal AQ, B-frames, lookahead or multipass, D3D11 BGRA8 -> 8-bit 4:2:0, 2 s GOP, {NVENC_NATIVE_IN_FLIGHT} in flight/{NVENC_PENDING_READY_LIMIT} pending+ready)"
        );
    }
    Ok(())
}

/// The throughput figures one batch is planned with.
struct BudgetFigures {
    profile: NvencBudgetProfile,
    source: &'static str,
    cache_status: String,
    /// Why measured figures were not applied, when they weren't.
    reason: Option<String>,
    /// The one settled snapshot of open sessions this batch is planned
    /// beside.
    active: Vec<crate::NvencAccounted>,
}

enum Figures {
    Planned(BudgetFigures),
    /// Only for an adapter without an NVENC profile, and never beside open
    /// planned work: the reason it is opened as before admission existed.
    Unplanned(String),
}

/// Whether a session runs the measured policy (see
/// `check_calibration_policy`): its QP, BGRA8 input and frame rate, so its
/// two-second GOP is the measured GOP too. `Some` names the difference.
fn calibration_mismatch(
    policy: &encoder_calibration::CalibrationPolicy,
    qp: Option<u32>,
    input: PixelFormat,
    fps: u32,
) -> Option<String> {
    match qp {
        None => Some("bitrate or MF rate control, not the measured constant QP".into()),
        Some(qp) if qp != policy.qp => Some(format!("QP {qp}, measured at QP {}", policy.qp)),
        Some(_) if input != PixelFormat::Bgra8 => {
            Some(format!("{input:?} input, measured with BGRA8"))
        }
        Some(_) if fps != policy.nominal_fps => Some(format!(
            "{fps} fps (GOP {}), measured at {} fps (GOP {})",
            u64::from(fps) * 2,
            policy.nominal_fps,
            policy.gop_frames
        )),
        Some(_) => None,
    }
}

/// `Unplanned` only when this adapter has no NVENC profile and no planned
/// session is open. On a profiled adapter an output the profile cannot
/// price refuses the batch. A missing, rejected or inapplicable cache uses
/// the labelled reference figures; it is never reported as calibrated.
fn budget_figures(
    device: &GraphicsDevice,
    admission: &NvencAdmission,
    (recording, take): (u64, u32),
    files: &[WriterRequest<'_, Frame>],
    formats: &[PixelFormat],
    cache: &Path,
) -> Result<Figures> {
    let reference = match NvencBudgetProfile::reference(device) {
        Ok(reference) => reference,
        Err(error) => {
            if admission.planned_active() {
                bail!(
                    "encoder.budget: planned NVENC sessions are open, but this batch cannot be priced ({error:#}); refusing to open it unplanned beside them"
                );
            }
            return Ok(Figures::Unplanned(format!("{error:#}")));
        }
    };
    for file in files {
        let video = &file.spec.video;
        if video.encoder.effective_bit_depth() != 8
            || !matches!(
                video.encoder.codec,
                isocast_protocol::Codec::H264 | isocast_protocol::Codec::Hevc
            )
        {
            bail!(
                "encoder.budget: take {} {} is not 8-bit H.264/HEVC, which this adapter's NVENC profile cannot price; refused before opening any file",
                file.take,
                file.output
            );
        }
    }
    // One non-blocking snapshot for the whole batch: scope, cache
    // applicability and the plan all see the same sessions.
    let active = admission.settled()?;
    let reference_with = |cache_status: String, reason: String| {
        Figures::Planned(BudgetFigures {
            profile: reference.clone(),
            source: "reference",
            cache_status,
            reason: Some(reason),
            active: active.clone(),
        })
    };
    let rejected = |error: anyhow::Error| {
        let reason = format!("calibration cache rejected: {error:#}");
        isocast_diagnostics::emit(
            "warn",
            "encoder budget",
            &reason,
            cache.display().to_string().into(),
        );
        reference_with(format!("rejected: {error:#}"), reason)
    };
    let calibration = match encoder_calibration::load(cache) {
        Ok(None) => {
            return Ok(reference_with(
                "missing".into(),
                "no calibration cache; using reference estimates, not measurements".into(),
            ));
        }
        Err(error) => return Ok(rejected(error)),
        Ok(Some(calibration)) => calibration,
    };
    if let Err(error) = check_calibration_policy(&calibration) {
        return Ok(rejected(error));
    }
    let not_applicable = |what: String| -> Result<Figures> {
        Ok(reference_with(
            "valid; not applicable to this batch".into(),
            format!("calibration not applied: {what}; using reference estimates"),
        ))
    };
    // Measured figures describe only sessions that run the measured policy:
    // this batch's outputs, and every open session it is charged beside (a
    // live CBR or MF session is never priced as a measured CQP one).
    for (file, &format) in files.iter().zip(formats) {
        let video = &file.spec.video;
        let qp = Some(RecordingQuality::new(video.encoder.quality)?.nvenc_qp);
        if let Some(mismatch) = calibration_mismatch(&calibration.policy, qp, format, video.fps) {
            return not_applicable(format!(
                "take {} {} uses {mismatch}",
                file.take, file.output
            ));
        }
    }
    let handover: std::collections::BTreeSet<&str> =
        files.iter().map(|file| file.output.as_str()).collect();
    for session in &active {
        if session.handed_over(recording, take, &handover) {
            continue;
        }
        if let Some(mismatch) =
            calibration_mismatch(&calibration.policy, session.qp, session.input, session.fps)
        {
            return not_applicable(format!(
                "open session {}/{} uses {mismatch}",
                session.take, session.output
            ));
        }
    }
    match reference
        .clone()
        .calibrated(&calibration.key, calibration.h264, calibration.hevc)
    {
        Ok(profile) => Ok(Figures::Planned(BudgetFigures {
            profile,
            source: "calibrated",
            cache_status: "applied".into(),
            reason: None,
            active: active.clone(),
        })),
        Err(error) => Ok(rejected(error)),
    }
}

fn budget_id(file: &WriterRequest<'_, Frame>) -> String {
    format!("take{}/{}", file.take, file.output)
}

impl WindowsPlatform {
    /// One encoder outside a recording batch (a single writer or a live
    /// encoder), serialized with every other open. While planned work is
    /// open it is priced at its fixed P5 configuration, with reference
    /// figures, before its native session exists, and opened on NVENC only;
    /// otherwise it opens as before admission existed. Either way it is
    /// accounted for later batches.
    fn open_single(
        &self,
        owner: NvencOwner,
        (codec, bit_depth, qp): (isocast_protocol::Codec, u8, Option<u32>),
        priced: impl FnOnce(&GraphicsDevice) -> Result<WindowsVideoEncoder>,
        legacy: impl FnOnce(&GraphicsDevice) -> Result<WindowsVideoEncoder>,
    ) -> Result<WindowsVideoEncoder> {
        let device = self.graphics()?;
        let admission = NvencAdmission::begin();
        if !admission.planned_active() {
            let mut encoder = legacy(&device)?;
            encoder.account(&admission, &device, owner);
            return Ok(encoder);
        }
        let owner = NvencOwner {
            planned: true,
            ..owner
        };
        let output = owner.output.clone();
        let profile = NvencBudgetProfile::reference(&device).context(
            "encoder.budget: planned NVENC sessions are open, but this adapter has no profile to price another encoder",
        )?;
        let candidate = owner.clone().candidate(codec, bit_depth, Some(5), qp);
        let load = profile.admit_fixed(&device, &admission, &candidate)?;
        let mut encoder = priced(&device)?;
        if encoder.nvenc_preset() != Some(5) {
            bail!(
                "encoder.budget: admitted fixed P5 for {output}, but the opened encoder reports {:?}",
                encoder.nvenc_preset()
            );
        }
        encoder.account(&admission, &device, owner);
        isocast_diagnostics::emit(
            "info",
            "encoder budget",
            &format!(
                "admitted {output} at fixed P5 beside planned NVENC work; estimated load {:.0}% (reference figures)",
                load * 100.0
            ),
            profile.key().to_string().into(),
        );
        Ok(encoder)
    }

    fn open_single_writer(&self, spec: &WriterSpec, input: PixelFormat) -> Result<VideoWriter> {
        let video = &spec.video;
        let qp = RecordingQuality::new(video.encoder.quality)?.nvenc_qp;
        let encoder = self.open_single(
            NvencOwner {
                recording: 0,
                take: 0,
                output: "writer".into(),
                size: (video.width, video.height, video.fps),
                input,
                planned: false,
            },
            (
                video.encoder.codec,
                video.encoder.effective_bit_depth(),
                Some(qp),
            ),
            |device| WindowsVideoEncoder::recording_with_preset(device, video, input, 5),
            |device| WindowsVideoEncoder::recording(device, video, input),
        )?;
        VideoWriter::from_encoder(spec, encoder)
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
    type Media = crate::media::Reader;
    type Playback = crate::playback::Playback;
    type VideoEncoder = WindowsVideoEncoder;
    type AudioEncoder = MfAacEncoder;
    /// The portable surface type, so the daemon's bound holds; creating one
    /// still fails on Windows until native preview sharing lands.
    type SharedSurface = isocast_surfaces::Surface;

    fn now(&self) -> Time {
        now()
    }

    fn sleep_until(&self, at: Time) {
        if let Err(error) = crate::sleep_until(at) {
            static REPORTED: AtomicBool = AtomicBool::new(false);
            if !REPORTED.swap(true, Ordering::Relaxed) {
                isocast_diagnostics::diagnostic!("warn";
                    "Windows media timer failed; using ordinary sleep: {error:#}"
                );
            }
            let remaining = at.saturating_since(now());
            std::thread::sleep(remaining);
        }
    }

    fn prefers_deadline_timer(&self) -> bool {
        true
    }

    fn gpu_adapter_luid(&self) -> Result<Option<(u32, i32)>> {
        Ok(Some(self.graphics()?.adapter_luid()))
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

    fn media_info(&self, path: &Path) -> Result<MediaInfo> {
        crate::media::media_info(path)
    }

    fn open_media(&self, path: &Path, part: MediaPart) -> Result<crate::media::Reader> {
        // Pictures go to the engine's device; sound needs none.
        let device = match part {
            MediaPart::Video => Some(self.graphics()?),
            MediaPart::Audio => None,
        };
        crate::media::open(device, path, part)
    }

    fn frame_importer(&self, gpu: &Gpu) -> Result<Dx12Importer> {
        Dx12Importer::new(gpu)
    }

    fn machine(&self) -> Machine {
        system::machine()
    }

    fn video_encoders(&self) -> isocast_engine::VideoEncoders {
        // The GPU recording would use; without one (a VM's basic display
        // driver can still give one) only software encoders are listed.
        crate::encoder::video_encoders(self.graphics().ok().as_ref())
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
            SourceKind::Microphone(mic) => {
                let (device, channels) =
                    crate::probe_microphone(&mic.device, mic.channels.as_deref())?;
                Ok(Some(Probe {
                    device: device.name,
                    video: None,
                    channels: Some(channels),
                }))
            }
            SourceKind::SystemAudio(settings) => {
                if !settings.exclude.is_empty() {
                    bail!(
                        "system_audio.exclude: process exclusion is not supported by this Windows endpoint-loopback backend; use exclude=[]"
                    );
                }
                let (device, channels) = crate::probe_loopback()?;
                Ok(Some(Probe {
                    device: device.name,
                    video: None,
                    channels: Some(channels),
                }))
            }
            SourceKind::TestPattern(_)
            | SourceKind::Tone(_)
            | SourceKind::Color(_)
            | SourceKind::Image(_)
            | SourceKind::Slideshow(_)
            | SourceKind::Text(_)
            | SourceKind::Media(_) => Ok(None),
            other => Err(unsupported(&format!("probing {} sources", other.label()))),
        }
    }

    fn volume(&self, path: &Path) -> Result<Volume> {
        system::volume(path)
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

    fn audio_thread(&self) {
        system::audio_thread();
    }

    fn recording_activity(&self) -> Result<Option<Box<dyn Send>>> {
        match crate::RecordingPowerGuard::new() {
            Ok(guard) => Ok(guard.map(|guard| Box::new(guard) as Box<dyn Send>)),
            Err(error) => {
                isocast_diagnostics::diagnostic!("warn"; "Windows recording power policy: {error:#}");
                Ok(None)
            }
        }
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
        gpu: &Gpu,
        width: u32,
        height: u32,
    ) -> Result<(isocast_surfaces::Surface, wgpu::Texture)> {
        let surface = isocast_surfaces::Surface::rgba_on(&gpu.device, width, height)?;
        let texture = surface.texture(&gpu.device, wgpu::TextureUsages::RENDER_ATTACHMENT)?;
        Ok((surface, texture))
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
        self.open_single_writer(spec, PixelFormat::Bgra8)
    }

    fn create_writer_for_frame(&self, spec: &WriterSpec, frame: &Frame) -> Result<VideoWriter> {
        // The encoder takes the source's frames as captured: NV12 from a
        // native camera mode, BGRA8 from screens, windows and shared cameras.
        self.open_single_writer(spec, frame.pixel_format())
    }

    fn create_writer_batch(
        &self,
        files: &[WriterRequest<'_, Frame>],
    ) -> Result<(Vec<VideoWriter>, Option<EncoderAdmission>)> {
        if files.is_empty() {
            return Ok((Vec::new(), None));
        }
        // A raw input's first frame decides its encoder's input format (NV12
        // from a native camera mode; never converted). Wait for every one
        // before planning, outside the admission lock, keeping only formats:
        // no captured picture is held while encoders open.
        let mut formats = Vec::with_capacity(files.len());
        for file in files {
            formats.push(match file.input {
                Some(input) => input()
                    .with_context(|| format!("could not create {}", file.spec.path.display()))?
                    .pixel_format(),
                None => PixelFormat::Bgra8,
            });
        }
        let device = self.graphics()?;
        // Held until every file is open and accounted, or released.
        let admission = NvencAdmission::begin();
        let cache = budget_cache_path();
        let mut report = EncoderAdmission {
            mode: "unplanned",
            reason: None,
            source: None,
            calibrated: false,
            key: None,
            cache: Some(cache.display().to_string()),
            cache_status: None,
            estimated_load: None,
            accounted_load: None,
            accounted_sessions: 0,
            handed_over_sessions: 0,
            outputs: Vec::with_capacity(files.len()),
        };
        // Every file of a batch belongs to one take of one recording.
        let recording = files[0].recording;
        if files.iter().any(|file| file.recording != recording) {
            bail!("encoder.budget: one batch must belong to one recording");
        }
        // The earliest take is conservative: fewer sessions are handed over.
        let take = files.iter().map(|file| file.take).min().unwrap_or(0);
        let figures = budget_figures(
            &device,
            &admission,
            (recording, take),
            files,
            &formats,
            &cache,
        )
        .context("encoder admission refused before opening any file")?;
        let plan = match figures {
            Figures::Unplanned(reason) => {
                report.reason = Some(reason);
                None
            }
            Figures::Planned(figures) => {
                let outputs: Vec<_> = files
                    .iter()
                    .map(|file| {
                        (
                            NvencBudgetOutput {
                                id: budget_id(file),
                                video: file.spec.video.clone(),
                                iso: file.iso,
                            },
                            file.output.clone(),
                        )
                    })
                    .collect();
                let plan = figures
                    .profile
                    .plan_accounted(
                        &device,
                        &admission,
                        &figures.active,
                        (recording, take),
                        &outputs,
                        false,
                    )
                    .with_context(|| {
                        format!(
                            "encoder admission refused before opening any file ({} figures, key {}{})",
                            figures.source,
                            figures.profile.key(),
                            figures
                                .reason
                                .as_ref()
                                .map(|reason| format!("; {reason}"))
                                .unwrap_or_default()
                        )
                    })?;
                report.mode = "planned";
                report.source = Some(figures.source);
                report.calibrated = plan.calibrated;
                report.key = Some(plan.calibration_key.clone());
                report.cache_status = Some(figures.cache_status);
                report.reason = figures.reason;
                report.estimated_load = Some(plan.estimated_load);
                report.accounted_load = Some(plan.accounted_load);
                report.accounted_sessions = plan.accounted_sessions;
                report.handed_over_sessions = plan.handed_over_sessions;
                Some(plan)
            }
        };
        let mut writers = Vec::with_capacity(files.len());
        for (file, &format) in files.iter().zip(&formats) {
            let opened = (|| -> Result<(VideoWriter, EncoderAdmissionOutput)> {
                let video = &file.spec.video;
                let planned = match &plan {
                    Some(plan) => Some(
                        *plan
                            .presets
                            .get(&budget_id(file))
                            .context("encoder.budget: output missing from its plan")?,
                    ),
                    None => None,
                };
                let mut encoder = match planned {
                    // Never falls back to another provider: the plan
                    // covered only this NVENC session.
                    Some(preset) => {
                        WindowsVideoEncoder::recording_with_preset(&device, video, format, preset)?
                    }
                    None => WindowsVideoEncoder::recording(&device, video, format)?,
                };
                let applied = encoder.nvenc_preset();
                if let Some(preset) = planned
                    && applied != Some(preset)
                {
                    bail!(
                        "encoder.budget: planned P{preset}, but the opened encoder reports {applied:?}"
                    );
                }
                encoder.account(
                    &admission,
                    &device,
                    NvencOwner {
                        recording: file.recording,
                        take: file.take,
                        output: file.output.clone(),
                        size: (video.width, video.height, video.fps),
                        input: format,
                        planned: planned.is_some(),
                    },
                );
                let output = EncoderAdmissionOutput {
                    take: file.take,
                    output: file.output.clone(),
                    path: file.spec.path.clone(),
                    planned_preset: planned,
                    applied_preset: applied,
                    backend: format!("{:?}", encoder.backend()),
                };
                Ok((VideoWriter::from_encoder(file.spec, encoder)?, output))
            })();
            match opened {
                Ok((writer, output)) => {
                    writers.push(writer);
                    report.outputs.push(output);
                }
                Err(err) => {
                    // Cancelling drops each encoder, then its reservation,
                    // before the admission lock is released.
                    writers.into_iter().for_each(FileWriter::cancel);
                    return Err(
                        err.context(format!("could not create {}", file.spec.path.display()))
                    );
                }
            }
        }
        drop(admission);
        Ok((writers, Some(report)))
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
            bail!(
                "recording.iso.encoder.bit_depth or sources.<id>.encoder.bit_depth: \
                 Windows screen capture supports 8-bit only; valid option: 8"
            );
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
        crate::capture_microphone(device, channels, sink, system::audio_thread)
            .map(|(capture, channels)| (Capture::Audio(capture), channels))
    }

    fn capture_system_audio(
        &self,
        exclude: &[String],
        sink: AudioSink,
        exclude_own: bool,
    ) -> Result<(Capture, u32)> {
        crate::capture_system_audio(exclude, sink, exclude_own, system::audio_thread)
            .map(|(capture, channels)| (Capture::Audio(capture), channels))
    }

    fn play_audio(&self, device: &str, render: AudioRender) -> Result<(Self::Playback, String)> {
        crate::playback::play(&self.outputs, device, render)
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
        // Fixed P5 bitrate control: priced as such, never reconfigured.
        let encoder = self.open_single(
            NvencOwner {
                recording: 0,
                take: 0,
                output: "live".into(),
                size: (spec.width, spec.height, spec.fps),
                input: PixelFormat::Bgra8,
                planned: false,
            },
            (isocast_protocol::Codec::H264, 8, None),
            |device| WindowsVideoEncoder::new_nvenc_only(device, spec),
            |device| WindowsVideoEncoder::new(device, spec),
        )?;
        isocast_diagnostics::debug!(
            "Windows live encoder: {:?}, {}x{}@{} at {} kb/s; fallback reasons: {:?}",
            encoder.backend(),
            spec.width,
            spec.height,
            spec.fps,
            spec.bitrate_kbps,
            encoder.fallback_reasons()
        );
        Ok(encoder)
    }

    fn create_audio_encoder(&self, spec: &LiveAudioSpec) -> Result<MfAacEncoder> {
        MfAacEncoder::streaming(spec, system::audio_thread)
    }

    fn has_secret(&self, service: &str, account: &str) -> Result<bool> {
        crate::credentials::has(service, account)
    }

    fn secret(&self, service: &str, account: &str) -> Result<Option<String>> {
        crate::credentials::get(service, account)
    }

    fn set_secret(&self, service: &str, account: &str, secret: &str) -> Result<()> {
        crate::credentials::set(service, account, secret)
    }

    fn delete_secret(&self, service: &str, account: &str) -> Result<bool> {
        crate::credentials::delete(service, account)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use isocast_protocol::{MicrophoneSource, SystemAudioSource};

    #[test]
    fn audio_sources_probe_the_selected_endpoint() {
        let platform = WindowsPlatform::default();
        let canvas = Canvas {
            width: 320,
            height: 180,
            fps: 30,
        };
        let mic = SourceKind::Microphone(MicrophoneSource {
            device: "default".into(),
            channels: Some(vec![]),
        });
        assert!(
            platform
                .probe(&mic, &canvas)
                .unwrap_err()
                .to_string()
                .contains("microphone.channels")
        );

        let source = SourceKind::SystemAudio(SystemAudioSource { exclude: vec![] });
        let actual = platform.probe(&source, &canvas);
        match crate::probe_loopback() {
            Ok((device, channels)) => {
                let probe = actual.unwrap().unwrap();
                assert_eq!(probe.device, device.name);
                assert!(probe.video.is_none());
                assert_eq!(probe.channels, Some(channels));
            }
            Err(error) => assert_eq!(actual.unwrap_err().to_string(), error.to_string()),
        }
    }
}
