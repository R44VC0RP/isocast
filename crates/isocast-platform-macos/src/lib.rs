//! macOS back end for Isocast.
//!
//! The master clock is the host clock, which CoreMedia, ScreenCaptureKit and
//! AVFoundation already stamp their samples with. Frames are IOSurface-backed
//! `CVPixelBuffer`s, so later stages can use them on the GPU without copying.
//! Files are written by AVAssetWriter, which encodes with VideoToolbox on the
//! media engines.
#![cfg(target_os = "macos")]

// Isocast is built for Apple silicon only: its media engines (hardware
// HEVC, ProRes) and unified memory are what the design relies on.
#[cfg(not(target_arch = "aarch64"))]
compile_error!("Isocast supports Apple silicon Macs only (aarch64-apple-darwin), not Intel Macs");

use std::ffi::{CStr, CString};
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::{OnceLock, mpsc};

mod audio;
mod browser;
mod camera;
mod capture;
mod crash_safe;
mod devices;
pub mod diagnostics;
mod frames;
mod gpu;
pub mod hotkeys;
mod jpeg;
mod keychain;
mod matroska;
mod media;
pub mod network;
mod output;
mod pacing;
mod permissions;
pub mod plugin;
pub mod pressure;
mod stream;
mod system;
mod tap;
mod termination;
pub mod virtual_camera;
mod vp8;
mod webm;
mod writer;

use isocast_engine::{
    AudioSink, CameraRequest, CaptureFormat, Platform, Probe, Time, VideoSink, Volume, WriterSpec,
};
use isocast_protocol::api::Machine;
use isocast_protocol::devices::{Devices, PermissionKind, PermissionsReport};
use isocast_protocol::{Canvas, SourceKind};
use objc2_core_media::{CMClock, CMTime};
use objc2_core_video::{
    CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
    kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
    kCVPixelFormatType_420YpCbCr10BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
};

pub use audio::{MicrophoneCapture, SystemAudioCapture};
pub use camera::CameraCapture;
pub use capture::DisplayCapture;
pub use frames::{FramePool, PixelBuffer};
pub use gpu::MetalImporter;
pub use keychain::app_identifier;
pub use stream::{AacEncoder, H264Encoder};
pub use writer::AssetWriter;

/// A recording file: AVAssetWriter's, or the crash-safe prototype's
/// (`ISOCAST_CRASH_SAFE=1`; see `crash_safe`).
// One writer per file, made once per take: its size doesn't matter, and
// the AVAssetWriter path must stay exactly as it is (no box to go through).
#[allow(clippy::large_enum_variant)]
pub enum MacWriter {
    Asset(AssetWriter),
    CrashSafe(crash_safe::CrashSafeWriter),
}

impl isocast_engine::FileWriter for MacWriter {
    type Frame = PixelBuffer;

    fn hardware_accelerated(&self) -> Option<bool> {
        match self {
            Self::Asset(w) => w.hardware_accelerated(),
            Self::CrashSafe(w) => w.hardware_accelerated(),
        }
    }

    fn intentional_video_drops(&self) -> u64 {
        match self {
            Self::Asset(w) => w.intentional_video_drops(),
            Self::CrashSafe(w) => w.intentional_video_drops(),
        }
    }

    fn prioritize_video(&mut self, canvas: isocast_protocol::CanvasId) {
        match self {
            Self::Asset(w) => w.prioritize_video(canvas),
            Self::CrashSafe(w) => w.prioritize_video(canvas),
        }
    }

    fn start(&mut self, at: Time) -> anyhow::Result<()> {
        match self {
            Self::Asset(w) => w.start(at),
            Self::CrashSafe(w) => w.start(at),
        }
    }

    fn append_video(&mut self, frame: &PixelBuffer, pts: Time) -> anyhow::Result<bool> {
        match self {
            Self::Asset(w) => w.append_video(frame, pts),
            Self::CrashSafe(w) => w.append_video(frame, pts),
        }
    }

    fn skip_video(&mut self) -> anyhow::Result<()> {
        match self {
            Self::Asset(w) => w.skip_video(),
            Self::CrashSafe(w) => w.skip_video(),
        }
    }

    fn append_audio(&mut self, track: usize, samples: &[f32], pts: Time) -> anyhow::Result<bool> {
        match self {
            Self::Asset(w) => w.append_audio(track, samples, pts),
            Self::CrashSafe(w) => w.append_audio(track, samples, pts),
        }
    }

    fn queue_depth(&self) -> usize {
        match self {
            Self::Asset(w) => w.queue_depth(),
            Self::CrashSafe(w) => w.queue_depth(),
        }
    }

    fn finish(self, end: Time) -> anyhow::Result<isocast_engine::Finished> {
        match self {
            Self::Asset(w) => w.finish(end),
            Self::CrashSafe(w) => w.finish(end),
        }
    }

    fn cancel(self) {
        match self {
            Self::Asset(w) => w.cancel(),
            Self::CrashSafe(w) => w.cancel(),
        }
    }

    fn crash_safe(&self) -> bool {
        match self {
            Self::Asset(w) => w.crash_safe(),
            Self::CrashSafe(w) => w.crash_safe(),
        }
    }

    fn completed_video_frames(&self) -> Option<u64> {
        match self {
            Self::CrashSafe(writer) => writer.completed_video_frames(),
            Self::Asset(writer) => writer.completed_video_frames(),
        }
    }

    fn commit(&mut self) -> anyhow::Result<Option<std::time::Duration>> {
        match self {
            Self::Asset(w) => w.commit(),
            Self::CrashSafe(w) => w.commit(),
        }
    }

    fn committed(&self) -> Option<std::time::Duration> {
        match self {
            Self::Asset(w) => w.committed(),
            Self::CrashSafe(w) => w.committed(),
        }
    }

    fn lost_frames(&mut self) -> u64 {
        match self {
            Self::Asset(w) => w.lost_frames(),
            Self::CrashSafe(w) => w.lost_frames(),
        }
    }
}

const NANOS_PER_SEC: i128 = 1_000_000_000;

#[derive(Clone, Copy)]
pub struct MacPlatform;

/// A running capture of any kind. Dropping it stops the capture.
pub enum Capture {
    Display(DisplayCapture),
    Camera(CameraCapture),
    Microphone(MicrophoneCapture),
    SystemAudio(SystemAudioCapture),
    Browser(browser::BrowserCapture),
}

impl Platform for MacPlatform {
    type Frame = PixelBuffer;
    type FramePool = FramePool;
    type Writer = MacWriter;
    type Capture = Capture;
    type Importer = MetalImporter;
    type Media = media::MediaReader;
    type Playback = output::Playback;
    type VideoEncoder = H264Encoder;
    type AudioEncoder = AacEncoder;
    type SharedSurface = isocast_surfaces::Surface;

    fn virtual_camera_sink(
        &self,
        width: u32,
        height: u32,
        fps: u32,
    ) -> anyhow::Result<Box<dyn isocast_engine::record::CameraSink<PixelBuffer>>> {
        virtual_camera::open(width, height, fps)
    }

    fn now(&self) -> Time {
        host_time()
    }

    fn app_identifier(&self) -> Option<String> {
        keychain::app_identifier()
    }

    fn sleep_until(&self, at: Time) {
        pacing::sleep_until(at);
    }

    fn devices(&self) -> anyhow::Result<Devices> {
        devices::devices()
    }

    fn volume(&self, path: &Path) -> anyhow::Result<Volume> {
        volume(path)
    }

    fn process_cpu_seconds(&self) -> Option<f64> {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
        // SAFETY: getrusage fills only the correctly sized output structure.
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
            return None;
        }
        let usage = unsafe { usage.assume_init() };
        let seconds = |t: libc::timeval| t.tv_sec as f64 + f64::from(t.tv_usec) / 1e6;
        Some(seconds(usage.ru_utime) + seconds(usage.ru_stime))
    }

    fn system_cpu_seconds(&self) -> Option<f64> {
        crate::pressure::system_cpu_seconds()
    }

    fn write_latency(&self, probe: &Path) -> Option<std::time::Duration> {
        write_latency(probe).ok()
    }

    fn source_permission(&self, source: &SourceKind) -> Option<String> {
        permissions::source_permission(source, || self.permissions().ok())
    }

    fn permissions(&self) -> anyhow::Result<PermissionsReport> {
        Ok(permissions::live_report(devices::permissions()?))
    }

    fn request_permissions(
        &self,
        only: Option<PermissionKind>,
    ) -> anyhow::Result<PermissionsReport> {
        let permissions = devices::request_permissions(std::time::Duration::from_secs(60), only)?;
        Ok(permissions::live_report(permissions))
    }

    fn open_permission_settings(&self, permission: PermissionKind) -> anyhow::Result<String> {
        permissions::open_settings(permission)
    }

    fn media_info(&self, path: &Path) -> anyhow::Result<isocast_engine::MediaInfo> {
        media::media_info(path)
    }

    fn open_media(
        &self,
        path: &Path,
        part: isocast_engine::MediaPart,
    ) -> anyhow::Result<media::MediaReader> {
        media::open_media(path, part)
    }

    fn frame_importer(&self, gpu: &isocast_engine::gpu::Gpu) -> anyhow::Result<MetalImporter> {
        MetalImporter::new(gpu)
    }

    fn machine(&self) -> Machine {
        system::machine()
    }

    fn probe(
        &self,
        _id: &str,
        source: &SourceKind,
        canvas: &Canvas,
    ) -> anyhow::Result<Option<Probe>> {
        system::probe(source, canvas)
    }

    fn frame_pool(&self, width: u32, height: u32) -> anyhow::Result<FramePool> {
        FramePool::new(width, height)
    }

    fn ycbcr_frame_pool(&self, width: u32, height: u32) -> anyhow::Result<FramePool> {
        FramePool::ycbcr(width, height)
    }

    fn retain_frame(frame: &PixelBuffer, usage: isocast_engine::FrameUse) -> Option<PixelBuffer> {
        frame.retain(usage)
    }

    fn frame_budget(&self) -> Option<serde_json::Value> {
        Some(frames::diagnostics())
    }

    fn real_time_thread(&self) {
        // SAFETY: sets the calling thread's own quality of service.
        unsafe {
            libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
        }
    }

    fn calibration_priority(&self) -> Option<&'static str> {
        // Calibration runs only in a disposable helper. Lower the whole
        // process too: a writer creates its own threads after this call.
        // SAFETY: changes only this process and the calling thread.
        let process = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 15) };
        let status =
            unsafe { libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0) };
        (process == 0 && status == 0).then_some("nice 15, utility quality of service")
    }

    fn governor_headroom(&self) -> Option<isocast_engine::governor::Headroom> {
        pressure::headroom()
    }

    fn thread_cpu_time(&self) -> Option<std::time::Duration> {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: writes the calling thread's CPU time into `time`.
        let status = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) };
        (status == 0).then(|| {
            std::time::Duration::new(time.tv_sec.max(0) as u64, time.tv_nsec.max(0) as u32)
        })
    }

    fn frame_size(&self, frame: &PixelBuffer) -> (u32, u32) {
        (
            CVPixelBufferGetWidth(&frame.0) as u32,
            CVPixelBufferGetHeight(&frame.0) as u32,
        )
    }

    fn frame_is_ycbcr(&self, frame: &PixelBuffer) -> bool {
        [
            kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
            kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
            kCVPixelFormatType_420YpCbCr10BiPlanarFullRange,
        ]
        .contains(&CVPixelBufferGetPixelFormatType(&frame.0))
    }

    fn shared_surface(
        &self,
        gpu: &isocast_engine::gpu::Gpu,
        width: u32,
        height: u32,
    ) -> anyhow::Result<(isocast_surfaces::Surface, wgpu::Texture)> {
        let surface = isocast_surfaces::Surface::rgba(width, height)?;
        let texture = surface.texture(&gpu.device, wgpu::TextureUsages::RENDER_ATTACHMENT)?;
        Ok((surface, texture))
    }

    fn encode_jpeg(
        &self,
        frame: &PixelBuffer,
        max_width: Option<u32>,
        quality: f32,
    ) -> anyhow::Result<(Vec<u8>, u32, u32)> {
        jpeg::encode(frame, max_width, quality)
    }

    fn create_writer(&self, spec: &WriterSpec) -> anyhow::Result<MacWriter> {
        if crash_safe::enabled() {
            return crash_safe::CrashSafeWriter::create(spec).map(MacWriter::CrashSafe);
        }
        AssetWriter::create(spec).map(MacWriter::Asset)
    }

    fn recover_file(
        &self,
        folder: &Path,
        name: &std::ffi::OsStr,
    ) -> anyhow::Result<Option<isocast_engine::RecoveredFile>> {
        crash_safe::recover_file(folder, name)
    }

    fn capture_screen(
        &self,
        target: isocast_engine::ScreenTarget<'_>,
        fps: u32,
        ten_bit: bool,
        cursor: bool,
        sink: VideoSink<PixelBuffer>,
    ) -> anyhow::Result<(Capture, CaptureFormat)> {
        let (capture, format) = capture::capture_screen(target, fps, ten_bit, cursor, sink)?;
        Ok((Capture::Display(capture), format))
    }

    fn capture_camera(
        &self,
        device: &str,
        request: CameraRequest,
        canvas: &Canvas,
        sink: VideoSink<PixelBuffer>,
    ) -> anyhow::Result<(Capture, CaptureFormat)> {
        let (capture, format) = camera::capture_camera(device, request, canvas, sink)?;
        Ok((Capture::Camera(capture), format))
    }

    fn capture_microphone(
        &self,
        device: &str,
        channels: Option<&[u32]>,
        sink: AudioSink,
    ) -> anyhow::Result<(Capture, u32)> {
        let (capture, channels) = audio::capture_microphone(device, channels, sink)?;
        Ok((Capture::Microphone(capture), channels))
    }

    fn capture_system_audio(
        &self,
        exclude: &[String],
        sink: AudioSink,
        exclude_own: bool,
    ) -> anyhow::Result<(Capture, u32)> {
        let (capture, channels) = audio::capture_system_audio(exclude, sink, exclude_own)?;
        Ok((Capture::SystemAudio(capture), channels))
    }

    fn play_audio(
        &self,
        device: &str,
        render: isocast_engine::AudioRender,
    ) -> anyhow::Result<(output::Playback, String)> {
        output::play_audio(device, render)
    }

    fn capture_browser(
        &self,
        page: &isocast_engine::BrowserPage<'_>,
        video: VideoSink<PixelBuffer>,
        audio: Option<AudioSink>,
    ) -> anyhow::Result<(
        Capture,
        CaptureFormat,
        std::sync::Arc<dyn isocast_engine::BrowserControl>,
    )> {
        let (capture, format, control) = browser::capture_browser(page, video, audio)?;
        Ok((Capture::Browser(capture), format, control))
    }

    fn capture_application_audio(
        &self,
        app: &str,
        sink: AudioSink,
    ) -> anyhow::Result<(Capture, u32)> {
        let (capture, channels) = audio::capture_application_audio(app, sink)?;
        Ok((Capture::SystemAudio(capture), channels))
    }

    fn create_video_encoder(
        &self,
        spec: &isocast_engine::LiveVideoSpec,
    ) -> anyhow::Result<H264Encoder> {
        H264Encoder::new(spec)
    }

    fn create_audio_encoder(
        &self,
        spec: &isocast_engine::LiveAudioSpec,
    ) -> anyhow::Result<AacEncoder> {
        AacEncoder::new(spec)
    }

    fn has_secret(&self, service: &str, account: &str) -> anyhow::Result<bool> {
        keychain::has_secret(service, account)
    }

    fn secret(&self, service: &str, account: &str) -> anyhow::Result<Option<String>> {
        keychain::secret(service, account)
    }

    fn set_secret(&self, service: &str, account: &str, secret: &str) -> anyhow::Result<()> {
        keychain::set_secret(service, account, secret)
    }

    fn delete_secret(&self, service: &str, account: &str) -> anyhow::Result<bool> {
        keychain::delete_secret(service, account)
    }
}

/// The volume holding `path`: space available to an unprivileged process,
/// its file system, and whether it is a network share.
pub fn volume(path: &Path) -> anyhow::Result<Volume> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    let mut stats = MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::statfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let stats = unsafe { stats.assume_init() };
    let name = unsafe { CStr::from_ptr(stats.f_fstypename.as_ptr()) };
    Ok(Volume {
        free_bytes: stats.f_bavail * stats.f_bsize as u64,
        file_system: name.to_string_lossy().into_owned(),
        network: stats.f_flags & libc::MNT_LOCAL as u32 == 0,
    })
}

/// Whether the file system holding `path` can give a file a second name
/// (`_PC_LINK_MAX` above 1). One that can't (exFAT and FAT say 1) may
/// report any link count, and there a count above one isn't a hard link.
/// An unanswered call counts as able, so a link check stays strict.
pub(crate) fn hard_links_possible(path: &Path) -> bool {
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return true;
    };
    // -1 is "no limit" (or an error): able, either way.
    !matches!(
        unsafe { libc::pathconf(path.as_ptr(), libc::_PC_LINK_MAX) },
        0 | 1
    )
}

/// [`hard_links_possible`] for the file system of an open file.
pub(crate) fn hard_links_possible_fd(file: &std::fs::File) -> bool {
    use std::os::fd::AsRawFd;
    !matches!(
        unsafe { libc::fpathconf(file.as_raw_fd(), libc::_PC_LINK_MAX) },
        0 | 1
    )
}

/// Write 4 KB at the start of `probe` and fsync it, timed. Plain fsync,
/// not F_FULLFSYNC (what Rust's `sync_data` does here), which would flush
/// the drive's whole cache and slow the recording it measures.
fn write_latency(probe: &Path) -> std::io::Result<std::time::Duration> {
    use std::os::unix::fs::FileExt;
    use std::os::unix::io::AsRawFd;
    let started = std::time::Instant::now();
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(probe)?;
    file.write_all_at(&[0; 4096], 0)?;
    if unsafe { libc::fsync(file.as_raw_fd()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(started.elapsed())
}

/// The main thread runs (or is about to run) its run loop (see
/// [`run_main_queue`]).
static MAIN_LOOP: OnceLock<()> = OnceLock::new();

/// Declare that this process's main thread is about to run
/// [`run_main_queue`], so main-thread work asked for before it starts waits
/// for it instead of being skipped.
/// `isocastd --preflight-permissions`: the permissions this new process
/// has, as an exit status with a bit for each one not granted (0: all), for
/// a running engine to learn that a grant made in System Settings waits for
/// its restart. Never prompts.
pub fn preflight_permissions() -> Option<u8> {
    (std::env::args().nth(1).as_deref() == Some(permissions::PREFLIGHT_ARG)).then(|| {
        devices::permissions()
            .map(|p| permissions::preflight_status(&p))
            // Unreadable: "unknown" to the parent (not a bit pattern).
            .unwrap_or(u8::MAX)
    })
}

pub fn will_run_main_queue() {
    let _ = MAIN_LOOP.set(());
}

/// Run this (the main) thread's run loop forever, so that work that must
/// run on the main thread can: AppKit's display names, and WebKit for
/// browser sources, which needs a live run loop. Jobs from other threads
/// arrive on the main dispatch queue, which the run loop drains.
/// (`dispatch_main` won't do: main-queue blocks then run on other threads,
/// and there is no run loop.)
pub fn run_main_queue(on_quit: impl Fn() + Send + Sync + 'static) -> ! {
    will_run_main_queue();
    let mtm = objc2::MainThreadMarker::new().expect("run_main_queue runs on the main thread");
    // A background app: no Dock icon or menu bar, but AppKit is set up.
    let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(objc2_app_kit::NSApplicationActivationPolicy::Prohibited);
    // NSApplication's delegate is weak; retain it for the entire event loop.
    let _termination = termination::install(&app, on_quit);
    // AppKit's event pump dispatches Carbon hotkey edges too. NSRunLoop::run
    // alone drains dispatch work but does not dispatch application events.
    app.run();
    unreachable!("the background application's event loop does not stop")
}

/// Run `job` on the main thread and wait for its result, if this process
/// serves the main thread (see [`run_main_queue`]) or this is the main
/// thread. `None` if nobody answers within `timeout`.
pub(crate) fn on_main_thread<T: Send + 'static>(
    timeout: std::time::Duration,
    job: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    if objc2::MainThreadMarker::new().is_some() {
        return Some(job());
    }
    MAIN_LOOP.get()?;
    let (tx, rx) = mpsc::channel();
    dispatch2::DispatchQueue::main().exec_async(move || {
        let _ = tx.send(job());
    });
    rx.recv_timeout(timeout).ok()
}

/// The current host time.
pub fn host_time() -> Time {
    from_cm_time(unsafe { CMClock::host_time_clock().time() })
}

/// Convert a host-clock `CMTime` (as stamped on captured samples) to `Time`.
pub fn from_cm_time(time: CMTime) -> Time {
    Time::from_nanos((time.value as i128 * NANOS_PER_SEC / time.timescale as i128) as i64)
}
