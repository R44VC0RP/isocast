//! VideoToolbox: H.264 access units to RGBA pictures, decoded on the media
//! engine, synchronously (one picture in, one out: the `frames` stream has
//! no reordered pictures). Pictures are BGRA IOSurfaces: the window's
//! layers show them in place (as their contents, as they show the local
//! engine's frames); a monitor that isn't layers gets them copied out as
//! RGBA pixels.

use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, bail};
use isocast_client::shared::{LocalRing, Surface};
use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer,
    CMVideoFormatDescriptionCreateFromH264ParameterSets, kCMBlockBufferAssureMemoryNowFlag,
};
use objc2_core_video::{
    CVImageBuffer, CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetHeight, CVPixelBufferGetIOSurface, CVPixelBufferGetPixelFormatType,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_32BGRA,
};
use objc2_video_toolbox::{
    VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord,
    VTDecompressionSession,
};

use super::{AccessUnit, Decoder, ParameterSets, Picture};

/// What the session's callback gave back.
#[derive(Default)]
struct Decoded {
    picture: Option<CFRetained<CVPixelBuffer>>,
    error: Option<i32>,
}

unsafe extern "C-unwind" fn on_decoded(
    refcon: *mut c_void,
    _frame: *mut c_void,
    status: i32,
    _flags: VTDecodeInfoFlags,
    image: *mut CVImageBuffer,
    _pts: objc2_core_media::CMTime,
    _duration: objc2_core_media::CMTime,
) {
    // SAFETY: the refcon is the session's boxed `Decoded`, which outlives
    // the session (see `Session::drop`).
    let decoded = unsafe { &*(refcon as *const Mutex<Decoded>) };
    let mut decoded = decoded.lock().unwrap();
    if status != 0 {
        decoded.error = Some(status);
        return;
    }
    if let Some(image) = NonNull::new(image) {
        decoded.picture = Some(unsafe { CFRetained::retain(image.cast()) });
    }
}

struct Session {
    sets: ParameterSets,
    format: CFRetained<CMFormatDescription>,
    session: CFRetained<VTDecompressionSession>,
    decoded: Box<Mutex<Decoded>>,
}

impl Drop for Session {
    fn drop(&mut self) {
        // No callback may run once `decoded` is freed.
        unsafe {
            self.session.wait_for_asynchronous_frames();
            self.session.invalidate();
        }
    }
}

/// A VideoToolbox decoder, started at the first keyframe and again when
/// the parameter sets change (the picture's size did).
#[derive(Default)]
pub struct VideoToolbox {
    session: Option<Session>,
}

// SAFETY: VideoToolbox sessions may be used from any thread, one at a time.
unsafe impl Send for VideoToolbox {}

impl VideoToolbox {
    fn start(sets: &ParameterSets) -> Result<Session> {
        let units: Vec<&Vec<u8>> = sets.sps.iter().chain(&sets.pps).collect();
        let mut pointers: Vec<NonNull<u8>> = units
            .iter()
            .map(|unit| NonNull::new(unit.as_ptr() as *mut u8).unwrap())
            .collect();
        let mut sizes: Vec<usize> = units.iter().map(|unit| unit.len()).collect();
        let mut format: *const CMFormatDescription = ptr::null();
        let status = unsafe {
            CMVideoFormatDescriptionCreateFromH264ParameterSets(
                None,
                units.len(),
                NonNull::new(pointers.as_mut_ptr()).unwrap(),
                NonNull::new(sizes.as_mut_ptr()).unwrap(),
                4,
                NonNull::from(&mut format),
            )
        };
        let format = NonNull::new(format as *mut CMFormatDescription)
            .filter(|_| status == 0)
            .ok_or_else(|| anyhow!("VideoToolbox refused the stream's SPS/PPS (error {status})"))?;
        let format = unsafe { CFRetained::from_raw(format) };
        // BGRA IOSurfaces (VideoToolbox gives no RGBA: error -6680):
        // VideoToolbox converts from 4:2:0 on the GPU, with the colours the
        // stream says.
        let pixels = CFNumber::new_i32(kCVPixelFormatType_32BGRA as i32);
        let surface: CFRetained<CFDictionary<CFString, CFType>> =
            CFDictionary::from_slices(&[], &[]);
        let attributes = unsafe {
            CFDictionary::<CFString, CFType>::from_slices(
                &[
                    kCVPixelBufferPixelFormatTypeKey,
                    kCVPixelBufferIOSurfacePropertiesKey,
                ],
                &[&*pixels, surface.as_ref()],
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
                &format,
                None,
                Some(attributes.as_opaque()),
                &callback,
                NonNull::from(&mut session),
            )
        };
        let session = NonNull::new(session)
            .filter(|_| status == 0)
            .ok_or_else(|| anyhow!("VideoToolbox could not start a decoder (error {status})"))?;
        Ok(Session {
            sets: sets.clone(),
            format,
            session: unsafe { CFRetained::from_raw(session) },
            decoded,
        })
    }
}

impl Decoder for VideoToolbox {
    fn name(&self) -> String {
        "VideoToolbox".into()
    }

    fn decode(
        &mut self,
        sets: &ParameterSets,
        unit: &AccessUnit,
        surfaces: bool,
    ) -> Result<Option<super::Decoded>> {
        if self.session.as_ref().is_none_or(|s| s.sets != *sets) {
            if !unit.keyframe {
                return Ok(None);
            }
            self.session = None;
            self.session = Some(Self::start(sets)?);
        }
        let session = self.session.as_ref().unwrap();
        let sample = sample(&session.format, &unit.data)?;
        let mut info = VTDecodeInfoFlags(0);
        let status = unsafe {
            session
                .session
                .decode_frame(&sample, VTDecodeFrameFlags(0), ptr::null_mut(), &mut info)
        };
        unsafe { session.session.wait_for_asynchronous_frames() };
        let (picture, error) = {
            let mut decoded = session.decoded.lock().unwrap();
            (decoded.picture.take(), decoded.error.take())
        };
        if let Some(error) = error.or((status != 0).then_some(status)) {
            // Start again at the next keyframe.
            self.session = None;
            bail!("VideoToolbox could not decode a picture (error {error})");
        }
        let Some(picture) = picture else {
            return Ok(None);
        };
        if surfaces && let Some(surface) = CVPixelBufferGetIOSurface(Some(&picture)) {
            // One surface per picture (the decoder's pool cycles a few),
            // its buffer kept until the window is done with it.
            let ring = LocalRing::keeping(
                vec![Surface::from_io_surface(surface)],
                Some(Box::new(Keep(picture))),
            )
            .context("could not hand over a decoded picture")?;
            let slot = ring.take()?.context("a new ring without a slot")?;
            let frame = ring.frame(slot, isocast_client::shared::host_now(), None);
            return Ok(Some(super::Decoded::Frame(frame)));
        }
        rgba(&picture).map(|pixels| Some(super::Decoded::Pixels(pixels)))
    }
}

/// `data` (AVCC) as a sample of `format`. The block buffer holds a copy,
/// so the sample owns its bytes.
fn sample(format: &CMFormatDescription, data: &[u8]) -> Result<CFRetained<CMSampleBuffer>> {
    if data.is_empty() {
        bail!("an empty access unit");
    }
    let mut block = ptr::null_mut();
    let status = unsafe {
        CMBlockBuffer::create_with_memory_block(
            None,
            ptr::null_mut(),
            data.len(),
            None,
            ptr::null(),
            0,
            data.len(),
            kCMBlockBufferAssureMemoryNowFlag,
            NonNull::from(&mut block),
        )
    };
    let block = NonNull::new(block)
        .filter(|_| status == 0)
        .ok_or_else(|| anyhow!("could not make a block buffer (error {status})"))?;
    let block = unsafe { CFRetained::<CMBlockBuffer>::from_raw(block) };
    let status = unsafe {
        CMBlockBuffer::replace_data_bytes(
            NonNull::new(data.as_ptr() as *mut c_void).unwrap(),
            &block,
            0,
            data.len(),
        )
    };
    if status != 0 {
        bail!("could not fill a block buffer (error {status})");
    }
    let size = data.len();
    let mut sample = ptr::null_mut();
    let status = unsafe {
        CMSampleBuffer::create_ready(
            None,
            Some(&block),
            Some(format),
            1,
            0,
            ptr::null(),
            1,
            &size,
            NonNull::from(&mut sample),
        )
    };
    let sample = NonNull::new(sample)
        .filter(|_| status == 0)
        .ok_or_else(|| anyhow!("could not make a sample (error {status})"))?;
    Ok(unsafe { CFRetained::from_raw(sample) })
}

/// A decoded picture's buffer, kept while the window shows its surface (the
/// decoder's pool would otherwise reuse it).
struct Keep(#[expect(dead_code, reason = "held, never read")] CFRetained<CVPixelBuffer>);

// SAFETY: Core Video buffers are reference counted atomically; this one is
// only held, never touched.
unsafe impl Send for Keep {}
unsafe impl Sync for Keep {}

/// A BGRA or RGBA pixel buffer's picture as RGBA.
fn rgba(buffer: &CVPixelBuffer) -> Result<Picture> {
    let bgra = CVPixelBufferGetPixelFormatType(buffer) == kCVPixelFormatType_32BGRA;
    let read_only = CVPixelBufferLockFlags(1);
    if unsafe { CVPixelBufferLockBaseAddress(buffer, read_only) } != 0 {
        bail!("could not read a decoded picture");
    }
    let width = CVPixelBufferGetWidth(buffer);
    let height = CVPixelBufferGetHeight(buffer);
    let stride = CVPixelBufferGetBytesPerRow(buffer);
    let base = CVPixelBufferGetBaseAddress(buffer) as *const u8;
    let result = if base.is_null() || stride < width * 4 {
        Err(anyhow!("a decoded picture without pixels"))
    } else {
        let mut picture = Picture::new(width as u32, height as u32);
        let out = picture.make_mut_bytes();
        for y in 0..height {
            // SAFETY: the buffer is locked and holds `height` rows of
            // `stride` bytes.
            let row = unsafe { std::slice::from_raw_parts(base.add(y * stride), width * 4) };
            let dest = &mut out[y * width * 4..(y + 1) * width * 4];
            for (d, s) in dest
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(row.as_chunks::<4>().0)
            {
                *d = match bgra {
                    true => [s[2], s[1], s[0], 255],
                    false => [s[0], s[1], s[2], 255],
                };
            }
        }
        Ok(picture)
    };
    unsafe { CVPixelBufferUnlockBaseAddress(buffer, read_only) };
    result
}
