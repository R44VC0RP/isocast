use std::ptr::{self, NonNull};
use std::slice;

use anyhow::{Result, bail};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferPool, CVPixelBufferUnlockBaseAddress,
    kCVBufferPropagatedAttachmentsKey, kCVImageBufferColorPrimaries_ITU_R_709_2,
    kCVImageBufferColorPrimariesKey, kCVImageBufferTransferFunction_ITU_R_709_2,
    kCVImageBufferTransferFunctionKey, kCVImageBufferYCbCrMatrix_ITU_R_709_2,
    kCVImageBufferYCbCrMatrixKey, kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferMetalCompatibilityKey, kCVPixelBufferPixelFormatTypeKey,
    kCVPixelBufferPoolAllocationThresholdKey, kCVPixelBufferWidthKey, kCVPixelFormatType_32BGRA,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, kCVReturnSuccess,
    kCVReturnWouldExceedAllocationThreshold,
};

/// Frames from one pool that may be in use at once. If writers stall, the
/// source drops frames instead of using ever more memory.
const MAX_FRAMES_IN_FLIGHT: i32 = 32;

/// A retained `CVPixelBuffer`. Cloning shares the same pixels.
#[derive(Clone)]
pub struct PixelBuffer(pub CFRetained<CVPixelBuffer>);

// SAFETY: CoreVideo buffers are reference counted atomically and may be used
// from any thread. Pixels are only written before a frame is shared.
unsafe impl Send for PixelBuffer {}
unsafe impl Sync for PixelBuffer {}

/// IOSurface-backed pixel buffers of one size and format: BGRA, or 8-bit
/// 4:2:0 Y'CbCr (video range, tagged Rec. 709) for the GPU to convert into.
pub struct FramePool {
    pool: CFRetained<CVPixelBufferPool>,
    aux_attributes: CFRetained<CFDictionary<CFString, CFNumber>>,
}

// SAFETY: CVPixelBufferPool is thread-safe; the pool is used by one thread
// at a time anyway.
unsafe impl Send for FramePool {}

impl FramePool {
    /// BGRA frames.
    pub fn new(width: u32, height: u32) -> Result<Self> {
        Self::with_format(width, height, kCVPixelFormatType_32BGRA)
    }

    /// 8-bit 4:2:0 Y'CbCr frames in video range, tagged Rec. 709: what the
    /// encoders take without converting, at under half BGRA's size.
    pub fn ycbcr(width: u32, height: u32) -> Result<Self> {
        Self::with_format(
            width,
            height,
            kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
        )
    }

    fn with_format(width: u32, height: u32, pixel_format: u32) -> Result<Self> {
        let format = CFNumber::new_i32(pixel_format as i32);
        let width = CFNumber::new_i32(width as i32);
        let height = CFNumber::new_i32(height as i32);
        let io_surface = CFDictionary::<CFString, CFType>::empty();
        // The GPU can draw into them (the compositor) or read them.
        let metal = CFBoolean::new(true);
        // Every frame says how its Y'CbCr encodes colour, so encoders and
        // readers don't guess.
        let colour = unsafe {
            CFDictionary::<CFString, CFString>::from_slices(
                &[
                    kCVImageBufferYCbCrMatrixKey,
                    kCVImageBufferColorPrimariesKey,
                    kCVImageBufferTransferFunctionKey,
                ],
                &[
                    kCVImageBufferYCbCrMatrix_ITU_R_709_2,
                    kCVImageBufferColorPrimaries_ITU_R_709_2,
                    kCVImageBufferTransferFunction_ITU_R_709_2,
                ],
            )
        };
        let mut keys = unsafe {
            vec![
                kCVPixelBufferPixelFormatTypeKey,
                kCVPixelBufferWidthKey,
                kCVPixelBufferHeightKey,
                kCVPixelBufferIOSurfacePropertiesKey,
                kCVPixelBufferMetalCompatibilityKey,
            ]
        };
        let mut values: Vec<&CFType> = vec![&format, &width, &height, &io_surface, metal];
        if pixel_format != kCVPixelFormatType_32BGRA {
            keys.push(unsafe { kCVBufferPropagatedAttachmentsKey });
            values.push(&colour);
        }
        let attributes = CFDictionary::<CFString, CFType>::from_slices(&keys, &values);
        let mut pool = ptr::null_mut();
        let status = unsafe {
            CVPixelBufferPool::create(
                None,
                None,
                Some(attributes.as_opaque()),
                NonNull::from(&mut pool),
            )
        };
        let Some(pool) = NonNull::new(pool).filter(|_| status == kCVReturnSuccess) else {
            bail!("could not create a pixel buffer pool (CoreVideo error {status})");
        };
        let threshold = CFNumber::new_i32(MAX_FRAMES_IN_FLIGHT);
        let aux_attributes = unsafe {
            CFDictionary::from_slices(&[kCVPixelBufferPoolAllocationThresholdKey], &[&*threshold])
        };
        Ok(Self {
            pool: unsafe { CFRetained::from_raw(pool) },
            aux_attributes,
        })
    }
}

impl isocast_engine::FramePool for FramePool {
    type Frame = PixelBuffer;

    fn draw(&mut self, draw: &mut dyn FnMut(&mut [u8], usize)) -> Result<Option<PixelBuffer>> {
        let Some(PixelBuffer(buffer)) = self.take()? else {
            return Ok(None);
        };
        if CVPixelBufferGetPixelFormatType(&buffer) != kCVPixelFormatType_32BGRA {
            bail!("only BGRA frames are drawn on the CPU");
        }
        let flags = CVPixelBufferLockFlags(0);
        let status = unsafe { CVPixelBufferLockBaseAddress(&buffer, flags) };
        if status != kCVReturnSuccess {
            bail!("could not lock a frame for drawing (CoreVideo error {status})");
        }
        let stride = CVPixelBufferGetBytesPerRow(&buffer);
        let len = stride * CVPixelBufferGetHeight(&buffer);
        let base = CVPixelBufferGetBaseAddress(&buffer).cast::<u8>();
        // SAFETY: the buffer is locked, and a locked BGRA buffer holds
        // `bytes per row × height` bytes at its base address.
        draw(unsafe { slice::from_raw_parts_mut(base, len) }, stride);
        unsafe { CVPixelBufferUnlockBaseAddress(&buffer, flags) };
        Ok(Some(PixelBuffer(buffer)))
    }

    fn take(&mut self) -> Result<Option<PixelBuffer>> {
        let mut buffer = ptr::null_mut();
        let status = unsafe {
            CVPixelBufferPool::create_pixel_buffer_with_aux_attributes(
                None,
                &self.pool,
                Some(self.aux_attributes.as_opaque()),
                NonNull::from(&mut buffer),
            )
        };
        if status == kCVReturnWouldExceedAllocationThreshold {
            return Ok(None);
        }
        let Some(buffer) = NonNull::new(buffer).filter(|_| status == kCVReturnSuccess) else {
            bail!("could not get a frame from the pool (CoreVideo error {status})");
        };
        Ok(Some(PixelBuffer(unsafe { CFRetained::from_raw(buffer) })))
    }
}
