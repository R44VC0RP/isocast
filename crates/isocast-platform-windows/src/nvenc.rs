//! Asynchronous native H.264/HEVC from D3D11 textures. Four pictures at most
//! remain leased to NVENC; encoded bytes, never raw pixels, cross to the CPU.

use std::collections::VecDeque;
use std::ffi::{CStr, c_char, c_void};
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use isocast_engine::{EncodedVideo, LiveVideoSpec, Time, VideoEncoder};
use windows::core::Interface;

use crate::encoder::Parameters;
use crate::h264::MAX_PACKET;
use crate::{Frame, GraphicsDevice, PixelFormat, VideoEncoding};

// Matches native/nvenc.cpp: asynchronous output needs 4 + B-frame count.
// B-frames and lookahead are disabled for this encoder.
const IN_FLIGHT: usize = 4;
const STALL_WAIT: Duration = Duration::from_millis(400);
const FINISH_WAIT: Duration = Duration::from_secs(5);

#[repr(C)]
struct Config {
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u32,
    constant_bitrate: u32,
    keyframe_interval: u32,
    codec: u32,
    input_depth: u32,
    output_depth: u32,
    quality_qp: u32,
    input_format: u32,
}

#[repr(C)]
#[derive(Default)]
struct Packet {
    data: *const u8,
    pts: u64,
    size: u32,
    keyframe: u32,
}

const _: () = assert!(std::mem::size_of::<Config>() == 44);
const _: () = assert!(std::mem::size_of::<Packet>() == 24);

unsafe extern "C" {
    fn isocast_nvenc_open(
        device: *mut c_void,
        spec: *const Config,
        error: *mut c_char,
        size: usize,
    ) -> *mut c_void;
    fn isocast_nvenc_error(encoder: *mut c_void) -> *const c_char;
    fn isocast_nvenc_sessions() -> u32;
    fn isocast_nvenc_headers(encoder: *mut c_void, size: *mut u32) -> *const u8;
    fn isocast_nvenc_submit(
        encoder: *mut c_void,
        texture: *mut c_void,
        pts: u64,
        keyframe: u32,
    ) -> i32;
    fn isocast_nvenc_wait_copy(encoder: *mut c_void, timeout_ms: u32) -> i32;
    fn isocast_nvenc_lock(encoder: *mut c_void, timeout_ms: u32, packet: *mut Packet) -> i32;
    fn isocast_nvenc_unlock(encoder: *mut c_void) -> i32;
    fn isocast_nvenc_flush(encoder: *mut c_void) -> i32;
    fn isocast_nvenc_wait_flush(encoder: *mut c_void, timeout_ms: u32) -> i32;
    fn isocast_nvenc_set_rate(encoder: *mut c_void, fps: u32, bitrate: u32) -> i32;
    fn isocast_nvenc_close(encoder: *mut c_void) -> i32;
}

struct Pending {
    _frame: Frame,
    pts: Time,
    submitted: Instant,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct EncodeStats {
    pub accepted: u64,
    pub completed: u64,
    pub dropped: u64,
    pub in_flight: usize,
    pub ready_packets: usize,
    /// Time inside the driver submission, including its input synchronization.
    pub last_submit_latency: Duration,
    /// Submission to receive, including any delay before the caller polled.
    /// This is not a hardware-only encoder timestamp.
    pub last_receive_latency: Duration,
}

/// H.264 High by default, or HEVC Main/Main10; SDR Rec.709 limited range,
/// P5 low-latency, single pass, spatial AQ8, no temporal AQ/lookahead/B-frames.
/// Shared input is copied on
/// GPU into one of four private slots: NVENC forbids client access while
/// its input is mapped. Other consumers can still use the original Frame.
pub struct NvencEncoder {
    native: NonNull<c_void>,
    device: GraphicsDevice,
    size: (u32, u32),
    low_latency: bool,
    recording: bool,
    copying: Option<Frame>,
    pending: VecDeque<Pending>,
    ready: VecDeque<EncodedVideo>,
    parameters: Parameters,
    input: PixelFormat,
    last_pts: Option<Time>,
    finishing: bool,
    finish_deadline: Option<Instant>,
    finished: bool,
    failed: Option<String>,
    stats: EncodeStats,
}

// SAFETY: NVENC sessions may be used on any thread, serialized here by &mut
// self. D3D11 is multithread protected; all native resources and frame leases
// remain owned until completion. No native callback enters Rust.
unsafe impl Send for NvencEncoder {}

impl NvencEncoder {
    pub fn new(device: &GraphicsDevice, spec: &LiveVideoSpec) -> Result<Self> {
        Self::with_format(device, spec, VideoEncoding::H264, PixelFormat::Bgra8)
    }

    /// HEVC output has an hvcC decoder config and no H.264 `parameter_sets`.
    /// The caller must select a matching HEVC mux track, never the live FLV path.
    /// NV12 input keeps its qualified limited-range Rec.709 codes. Main10
    /// output may expand eight-bit input, but cannot add source precision.
    pub fn with_format(
        device: &GraphicsDevice,
        spec: &LiveVideoSpec,
        encoding: VideoEncoding,
        input: PixelFormat,
    ) -> Result<Self> {
        Self::open(device, spec, encoding, input, None)
    }

    /// Recording at a native base QP (0..=51), adjusted by spatial AQ8.
    /// This is neither lossless at QP 0 nor equivalent to macOS quality.
    /// Ignores `spec.bitrate_kbps` and `spec.constant_bitrate`; changes to
    /// recording settings, including fps, require opening a new encoder.
    pub fn with_quantizer(
        device: &GraphicsDevice,
        spec: &LiveVideoSpec,
        encoding: VideoEncoding,
        input: PixelFormat,
        qp: u32,
    ) -> Result<Self> {
        Self::open(device, spec, encoding, input, Some(qp))
    }

    fn open(
        device: &GraphicsDevice,
        spec: &LiveVideoSpec,
        encoding: VideoEncoding,
        input: PixelFormat,
        quality_qp: Option<u32>,
    ) -> Result<Self> {
        encoding.validate(input)?;
        let bitrate = if let Some(qp) = quality_qp {
            if qp > 51 {
                bail!("video.quantizer: NVENC valid range is 0..=51");
            }
            validate_fps(spec.fps)?;
            0
        } else {
            bitrate(spec.fps, spec.bitrate_kbps)?
        };
        if spec.width == 0
            || spec.height == 0
            || !spec.width.is_multiple_of(2)
            || !spec.height.is_multiple_of(2)
        {
            bail!("video.size: 4:2:0 requires positive even width and height");
        }
        if spec.keyframe_interval == 0 {
            bail!("video.keyframe_interval: must be at least 1");
        }
        let config = Config {
            width: spec.width,
            height: spec.height,
            fps: spec.fps,
            bitrate,
            constant_bitrate: u32::from(quality_qp.is_none() && spec.constant_bitrate),
            keyframe_interval: spec.keyframe_interval,
            codec: u32::from(encoding != VideoEncoding::H264),
            input_depth: if input == PixelFormat::Rgba10 { 10 } else { 8 },
            output_depth: u32::from(encoding.bit_depth()),
            quality_qp: quality_qp.unwrap_or(u32::MAX),
            // Matches InputFormat in native/nvenc.cpp; storage is not bit depth.
            input_format: match input {
                PixelFormat::Bgra8 => 0,
                PixelFormat::Rgba10 => 1,
                PixelFormat::Nv12 => 2,
            },
        };
        let mut error = [0 as c_char; 512];
        let native = unsafe {
            isocast_nvenc_open(
                device.raw().as_raw(),
                &config,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        let native = NonNull::new(native).ok_or_else(|| {
            anyhow!(
                "encoder.nvenc: {}",
                unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy()
            )
        })?;
        let mut encoder = Self {
            native,
            device: device.clone(),
            size: (spec.width, spec.height),
            low_latency: spec.low_latency,
            recording: quality_qp.is_some(),
            copying: None,
            pending: VecDeque::new(),
            ready: VecDeque::new(),
            parameters: Parameters::new(encoding),
            input,
            last_pts: None,
            finishing: false,
            finish_deadline: None,
            finished: false,
            failed: None,
            stats: EncodeStats::default(),
        };
        let mut size = 0;
        let data = unsafe { isocast_nvenc_headers(encoder.raw(), &mut size) };
        if data.is_null() || size == 0 || size > 4096 {
            bail!("NVENC returned invalid decoder configuration size");
        }
        encoder
            .parameters
            .headers(unsafe { std::slice::from_raw_parts(data, size as usize) })?;
        Ok(encoder)
    }

    /// Sessions opened by this process, including any retained after a failed
    /// teardown. Not a guessed driver-wide session limit or availability claim.
    pub fn active_sessions() -> u32 {
        unsafe { isocast_nvenc_sessions() }
    }

    pub fn stats(&self) -> EncodeStats {
        EncodeStats {
            in_flight: self.pending.len(),
            ready_packets: self.ready.len(),
            ..self.stats
        }
    }

    /// avcC or hvcC for the encoded-packet muxer. No B-frame reordering.
    pub fn decoder_config(&self) -> Result<Vec<u8>> {
        self.parameters.config()
    }

    /// Try admission without waiting for an output slot. Native mapping and
    /// submission can wait on the driver, so call on the encoder's worker.
    /// False means NOT accepted; a caller may retry, so it is not counted as
    /// a dropped picture here.
    pub fn try_encode(&mut self, frame: &Frame, pts: Time, keyframe: bool) -> Result<bool> {
        self.healthy()?;
        if self.finishing {
            bail!("NVENC encoder has already begun finishing");
        }
        if self.last_pts.is_some_and(|last| pts <= last) {
            bail!("video.pts: must increase between accepted frames");
        }
        let timestamp = u64::try_from(pts.as_nanos()).context("video.pts: must be nonnegative")?;
        if self.size != (frame.width(), frame.height()) {
            bail!(
                "video.size: encoder is {}x{}, frame is {}x{}",
                self.size.0,
                self.size.1,
                frame.width(),
                frame.height()
            );
        }
        if !self.device.owns(frame) {
            bail!("video.device: NVENC input must belong to its D3D11 device");
        }
        if frame.pixel_format() != self.input {
            bail!(
                "video.input: NVENC requires {:?}, got {:?}",
                self.input,
                frame.pixel_format()
            );
        }
        if self.pending.len() == IN_FLIGHT {
            return Ok(false);
        }
        let submitted = Instant::now();
        // CopyResource precedes mapping. Even a rejected submission can still
        // be reading this picture; a COM texture reference is not a pool lease.
        self.copying = Some(frame.clone());
        let status = unsafe {
            isocast_nvenc_submit(
                self.raw(),
                frame.texture().as_raw(),
                timestamp,
                u32::from(keyframe),
            )
        };
        if status == 0 {
            self.pending.push_back(Pending {
                _frame: self.copying.take().expect("retained before native copy"),
                pts,
                submitted,
            });
            self.last_pts = Some(pts);
            self.stats.accepted += 1;
        }
        // Preserve a submission error before querying can replace the native
        // error buffer, but do not return until the source copy is accounted for.
        let submission = if status == -1 {
            Ok(())
        } else {
            self.check(status)
        };
        let deadline = Instant::now() + FINISH_WAIT;
        let copied = unsafe { isocast_nvenc_wait_copy(self.raw(), millis(FINISH_WAIT)) };
        self.stats.last_submit_latency = submitted.elapsed();
        if copied != 0 {
            // Teardown shares this bound and quarantines both accepted and
            // attempted sources if completion cannot be proved.
            self.finish_deadline = Some(deadline);
            if let Err(copy_error) = self.check(copied) {
                return match submission {
                    Ok(()) => Err(copy_error),
                    Err(error) => Err(anyhow!("{error:#}; additionally, {copy_error:#}")),
                };
            }
        }
        self.copying = None;
        submission?;
        Ok(status != -1)
    }

    fn raw(&self) -> *mut c_void {
        self.native.as_ptr()
    }

    fn healthy(&self) -> Result<()> {
        if let Some(error) = &self.failed {
            bail!("NVENC encoder failed: {error}");
        }
        Ok(())
    }

    fn check(&mut self, status: i32) -> Result<()> {
        if status == 0 {
            return Ok(());
        }
        let error = unsafe { CStr::from_ptr(isocast_nvenc_error(self.raw())) }
            .to_string_lossy()
            .into_owned();
        self.failed.get_or_insert_with(|| error.clone());
        bail!("encoder.nvenc: {error} (status {status})")
    }

    fn read(&mut self, timeout: Duration) -> Result<Option<EncodedVideo>> {
        let mut packet = Packet::default();
        let status = unsafe { isocast_nvenc_lock(self.raw(), millis(timeout), &mut packet) };
        if status == -1 {
            return Ok(None);
        }
        self.check(status)?;
        // Always unlock a successfully locked bitstream, even if its data or
        // timestamps are invalid. Only then may its retained input be released.
        let decoded = self.decode(&packet);
        let status = unsafe { isocast_nvenc_unlock(self.raw()) };
        self.check(status)?;
        let pending = self
            .pending
            .pop_front()
            .context("NVENC output has no pending input")?;
        self.stats.last_receive_latency = pending.submitted.elapsed();
        match decoded {
            Ok(packet) => {
                self.stats.completed += 1;
                Ok(Some(packet))
            }
            Err(error) => {
                self.stats.dropped += 1;
                self.failed = Some(error.to_string());
                Err(error)
            }
        }
    }

    fn decode(&mut self, packet: &Packet) -> Result<EncodedVideo> {
        let pts = self
            .pending
            .front()
            .context("NVENC returned an unsubmitted picture")?
            .pts;
        if packet.pts != pts.as_nanos() as u64 {
            bail!("NVENC reordered or changed a submitted timestamp");
        }
        if packet.data.is_null() || packet.size == 0 || packet.size as usize > MAX_PACKET {
            bail!("NVENC returned invalid packet size {}", packet.size);
        }
        let source = unsafe { std::slice::from_raw_parts(packet.data, packet.size as usize) };
        let decoded = self
            .parameters
            .packet(source, pts)?
            .context("NVENC packet contains no picture")?;
        if decoded.keyframe != (packet.keyframe != 0) {
            bail!("NVENC keyframe flag disagrees with its picture");
        }
        Ok(decoded)
    }

    /// Flush and return every remaining compressed picture, in order. Only a
    /// successful flush permits destroying the input/output GPU resources.
    /// On failure, valid packets remain available through `receive`, followed
    /// by the sticky encoder error once that prefix has been consumed.
    pub fn finish(&mut self) -> Result<Vec<EncodedVideo>> {
        if let Err(error) = self.finish_native() {
            self.failed.get_or_insert_with(|| format!("{error:#}"));
            return Err(error);
        }
        Ok(self.ready.drain(..).collect())
    }

    fn finish_native(&mut self) -> Result<()> {
        if self.finished {
            return self.healthy();
        }
        self.finishing = true;
        // Drop retries share this deadline, never adding a second five-second
        // wait after a timed-out finish. Busy flushes are transient, not loss.
        let deadline = *self
            .finish_deadline
            .get_or_insert_with(|| Instant::now() + FINISH_WAIT);
        let status = unsafe {
            isocast_nvenc_wait_copy(
                self.raw(),
                millis(deadline.saturating_duration_since(Instant::now())),
            )
        };
        self.check(status)?;
        self.copying = None;
        loop {
            if Instant::now() >= deadline {
                bail!("NVENC did not finish within 5 seconds");
            }
            let status = unsafe { isocast_nvenc_flush(self.raw()) };
            if status != -1 {
                self.check(status)?;
                break;
            }
            if let Some(packet) = self.read(Duration::ZERO)? {
                self.ready.push_back(packet);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        while !self.pending.is_empty() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("NVENC did not finish its accepted pictures within 5 seconds");
            }
            if let Some(packet) = self.read(remaining)? {
                self.ready.push_back(packet);
            } else {
                // A signalled event with LOCK_BUSY must not spin a CPU core.
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        let status = unsafe {
            isocast_nvenc_wait_flush(
                self.raw(),
                millis(deadline.saturating_duration_since(Instant::now())),
            )
        };
        if status == -1 {
            bail!("NVENC flush completion did not arrive within 5 seconds");
        }
        self.check(status)?;
        self.finished = true;
        self.healthy()
    }
}

impl VideoEncoder for NvencEncoder {
    type Frame = Frame;

    fn encode(&mut self, frame: &Frame, pts: Time, keyframe: bool) -> Result<()> {
        if self.try_encode(frame, pts, keyframe)? {
            return Ok(());
        }
        if !self.pending.is_empty() && self.ready.len() < IN_FLIGHT {
            let wait = if self.low_latency {
                Duration::ZERO
            } else {
                STALL_WAIT
            };
            if let Some(packet) = self.read(wait)? {
                self.ready.push_back(packet);
            }
            if self.try_encode(frame, pts, keyframe)? {
                return Ok(());
            }
        }
        self.stats.dropped += 1;
        Ok(())
    }

    fn receive(&mut self) -> Result<Option<EncodedVideo>> {
        self.receive_timeout(Duration::ZERO)
    }

    fn receive_timeout(&mut self, timeout: Duration) -> Result<Option<EncodedVideo>> {
        if let Some(packet) = self.ready.pop_front() {
            return Ok(Some(packet));
        }
        self.healthy()?;
        self.read(timeout)
    }

    fn dropped(&self) -> u64 {
        self.stats.dropped
    }

    fn set_rate(&mut self, fps: u32, bitrate_kbps: u32) -> Result<()> {
        if self.recording {
            bail!("encoder.nvenc: recording settings cannot change; open a new encoder");
        }
        self.healthy()?;
        if self.finishing {
            bail!("NVENC encoder has already begun finishing");
        }
        let bits = bitrate(fps, bitrate_kbps)?;
        let status = unsafe { isocast_nvenc_set_rate(self.raw(), fps, bits) };
        self.check(status)
    }
}

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            eprintln!("NVENC teardown: {error:#}");
            if !self.finished {
                // A hung native encoder must never outlive its frame leases
                // OR its device, even if no input is currently pending.
                eprintln!("NVENC: unfinished GPU resources retained until process exit");
                std::mem::forget(self.copying.take());
                std::mem::forget(std::mem::take(&mut self.pending));
                std::mem::forget(self.device.clone());
                return;
            }
        }
        let status = unsafe { isocast_nvenc_close(self.raw()) };
        if status != 0 {
            let _ = self
                .check(status)
                .map_err(|error| eprintln!("NVENC teardown: {error:#}; driver resources retained"));
            std::mem::forget(self.device.clone());
        }
    }
}

fn validate_fps(fps: u32) -> Result<()> {
    if !(1..=240).contains(&fps) {
        bail!("video.fps: valid range is 1..=240");
    }
    Ok(())
}

fn bitrate(fps: u32, kbps: u32) -> Result<u32> {
    validate_fps(fps)?;
    kbps.checked_mul(1000)
        .filter(|value| *value > 0)
        .context("video.bitrate_kbps: must be positive and fit in 32-bit bits/second")
}

fn millis(duration: Duration) -> u32 {
    duration.as_millis().min(u128::from(u32::MAX - 1)) as u32
}
