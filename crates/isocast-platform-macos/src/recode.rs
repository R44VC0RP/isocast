//! Decoding and encoding again the few frames an edited export can't copy
//! (C.3, see `join`): the frames of a keyframe interval after a cut that
//! isn't on a keyframe, and whole parts whose file is in another format
//! than the joined file (an ISO angle in the edited file). VideoToolbox
//! decodes compressed samples as AVAssetReader passes them through, and
//! encodes with the joined file's codec at a high quality, every frame in
//! presentation order, so the encoded samples join the copied ones.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::Mutex;

use anyhow::{Result, anyhow, bail};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMFormatDescription, CMSampleBuffer, CMTime, CMVideoCodecType, kCMSampleAttachmentKey_NotSync,
    kCMTimeInvalid, kCMVideoCodecType_H264, kCMVideoCodecType_HEVC,
};
use objc2_core_video::{
    CVImageBuffer, CVPixelBuffer, CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType,
    CVPixelBufferGetWidth, kCVImageBufferColorPrimaries_ITU_R_709_2,
    kCVImageBufferTransferFunction_ITU_R_709_2, kCVImageBufferYCbCrMatrix_ITU_R_709_2,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelFormatType_420YpCbCr10BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTDecodeFrameFlags, VTDecodeInfoFlags,
    VTDecompressionOutputCallbackRecord, VTDecompressionSession, VTEncodeInfoFlags,
    VTPixelTransferSession, VTSessionSetProperty, kVTCompressionPropertyKey_AllowFrameReordering,
    kVTCompressionPropertyKey_ColorPrimaries, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_Quality, kVTCompressionPropertyKey_RealTime,
    kVTCompressionPropertyKey_TransferFunction, kVTCompressionPropertyKey_YCbCrMatrix,
    kVTEncodeFrameOptionKey_ForceKeyFrame, kVTPixelTransferPropertyKey_ScalingMode,
    kVTProfileLevel_H264_High_AutoLevel, kVTProfileLevel_HEVC_Main_AutoLevel,
    kVTProfileLevel_HEVC_Main10_AutoLevel, kVTScalingMode_Letterbox,
    kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
};

use crate::frames::{FramePool, PixelBuffer};
use isocast_engine::FramePool as _;

/// The quality H.264 and HEVC frames are encoded again at (VideoToolbox's
/// 0 to 1): above what recordings use, so a re-encoded keyframe interval
/// looks like the copied frames around it.
const QUALITY: f64 = 0.85;
/// The longest keyframe interval of encoded frames, as recordings have.
const KEYFRAME_SECONDS: f64 = 1.0;

/// A sample is a keyframe unless its attachments say "not sync".
pub(crate) fn is_sync(sample: &CMSampleBuffer) -> bool {
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

/// What a decompression session gave back, filled by its callback.
#[derive(Default)]
struct Decoded {
    frames: Vec<(CFRetained<CVPixelBuffer>, CMTime)>,
    error: Option<i32>,
}

unsafe extern "C-unwind" fn on_decoded(
    refcon: *mut c_void,
    _frame: *mut c_void,
    status: i32,
    _flags: VTDecodeInfoFlags,
    image: *mut CVImageBuffer,
    pts: CMTime,
    _duration: CMTime,
) {
    // SAFETY: the refcon is the decoder's boxed `Decoded`, which outlives
    // its session.
    let decoded = unsafe { &*(refcon as *const Mutex<Decoded>) };
    let mut decoded = decoded.lock().unwrap();
    if status != 0 {
        decoded.error = Some(status);
        return;
    }
    if let Some(image) = NonNull::new(image) {
        decoded
            .frames
            .push((unsafe { CFRetained::retain(image.cast()) }, pts));
    }
}

/// Decodes one file's compressed video samples, in decode order.
pub(crate) struct Decoder {
    session: CFRetained<VTDecompressionSession>,
    // Boxed so the callback's pointer to it stays put.
    decoded: Box<Mutex<Decoded>>,
}

impl Decoder {
    pub fn new(format: &CMFormatDescription) -> Result<Self> {
        let io_surface = CFDictionary::<CFString, CFType>::empty();
        let attributes = unsafe {
            CFDictionary::<CFString, CFType>::from_slices(
                &[kCVPixelBufferIOSurfacePropertiesKey],
                &[&*io_surface],
            )
        };
        let decoded = Box::new(Mutex::new(Decoded::default()));
        let callback = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: Some(on_decoded),
            decompressionOutputRefCon: &*decoded as *const Mutex<Decoded> as *mut c_void,
        };
        let mut session = ptr::null_mut();
        let status = unsafe {
            VTDecompressionSession::create(
                None,
                format,
                None,
                Some(attributes.as_opaque()),
                &callback,
                NonNull::from(&mut session),
            )
        };
        let session = NonNull::new(session)
            .filter(|_| status == 0)
            .ok_or_else(|| anyhow!("VideoToolbox could not start a decoder (error {status})"))?;
        Ok(Self {
            session: unsafe { CFRetained::from_raw(session) },
            decoded,
        })
    }

    /// Whether this decoder can decode samples of `format`.
    pub fn accepts(&self, format: &CMFormatDescription) -> bool {
        unsafe { self.session.can_accept_format_description(format) }
    }

    /// Decode `sample`: the pictures it produced, with their times.
    pub fn decode(
        &mut self,
        sample: &CMSampleBuffer,
    ) -> Result<Vec<(CFRetained<CVPixelBuffer>, CMTime)>> {
        let mut info = VTDecodeInfoFlags(0);
        let status = unsafe {
            self.session
                .decode_frame(sample, VTDecodeFrameFlags(0), ptr::null_mut(), &mut info)
        };
        unsafe { self.session.wait_for_asynchronous_frames() };
        let mut decoded = self.decoded.lock().unwrap();
        let frames = std::mem::take(&mut decoded.frames);
        if let Some(error) = decoded.error.take() {
            bail!("VideoToolbox could not decode a frame (error {error})");
        }
        if status != 0 {
            bail!("VideoToolbox could not decode a frame (error {status})");
        }
        Ok(frames)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // No callback may run once `decoded` is freed.
        unsafe {
            self.session.wait_for_asynchronous_frames();
            self.session.invalidate();
        }
    }
}

/// What a compression session gave back, filled by its callback.
#[derive(Default)]
struct Encoded {
    samples: VecDeque<CFRetained<CMSampleBuffer>>,
    error: Option<i32>,
    dropped: u64,
}

unsafe extern "C-unwind" fn on_encoded(
    refcon: *mut c_void,
    _frame: *mut c_void,
    status: i32,
    flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    // SAFETY: the refcon is the encoder's boxed `Encoded`, which outlives
    // its session.
    let encoded = unsafe { &*(refcon as *const Mutex<Encoded>) };
    let mut encoded = encoded.lock().unwrap();
    if status != 0 {
        encoded.error = Some(status);
        return;
    }
    match NonNull::new(sample) {
        Some(sample) if !flags.contains(VTEncodeInfoFlags::FrameDropped) => {
            encoded
                .samples
                .push_back(unsafe { CFRetained::retain(sample) });
        }
        _ => encoded.dropped += 1,
    }
}

/// Encodes pictures with the joined file's codec, size and frame rate.
pub(crate) struct Encoder {
    session: CFRetained<VTCompressionSession>,
    encoded: Box<Mutex<Encoded>>,
    force_keyframe: CFRetained<CFDictionary<CFString, CFBoolean>>,
    size: (usize, usize),
    /// It encodes 10-bit HEVC.
    pub ten_bit: bool,
    /// Scales pictures of another size to `size`, keeping their shape.
    scaler: Option<(CFRetained<VTPixelTransferSession>, FramePool)>,
}

// SAFETY: VideoToolbox sessions may be used from any thread, one at a time.
unsafe impl Send for Encoder {}
unsafe impl Send for Decoder {}

impl Encoder {
    /// An encoder for `codec` (the joined file's: H.264, HEVC or ProRes)
    /// at `width`×`height` and `fps`, 10-bit HEVC if `ten_bit`.
    pub fn new(
        codec: CMVideoCodecType,
        (width, height): (u32, u32),
        fps: f64,
        ten_bit: bool,
    ) -> Result<Self> {
        let encoded = Box::new(Mutex::new(Encoded::default()));
        let specification = unsafe {
            CFDictionary::<CFString, CFBoolean>::from_slices(
                &[kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder],
                &[CFBoolean::new(true)],
            )
        };
        let mut session = ptr::null_mut();
        let status = unsafe {
            VTCompressionSession::create(
                None,
                width as i32,
                height as i32,
                codec,
                Some(specification.as_opaque()),
                None,
                None,
                Some(on_encoded),
                &*encoded as *const Mutex<Encoded> as *mut c_void,
                NonNull::from(&mut session),
            )
        };
        let session = NonNull::new(session).filter(|_| status == 0).ok_or_else(|| {
            anyhow!(
                "VideoToolbox could not start an encoder for {width}×{height} {} (error {status})",
                fourcc(codec)
            )
        })?;
        let session = unsafe { CFRetained::<VTCompressionSession>::from_raw(session) };
        let set = |key: &CFString, value: &CFType| -> Result<()> {
            let status = unsafe { VTSessionSetProperty(&session, key, Some(value)) };
            if status != 0 {
                bail!("the encoder refused {key} (error {status})");
            }
            Ok(())
        };
        unsafe {
            set(kVTCompressionPropertyKey_RealTime, CFBoolean::new(false))?;
            set(
                kVTCompressionPropertyKey_ExpectedFrameRate,
                &CFNumber::new_f64(fps),
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
            if codec == kCMVideoCodecType_H264 || codec == kCMVideoCodecType_HEVC {
                // Presentation order, as recordings are, so the encoded
                // frames sit between copied ones without reordering.
                set(
                    kVTCompressionPropertyKey_AllowFrameReordering,
                    CFBoolean::new(false),
                )?;
                set(
                    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
                    &CFNumber::new_f64(KEYFRAME_SECONDS),
                )?;
                set(
                    kVTCompressionPropertyKey_Quality,
                    &CFNumber::new_f64(QUALITY),
                )?;
                let profile = match (codec == kCMVideoCodecType_H264, ten_bit) {
                    (true, _) => kVTProfileLevel_H264_High_AutoLevel,
                    (false, false) => kVTProfileLevel_HEVC_Main_AutoLevel,
                    (false, true) => kVTProfileLevel_HEVC_Main10_AutoLevel,
                };
                set(kVTCompressionPropertyKey_ProfileLevel, profile)?;
            }
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
            encoded,
            force_keyframe,
            size: (width as usize, height as usize),
            ten_bit,
            scaler: None,
        })
    }

    /// Encode `picture` (scaled to the encoder's size if it isn't) shown
    /// from `pts` for `duration`; a keyframe if `keyframe`.
    pub fn encode(
        &mut self,
        picture: &CVPixelBuffer,
        pts: CMTime,
        duration: CMTime,
        keyframe: bool,
    ) -> Result<()> {
        let scaled;
        let picture = if (
            CVPixelBufferGetWidth(picture),
            CVPixelBufferGetHeight(picture),
        ) == self.size
        {
            picture
        } else {
            scaled = self.scale(picture)?;
            &*scaled.0
        };
        let properties = keyframe.then(|| self.force_keyframe.as_opaque());
        let mut flags = VTEncodeInfoFlags(0);
        let status = unsafe {
            self.session.encode_frame(
                picture,
                pts,
                duration,
                properties,
                ptr::null_mut(),
                &mut flags,
            )
        };
        if status != 0 {
            bail!("VideoToolbox could not encode a frame (error {status})");
        }
        self.check()
    }

    fn scale(&mut self, picture: &CVPixelBuffer) -> Result<PixelBuffer> {
        if self.scaler.is_none() {
            let mut session = ptr::null_mut();
            let status =
                unsafe { VTPixelTransferSession::create(None, NonNull::from(&mut session)) };
            let session = NonNull::new(session)
                .filter(|_| status == 0)
                .ok_or_else(|| anyhow!("could not start a pixel transfer (error {status})"))?;
            let session = unsafe { CFRetained::<VTPixelTransferSession>::from_raw(session) };
            // Fit, keeping the picture's shape (black bars if it differs).
            unsafe {
                VTSessionSetProperty(
                    &session,
                    kVTPixelTransferPropertyKey_ScalingMode,
                    Some(kVTScalingMode_Letterbox),
                )
            };
            let pool = FramePool::ycbcr(self.size.0 as u32, self.size.1 as u32)?;
            self.scaler = Some((session, pool));
        }
        let mut frame = None;
        for _ in 0..2 {
            let (_, pool) = self.scaler.as_mut().expect("made");
            frame = pool.take()?;
            if frame.is_some() {
                break;
            }
            // Every frame is still with the encoder: let it finish them.
            self.flush()?;
        }
        let frame = frame.ok_or_else(|| anyhow!("no frame to scale into"))?;
        let (session, _) = self.scaler.as_ref().expect("made");
        let status = unsafe { session.transfer_image(picture, &frame.0) };
        if status != 0 {
            bail!("could not scale a frame (VideoToolbox error {status})");
        }
        Ok(frame)
    }

    fn check(&self) -> Result<()> {
        let encoded = self.encoded.lock().unwrap();
        if let Some(error) = encoded.error {
            bail!("VideoToolbox could not encode a frame (error {error})");
        }
        if encoded.dropped > 0 {
            bail!("VideoToolbox dropped {} frames", encoded.dropped);
        }
        Ok(())
    }

    /// Finish every frame given so far.
    pub fn flush(&mut self) -> Result<()> {
        let status = unsafe { self.session.complete_frames(kCMTimeInvalid) };
        if status != 0 {
            bail!("VideoToolbox could not finish encoding (error {status})");
        }
        self.check()
    }

    /// The encoded samples so far, in order.
    pub fn take(&mut self) -> Vec<CFRetained<CMSampleBuffer>> {
        self.encoded.lock().unwrap().samples.drain(..).collect()
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // Finish (or drop) frames in flight first: no callback may run
        // once `encoded` is freed (an export stopped mid-render).
        unsafe {
            self.session.complete_frames(kCMTimeInvalid);
            self.session.invalidate();
        }
    }
}

/// Whether `picture` holds 10-bit samples.
pub(crate) fn ten_bit(picture: &CVPixelBuffer) -> bool {
    let format = CVPixelBufferGetPixelFormatType(picture);
    format == kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange
        || format == kCVPixelFormatType_420YpCbCr10BiPlanarFullRange
}

/// A codec as its four characters, for messages.
pub(crate) fn fourcc(code: u32) -> String {
    code.to_be_bytes()
        .iter()
        .map(|b| match b {
            0x20..=0x7e => *b as char,
            _ => '?',
        })
        .collect()
}
