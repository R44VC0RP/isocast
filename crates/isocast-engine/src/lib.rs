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
pub mod encoder_calibration;
pub mod estimate;
pub mod filter;
pub mod governor;
pub mod gpu;
pub mod hotkeys;
pub mod layout;
pub mod machine_profile;
mod mask;
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

/// A codec this machine can't use now ([`Platform::missing_codecs`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MissingCodec {
    /// Like "OpenH264".
    pub name: String,
    pub reason: String,
    pub fix: String,
}

/// Something that works, but less well than it should, with how to fix it
/// ([`Platform::browser_warning`]); `doctor` reports it as a warning.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DoctorWarning {
    pub detail: String,
    pub fix: String,
}

/// The video encoders found by [`Platform::video_encoders`], by name, in
/// the order a recording tries them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VideoEncoders {
    /// On a GPU or media engine.
    pub hardware: Vec<String>,
    /// On the CPU: where recording falls back to without hardware.
    pub software: Vec<String>,
    /// Reasons a hardware encoder could not be opened during discovery.
    pub unavailable: Vec<String>,
}

/// Audio inside the engine is 48 kHz, 32-bit float, interleaved.
pub const SAMPLE_RATE: u32 = 48_000;
/// Recordings are written in movie fragments of this many seconds of media
/// time. Passing a boundary with input samples allows the writer to close a
/// fragment; accepting those samples does not acknowledge an on-disk commit.
/// Queued inputs and track timing mean this is not a maximum crash-loss bound.
pub const MOVIE_FRAGMENT_SECONDS: i64 = 2;

/// A frame's retention domain. Domains reserve independent capacity; clones
/// and repeated deliveries of the same picture share that capacity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameUse {
    Production = 0,
    Recording = 1,
    Auxiliary = 2,
    /// Protected compositors must keep reading even when an ISO's file queue
    /// has filled its separate recording reserve on the same source pool.
    Composition = 3,
}

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

    /// Adapter identity clients must select before opening shared previews.
    /// Windows returns its capture/compositor LUID (low, high); platforms
    /// without an explicit adapter-selection contract return None.
    fn gpu_adapter_luid(&self) -> anyhow::Result<Option<(u32, i32)>> {
        Ok(None)
    }

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

    /// This application's identity in the platform's credential store.
    /// `None` for an unbundled executable.
    fn app_identifier(&self) -> Option<String> {
        None
    }

    /// Media deadline used after a clock detects stretched ordinary sleeps,
    /// or from the first tick when `prefers_deadline_timer` is true. Back ends
    /// should bound background timer coalescing.
    fn sleep_until(&self, at: Time) {
        let now = self.now();
        if at > now {
            std::thread::sleep(at.saturating_since(now));
        }
    }

    /// Use the platform deadline timer from the first media tick, without
    /// waiting for evidence of stretched ordinary sleeps. Windows has a
    /// cheap high-resolution waitable timer; other platforms keep the
    /// existing adaptive policy unless they opt in. Opted-in video workers
    /// also discard ticks whose next deadline has already passed, rather
    /// than rendering a burst of obsolete pictures to catch up. File start
    /// offsets and drop counters expose gaps; timestamps are not rebased.
    fn prefers_deadline_timer(&self) -> bool {
        false
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

    /// Native opens normally time out quickly. A desktop consent dialog can
    /// need longer while the person chooses the screen to share.
    fn source_open_timeout(&self, _source: &SourceKind) -> std::time::Duration {
        std::time::Duration::from_secs(8)
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

    /// Codecs this machine should have but can't use now (Linux: Cisco's
    /// OpenH264, which the installer downloads), each with why and the
    /// command that fixes it. `doctor` reports each as a problem.
    fn missing_codecs(&self) -> Vec<MissingCodec> {
        Vec::new()
    }

    /// Why browser sources here run less safely or not at all (Linux:
    /// pages without WebKit's sandbox, or WebKitGTK missing), if they do.
    /// `doctor` reports it as a warning when the setup has a browser
    /// source, or without a setup.
    fn browser_warning(&self) -> Option<DoctorWarning> {
        None
    }

    /// The video encoders a recording here would use, looked for in full
    /// (`doctor`): what `machine` lists, unless finding them is too slow
    /// for it (Windows opens encoder sessions to know).
    fn video_encoders(&self) -> VideoEncoders {
        VideoEncoders {
            hardware: self.machine().hardware_encoders,
            software: Vec::new(),
            unavailable: Vec::new(),
        }
    }

    /// Find the device a source would record, and the format it would
    /// record in, without opening it. `None` for synthetic sources; an
    /// error naming the valid devices if none matches.
    fn probe(&self, source: &SourceKind, canvas: &Canvas) -> anyhow::Result<Option<Probe>>;

    /// Free space and kind of the volume holding `path`.
    fn volume(&self, path: &Path) -> anyhow::Result<Volume>;

    /// CPU seconds used by this process, user and kernel time together.
    fn process_cpu_seconds(&self) -> Option<f64> {
        None
    }

    /// Cumulative busy CPU core-seconds across the system. Calibration uses
    /// deltas to include native encoder services, not only the helper process.
    /// Outside activity is included and makes this observation conservative.
    fn system_cpu_seconds(&self) -> Option<f64> {
        None
    }

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

    /// What the program and ISO stages hand H.264 and HEVC encoders:
    /// Y'CbCr from [`Platform::ycbcr_frame_pool`] (converted on the GPU),
    /// or BGRA from [`Platform::frame_pool`] for backends that accept RGB
    /// frames and own their conversion.
    fn encoder_frame_format(&self) -> EncoderFrameFormat {
        EncoderFrameFormat::Ycbcr
    }

    /// Whether a monitor encoder asked for High profile is guaranteed to make
    /// Constrained High (no B-frames, `640c..`), so a WHEP answer may offer
    /// a client that takes only Constrained High the High stream. A promise,
    /// not a preference: a backend whose encoder may fall back to plain High
    /// (VideoToolbox), or always makes it (NVENC, OpenH264 at High), says
    /// `false`, and such clients get Constrained Baseline instead. Not a
    /// probe of the native encoder; a backend says `true` only when its
    /// session cannot make anything else.
    fn monitor_h264_constrained_high(&self) -> bool {
        false
    }

    /// Require a fresh program picture before choosing a recording's epoch.
    /// Software GPU drivers may compile their first draw lazily; preparing
    /// that work must not consume the first slots of an already armed take,
    /// so the recording fails if none arrives. Every other backend warms
    /// its programs too, bounded by `PROGRAM_WARM_TIMEOUT` (record/mod.rs),
    /// then starts anyway and counts the skipped head as dropped (#150).
    fn recording_needs_program_frame(&self) -> bool {
        false
    }

    /// Move a retained reference to an output's domain before queueing it or
    /// giving it to a GPU/native encoder. Refusal is local to that domain,
    /// never charged to another output's reserve.
    fn retain_frame(frame: &Self::Frame, _usage: FrameUse) -> Option<Self::Frame> {
        Some(frame.clone())
    }

    /// The platform's distinct-buffer ledger, including GPU/native owners.
    fn frame_budget(&self) -> Option<serde_json::Value> {
        None
    }

    /// `doctor` checks only this platform can make, such as which media
    /// decoders the system provides (Linux's GStreamer plugins).
    fn doctor_checks(&self) -> Vec<isocast_protocol::api::Check> {
        Vec::new()
    }

    /// Ask the OS to run the calling thread ahead of background work: it
    /// paces frames or audio in real time (the program, a file's frame
    /// clock, an audio reader, the mix, a file writer).
    fn real_time_thread(&self) {}

    /// Audio mixer scheduling. The default preserves existing back ends;
    /// Windows distinguishes MMCSS audio from above-normal video workers.
    fn audio_thread(&self) {
        self.real_time_thread();
    }

    /// Optional OS policy held only while a recording is being prepared,
    /// captured and drained, not for the persistent preview engine's life.
    /// The returned lease restores its policy when dropped, including on
    /// startup failure or unwinding. Back ends must handle nested sessions.
    fn recording_activity(&self) -> anyhow::Result<Option<Box<dyn Send>>> {
        Ok(None)
    }

    /// Lower a disposable calibration helper's process/thread priority.
    /// Only [`machine_profile::measure`] in the helper may call this.
    /// Returns what was set, for the profile's notes; `None` where the
    /// platform doesn't lower it.
    fn calibration_priority(&self) -> Option<&'static str> {
        None
    }

    /// Read the existing sampler's latest non-consuming headroom observation.
    fn governor_headroom(&self) -> Option<governor::Headroom> {
        None
    }

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

    /// Whether shared surfaces live in CPU memory (Linux's memfds): each
    /// frame drawn into the texture is then read back and written with
    /// [`Self::write_shared_surface`] before it's announced. GPU surfaces
    /// (IOSurfaces, D3D12 resources) are the texture itself.
    fn shared_surface_in_memory(&self) -> bool {
        false
    }

    /// Copy a drawn picture into `surface`: `rows` holds its rows `stride`
    /// bytes apart, RGBA (BGRA if `bgra`, swapped on the way). Only for
    /// [`Self::shared_surface_in_memory`].
    fn write_shared_surface(
        &self,
        _surface: &Self::SharedSurface,
        _rows: &[u8],
        _stride: usize,
        _bgra: bool,
    ) -> anyhow::Result<()> {
        anyhow::bail!("this platform's shared surfaces are drawn in place")
    }

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

    /// [`Platform::create_writer`] for a file that takes a source's frames
    /// as captured, with `frame` one of them, for backends whose encoders
    /// must know their input's pixel format up front. The frame is only
    /// looked at, not kept or written. By default it is ignored.
    fn create_writer_for_frame(
        &self,
        spec: &WriterSpec,
        frame: &Self::Frame,
    ) -> anyhow::Result<Self::Writer> {
        let _ = frame;
        self.create_writer(spec)
    }

    /// Create every file of one take together (the whole coalesced
    /// workload: main and named programs and ISOs), so a platform with a
    /// shared encoder budget can admit the batch before opening any of it.
    /// On error nothing of the batch remains open: writers already created
    /// are cancelled. By default each file is created in order exactly as
    /// [`Platform::create_writer`] / [`Platform::create_writer_for_frame`],
    /// waiting for an input's first frame just before its file.
    fn create_writer_batch(
        &self,
        files: &[WriterRequest<'_, Self::Frame>],
    ) -> anyhow::Result<(Vec<Self::Writer>, Option<EncoderAdmission>)> {
        let mut writers = Vec::with_capacity(files.len());
        for file in files {
            let writer = match file.input {
                Some(input) => {
                    input().and_then(|frame| self.create_writer_for_frame(file.spec, &frame))
                }
                None => self.create_writer(file.spec),
            };
            match writer {
                Ok(writer) => writers.push(writer),
                Err(err) => {
                    writers.into_iter().for_each(FileWriter::cancel);
                    return Err(
                        err.context(format!("could not create {}", file.spec.path.display()))
                    );
                }
            }
        }
        Ok((writers, None))
    }

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

    /// A reader of what a camera capture from [`Platform::capture_camera`]
    /// has observed, for the session manifest. `None` (the default) when
    /// the platform reports nothing. The reader must hold only the
    /// device's identity and shared flags, never the capture or GPU
    /// resources, so it stays readable after the capture stops.
    fn camera_metadata(&self, _capture: &Self::Capture) -> Option<CameraMetadataReader> {
        None
    }

    /// Start capturing a microphone ("default", or an id, name or part of
    /// a name from [`Platform::devices`]): the input channels `channels`
    /// picks (numbered from 1; one is delivered as mono, two as stereo),
    /// or by default its first two. Returns the channel count delivered.
    fn capture_microphone(
        &self,
        device: &str,
        channels: Option<&[u32]>,
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
}

/// The frames a platform's encoders want drawn for them (see
/// [`Platform::encoder_frame_format`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderFrameFormat {
    /// 8-bit BGRA, as drawn: conversion belongs to the encoder backend.
    Bgra,
    /// 8-bit 4:2:0 Y'CbCr, converted by the engine.
    Ycbcr,
}

/// What a live H.264 encoder makes: constant-ish bitrate, the requested profile,
/// no B-frames, Rec. 709 tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveVideoSpec {
    pub profile: isocast_protocol::api::H264Profile,
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

    /// Cumulative intentional admission refusals included in `dropped()`.
    /// They remain losses in public accounting, but aren't new overloads.
    fn intentional_video_drops(&self) -> u64 {
        0
    }

    /// Rank the canvas's live output ahead of ISO encodes; main comes first.
    fn prioritize_video(&mut self, _canvas: isocast_protocol::CanvasId) {}

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

    /// A refused rate change could not be rolled back completely. The last
    /// acknowledged rate is not proof of the current native properties;
    /// requesting that rate again must still attempt restoration.
    fn rate_uncertain(&self) -> bool {
        false
    }

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

/// Reads a camera capture's metadata as observed so far. Called only when
/// a session manifest is written, never per frame.
pub type CameraMetadataReader =
    Arc<dyn Fn() -> isocast_protocol::CameraCaptureMetadata + Send + Sync>;

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

    /// Cumulative deliberate admission refusals. Read around append_video
    /// so a later policy restoration cannot reclassify a refused picture.
    fn intentional_video_drops(&self) -> u64 {
        0
    }

    /// Protect this program output ahead of optional ISO encodes when the
    /// platform's shared encoder hardware is overloaded.
    fn prioritize_video(&mut self, _canvas: isocast_protocol::CanvasId) {}

    /// Start the file's timeline at `at`. Called once, before anything is
    /// appended.
    fn start(&mut self, at: Time) -> anyhow::Result<()>;

    /// Append a video frame, waiting briefly for a busy encoder. Returns
    /// `false` if the encoder could not keep up and the frame was dropped.
    fn append_video(&mut self, frame: &Self::Frame, pts: Time) -> anyhow::Result<bool>;

    /// Admission skipped a picture upstream. Make the same progress on
    /// previously accepted work as a refused append, without new pixels.
    fn skip_video(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    /// Video frames waiting inside the writer for its encoder, for
    /// developer mode's traces.
    fn queue_depth(&self) -> usize {
        0
    }

    /// Completed encoded video pictures observed so far, when the backend
    /// exposes them. Calibration uses deltas, never queued submissions.
    fn completed_video_frames(&self) -> Option<u64> {
        None
    }

    /// Notify once the muxer has written the first encoded video packet to
    /// the output, not merely queued or buffered it. This is not a durability
    /// barrier. Returns false when the backend cannot observe that milestone.
    /// The callback must be cheap and must not call back into the writer.
    fn on_first_video_packet(&mut self, _notify: Box<dyn FnOnce() + Send>) -> bool {
        false
    }

    /// Actual native session selection, not the machine's codec catalogue.
    /// Unknown for backends that cannot inspect the running encoder.
    fn hardware_accelerated(&self) -> Option<bool> {
        None
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

/// One file of a batch for [`Platform::create_writer_batch`].
pub struct WriterRequest<'a, F> {
    pub spec: &'a WriterSpec,
    /// The engine's identity for one recording (all its takes), so a split
    /// is never confused with another recording; never a path or name.
    pub recording: u64,
    /// The take the file belongs to, from 1.
    pub take: u32,
    /// Stable role and source within a take (`iso/<source>` or
    /// `program/<source>`): the same output in the next take has the same
    /// value. Never a display name or path.
    pub output: String,
    /// An ISO source's file, not a canvas program (main or named): programs
    /// keep their priority and are never relabelled as ISOs.
    pub iso: bool,
    /// For a file that takes a source's frames as captured: waits for one
    /// of them (see [`Platform::create_writer_for_frame`]).
    pub input: Option<&'a dyn Fn() -> anyhow::Result<F>>,
}

/// How a platform admitted one batch of files against a shared encoder
/// budget, for the recording's structured diagnostics.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EncoderAdmission {
    /// `planned` (presets selected before any file opened) or `unplanned`
    /// (each file opened with its provider's default; see `reason`).
    pub mode: &'static str,
    /// Why the batch is unplanned, or why a calibration was not applied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Where the throughput figures came from: `calibrated` or `reference`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<&'static str>,
    pub calibrated: bool,
    /// The adapter/driver key the figures belong to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// The calibration cache's path and what became of it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_status: Option<String>,
    /// Estimated share of the encoder, including already-open sessions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_load: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accounted_load: Option<f64>,
    pub accounted_sessions: usize,
    pub handed_over_sessions: usize,
    pub outputs: Vec<EncoderAdmissionOutput>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct EncoderAdmissionOutput {
    pub take: u32,
    pub output: String,
    pub path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub planned_preset: Option<u32>,
    /// The preset the native session was opened and validated with (the
    /// opened configuration, as the SDK accepted it), not an independent
    /// readback of the running session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied_preset: Option<u32>,
    pub backend: String,
}
