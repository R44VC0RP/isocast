//! Live frames as JPEG for clients' monitors: converted to BGRA and scaled
//! by VideoToolbox's pixel transfer (the media engines' scaler, not the
//! CPU) and encoded by ImageIO, which stay fast however Isocast itself was
//! built.

use std::ptr::{self, NonNull};
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, bail};
use isocast_engine::FramePool as _;
use objc2::rc::autoreleasepool;
use objc2_core_foundation::{CFDictionary, CFMutableData, CFNumber, CFRetained, CFString};
use objc2_core_graphics::CGImage;
use objc2_core_video::{
    CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
    kCVPixelFormatType_32BGRA,
};
use objc2_image_io::{CGImageDestination, kCGImageDestinationLossyCompressionQuality};
use objc2_video_toolbox::{VTCreateCGImageFromCVPixelBuffer, VTPixelTransferSession};

use crate::frames::{FramePool, PixelBuffer};

/// Pools of BGRA pictures kept for this many sizes (monitors of a few
/// widths).
const SIZES_KEPT: usize = 3;

/// A pixel transfer session and BGRA pools by size, shared by every
/// request: a session is costly to make, and transfers are quick.
struct Scaler {
    session: CFRetained<VTPixelTransferSession>,
    pools: Vec<((u32, u32), FramePool)>,
}

// SAFETY: the session is only used under the mutex, one call at a time.
unsafe impl Send for Scaler {}

static SCALER: Mutex<Option<Scaler>> = Mutex::new(None);

/// `frame` as a JPEG of `quality` (0 to 1), scaled down to at most
/// `max_width` wide; the bytes and their size.
pub(crate) fn encode(
    frame: &PixelBuffer,
    max_width: Option<u32>,
    quality: f32,
) -> Result<(Vec<u8>, u32, u32)> {
    // Requests come on threads without an autorelease pool of their own.
    autoreleasepool(|_| encode_in_pool(frame, max_width, quality))
}

fn encode_in_pool(
    frame: &PixelBuffer,
    max_width: Option<u32>,
    quality: f32,
) -> Result<(Vec<u8>, u32, u32)> {
    let width = CVPixelBufferGetWidth(&frame.0) as u32;
    let height = CVPixelBufferGetHeight(&frame.0) as u32;
    let (w, h) = match max_width {
        Some(max) if max > 0 && max < width => (
            max,
            ((u64::from(height) * u64::from(max) / u64::from(width)) as u32).max(1),
        ),
        _ => (width, height),
    };
    // A BGRA frame at its own size is encoded as it is; anything else (the
    // program and captures are 4:2:0 Y'CbCr) is converted and scaled first.
    let bgra = CVPixelBufferGetPixelFormatType(&frame.0) == kCVPixelFormatType_32BGRA;
    let picture = if bgra && (w, h) == (width, height) {
        frame.clone()
    } else {
        transfer(frame, w, h)?
    };
    let mut image: *mut CGImage = ptr::null_mut();
    let status =
        unsafe { VTCreateCGImageFromCVPixelBuffer(&picture.0, None, NonNull::from(&mut image)) };
    let image = NonNull::new(image)
        .filter(|_| status == 0)
        .ok_or_else(|| anyhow!("could not read the frame (VideoToolbox error {status})"))?;
    let image = unsafe { CFRetained::from_raw(image) };
    let data = CFMutableData::new(None, 0).ok_or_else(|| anyhow!("out of memory"))?;
    let kind = CFString::from_static_str("public.jpeg");
    let destination = unsafe { CGImageDestination::with_data(&data, &kind, 1, None) }
        .ok_or_else(|| anyhow!("ImageIO can't write JPEG"))?;
    let quality = CFNumber::new_f64(f64::from(quality.clamp(0.0, 1.0)));
    let properties = unsafe {
        CFDictionary::<CFString, CFNumber>::from_slices(
            &[kCGImageDestinationLossyCompressionQuality],
            &[&*quality],
        )
    };
    unsafe { destination.add_image(&image, Some(properties.as_opaque())) };
    if !unsafe { destination.finalize() } {
        bail!("ImageIO could not encode the frame");
    }
    Ok((data.to_vec(), w, h))
}

/// `frame` converted to a `w`×`h` BGRA picture.
fn transfer(frame: &PixelBuffer, w: u32, h: u32) -> Result<PixelBuffer> {
    let mut scaler = SCALER.lock().unwrap();
    let scaler = match &mut *scaler {
        Some(scaler) => scaler,
        none => {
            let mut session = ptr::null_mut();
            let status =
                unsafe { VTPixelTransferSession::create(None, NonNull::from(&mut session)) };
            let session = NonNull::new(session)
                .filter(|_| status == 0)
                .ok_or_else(|| anyhow!("could not start a pixel transfer (error {status})"))?;
            none.insert(Scaler {
                session: unsafe { CFRetained::from_raw(session) },
                pools: Vec::new(),
            })
        }
    };
    let index = match scaler.pools.iter().position(|(size, _)| *size == (w, h)) {
        Some(index) => index,
        None => {
            if scaler.pools.len() == SIZES_KEPT {
                scaler.pools.remove(0);
            }
            scaler.pools.push(((w, h), FramePool::new(w, h)?));
            scaler.pools.len() - 1
        }
    };
    let picture = scaler.pools[index]
        .1
        .take()?
        .context("every picture for monitors is still in use")?;
    let status = unsafe { scaler.session.transfer_image(&frame.0, &picture.0) };
    if status != 0 {
        bail!("could not convert the frame (VideoToolbox error {status})");
    }
    // Plain BGRA, like the frames the engine draws: the picture keeps its
    // colours rather than being tagged for colour matching.
    picture.0.remove_all_attachments();
    Ok(picture)
}
