//! Encoders for streaming: H.264 with a VTCompressionSession (real time,
//! on the media engines) and AAC-LC with an AudioToolbox AudioConverter.
//! The engine wraps what they make in FLV and sends it over RTMP. The same
//! H.264 encoder, in its low-latency form, makes monitors' video.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use isocast_engine::{
    AudioEncoder, EncodedVideo, LiveAudioSpec, LiveVideoSpec, ParameterSets, SAMPLE_RATE, Time,
    VideoEncoder,
};
use objc2_audio_toolbox::{
    AudioConverterDispose, AudioConverterFillComplexBuffer, AudioConverterGetProperty,
    AudioConverterNew, AudioConverterPrimeInfo, AudioConverterRef, AudioConverterSetProperty,
    kAudioCodecBitRateControlMode_Constant, kAudioCodecPropertyBitRateControlMode,
    kAudioConverterEncodeBitRate, kAudioConverterPrimeInfo,
    kAudioConverterPropertyMaximumOutputPacketSize,
};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, AudioStreamPacketDescription,
    MPEG4ObjectID, kAudioFormatFlagIsFloat, kAudioFormatFlagIsPacked, kAudioFormatLinearPCM,
    kAudioFormatMPEG4AAC,
};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType,
};
use objc2_core_media::{
    CMFormatDescription, CMSampleBuffer, CMTime, CMTimeFlags,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex, kCMSampleAttachmentKey_NotSync,
    kCMVideoCodecType_H264,
};
use objc2_core_video::{
    kCVImageBufferColorPrimaries_ITU_R_709_2, kCVImageBufferTransferFunction_ITU_R_709_2,
    kCVImageBufferYCbCrMatrix_ITU_R_709_2,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, VTSessionSetProperty,
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_BaseLayerFrameRateFraction, kVTCompressionPropertyKey_ColorPrimaries,
    kVTCompressionPropertyKey_ConstantBitRate, kVTCompressionPropertyKey_DataRateLimits,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_H264EntropyMode,
    kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
    kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTCompressionPropertyKey_TransferFunction, kVTCompressionPropertyKey_YCbCrMatrix,
    kVTEncodeFrameOptionKey_ForceKeyFrame, kVTH264EntropyMode_CABAC,
    kVTProfileLevel_H264_High_AutoLevel,
    kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
    kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
};

use crate::writer::{
    EncoderPressure, cancel_encoder_start, claim_encoder_start, encoder_start_rank,
    request_encoder_start,
};
use crate::{PixelBuffer, from_cm_time};

/// Nanoseconds: frame times go to the encoder exactly as the engine has
/// them.
const TIMESCALE: i32 = 1_000_000_000;
/// Frames submitted to the encoder at once. Monitors discard at this
/// limit; streams wait on their own worker, with a bounded tap backlog.
const MAX_IN_FLIGHT: u64 = 8;
/// Wait only on a stream's private encoder thread when its hardware stalls.
/// The program tap absorbs the backlog; monitors still discard stale frames.
const STALL_WAIT: Duration = Duration::from_millis(400);

/// A live H.264 encoder: High profile, no B-frames (decode order is
/// presentation order, so FLV needs no composition offsets), bitrate held
/// to the target each second, keyframes when the engine asks.
pub struct H264Encoder {
    session: CFRetained<VTCompressionSession>,
    /// Where the session's callback leaves frames. The callback holds a
    /// raw pointer to it, so it lives as long as the session (see Drop).
    output: Arc<Output>,
    fps: u32,
    low_latency: bool,
    force_keyframe: CFRetained<CFDictionary<CFString, CFBoolean>>,
    priority: Option<EncoderPressure>,
    /// The session has its first frame; until then `start_ticket` is its
    /// place in the process's encoder start order (see
    /// [`claim_encoder_start`]).
    started: bool,
    start_ticket: Option<u64>,
    size: (u32, u32),
}

// SAFETY: VideoToolbox sessions may be used from any thread; this one is
// used by one thread at a time.
unsafe impl Send for H264Encoder {}

#[derive(Default)]
struct Output {
    session_id: u64,
    purpose: &'static str,
    size: [u32; 2],
    frames: Mutex<VecDeque<Result<EncodedVideo>>>,
    /// Signalled when a frame is added.
    ready: Condvar,
    dropped: AtomicU64,
    in_flight: AtomicU64,
    latency_ns: AtomicU64,
    callbacks: AtomicU64,
    last_os_status: std::sync::atomic::AtomicI32,
}

static ENCODERS: Mutex<Vec<Weak<Output>>> = Mutex::new(Vec::new());
static NEXT_ENCODER: AtomicU64 = AtomicU64::new(1);

pub(crate) fn diagnostics() -> serde_json::Value {
    let mut encoders = ENCODERS.lock().unwrap();
    encoders.retain(|e| e.strong_count() > 0);
    serde_json::json!({"h264":encoders.iter().filter_map(Weak::upgrade).map(|e|serde_json::json!({
        "session":e.session_id,"purpose":e.purpose,"size":e.size,"in_flight":e.in_flight.load(Ordering::Relaxed),"ready_packets":e.frames.lock().unwrap().len(),
        "last_submit_to_callback_ns":e.latency_ns.load(Ordering::Relaxed),"callbacks":e.callbacks.load(Ordering::Relaxed),
        "dropped":e.dropped.load(Ordering::Relaxed),"last_os_status":e.last_os_status.load(Ordering::Relaxed)
    })).collect::<Vec<_>>(),"asset_writer_internal_queue":null,"asset_writer_queue_reason":"AVAssetWriter has no public queue-depth or encode-latency counter; per-file drops and disk latency are in recording"})
}

fn monotonic_ns() -> u64 {
    // A scalar cookie, never dereferenced. No allocation or lock per frame.
    let mut time = std::mem::MaybeUninit::<libc::timespec>::zeroed();
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC_RAW, time.as_mut_ptr()) } != 0 {
        return 0;
    }
    let time = unsafe { time.assume_init() };
    time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64
}

impl H264Encoder {
    pub fn new(spec: &LiveVideoSpec) -> Result<Self> {
        let output = Arc::new(Output {
            session_id: NEXT_ENCODER.fetch_add(1, Ordering::Relaxed),
            purpose: if spec.low_latency {
                "monitor"
            } else {
                "stream"
            },
            size: [spec.width, spec.height],
            ..Default::default()
        });
        ENCODERS.lock().unwrap().push(Arc::downgrade(&output));
        // Monitors want each frame decoded as soon as it arrives: VideoToolbox's
        // low-latency mode says in the stream that frames are never reordered
        // (without it, decoders hold up to five frames back). Encoders without
        // it get the usual mode.
        let mut status = -1;
        let mut session = ptr::null_mut();
        for low_latency in [true, false] {
            if low_latency && !spec.low_latency {
                continue;
            }
            status = unsafe { create_session(spec, &output, low_latency, &mut session) };
            if status == 0 {
                break;
            }
        }
        let Some(session) = NonNull::new(session).filter(|_| status == 0) else {
            let what = match spec.low_latency {
                true => "a monitor",
                false => "the stream",
            };
            // kVTCouldNotFindVideoEncoderErr: no hardware encoder is free.
            if status == -12908 {
                bail!(
                    "no hardware H.264 encoder is free for {what}: this Mac's video \
                     encoders are busy with other recordings or streams (in Isocast or other \
                     apps); stop one, or record fewer ISO files, and try again"
                );
            }
            bail!(
                "VideoToolbox could not start an H.264 encoder for {}×{} (error {status})",
                spec.width,
                spec.height
            );
        };
        let session = unsafe { CFRetained::from_raw(session) };
        isocast_diagnostics::debug!(
            "encoder: h264 {} session {} {}x{} at {} kb/s",
            output.purpose,
            output.session_id,
            spec.width,
            spec.height,
            spec.bitrate_kbps
        );
        let bits = i64::from(spec.bitrate_kbps) * 1000;
        let set = |key: &CFString, value: &CFType| -> Result<()> {
            let status = unsafe { VTSessionSetProperty(&session, key, Some(value)) };
            if status != 0 {
                bail!("the H.264 encoder refused {key} (error {status})");
            }
            Ok(())
        };
        unsafe {
            set(kVTCompressionPropertyKey_RealTime, CFBoolean::new(true))?;
            set(
                kVTCompressionPropertyKey_ProfileLevel,
                kVTProfileLevel_H264_High_AutoLevel,
            )?;
            set(
                kVTCompressionPropertyKey_AllowFrameReordering,
                CFBoolean::new(false),
            )?;
            set(
                kVTCompressionPropertyKey_H264EntropyMode,
                kVTH264EntropyMode_CABAC,
            )?;
            // Every frame in the base layer: with the low-latency mode's
            // default of half, every other frame came out ~35 ms late.
            if spec.low_latency {
                let _ = set(
                    kVTCompressionPropertyKey_BaseLayerFrameRateFraction,
                    &CFNumber::new_f64(1.0),
                );
            }
            // Constant bitrate pads still pictures up to the target (YouTube
            // flags streams far under it); if the encoder won't, average.
            let constant = spec.constant_bitrate
                && set(
                    kVTCompressionPropertyKey_ConstantBitRate,
                    &CFNumber::new_i64(bits),
                )
                .inspect_err(|err| {
                    isocast_diagnostics::diagnostic!("warn";
                        "isocastd: constant bitrate unavailable, using average: {err:#}"
                    )
                })
                .is_ok();
            if !constant {
                set(
                    kVTCompressionPropertyKey_AverageBitRate,
                    &CFNumber::new_i64(bits),
                )?;
            }
            // A stream may carry no more than the target in any second:
            // near-constant bitrate, as services ask. A monitor's is an
            // average.
            if !constant && !spec.low_latency {
                let limits = CFArray::<CFNumber>::from_retained_objects(&[
                    CFNumber::new_i64(bits / 8),
                    CFNumber::new_f64(1.0),
                ]);
                set(kVTCompressionPropertyKey_DataRateLimits, &limits)?;
            }
            set(
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                &CFNumber::new_i32(spec.keyframe_interval as i32),
            )?;
            // Half a frame over the interval: the engine forces each
            // keyframe on time, and at exactly the interval VideoToolbox
            // made one of its own a frame early whenever frame times
            // rounded a nanosecond past it (every time at 30 fps).
            set(
                kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
                &CFNumber::new_f64((f64::from(spec.keyframe_interval) + 0.5) / f64::from(spec.fps)),
            )?;
            set(
                kVTCompressionPropertyKey_ExpectedFrameRate,
                &CFNumber::new_i32(spec.fps as i32),
            )?;
            set(
                kVTCompressionPropertyKey_ColorPrimaries,
                kCVImageBufferColorPrimaries_ITU_R_709_2,
            )?;
            set(
                kVTCompressionPropertyKey_TransferFunction,
                kCVImageBufferTransferFunction_ITU_R_709_2,
            )?;
            set(
                kVTCompressionPropertyKey_YCbCrMatrix,
                kCVImageBufferYCbCrMatrix_ITU_R_709_2,
            )?;
            // Speed over quality, so a busy media engine (recordings run
            // on it too) keeps up in real time. Older encoders lack it.
            let _ = set(
                kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
                CFBoolean::new(true),
            );
            session.prepare_to_encode_frames();
        }
        let force_keyframe = unsafe {
            CFDictionary::<CFString, CFBoolean>::from_slices(
                &[kVTEncodeFrameOptionKey_ForceKeyFrame],
                &[CFBoolean::new(true)],
            )
        };
        Ok(Self {
            session,
            output,
            fps: spec.fps,
            low_latency: spec.low_latency,
            force_keyframe,
            priority: (!spec.low_latency).then(EncoderPressure::new),
            started: false,
            start_ticket: None,
            size: (spec.width, spec.height),
        })
    }
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        if let (false, Some(ticket)) = (self.started, self.start_ticket) {
            cancel_encoder_start(ticket);
        }
        // No callbacks after this, so `output` can go.
        unsafe { self.session.invalidate() };
    }
}

impl VideoEncoder for H264Encoder {
    type Frame = PixelBuffer;

    fn encode(&mut self, frame: &PixelBuffer, pts: Time, keyframe: bool) -> Result<()> {
        // Apple's hardware encoder can expose MaxFrameDelayCount read-only.
        // Bound outstanding submissions ourselves so a stalled callback
        // cannot pin the compositor's entire surface pool.
        if self.output.in_flight.load(Ordering::Relaxed) >= MAX_IN_FLIGHT {
            if let Some(priority) = &mut self.priority {
                priority.set(true);
            }
            if !self.low_latency {
                let frames = self.output.frames.lock().unwrap();
                let _ = self
                    .output
                    .ready
                    .wait_timeout_while(frames, STALL_WAIT, |_| {
                        self.output.in_flight.load(Ordering::Relaxed) >= MAX_IN_FLIGHT
                    })
                    .unwrap();
            }
            if self.output.in_flight.load(Ordering::Relaxed) >= MAX_IN_FLIGHT {
                self.output.dropped.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
        }
        let (time, duration) = unsafe {
            (
                CMTime::new(pts.as_nanos(), TIMESCALE),
                CMTime::new(1, self.fps as i32),
            )
        };
        let options = keyframe.then(|| self.force_keyframe.as_opaque());
        let first = !self.started;
        if first {
            // The first frame starts the hardware session: in its turn, not
            // within a few milliseconds of another session's start.
            let (width, height) = self.size;
            let ticket = *self.start_ticket.get_or_insert_with(|| {
                request_encoder_start(encoder_start_rank(false, width, height))
            });
            while !claim_encoder_start(ticket) {
                thread::sleep(Duration::from_millis(2));
            }
            self.started = true;
        }
        self.output.in_flight.fetch_add(1, Ordering::Relaxed);
        let submitted = Instant::now();
        let status = unsafe {
            self.session.encode_frame(
                &frame.0,
                time,
                duration,
                options,
                monotonic_ns() as usize as *mut c_void,
                ptr::null_mut(),
            )
        };
        if let Some(priority) = &mut self.priority {
            // VideoToolbox can wait inside encode_frame before our submission
            // window fills (seven in flight on M2). Without this feedback the
            // stream's tap pins the compositor's pool, dropping both protected
            // outputs while the ISOs keep running. The first picture also
            // initializes the session, even with spare capacity: that is not
            // pressure. Keep pressure until a later submission fits its budget.
            priority.set(!first && submitted.elapsed() >= Duration::from_secs(1) / self.fps);
        }
        if status != 0 {
            self.output.in_flight.fetch_sub(1, Ordering::Relaxed);
            self.output.last_os_status.store(status, Ordering::Relaxed);
            bail!("the H.264 encoder refused a frame (error {status})");
        }
        Ok(())
    }

    fn receive(&mut self) -> Result<Option<EncodedVideo>> {
        self.output.frames.lock().unwrap().pop_front().transpose()
    }

    fn receive_timeout(&mut self, timeout: Duration) -> Result<Option<EncodedVideo>> {
        let frames = self.output.frames.lock().unwrap();
        let (mut frames, _) = self
            .output
            .ready
            .wait_timeout_while(frames, timeout, |frames| frames.is_empty())
            .unwrap();
        frames.pop_front().transpose()
    }

    fn dropped(&self) -> u64 {
        self.output.dropped.load(Ordering::Relaxed)
    }

    fn set_rate(&mut self, fps: u32, bitrate_kbps: u32) -> Result<()> {
        let set = |key: &CFString, value: &CFType| -> Result<()> {
            let status = unsafe { VTSessionSetProperty(&self.session, key, Some(value)) };
            if status != 0 {
                bail!("the H.264 encoder refused {key} (error {status})");
            }
            Ok(())
        };
        unsafe {
            set(
                kVTCompressionPropertyKey_ExpectedFrameRate,
                &CFNumber::new_i32(fps as i32),
            )?;
            set(
                kVTCompressionPropertyKey_AverageBitRate,
                &CFNumber::new_i64(i64::from(bitrate_kbps) * 1000),
            )?;
        }
        self.fps = fps;
        Ok(())
    }
}

/// Create a hardware H.264 session for `spec` delivering to `output`,
/// in VideoToolbox's low-latency mode if asked. Returns its status.
unsafe fn create_session(
    spec: &LiveVideoSpec,
    output: &Arc<Output>,
    low_latency: bool,
    session: &mut *mut VTCompressionSession,
) -> i32 {
    // Only the hardware encoder: when it is busy, VideoToolbox would
    // otherwise fall back to a software encoder that takes most of the
    // Mac's cores and starves everything else.
    let mut keys = unsafe {
        vec![
            kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
            kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
        ]
    };
    if low_latency {
        keys.push(unsafe { kVTVideoEncoderSpecification_EnableLowLatencyRateControl });
    }
    let values = vec![CFBoolean::new(true); keys.len()];
    let specification = CFDictionary::<CFString, CFBoolean>::from_slices(&keys, &values);
    unsafe {
        VTCompressionSession::create(
            None,
            spec.width as i32,
            spec.height as i32,
            kCMVideoCodecType_H264,
            Some(specification.as_opaque()),
            None,
            None,
            Some(encoded),
            Arc::as_ptr(output) as *mut c_void,
            NonNull::from(session),
        )
    }
}

/// The session's output callback, on a VideoToolbox thread.
unsafe extern "C-unwind" fn encoded(
    refcon: *mut c_void,
    frame: *mut c_void,
    status: i32,
    flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    // SAFETY: the refcon is the encoder's `Output`, alive until the session
    // is invalidated.
    let output = unsafe { &*(refcon as *const Output) };
    output.latency_ns.store(
        monotonic_ns().saturating_sub(frame as usize as u64),
        Ordering::Relaxed,
    );
    output.callbacks.fetch_add(1, Ordering::Relaxed);
    if status != 0 {
        output.last_os_status.store(status, Ordering::Relaxed);
    }
    let result = match unsafe { sample.as_ref() } {
        _ if status != 0 => Err(anyhow!("the H.264 encoder failed (error {status})")),
        None => {
            let _frames = output.frames.lock().unwrap();
            output.dropped.fetch_add(1, Ordering::Relaxed);
            output.in_flight.fetch_sub(1, Ordering::Relaxed);
            output.ready.notify_all();
            return;
        }
        Some(_) if flags.contains(VTEncodeInfoFlags::FrameDropped) => {
            let _frames = output.frames.lock().unwrap();
            output.dropped.fetch_add(1, Ordering::Relaxed);
            output.in_flight.fetch_sub(1, Ordering::Relaxed);
            output.ready.notify_all();
            return;
        }
        Some(sample) => unsafe { encoded_frame(sample) },
    };
    let mut frames = output.frames.lock().unwrap();
    frames.push_back(result);
    output.in_flight.fetch_sub(1, Ordering::Relaxed);
    drop(frames);
    output.ready.notify_all();
}

/// A compressed frame's bytes, times, keyframe flag and parameter sets.
unsafe fn encoded_frame(sample: &CMSampleBuffer) -> Result<EncodedVideo> {
    let block = unsafe { sample.data_buffer() }.context("an encoded frame without data")?;
    let len = unsafe { block.data_length() };
    let mut data = vec![0u8; len];
    if len > 0 {
        let status = unsafe {
            block.copy_data_bytes(0, len, NonNull::new(data.as_mut_ptr().cast()).expect("vec"))
        };
        if status != 0 {
            bail!("could not read an encoded frame (CoreMedia error {status})");
        }
    }
    let pts = unsafe { sample.presentation_time_stamp() };
    let dts = unsafe { sample.decode_time_stamp() };
    let valid = |t: &CMTime| t.flags.contains(CMTimeFlags::Valid);
    let pts = from_cm_time(pts);
    let dts = if valid(&dts) { from_cm_time(dts) } else { pts };
    let keyframe = unsafe { is_sync(sample) };
    let parameter_sets = match keyframe {
        true => unsafe { sample.format_description() }
            .map(|format| unsafe { parameter_sets(&format) })
            .transpose()?,
        false => None,
    };
    Ok(EncodedVideo {
        data,
        pts,
        dts,
        keyframe,
        parameter_sets,
    })
}

/// A sample is a keyframe unless its attachments say "not sync".
pub(crate) unsafe fn is_sync(sample: &CMSampleBuffer) -> bool {
    let Some(attachments) = (unsafe { sample.sample_attachments_array(false) }) else {
        return true;
    };
    if attachments.count() == 0 {
        return true;
    }
    let first = unsafe { attachments.value_at_index(0) } as *const CFDictionary;
    let Some(first) = (unsafe { first.as_ref() }) else {
        return true;
    };
    let key: *const CFString = unsafe { kCMSampleAttachmentKey_NotSync };
    let value = unsafe { first.value(key.cast()) } as *const CFBoolean;
    match unsafe { value.as_ref() } {
        Some(not_sync) => !not_sync.as_bool(),
        None => true,
    }
}

/// The SPS and PPS NAL units of an H.264 format description.
unsafe fn parameter_sets(format: &CMFormatDescription) -> Result<ParameterSets> {
    let mut count = 0usize;
    let mut length_size = 0i32;
    let status = unsafe {
        CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            format,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut count,
            &mut length_size,
        )
    };
    if status != 0 {
        bail!("the encoder's format has no H.264 parameter sets (CoreMedia error {status})");
    }
    if length_size != 4 {
        bail!("the encoder used {length_size}-byte NAL lengths; streaming expects 4");
    }
    let mut sets = ParameterSets::default();
    for index in 0..count {
        let mut pointer = ptr::null();
        let mut size = 0usize;
        let status = unsafe {
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                format,
                index,
                &mut pointer,
                &mut size,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if status != 0 || pointer.is_null() || size == 0 {
            continue;
        }
        let nal = unsafe { std::slice::from_raw_parts(pointer, size) }.to_vec();
        match nal[0] & 0x1f {
            7 => sets.sps.push(nal),
            8 => sets.pps.push(nal),
            _ => {}
        }
    }
    Ok(sets)
}

/// Samples per channel in one AAC frame.
const AAC_FRAME: usize = 1024;
/// The input callback's "nothing more right now", which ends a fill early
/// without ending the stream.
const NO_MORE_INPUT: i32 = 0x6d6f7265; // 'more'

/// An AAC-LC encoder: 48 kHz interleaved floats in, raw 1024-sample AAC
/// frames out.
pub struct AacEncoder {
    converter: AudioConverterRef,
    channels: usize,
    /// Samples given to `encode` and not yet handed to the converter.
    pending: Vec<f32>,
    /// What the converter is reading; it may keep reading it until it
    /// next asks for input, so it stays put until then.
    lent: Vec<f32>,
    max_packet: usize,
    delay: u32,
}

// SAFETY: an AudioConverter may be used from any thread, one at a time.
unsafe impl Send for AacEncoder {}

impl AacEncoder {
    pub fn new(spec: &LiveAudioSpec) -> Result<Self> {
        let channels = spec.channels.clamp(1, 2);
        let input = AudioStreamBasicDescription {
            mSampleRate: f64::from(SAMPLE_RATE),
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
            mBytesPerPacket: 4 * channels,
            mFramesPerPacket: 1,
            mBytesPerFrame: 4 * channels,
            mChannelsPerFrame: channels,
            mBitsPerChannel: 32,
            mReserved: 0,
        };
        let aac = AudioStreamBasicDescription {
            mSampleRate: f64::from(SAMPLE_RATE),
            mFormatID: kAudioFormatMPEG4AAC,
            mFormatFlags: MPEG4ObjectID::AAC_LC.0 as u32,
            mBytesPerPacket: 0,
            mFramesPerPacket: AAC_FRAME as u32,
            mBytesPerFrame: 0,
            mChannelsPerFrame: channels,
            mBitsPerChannel: 0,
            mReserved: 0,
        };
        let mut converter: AudioConverterRef = ptr::null_mut();
        let status = unsafe {
            AudioConverterNew(
                NonNull::from(&input),
                NonNull::from(&aac),
                NonNull::from(&mut converter),
            )
        };
        if status != 0 || converter.is_null() {
            bail!("AudioToolbox could not start an AAC encoder (error {status})");
        }
        // Owned from here, so errors below dispose of it.
        let mut encoder = Self {
            converter,
            channels: channels as usize,
            pending: Vec::new(),
            lent: Vec::with_capacity(AAC_FRAME * channels as usize),
            max_packet: 0,
            delay: 0,
        };
        // Constant bitrate, as streaming services ask. In its default mode
        // the encoder spends only what the sound needs (~2 kb/s for
        // silence, ~20 for near silence at a 160 kb/s target), and ingests
        // warn that the audio bitrate is far below what they recommend.
        // Set before the bitrate, whose valid values depend on the mode.
        let mode = kAudioCodecBitRateControlMode_Constant;
        let status = unsafe {
            AudioConverterSetProperty(
                converter,
                kAudioCodecPropertyBitRateControlMode,
                size_of::<u32>() as u32,
                NonNull::from(&mode).cast(),
            )
        };
        if status != 0 {
            bail!("the AAC encoder has no constant bitrate mode (error {status})");
        }
        let bitrate = spec.bitrate_kbps * 1000;
        let status = unsafe {
            AudioConverterSetProperty(
                converter,
                kAudioConverterEncodeBitRate,
                size_of::<u32>() as u32,
                NonNull::from(&bitrate).cast(),
            )
        };
        if status != 0 {
            bail!(
                "the AAC encoder doesn't offer {} kb/s (error {status})",
                spec.bitrate_kbps
            );
        }
        let max_packet: u32 =
            unsafe { get_property(converter, kAudioConverterPropertyMaximumOutputPacketSize) }
                .context("could not ask the AAC encoder its largest frame")?;
        encoder.max_packet = max_packet.max(1) as usize;
        // Apple's AAC encoder starts with priming samples; the engine
        // shifts frame times back by them so audio and video line up.
        let prime: AudioConverterPrimeInfo =
            unsafe { get_property(converter, kAudioConverterPrimeInfo) }.unwrap_or(
                AudioConverterPrimeInfo {
                    leadingFrames: 2112,
                    trailingFrames: 0,
                },
            );
        encoder.delay = prime.leadingFrames;
        Ok(encoder)
    }
}

impl Drop for AacEncoder {
    fn drop(&mut self) {
        unsafe { AudioConverterDispose(self.converter) };
    }
}

/// What the input callback reads from.
struct Feed<'a> {
    pending: &'a [f32],
    /// Frames of `pending` handed over so far.
    taken: usize,
    lent: &'a mut Vec<f32>,
    channels: usize,
}

impl AudioEncoder for AacEncoder {
    fn encode(&mut self, samples: &[f32]) -> Result<Vec<Vec<u8>>> {
        self.pending.extend_from_slice(samples);
        let mut frames = Vec::new();
        let mut buffer = vec![0u8; self.max_packet];
        while self.pending.len() >= AAC_FRAME * self.channels {
            let mut feed = Feed {
                pending: &self.pending,
                taken: 0,
                lent: &mut self.lent,
                channels: self.channels,
            };
            let mut list = AudioBufferList {
                mNumberBuffers: 1,
                mBuffers: [AudioBuffer {
                    mNumberChannels: self.channels as u32,
                    mDataByteSize: buffer.len() as u32,
                    mData: buffer.as_mut_ptr().cast(),
                }],
            };
            let mut packets: u32 = 1;
            let mut description = AudioStreamPacketDescription {
                mStartOffset: 0,
                mVariableFramesInPacket: 0,
                mDataByteSize: 0,
            };
            let status = unsafe {
                AudioConverterFillComplexBuffer(
                    self.converter,
                    Some(supply),
                    (&mut feed as *mut Feed).cast(),
                    NonNull::from(&mut packets),
                    NonNull::from(&mut list),
                    &mut description,
                )
            };
            let taken = feed.taken;
            if status != 0 && status != NO_MORE_INPUT {
                bail!("the AAC encoder failed (error {status})");
            }
            self.pending.drain(..taken * self.channels);
            if packets > 0 {
                let size = list.mBuffers[0].mDataByteSize as usize;
                frames.push(buffer[..size.min(buffer.len())].to_vec());
            } else if taken == 0 {
                break;
            }
        }
        Ok(frames)
    }

    fn delay(&self) -> u32 {
        self.delay
    }
}

/// The converter's input callback: up to the frames it asks for from the
/// pending samples, copied to `lent` where they stay until the next call.
unsafe extern "C-unwind" fn supply(
    _converter: AudioConverterRef,
    count: NonNull<u32>,
    data: NonNull<AudioBufferList>,
    _descriptions: *mut *mut AudioStreamPacketDescription,
    user: *mut c_void,
) -> i32 {
    let feed = unsafe { &mut *(user as *mut Feed) };
    let available = feed.pending.len() / feed.channels - feed.taken;
    let wanted = unsafe { *count.as_ptr() } as usize;
    let n = wanted.min(available);
    let list = unsafe { &mut *data.as_ptr() };
    if n == 0 {
        unsafe { *count.as_ptr() = 0 };
        list.mBuffers[0].mDataByteSize = 0;
        list.mBuffers[0].mData = ptr::null_mut();
        return NO_MORE_INPUT;
    }
    let from = feed.taken * feed.channels;
    feed.lent.clear();
    feed.lent
        .extend_from_slice(&feed.pending[from..from + n * feed.channels]);
    feed.taken += n;
    list.mNumberBuffers = 1;
    list.mBuffers[0].mNumberChannels = feed.channels as u32;
    list.mBuffers[0].mDataByteSize = (n * feed.channels * 4) as u32;
    list.mBuffers[0].mData = feed.lent.as_mut_ptr().cast();
    unsafe { *count.as_ptr() = n as u32 };
    0
}

/// A fixed-size AudioConverter property.
unsafe fn get_property<T: Copy>(converter: AudioConverterRef, id: u32) -> Result<T> {
    let mut value = std::mem::MaybeUninit::<T>::zeroed();
    let mut size = size_of::<T>() as u32;
    let status = unsafe {
        AudioConverterGetProperty(
            converter,
            id,
            NonNull::from(&mut size),
            NonNull::new(value.as_mut_ptr().cast()).expect("pointer"),
        )
    };
    if status != 0 {
        bail!("AudioToolbox error {status}");
    }
    Ok(unsafe { value.assume_init() })
}
