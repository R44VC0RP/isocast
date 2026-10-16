//! The Isocast engine: recording sessions on one shared clock.
//!
//! The engine is portable. Everything that touches the operating system
//! (the host clock, frame buffers, encoders, file writing) comes from a
//! [`Platform`] implementation such as `isocast-platform-macos`.

mod animate;
mod audio_filter;
pub mod capacity;
pub(crate) mod compiled;
mod content;
pub mod critical;
pub mod data;
pub mod doctor;
pub(crate) mod draw_list;
pub mod estimate;
pub mod filter;
pub mod gpu;
pub mod hotkeys;
pub mod layout;
mod pacing;
mod picture;
pub mod project;
pub mod record;
pub mod render;
pub mod replay;
pub mod scene;
pub mod snapshot;
pub(crate) mod source;
pub mod space;
pub mod stream;
pub mod synthetic;
mod text;
mod time;
pub mod trace;
pub mod transition;
pub mod tune;
pub mod wasm;
mod ycbcr;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use isocast_protocol::api::Machine;
use isocast_protocol::devices::{Devices, PermissionKind, PermissionsReport};
use isocast_protocol::{Canvas, Container, Encoder, SourceKind};

pub use time::Time;

/// Audio inside the engine is 48 kHz, 32-bit float, interleaved.
pub const SAMPLE_RATE: u32 = 48_000;
/// Recordings are written in movie fragments of this many seconds of media
/// time. Passing a boundary with input samples allows the writer to close a
/// fragment; accepting those samples does not acknowledge an on-disk commit.
/// Queued inputs and track timing mean this is not a maximum crash-loss bound.
pub const MOVIE_FRAGMENT_SECONDS: i64 = 2;

/// What the engine needs from the operating system.
/// Platforms are cheap handles (macOS: a unit struct), cloned into the
/// threads that generate synthetic sources.
pub trait Platform: Clone + Send + Sync + 'static {
    /// A video frame (on macOS, a `CVPixelBuffer`). Cloning is cheap: it
    /// shares the same pixels.
    type Frame: Clone + Send + 'static;
    type FramePool: FramePool<Frame = Self::Frame>;
    type Writer: FileWriter<Frame = Self::Frame>;
    /// A running capture. Dropping it stops the capture.
    type Capture: Send;
    type Importer: gpu::FrameImporter<Frame = Self::Frame>;
    type Media: MediaReader<Frame = Self::Frame>;
    /// Audio playing on an output device. Dropping it stops playback.
    type Playback: Send + 'static;
    /// A real-time H.264 encoder, for streaming and monitors.
    type VideoEncoder: VideoEncoder<Frame = Self::Frame>;
    /// An AAC-LC encoder, for streaming.
    type AudioEncoder: AudioEncoder;
    /// A picture in GPU memory other processes can show without copying
    /// (on macOS an IOSurface): monitors in native clients' windows.
    type SharedSurface: Clone + Send + Sync + 'static;

    /// Open the installed virtual camera's bounded, IOSurface-backed sink.
    fn virtual_camera_sink(
        &self,
        _width: u32,
        _height: u32,
        _fps: u32,
    ) -> anyhow::Result<Box<dyn record::CameraSink<Self::Frame>>> {
        anyhow::bail!("virtual_camera: no camera extension backend on this platform")
    }

    /// The current time on the host clock, the session's master clock.
    fn now(&self) -> Time;

    /// Critical media deadline used only after a clock detects stretched
    /// ordinary sleeps. Back ends should bound background timer coalescing.
    fn sleep_until(&self, at: Time) {
        let now = self.now();
        if at > now {
            std::thread::sleep(at.saturating_since(now));
        }
    }

    /// Everything that can be captured, and the permissions that gate it.
    /// Never triggers a permission prompt.
    fn devices(&self) -> anyhow::Result<Devices>;

    /// The screen recording, camera and microphone permissions, with the
    /// next step for each that isn't granted. Never prompts.
    fn permissions(&self) -> anyhow::Result<PermissionsReport>;

    /// Permission a pending source open may be waiting for. Must not prompt
    /// or perform protected I/O; a successful open clears this provisional state.
    fn source_permission(&self, _source: &SourceKind) -> Option<String> {
        None
    }

    /// Ask the OS for screen recording, camera and microphone access where
    /// it hasn't been decided (only `only`, if given), showing its prompts,
    /// and wait a while for the answers. Returns the permissions afterwards.
    fn request_permissions(
        &self,
        only: Option<PermissionKind>,
    ) -> anyhow::Result<PermissionsReport>;

    /// Open the settings where `permission` is turned on by hand. Returns
    /// what was opened.
    fn open_permission_settings(&self, permission: PermissionKind) -> anyhow::Result<String>;

    /// What a video or audio file holds.
    fn media_info(&self, path: &Path) -> anyhow::Result<MediaInfo>;

    /// Decode `part` of a video or audio file from its start.
    fn open_media(&self, path: &Path, part: MediaPart) -> anyhow::Result<Self::Media>;

    /// Something that shows this platform's frames to `gpu` without
    /// copying them.
    fn frame_importer(&self, gpu: &gpu::Gpu) -> anyhow::Result<Self::Importer>;

    /// The chip, memory, OS and hardware encoders.
    fn machine(&self) -> Machine;

    /// Find the device a source would record, and the format it would
    /// record in, without opening it. `None` for synthetic sources; an
    /// error naming the valid devices if none matches.
    fn probe(&self, source: &SourceKind, canvas: &Canvas) -> anyhow::Result<Option<Probe>>;

    /// Free space and kind of the volume holding `path`.
    fn volume(&self, path: &Path) -> anyhow::Result<Volume>;

    /// How long a small write to `probe` (a scratch file beside a
    /// recording) takes to reach the disk: written and synced, without
    /// flushing the drive's own cache. `None` where it can't be measured.
    fn write_latency(&self, probe: &Path) -> Option<std::time::Duration> {
        let _ = probe;
        None
    }

    /// A pool of 8-bit BGRA frames the engine can draw into.
    fn frame_pool(&self, width: u32, height: u32) -> anyhow::Result<Self::FramePool>;

    /// A pool of 8-bit 4:2:0 Y'CbCr frames (video range, Rec. 709) the GPU
    /// can convert pictures into ([`gpu::FrameImporter::ycbcr_target`]):
    /// what encoders take as is, at under half the size of BGRA.
    fn ycbcr_frame_pool(&self, width: u32, height: u32) -> anyhow::Result<Self::FramePool>;

    /// Ask the OS to run the calling thread ahead of background work: it
    /// paces frames or audio in real time (the program, a file's frame
    /// clock, an audio reader, the mix, a file writer).
    fn real_time_thread(&self) {}

    /// The CPU time the calling thread has used so far, if the OS tells:
    /// what the compositor costs per frame is measured with it.
    fn thread_cpu_time(&self) -> Option<std::time::Duration> {
        None
    }

    /// A frame's width and height in pixels.
    fn frame_size(&self, frame: &Self::Frame) -> (u32, u32);

    /// Whether a frame is 4:2:0 Y'CbCr, which H.264 and HEVC encoders take
    /// as is; they convert anything else themselves.
    fn frame_is_ycbcr(&self, frame: &Self::Frame) -> bool;

    /// A `width`×`height` 8-bit RGBA surface other processes can show
    /// without copying, and an `Rgba8Unorm` texture through which `gpu`
    /// draws into it (a render attachment).
    fn shared_surface(
        &self,
        gpu: &gpu::Gpu,
        width: u32,
        height: u32,
    ) -> anyhow::Result<(Self::SharedSurface, wgpu::Texture)>;

    /// `frame` as a JPEG of `quality` (0 to 1), scaled down to at most
    /// `max_width` wide, for clients' live monitors: the bytes, width and
    /// height.
    fn encode_jpeg(
        &self,
        frame: &Self::Frame,
        max_width: Option<u32>,
        quality: f32,
    ) -> anyhow::Result<(Vec<u8>, u32, u32)>;

    /// Create a media file and get it ready to receive frames and audio.
    fn create_writer(&self, spec: &WriterSpec) -> anyhow::Result<Self::Writer>;

    /// Finish the file `name` in `folder` that an interrupted recording
    /// left behind, if it is one a crash-safe writer can complete from its
    /// fragments. `None`: it isn't one, or it is already a finished movie.
    /// The caller holds the folder's recording lock; the platform must
    /// touch only a regular file inside that folder (no links).
    fn recover_file(
        &self,
        _folder: &Path,
        _name: &std::ffi::OsStr,
    ) -> anyhow::Result<Option<RecoveredFile>> {
        Ok(None)
    }

    /// Start capturing `target` (a display, a window or an app's windows)
    /// at up to `fps`, at its native resolution, delivering frames to
    /// `sink`. `ten_bit` asks for 10-bit 4:2:0 frames instead of 8-bit;
    /// `cursor` shows the mouse pointer.
    fn capture_screen(
        &self,
        target: ScreenTarget<'_>,
        fps: u32,
        ten_bit: bool,
        cursor: bool,
        sink: VideoSink<Self::Frame>,
    ) -> anyhow::Result<(Self::Capture, CaptureFormat)>;

    /// Start capturing a camera (an id, name or part of a name from
    /// [`Platform::devices`]) in the format `request` asks for, or its best
    /// format for `canvas`.
    fn capture_camera(
        &self,
        device: &str,
        request: CameraRequest,
        canvas: &Canvas,
        sink: VideoSink<Self::Frame>,
    ) -> anyhow::Result<(Self::Capture, CaptureFormat)>;

    /// Start capturing a microphone ("default", or an id, name or part of
    /// a name from [`Platform::devices`]). Returns its channel count.
    fn capture_microphone(
        &self,
        device: &str,
        sink: AudioSink,
    ) -> anyhow::Result<(Self::Capture, u32)>;

    /// Start capturing what the computer plays, leaving out the apps
    /// `exclude` names. Returns its channel count. `exclude_own` leaves out
    /// what this process plays (monitoring), so it isn't recorded twice.
    fn capture_system_audio(
        &self,
        exclude: &[String],
        sink: AudioSink,
        exclude_own: bool,
    ) -> anyhow::Result<(Self::Capture, u32)>;

    /// Start playing audio on an output device: "default" (the system
    /// output, following it when it changes) or an id, name or part of a
    /// name from [`Platform::devices`]' audio outputs. The device calls
    /// `render`, on its own thread, for each buffer of interleaved stereo
    /// samples at [`SAMPLE_RATE`] it needs, which must be filled. Returns
    /// the playback and the device's name.
    fn play_audio(
        &self,
        device: &str,
        render: AudioRender,
    ) -> anyhow::Result<(Self::Playback, String)>;

    /// Start rendering a web page at its size and frame rate, transparent
    /// where the page is, delivering frames to `video`. With `audio`, the
    /// page's sound goes there (stereo) instead of playing aloud. Returns a
    /// handle to reload or unload the page while it runs.
    fn capture_browser(
        &self,
        page: &BrowserPage<'_>,
        video: VideoSink<Self::Frame>,
        audio: Option<AudioSink>,
    ) -> anyhow::Result<(Self::Capture, CaptureFormat, Arc<dyn BrowserControl>)>;

    /// Start capturing the sound of one app (a name, bundle id or part of
    /// a name). Returns its channel count.
    fn capture_application_audio(
        &self,
        app: &str,
        sink: AudioSink,
    ) -> anyhow::Result<(Self::Capture, u32)>;

    /// Start a real-time H.264 encoder (on the media engines where the
    /// machine has them) for this platform's frames.
    fn create_video_encoder(&self, spec: &LiveVideoSpec) -> anyhow::Result<Self::VideoEncoder>;

    /// Start an AAC-LC encoder taking interleaved samples at
    /// [`SAMPLE_RATE`].
    fn create_audio_encoder(&self, spec: &LiveAudioSpec) -> anyhow::Result<Self::AudioEncoder>;

    /// Whether the OS's secret store (the Keychain) holds a secret for
    /// `service` and `account`. Never shows a prompt.
    fn has_secret(&self, service: &str, account: &str) -> anyhow::Result<bool>;

    /// The secret kept for `service` and `account`, if any.
    fn secret(&self, service: &str, account: &str) -> anyhow::Result<Option<String>>;

    /// Keep `secret` for `service` and `account`, replacing any before.
    fn set_secret(&self, service: &str, account: &str, secret: &str) -> anyhow::Result<()>;

    /// Remove the secret for `service` and `account`. Returns whether there
    /// was one.
    fn delete_secret(&self, service: &str, account: &str) -> anyhow::Result<bool>;

    /// Join files end to end into one (see [`JoinSpec`]), calling
    /// `progress` as it goes. On failure nothing is left at the output.
    fn join_files(&self, spec: &JoinSpec, progress: OnJoinProgress) -> anyhow::Result<Joined> {
        let _ = (spec, progress);
        anyhow::bail!("joining files isn't supported on this platform yet")
    }
}

/// What a live H.264 encoder makes: constant-ish bitrate, High profile,
/// no B-frames, Rec. 709 tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveVideoSpec {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Hold the bitrate every second (padding still pictures), rather than
    /// on average.
    pub constant_bitrate: bool,
    /// The longest gap the encoder leaves between keyframes, in frames. A
    /// stream asks for each keyframe itself, on time, so for it this is
    /// only a backstop.
    pub keyframe_interval: u32,
    /// For monitors rather than a stream: each frame comes out as soon as
    /// it is encoded (no frames held back), and the bitrate is an average
    /// with no per-second cap.
    pub low_latency: bool,
}

/// What a live AAC encoder makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveAudioSpec {
    pub channels: u32,
    pub bitrate_kbps: u32,
}

/// One compressed H.264 frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedVideo {
    /// Its NAL units, each after its length as 4 big-endian bytes (the
    /// AVC file format's layout, which FLV uses too).
    pub data: Vec<u8>,
    pub pts: Time,
    pub dts: Time,
    pub keyframe: bool,
    /// The sequence and picture parameter sets decoding needs, on
    /// keyframes.
    pub parameter_sets: Option<ParameterSets>,
}

/// H.264 sequence (SPS) and picture (PPS) parameter sets, as NAL units.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParameterSets {
    pub sps: Vec<Vec<u8>>,
    pub pps: Vec<Vec<u8>>,
}

/// A real-time video encoder. Frames go in with [`VideoEncoder::encode`]
/// and come out, in decode order, from [`VideoEncoder::receive`].
pub trait VideoEncoder: Send {
    type Frame;

    /// Submit a frame shown at `pts`; `keyframe` makes it a keyframe.
    fn encode(&mut self, frame: &Self::Frame, pts: Time, keyframe: bool) -> anyhow::Result<()>;

    /// The next encoded frame, if one is ready.
    fn receive(&mut self) -> anyhow::Result<Option<EncodedVideo>>;

    /// The next encoded frame, waiting up to `timeout` for one.
    fn receive_timeout(
        &mut self,
        timeout: std::time::Duration,
    ) -> anyhow::Result<Option<EncodedVideo>>;

    /// Frames the encoder dropped so far instead of encoding them.
    fn dropped(&self) -> u64;

    /// From the next frame, expect `fps` frames a second and aim for
    /// `bitrate_kbps` on average (a monitor changing its rate), keeping the
    /// session: no new keyframe.
    fn set_rate(&mut self, fps: u32, bitrate_kbps: u32) -> anyhow::Result<()>;
}

/// An AAC encoder: raw AAC frames of 1024 samples per channel.
pub trait AudioEncoder: Send {
    /// Add interleaved samples; returns the frames completed so far.
    fn encode(&mut self, samples: &[f32]) -> anyhow::Result<Vec<Vec<u8>>>;

    /// How many samples (per channel) of encoder delay come before the
    /// first sample given: frame `n` holds the audio from sample
    /// `n × 1024 − delay`.
    fn delay(&self) -> u32;
}

/// A web page to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserPage<'a> {
    pub location: PageLocation<'a>,
    /// Size in CSS pixels, which is also the frame size.
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// CSS added to the page.
    pub css: &'a str,
}

/// Control of a running browser source's page.
pub trait BrowserControl: Send + Sync {
    /// Load the page again, bypassing the cache.
    fn refresh(&self);
    /// Unload the page, leaving an empty frame, or load it again.
    fn set_loaded(&self, loaded: bool);
}

/// Where a web page comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageLocation<'a> {
    /// An http(s) or file URL.
    Url(&'a str),
    /// A local HTML file, which may load files next to it.
    File(&'a std::path::Path),
}

/// What a screen capture shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenTarget<'a> {
    /// A display: "main", or a name or id.
    Display(&'a str),
    /// One window of an app, optionally by part of its title.
    Window {
        app: &'a str,
        title: Option<&'a str>,
    },
    /// Every window of an app on a display.
    Application { app: &'a str, display: &'a str },
}

/// Which part of a media file a reader decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaPart {
    Video,
    Audio,
}

/// Files to join end to end into one (a timeline's export): the video
/// copied as it is, except the frames from a cut that isn't on a keyframe
/// to the next keyframe, and parts whose video is in another format than
/// the joined file's, which are decoded and encoded again; each audio
/// track decoded and encoded again as one continuous track, cross-faded
/// at each join, so it lines up with the video at every join.
#[derive(Debug, Clone)]
pub struct JoinSpec {
    /// The files, in order. Each has one video track and the same number
    /// of audio tracks as the first, with the same channels.
    pub parts: Vec<JoinPart>,
    /// Where the joined file goes; it must not exist.
    pub output: PathBuf,
    pub container: Container,
    /// The joined video takes this file's codec, size and frame rate (the
    /// program's, for an edited file); the first part's if absent.
    pub format: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct JoinPart {
    pub path: PathBuf,
    /// How long the part lasts in the joined file, as `frames` at `fps`
    /// (its clip's length, so files at different rates join at the same
    /// moments): a shorter file's last frame is held and its audio padded
    /// with silence, a longer one's last frame is cut short, and frames
    /// starting past the end are left out. The file's own video length if
    /// absent.
    pub length: Option<(u64, u32)>,
    /// Where the file starts within its clip, in seconds (a file that
    /// started after the others): held before it, as at the end.
    pub offset_seconds: f64,
    /// Where the part starts on its take's clock (its clip's in point), in
    /// seconds: what the file holds from there on fills the part. The
    /// picture at the start is the frame on screen at that moment.
    pub in_seconds: f64,
    /// Take the part's audio from this file of the same take instead (and
    /// where it starts within the clip): an edited file shows an ISO
    /// file's picture with the program's sound.
    pub audio: Option<(PathBuf, f64)>,
    /// The file's frame rate as recorded (its manifest's), which a file
    /// with dropped frames can't tell; 0 if unknown (then the file's own
    /// average, rounded).
    pub fps: u32,
}

/// What a join wrote.
#[derive(Debug, Clone, PartialEq)]
pub struct Joined {
    pub duration_seconds: f64,
    pub frames: u64,
    /// Each part's place in the joined file, in order.
    pub parts: Vec<JoinedPart>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JoinedPart {
    pub start_seconds: f64,
    /// Its audio is cut or padded with silence to this.
    pub duration_seconds: f64,
    pub frames: u64,
    /// Frames starting past the part's length, left out.
    pub frames_left_out: u64,
    /// Frames decoded and encoded again; the others were copied.
    pub reencoded: u64,
    /// Its file is in another format than the joined file's (codec, size
    /// or frame rate), so it was encoded again whole.
    pub converted: bool,
}

/// Called with how far a join has got, 0 to 1, a few times a second;
/// returning false stops it (it then fails, leaving nothing at the output).
pub type OnJoinProgress<'a> = &'a mut dyn FnMut(f64) -> bool;

/// What a media file holds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MediaInfo {
    /// Width and height of its picture, if it has one.
    pub video: Option<(u32, u32)>,
    pub audio: bool,
    pub duration: std::time::Duration,
}

/// The size of a source's picture when it comes from a file or its text
/// (images, slide shows, text, media files), as the engine would draw it;
/// `None` for other kinds, or when the file can't be read. Imports use it
/// to lay out items sized relative to their source.
pub fn content_size<P: Platform>(platform: &P, kind: &SourceKind) -> Option<(u32, u32)> {
    use record::expand_home;
    let first_frame = |path: &str, size: Option<(u32, u32)>| {
        let (picture, _) = picture::load(&expand_home(path), size)
            .ok()?
            .into_iter()
            .next()?;
        Some((picture.width, picture.height))
    };
    match kind {
        SourceKind::Image(image) => first_frame(&image.path, image.width.zip(image.height)),
        SourceKind::Slideshow(slides) => slides
            .width
            .zip(slides.height)
            .or_else(|| first_frame(slides.files.first()?, None)),
        SourceKind::Text(settings) => {
            let text = match &settings.file {
                Some(file) => std::fs::read_to_string(expand_home(file)).ok()?,
                None => settings.text.clone().unwrap_or_default(),
            };
            let picture = text::render(settings, text.trim_end());
            Some((picture.width, picture.height))
        }
        SourceKind::Media(media) => {
            platform
                .media_info(&expand_home(media.files.first()?))
                .ok()?
                .video
        }
        _ => None,
    }
}

/// Decodes one part of a media file, from its start, as fast as asked.
pub trait MediaReader: Send {
    type Frame;

    /// The next frame and its time from the start of the file; `None` at
    /// the end.
    fn next_video(&mut self) -> anyhow::Result<Option<(Self::Frame, std::time::Duration)>>;

    /// The next audio, interleaved stereo at [`SAMPLE_RATE`], and the time
    /// of its first sample from the start of the file; `None` at the end.
    fn next_audio(&mut self) -> anyhow::Result<Option<(Vec<f32>, std::time::Duration)>>;
}

/// Receives interleaved samples and the capture time of the first.
pub type SamplesCallback = Box<dyn Fn(&[f32], Time) + Send + Sync>;

/// Fills a buffer of interleaved stereo samples for an output device.
pub type AudioRender = Box<dyn FnMut(&mut [f32]) + Send>;

/// Where a running audio capture delivers samples. Called on the capture's
/// own threads.
pub struct AudioSink {
    /// Interleaved samples at [`SAMPLE_RATE`], and when the first of them
    /// was captured on the host clock.
    pub samples: SamplesCallback,
    /// The capture stopped on its own, with the reason.
    pub stopped: Box<dyn Fn(String) + Send + Sync>,
}

/// The device a source matched, and what it would deliver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub device: String,
    /// Frame size and rate, for video sources.
    pub video: Option<CaptureFormat>,
    /// Channel count, for audio sources.
    pub channels: Option<u32>,
}

/// The format a setup asks of a camera. `None` leaves the choice to the
/// engine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CameraRequest {
    pub size: Option<(u32, u32)>,
    pub fps: Option<u32>,
}

/// Where a running capture delivers frames. Called on the capture's own
/// threads.
pub struct VideoSink<F> {
    /// A new frame, and when it was captured on the host clock.
    pub frame: Box<dyn Fn(F, Time) + Send + Sync>,
    /// The capture stopped on its own, with the reason.
    pub stopped: Box<dyn Fn(String) + Send + Sync>,
}

/// What a capture delivers: frame size in pixels, and the frame rate the
/// files recording it run at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureFormat {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

pub trait FramePool: Send {
    type Frame;

    /// Take a frame from the pool and let `draw` fill its BGRA pixels, given
    /// as the pixel bytes and the number of bytes per row. Returns `None` if
    /// every frame in the pool is still in use downstream.
    fn draw(
        &mut self,
        draw: &mut dyn FnMut(&mut [u8], usize),
    ) -> anyhow::Result<Option<Self::Frame>>;

    /// Take a frame for the GPU to draw into, with undefined pixels.
    /// Returns `None` if every frame is still in use downstream.
    fn take(&mut self) -> anyhow::Result<Option<Self::Frame>>;
}

/// One media file being written: a video track and zero or more audio
/// tracks, all timed on the master clock.
pub trait FileWriter: Send {
    type Frame;

    /// Protect this program output ahead of optional ISO encodes when the
    /// platform's shared encoder hardware is overloaded.
    fn prioritize_video(&mut self) {}

    /// Start the file's timeline at `at`. Called once, before anything is
    /// appended.
    fn start(&mut self, at: Time) -> anyhow::Result<()>;

    /// Append a video frame, waiting briefly for a busy encoder. Returns
    /// `false` if the encoder could not keep up and the frame was dropped.
    fn append_video(&mut self, frame: &Self::Frame, pts: Time) -> anyhow::Result<bool>;

    /// Video frames waiting inside the writer for its encoder, for
    /// developer mode's traces.
    fn queue_depth(&self) -> usize {
        0
    }

    /// Append interleaved samples to audio track `track`. Returns `false` if
    /// they were dropped.
    fn append_audio(&mut self, track: usize, samples: &[f32], pts: Time) -> anyhow::Result<bool>;

    /// End the timeline at `end` and finish the file: how many pictures
    /// were repeated to cover missing tail frames (stored frames that must
    /// not reduce the count of dropped original pictures), and what the
    /// encoders discarded on their own while finishing. A writer that
    /// repaired its file after failing attaches a [`RepairedFile`] to its
    /// error.
    fn finish(self, end: Time) -> anyhow::Result<Finished>;

    /// Abandon the file and delete what was written. Used when it never
    /// received a frame.
    fn cancel(self);

    /// Whether this writer can commit what it holds to the disk on demand
    /// and say how much of the file is recoverable after a crash.
    fn crash_safe(&self) -> bool {
        false
    }

    /// Write everything received so far to the disk and return how much of
    /// the file's timeline (from its start) a crash can no longer lose.
    /// `None`: the writer can't say (see [`FileWriter::crash_safe`]).
    fn commit(&mut self) -> anyhow::Result<Option<std::time::Duration>> {
        Ok(None)
    }

    /// How much of the file's timeline the fragments written so far make
    /// recoverable, without committing anything.
    fn committed(&self) -> Option<std::time::Duration> {
        None
    }

    /// Frames the encoder accepted from `append_video` but discarded on
    /// its own since the last call (a hardware encoder's dropped-frame
    /// callback), so the file's frame count stays what is in the file.
    fn lost_frames(&mut self) -> u64 {
        0
    }
}

/// What finishing a file established (see [`FileWriter::finish`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Finished {
    /// Pictures repeated at the end to cover missing tail frames.
    pub repeated: u64,
    /// Pictures the encoder accepted and then discarded, discovered while
    /// finishing (after the last `lost_frames` poll).
    pub lost_frames: u64,
    /// Audio tracks whose encoder delivered fewer packets than its input
    /// covered (each counts as one dropout).
    pub lost_audio_tracks: u64,
}

/// What a file holds after its writer failed and still finished it from
/// what had reached the disk; attached as context to the writer's error.
#[derive(Debug, Clone, PartialEq)]
pub struct RepairedFile {
    /// Fragments the movie was rebuilt from.
    pub fragments: u32,
    /// Pictures in the movie, if it was rebuilt from fragments (a crash
    /// between the final index and its header patch keeps every picture
    /// the writer counted).
    pub frames: Option<u64>,
    /// The shortest kept track's duration.
    pub seconds: Option<f64>,
    /// What the movie holds, track by track ("video 16 pictures (0.533 s);
    /// audio 0.319 s; left out: …"), empty when only the header patch was
    /// missing.
    pub summary: String,
}

impl std::fmt::Display for RepairedFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the file was still finished from its {} fragments",
            self.fragments
        )?;
        if !self.summary.is_empty() {
            write!(f, " ({})", self.summary)?;
        }
        Ok(())
    }
}

/// An interrupted file a platform finished from what reached the disk
/// (see [`Platform::recover_file`]).
#[derive(Debug, Clone, PartialEq)]
pub struct RecoveredFile {
    /// Fragments the file was rebuilt from; 0 when only its final header
    /// patch was missing.
    pub fragments: u32,
    /// Pictures in the rebuilt movie (unknown when only the header patch
    /// was missing: the writer's own counts stand).
    pub frames: Option<u64>,
    /// The shortest kept track's recovered duration, if the file was rebuilt.
    pub seconds: Option<f64>,
    /// What the movie holds, track by track, including what was left out
    /// (a track without any sample); empty when only the header patch was
    /// missing.
    pub summary: String,
}

/// A volume files are written to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    /// Bytes available for new files.
    pub free_bytes: u64,
    /// The file system, like `apfs`, `exfat` or `smbfs`.
    pub file_system: String,
    /// A network share rather than a local disk.
    pub network: bool,
}

/// Everything a platform needs to create a file.
#[derive(Debug, Clone)]
pub struct WriterSpec {
    pub path: PathBuf,
    pub container: Container,
    pub video: VideoTrackSpec,
    pub audio: Vec<AudioTrackSpec>,
    /// When the take began (wall clock): the file's creation time, the same
    /// for every file of a take.
    pub created: std::time::SystemTime,
}

#[derive(Debug, Clone)]
pub struct VideoTrackSpec {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub encoder: Encoder,
}

#[derive(Debug, Clone)]
pub struct AudioTrackSpec {
    pub channels: u32,
}
