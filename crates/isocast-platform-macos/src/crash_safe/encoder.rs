//! The recording video encoder: a VTCompressionSession with the same
//! settings `writer.rs` gives AVAssetWriter (constant quality, a keyframe
//! every second, no reordering, hardware H.264/HEVC, Rec. 709 tags), whose
//! compressed samples the crash-safe writer puts into fragments itself.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use isocast_engine::VideoTrackSpec;
use isocast_protocol::Codec;
use objc2_core_foundation::{
    CFBoolean, CFData, CFDictionary, CFNumber, CFRetained, CFString, CFType,
};
use objc2_core_media::{
    CMFormatDescription, CMSampleBuffer, CMTime, CMTimeFlags, CMVideoCodecType,
    CMVideoFormatDescriptionGetDimensions, kCMFormatDescriptionExtension_Depth,
    kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms,
    kCMVideoCodecType_AppleProRes422, kCMVideoCodecType_AppleProRes422HQ,
    kCMVideoCodecType_AppleProRes422LT, kCMVideoCodecType_AppleProRes422Proxy,
    kCMVideoCodecType_AppleProRes4444, kCMVideoCodecType_H264, kCMVideoCodecType_HEVC,
};
use objc2_core_video::{
    kCVImageBufferColorPrimaries_ITU_R_709_2, kCVImageBufferTransferFunction_ITU_R_709_2,
    kCVImageBufferYCbCrMatrix_ITU_R_709_2,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, VTSessionSetProperty,
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_ColorPrimaries,
    kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
    kVTCompressionPropertyKey_PixelTransferProperties, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_Quality, kVTCompressionPropertyKey_RealTime,
    kVTCompressionPropertyKey_TransferFunction, kVTCompressionPropertyKey_YCbCrMatrix,
    kVTPixelTransferPropertyKey_ScalingMode, kVTProfileLevel_HEVC_Main10_AutoLevel,
    kVTScalingMode_Letterbox, kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
};

use crate::PixelBuffer;

/// Keyframe at least every second, as the AVAssetWriter path asks.
const KEYFRAME_INTERVAL_SECONDS: f64 = 1.0;

/// One compressed picture, as the encoder delivered it.
pub struct Encoded {
    /// Length-prefixed NAL units (H.264/HEVC) or the ProRes frame: the
    /// sample's bytes exactly as a movie stores them.
    pub data: Vec<u8>,
    /// Presentation time in the session's timescale.
    pub time: i64,
    pub sync: bool,
    pub format: CFRetained<CMFormatDescription>,
}

#[derive(Default)]
struct Output {
    frames: Mutex<VecDeque<Result<Encoded>>>,
    ready: Condvar,
    in_flight: AtomicU64,
    dropped: AtomicU64,
}

pub struct VideoSession {
    session: CFRetained<VTCompressionSession>,
    output: Arc<Output>,
    timescale: i32,
    frame_duration: CMTime,
}

// SAFETY: VideoToolbox sessions may be used from any thread; this one is
// used by one thread at a time.
unsafe impl Send for VideoSession {}

impl VideoSession {
    pub fn new(spec: &VideoTrackSpec, timescale: i32) -> Result<Self> {
        let output = Arc::new(Output::default());
        let hardware_only = matches!(spec.encoder.codec, Codec::H264 | Codec::Hevc);
        let mut keys = Vec::new();
        if hardware_only {
            keys.push(unsafe {
                kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder
            });
            keys.push(unsafe {
                kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder
            });
        }
        let values = vec![CFBoolean::new(true); keys.len()];
        let specification = CFDictionary::<CFString, CFBoolean>::from_slices(&keys, &values);
        let mut session = ptr::null_mut();
        let status = unsafe {
            VTCompressionSession::create(
                None,
                spec.width as i32,
                spec.height as i32,
                codec_type(spec.encoder.codec),
                (!keys.is_empty()).then(|| specification.as_opaque()),
                None,
                None,
                Some(encoded),
                Arc::as_ptr(&output) as *mut c_void,
                NonNull::from(&mut session),
            )
        };
        let Some(session) = NonNull::new(session).filter(|_| status == 0) else {
            if status == -12908 {
                bail!(
                    "no hardware {:?} encoder is free: this Mac's video encoders are busy with \
                     other recordings or streams; stop one, or record fewer ISO files",
                    spec.encoder.codec
                );
            }
            bail!(
                "VideoToolbox could not start a {:?} recording encoder for {}×{} (error {status})",
                spec.encoder.codec,
                spec.width,
                spec.height
            );
        };
        let session = unsafe { CFRetained::from_raw(session) };
        let set = |key: &CFString, value: &CFType| -> Result<()> {
            let status = unsafe { VTSessionSetProperty(&session, key, Some(value)) };
            if status != 0 {
                bail!("the recording encoder refused {key} (error {status})");
            }
            Ok(())
        };
        unsafe {
            set(kVTCompressionPropertyKey_RealTime, CFBoolean::new(true))?;
            set(
                kVTCompressionPropertyKey_ExpectedFrameRate,
                &CFNumber::new_i32(spec.fps as i32),
            )?;
            // Frames of another size (the program drawn at the canvas size
            // for a smaller file) are scaled to fit, keeping their shape.
            let scaling = CFDictionary::<CFString, CFString>::from_slices(
                &[kVTPixelTransferPropertyKey_ScalingMode],
                &[kVTScalingMode_Letterbox],
            );
            let _ = set(
                kVTCompressionPropertyKey_PixelTransferProperties,
                scaling.as_opaque(),
            );
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
            if hardware_only {
                set(
                    kVTCompressionPropertyKey_Quality,
                    &CFNumber::new_f64(f64::from(spec.encoder.quality.value()) / 100.0),
                )?;
                set(
                    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
                    &CFNumber::new_f64(KEYFRAME_INTERVAL_SECONDS),
                )?;
                // Presentation order is decode order: recoverable fragments
                // never hold pictures out of order (see writer.rs).
                set(
                    kVTCompressionPropertyKey_AllowFrameReordering,
                    CFBoolean::new(false),
                )?;
                if spec.encoder.codec == Codec::Hevc
                    && objc2::available!(macos = 15.0)
                    && crate::system::sysctl_string("machdep.cpu.brand_string").as_deref()
                        == Some("Apple M4 Max")
                {
                    let _ = set(
                        &CFString::from_str("LookAheadFrames"),
                        &CFNumber::new_i32(0),
                    );
                }
                if spec.encoder.codec == Codec::Hevc && spec.encoder.effective_bit_depth() == 10 {
                    set(
                        kVTCompressionPropertyKey_ProfileLevel,
                        kVTProfileLevel_HEVC_Main10_AutoLevel,
                    )?;
                }
            }
            let status = session.prepare_to_encode_frames();
            if status != 0 {
                bail!("the recording encoder could not prepare (error {status})");
            }
        }
        Ok(Self {
            session,
            output,
            timescale,
            frame_duration: unsafe {
                CMTime::new(i64::from(timescale) / i64::from(spec.fps), timescale)
            },
        })
    }

    pub fn in_flight(&self) -> u64 {
        self.output.in_flight.load(Ordering::Relaxed)
    }

    /// Frames the encoder itself discarded (a null or FrameDropped
    /// callback): accepted by `encode`, never delivered.
    pub fn dropped(&self) -> u64 {
        self.output.dropped.load(Ordering::Relaxed)
    }

    /// Submit a frame at `ticks` of the session timescale.
    pub fn encode(&mut self, frame: &PixelBuffer, ticks: i64) -> Result<()> {
        let time = unsafe { CMTime::new(ticks, self.timescale) };
        self.output.in_flight.fetch_add(1, Ordering::Relaxed);
        let status = unsafe {
            self.session.encode_frame(
                &frame.0,
                time,
                self.frame_duration,
                None,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if status != 0 {
            self.output.in_flight.fetch_sub(1, Ordering::Relaxed);
            bail!("the recording encoder refused a frame (error {status})");
        }
        Ok(())
    }

    /// Everything the encoder has delivered so far, without waiting.
    pub fn drain(&mut self) -> Result<Vec<Encoded>> {
        let mut frames = self
            .output
            .frames
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        frames.drain(..).collect()
    }

    /// Force every submitted frame out, then take them all.
    pub fn complete(&mut self) -> Result<Vec<Encoded>> {
        let invalid = CMTime {
            value: 0,
            timescale: 0,
            flags: CMTimeFlags::empty(),
            epoch: 0,
        };
        let status = unsafe { self.session.complete_frames(invalid) };
        if status != 0 {
            bail!("the recording encoder could not complete its frames (error {status})");
        }
        // complete_frames returns once every callback ran.
        self.drain()
    }
}

impl Drop for VideoSession {
    fn drop(&mut self) {
        unsafe { self.session.invalidate() };
    }
}

fn codec_type(codec: Codec) -> CMVideoCodecType {
    match codec {
        Codec::H264 => kCMVideoCodecType_H264,
        Codec::Hevc => kCMVideoCodecType_HEVC,
        Codec::Prores422Proxy => kCMVideoCodecType_AppleProRes422Proxy,
        Codec::Prores422Lt => kCMVideoCodecType_AppleProRes422LT,
        Codec::Prores422 => kCMVideoCodecType_AppleProRes422,
        Codec::Prores422Hq => kCMVideoCodecType_AppleProRes422HQ,
        Codec::Prores4444 => kCMVideoCodecType_AppleProRes4444,
    }
}

/// The session's output callback, on a VideoToolbox thread.
unsafe extern "C-unwind" fn encoded(
    refcon: *mut c_void,
    _frame: *mut c_void,
    status: i32,
    flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    // SAFETY: the refcon is the session's `Output`, alive until the session
    // is invalidated.
    let output = unsafe { &*(refcon as *const Output) };
    let result = match unsafe { sample.as_ref() } {
        _ if status != 0 => Err(anyhow!("the recording encoder failed (error {status})")),
        None => {
            output.dropped.fetch_add(1, Ordering::Relaxed);
            output.in_flight.fetch_sub(1, Ordering::Relaxed);
            output.ready.notify_all();
            return;
        }
        Some(_) if flags.contains(VTEncodeInfoFlags::FrameDropped) => {
            output.dropped.fetch_add(1, Ordering::Relaxed);
            output.in_flight.fetch_sub(1, Ordering::Relaxed);
            output.ready.notify_all();
            return;
        }
        Some(sample) => unsafe { encoded_frame(sample) },
    };
    // Never unwind out of a VideoToolbox callback: a poisoned queue is
    // still a queue.
    let mut frames = output
        .frames
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    frames.push_back(result);
    output.in_flight.fetch_sub(1, Ordering::Relaxed);
    drop(frames);
    output.ready.notify_all();
}

unsafe fn encoded_frame(sample: &CMSampleBuffer) -> Result<Encoded> {
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
    if !pts.flags.contains(CMTimeFlags::Valid) {
        bail!("an encoded frame without a presentation time");
    }
    let format =
        unsafe { sample.format_description() }.context("an encoded frame without a format")?;
    Ok(Encoded {
        data,
        time: pts.value,
        sync: unsafe { crate::stream::is_sync(sample) },
        format,
    })
}

/// An untyped CoreFoundation dictionary as one of CF objects (the same
/// pointer; only the element types differ).
fn typed(dictionary: &CFDictionary) -> &CFDictionary<CFType, CFType> {
    // SAFETY: CFDictionary<K, V> is the same opaque CFDictionaryRef for
    // any K and V; CFType is every CF object's base.
    unsafe { &*(dictionary as *const CFDictionary as *const CFDictionary<CFType, CFType>) }
}

/// What a movie's sample entry needs from the encoder's format description:
/// the codec four-character code, the encoded size, the depth and the
/// codec's own atoms (`avcC`, `hvcC`, ProRes tags).
pub struct SampleFormat {
    pub fourcc: [u8; 4],
    pub width: u16,
    pub height: u16,
    pub depth: u16,
    pub atoms: Vec<([u8; 4], Vec<u8>)>,
}

pub fn sample_format(format: &CMFormatDescription) -> Result<SampleFormat> {
    let fourcc = unsafe { format.media_sub_type() }.to_be_bytes();
    let dimensions = unsafe { CMVideoFormatDescriptionGetDimensions(format) };
    let mut atoms = Vec::new();
    let mut depth = 24;
    if let Some(extensions) = unsafe { format.extensions() } {
        let (keys, values) = typed(&extensions).to_vecs();
        for (key, value) in keys.iter().zip(&values) {
            let Some(name) = key.downcast_ref::<CFString>() else {
                continue;
            };
            let name = name.to_string();
            if name
                == unsafe { kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms }
                    .to_string()
                && let Some(dictionary) = value.downcast_ref::<CFDictionary>()
            {
                let (atom_keys, atom_values) = typed(dictionary).to_vecs();
                for (atom, data) in atom_keys.iter().zip(&atom_values) {
                    let (Some(atom), Some(data)) = (
                        atom.downcast_ref::<CFString>(),
                        data.downcast_ref::<CFData>(),
                    ) else {
                        continue;
                    };
                    let atom = atom.to_string();
                    if atom.len() == 4 {
                        atoms.push((atom.as_bytes().try_into().unwrap(), data.to_vec()));
                    }
                }
            } else if name == unsafe { kCMFormatDescriptionExtension_Depth }.to_string()
                && let Some(number) = value.downcast_ref::<CFNumber>()
                && let Some(value) = number.as_i64()
            {
                depth = value.clamp(1, 64) as u16;
            }
        }
    }
    // Keep the codec configuration first, as Apple writes it.
    atoms.sort_by_key(|(kind, _)| !matches!(kind, b"avcC" | b"hvcC"));
    if atoms.iter().all(|(k, _)| k != b"avcC" && k != b"hvcC")
        && matches!(&fourcc, b"avc1" | b"hvc1")
    {
        bail!("the encoder's format carries no parameter sets");
    }
    Ok(SampleFormat {
        fourcc,
        width: dimensions.width.clamp(0, i32::from(u16::MAX)) as u16,
        height: dimensions.height.clamp(0, i32::from(u16::MAX)) as u16,
        depth,
        atoms,
    })
}
