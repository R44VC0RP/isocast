//! A fail-closed placeholder back end for Isocast on systems without a
//! real one yet (Windows until `isocast-platform-windows`, W.4).
//!
//! It is **not** a working compositor or recorder. All it does is:
//!
//! - keep the host clock: one process-wide monotonic [`std::time::Instant`]
//!   that every clone of [`StubPlatform`] shares;
//! - hand out bounded pools of CPU memory 8-bit BGRA frames, so the
//!   engine's own synthetic generators (test patterns, tones, colors,
//!   images, slide shows, text) can run.
//! - on Windows, read this process's CPU time and compare volume identities
//!   for the daemon's process and disk-protection hooks.
//!
//! Everything else fails with an error saying so: screen, camera,
//! microphone, audio and browser capture, permissions, media files, the
//! GPU frame import (so nothing is composited), shared surfaces, JPEG
//! monitors, encoders and file writers (so nothing is recorded or
//! streamed), audio playback, disk space and the secret store. It never
//! reports a recording, permission, device or free space it doesn't have.
//! The resource types it can't make are [`Unsupported`], an enum without
//! values, so none can ever be handed out.

use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use isocast_engine::gpu::{FrameImporter, Gpu, GpuFrame};
use isocast_engine::{
    AudioEncoder, AudioRender, AudioSink, BrowserControl, BrowserPage, CameraRequest,
    CaptureFormat, EncodedVideo, FileWriter, Finished, LiveAudioSpec, LiveVideoSpec, MediaInfo,
    MediaPart, MediaReader, Platform, Probe, ScreenTarget, Time, VideoEncoder, VideoSink, Volume,
    WriterSpec,
};
use isocast_protocol::api::Machine;
use isocast_protocol::devices::{Devices, PermissionKind, PermissionsReport};
use isocast_protocol::{Canvas, SourceKind};

#[cfg(windows)]
mod windows;

/// The most frames one pool hands out before it waits for some to come
/// back.
pub const MAX_POOL_FRAMES: usize = 32;

/// The error every unsupported operation returns.
fn unsupported(what: &str) -> anyhow::Error {
    anyhow!(
        "{what}: not supported on {}: this build has only the stub platform \
         (CPU synthetic frames; no capture, compositing, encoding or recording)",
        std::env::consts::OS
    )
}

/// The placeholder platform: a cheap handle, like macOS's.
#[derive(Debug, Clone, Copy, Default)]
pub struct StubPlatform;

/// An 8-bit BGRA frame in CPU memory. Cloning shares the same pixels.
#[derive(Clone)]
pub struct Frame {
    width: u32,
    height: u32,
    bytes_per_row: usize,
    pixels: Arc<Vec<u8>>,
}

impl Frame {
    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Bytes from one row's start to the next.
    pub fn bytes_per_row(&self) -> usize {
        self.bytes_per_row
    }

    /// The BGRA pixels, row after row.
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }
}

/// A pool of up to [`MAX_POOL_FRAMES`] BGRA frames of one size, allocated
/// as needed. A buffer is drawn into again only once no frame still
/// shares it.
pub struct FramePool {
    width: u32,
    height: u32,
    bytes_per_row: usize,
    len: usize,
    buffers: Vec<Arc<Vec<u8>>>,
}

impl FramePool {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        if width == 0 || height == 0 {
            bail!("frame pool: size {width}x{height} must be at least 1x1");
        }
        let bytes_per_row = usize::try_from(width)
            .ok()
            .and_then(|width| width.checked_mul(4))
            .with_context(|| format!("frame pool: width {width} is too large"))?;
        let len = usize::try_from(height)
            .ok()
            .and_then(|height| bytes_per_row.checked_mul(height))
            .filter(|len| isize::try_from(*len).is_ok())
            .with_context(|| format!("frame pool: size {width}x{height} is too large"))?;
        Ok(Self {
            width,
            height,
            bytes_per_row,
            len,
            buffers: Vec::new(),
        })
    }

    /// A buffer no frame shares any more, or a new one while the pool has
    /// room; `None` if every buffer is still in use.
    fn free_buffer(&mut self) -> Result<Option<usize>> {
        if let Some(index) = self
            .buffers
            .iter_mut()
            .position(|buffer| Arc::get_mut(buffer).is_some())
        {
            return Ok(Some(index));
        }
        if self.buffers.len() >= MAX_POOL_FRAMES {
            return Ok(None);
        }
        let mut pixels = Vec::new();
        pixels.try_reserve_exact(self.len).with_context(|| {
            format!(
                "frame pool: couldn't allocate a {}x{} frame ({} bytes)",
                self.width, self.height, self.len
            )
        })?;
        pixels.resize(self.len, 0);
        self.buffers.push(Arc::new(pixels));
        Ok(Some(self.buffers.len() - 1))
    }

    fn frame(&self, index: usize) -> Frame {
        Frame {
            width: self.width,
            height: self.height,
            bytes_per_row: self.bytes_per_row,
            pixels: Arc::clone(&self.buffers[index]),
        }
    }
}

impl isocast_engine::FramePool for FramePool {
    type Frame = Frame;

    fn draw(&mut self, draw: &mut dyn FnMut(&mut [u8], usize)) -> Result<Option<Frame>> {
        let Some(index) = self.free_buffer()? else {
            return Ok(None);
        };
        let pixels = Arc::get_mut(&mut self.buffers[index])
            .context("frame pool: a free frame is still shared")?;
        draw(pixels, self.bytes_per_row);
        Ok(Some(self.frame(index)))
    }

    fn take(&mut self) -> Result<Option<Frame>> {
        Ok(self.free_buffer()?.map(|index| self.frame(index)))
    }
}

/// A resource the stub platform can't make (a capture, writer, encoder,
/// media reader, GPU importer or playback). It has no values, so no
/// operation can pretend to have one.
#[derive(Debug)]
pub enum Unsupported {}

impl FileWriter for Unsupported {
    type Frame = Frame;

    fn start(&mut self, _at: Time) -> Result<()> {
        match *self {}
    }

    fn append_video(&mut self, _frame: &Frame, _pts: Time) -> Result<bool> {
        match *self {}
    }

    fn append_audio(&mut self, _track: usize, _samples: &[f32], _pts: Time) -> Result<bool> {
        match *self {}
    }

    fn finish(self, _end: Time) -> Result<Finished> {
        match self {}
    }

    fn cancel(self) {
        match self {}
    }
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

impl VideoEncoder for Unsupported {
    type Frame = Frame;

    fn encode(&mut self, _frame: &Frame, _pts: Time, _keyframe: bool) -> Result<()> {
        match *self {}
    }

    fn receive(&mut self) -> Result<Option<EncodedVideo>> {
        match *self {}
    }

    fn receive_timeout(&mut self, _timeout: std::time::Duration) -> Result<Option<EncodedVideo>> {
        match *self {}
    }

    fn dropped(&self) -> u64 {
        match *self {}
    }

    fn set_rate(&mut self, _fps: u32, _bitrate_kbps: u32) -> Result<()> {
        match *self {}
    }
}

impl AudioEncoder for Unsupported {
    fn encode(&mut self, _samples: &[f32]) -> Result<Vec<Vec<u8>>> {
        match *self {}
    }

    fn delay(&self) -> u32 {
        match *self {}
    }
}

impl FrameImporter for Unsupported {
    type Frame = Frame;

    fn import(&mut self, _frame: &Frame) -> Result<GpuFrame> {
        match *self {}
    }

    fn target(&mut self, _frame: &Frame) -> Result<wgpu::Texture> {
        match *self {}
    }

    fn ycbcr_target(&mut self, _frame: &Frame) -> Result<(wgpu::Texture, wgpu::Texture)> {
        match *self {}
    }
}

/// The host clock's origin, shared by every clone in the process.
fn origin() -> Instant {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    *ORIGIN.get_or_init(Instant::now)
}

impl Platform for StubPlatform {
    type Frame = Frame;
    type FramePool = FramePool;
    type Writer = Unsupported;
    type Capture = Unsupported;
    type Importer = Unsupported;
    type Media = Unsupported;
    type Playback = Unsupported;
    #[cfg(not(test))]
    type VideoEncoder = Unsupported;
    /// Counted, so tests can see how many encoders the engine runs.
    #[cfg(test)]
    type VideoEncoder = tests::CountingEncoder;
    type AudioEncoder = Unsupported;
    type SharedSurface = isocast_surfaces::Surface;

    fn now(&self) -> Time {
        let nanos = origin().elapsed().as_nanos();
        Time::from_nanos(i64::try_from(nanos).unwrap_or(i64::MAX))
    }

    fn devices(&self) -> Result<Devices> {
        Err(unsupported("listing devices and their permissions"))
    }

    fn permissions(&self) -> Result<PermissionsReport> {
        Err(unsupported("permissions"))
    }

    fn request_permissions(&self, _only: Option<PermissionKind>) -> Result<PermissionsReport> {
        Err(unsupported("requesting permissions"))
    }

    fn open_permission_settings(&self, _permission: PermissionKind) -> Result<String> {
        Err(unsupported("opening permission settings"))
    }

    fn media_info(&self, path: &Path) -> Result<MediaInfo> {
        Err(unsupported(&format!(
            "reading media file {}",
            path.display()
        )))
    }

    fn open_media(&self, path: &Path, _part: MediaPart) -> Result<Unsupported> {
        Err(unsupported(&format!(
            "decoding media file {}",
            path.display()
        )))
    }

    fn frame_importer(&self, _gpu: &Gpu) -> Result<Unsupported> {
        Err(unsupported("showing frames to the GPU (compositing)"))
    }

    fn machine(&self) -> Machine {
        Machine {
            chip: format!("{} (unknown chip)", std::env::consts::ARCH),
            // Not measured by the stub platform: 0 means unknown.
            memory_gb: 0.0,
            cores: std::thread::available_parallelism()
                .map_or(0, |cores| u32::try_from(cores.get()).unwrap_or(u32::MAX)),
            os: format!("{} (stub platform)", std::env::consts::OS),
            hardware_encoders: Vec::new(),
        }
    }

    fn probe(&self, _id: &str, source: &SourceKind, _canvas: &Canvas) -> Result<Option<Probe>> {
        match source {
            // Drawn by the engine itself into this platform's frame pools.
            SourceKind::TestPattern(_)
            | SourceKind::Tone(_)
            | SourceKind::Color(_)
            | SourceKind::Image(_)
            | SourceKind::Slideshow(_)
            | SourceKind::Text(_) => Ok(None),
            other => Err(unsupported(&format!("{} sources", other.label()))),
        }
    }

    fn volume(&self, path: &Path) -> Result<Volume> {
        Err(unsupported(&format!(
            "checking free space for {}",
            path.display()
        )))
    }

    fn process_cpu_seconds(&self) -> Option<f64> {
        #[cfg(windows)]
        return windows::process_cpu_seconds();
        #[cfg(not(windows))]
        None
    }

    fn frame_pool(&self, width: u32, height: u32) -> Result<FramePool> {
        FramePool::new(width, height)
    }

    fn ycbcr_frame_pool(&self, _width: u32, _height: u32) -> Result<FramePool> {
        Err(unsupported("Y'CbCr frames"))
    }

    fn frame_size(&self, frame: &Frame) -> (u32, u32) {
        (frame.width, frame.height)
    }

    fn frame_is_ycbcr(&self, _frame: &Frame) -> bool {
        false
    }

    /// The test encoder takes the BGRA frames as they are, so monitors
    /// need no GPU conversion stage (which this platform can't run).
    #[cfg(test)]
    fn encoder_frame_format(&self) -> isocast_engine::EncoderFrameFormat {
        isocast_engine::EncoderFrameFormat::Bgra
    }

    fn shared_surface(
        &self,
        _gpu: &Gpu,
        _width: u32,
        _height: u32,
    ) -> Result<(isocast_surfaces::Surface, wgpu::Texture)> {
        Err(unsupported("shared GPU surfaces"))
    }

    fn encode_jpeg(
        &self,
        _frame: &Frame,
        _max_width: Option<u32>,
        _quality: f32,
    ) -> Result<(Vec<u8>, u32, u32)> {
        Err(unsupported("JPEG monitors"))
    }

    fn create_writer(&self, spec: &WriterSpec) -> Result<Unsupported> {
        Err(unsupported(&format!("recording {}", spec.path.display())))
    }

    fn recover_file(
        &self,
        _folder: &Path,
        _name: &std::ffi::OsStr,
    ) -> Result<Option<isocast_engine::RecoveredFile>> {
        Err(unsupported("recovering recordings"))
    }

    fn capture_screen(
        &self,
        _target: ScreenTarget<'_>,
        _fps: u32,
        _ten_bit: bool,
        _cursor: bool,
        _sink: VideoSink<Frame>,
    ) -> Result<(Unsupported, CaptureFormat)> {
        Err(unsupported("screen capture"))
    }

    fn capture_camera(
        &self,
        _device: &str,
        _request: CameraRequest,
        _canvas: &Canvas,
        _sink: VideoSink<Frame>,
    ) -> Result<(Unsupported, CaptureFormat)> {
        Err(unsupported("camera capture"))
    }

    fn capture_microphone(
        &self,
        _device: &str,
        _channels: Option<&[u32]>,
        _sink: AudioSink,
    ) -> Result<(Unsupported, u32)> {
        Err(unsupported("microphone capture"))
    }

    fn capture_system_audio(
        &self,
        _exclude: &[String],
        _sink: AudioSink,
        _exclude_own: bool,
    ) -> Result<(Unsupported, u32)> {
        Err(unsupported("system audio capture"))
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
        Err(unsupported("browser sources"))
    }

    fn capture_application_audio(
        &self,
        _app: &str,
        _sink: AudioSink,
    ) -> Result<(Unsupported, u32)> {
        Err(unsupported("application audio capture"))
    }

    fn create_video_encoder(&self, _spec: &LiveVideoSpec) -> Result<Self::VideoEncoder> {
        #[cfg(test)]
        return Ok(tests::CountingEncoder::new(_spec));
        #[cfg(not(test))]
        Err(unsupported("H.264 encoding"))
    }

    fn create_audio_encoder(&self, _spec: &LiveAudioSpec) -> Result<Unsupported> {
        Err(unsupported("AAC encoding"))
    }

    fn has_secret(&self, _service: &str, _account: &str) -> Result<bool> {
        Err(unsupported("the secret store"))
    }

    fn secret(&self, _service: &str, _account: &str) -> Result<Option<String>> {
        Err(unsupported("the secret store"))
    }

    fn set_secret(&self, _service: &str, _account: &str, _secret: &str) -> Result<()> {
        Err(unsupported("the secret store"))
    }

    fn delete_secret(&self, _service: &str, _account: &str) -> Result<bool> {
        Err(unsupported("the secret store"))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use isocast_engine::record::Engine;
    use isocast_protocol::Setup;
    use isocast_protocol::api::H264Profile;

    use super::*;

    /// Encoders made, and the profile each was asked for.
    static MADE: Mutex<Vec<H264Profile>> = Mutex::new(Vec::new());
    /// Encoders not dropped yet.
    static RUNNING: AtomicUsize = AtomicUsize::new(0);

    /// An encoder that takes frames and makes nothing, counted while it
    /// lives.
    pub struct CountingEncoder;

    impl CountingEncoder {
        pub fn new(spec: &LiveVideoSpec) -> Self {
            RUNNING.fetch_add(1, Ordering::SeqCst);
            // Publish construction only after the live count is observable.
            MADE.lock().unwrap().push(spec.profile);
            Self
        }
    }

    impl Drop for CountingEncoder {
        fn drop(&mut self) {
            RUNNING.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl VideoEncoder for CountingEncoder {
        type Frame = Frame;

        fn encode(&mut self, _frame: &Frame, _pts: Time, _keyframe: bool) -> Result<()> {
            Ok(())
        }

        fn receive(&mut self) -> Result<Option<EncodedVideo>> {
            Ok(None)
        }

        fn receive_timeout(&mut self, _timeout: Duration) -> Result<Option<EncodedVideo>> {
            Ok(None)
        }

        fn dropped(&self) -> u64 {
            0
        }

        fn set_rate(&mut self, _fps: u32, _bitrate_kbps: u32) -> Result<()> {
            Ok(())
        }
    }

    fn made() -> Vec<H264Profile> {
        MADE.lock().unwrap().clone()
    }

    fn running() -> usize {
        RUNNING.load(Ordering::SeqCst)
    }

    /// Wait up to five seconds for `done`.
    fn eventually(what: &str, done: impl Fn() -> bool) {
        let until = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(
                Instant::now() < until,
                "{what}: made {:?}, {} running",
                made(),
                running()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn monitor_viewers_share_one_encoder_per_profile_until_the_last_leaves() {
        let setup = Setup::quick(&["test-pattern:320x180@30".to_string()]).unwrap();
        let engine = Engine::start(&StubPlatform, &setup, None, None).unwrap();
        // The source opens in the background after the engine starts.
        let watch = |profile| {
            let until = Instant::now() + Duration::from_secs(5);
            loop {
                match engine.frames("test-pattern", None, 30, profile) {
                    Ok(feed) => break feed,
                    Err(err) if Instant::now() >= until => panic!("watching: {err}"),
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        };
        // Long enough for a stream thread to make an encoder it shouldn't.
        let settle = || std::thread::sleep(Duration::from_millis(300));

        let high = watch(H264Profile::High);
        eventually("the first High viewer starts an encoder", || {
            made().len() == 1
        });
        let high_again = watch(H264Profile::High);
        settle();
        assert_eq!(
            made(),
            [H264Profile::High],
            "same profile shares one encoder"
        );
        assert_eq!(running(), 1);

        let baseline = watch(H264Profile::ConstrainedBaseline);
        eventually("a Constrained Baseline viewer starts its own", || {
            made().len() == 2
        });
        assert_eq!(
            made(),
            [H264Profile::High, H264Profile::ConstrainedBaseline]
        );
        assert_eq!(running(), 2);

        drop(high);
        settle();
        assert_eq!(running(), 2, "a High viewer still watches");
        drop(high_again);
        eventually("the last High viewer's encoder stops", || running() == 1);
        drop(baseline);
        eventually("the last viewer's encoder stops", || running() == 0);
        assert_eq!(made().len(), 2);
        engine.stop();
    }
}
