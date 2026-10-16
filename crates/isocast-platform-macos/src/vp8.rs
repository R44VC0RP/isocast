//! VP8 video in WebM/Matroska (common in older OBS stingers), which macOS
//! can't decode: libvpx's VP8 decoder (`isocast-vp8`) on the CPU, with its
//! alpha channel from a second VP8 stream in BlockAdditional, like VP9's.
//! Frames come out like the VideoToolbox path's: 8-bit 4:2:0 in video
//! range (`420v`), or BGRA with straight alpha when the video has alpha
//! (or is full range), converted by vImage.

use std::ffi::c_void;
use std::ptr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use objc2_core_foundation::CFRetained;
use objc2_core_video::{
    CVImageBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRow, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
    CVPixelBufferGetWidthOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelFormatType_32BGRA,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};

use crate::frames::{FramePool, PixelBuffer};
use crate::matroska::Colour;
use isocast_engine::FramePool as _;

/// Decoding threads per stream (libvpx uses more than one only for
/// streams with several token partitions).
const THREADS: u32 = 4;

/// A VP8 track's decoders and the frames they're turned into.
pub struct Vp8Video {
    color: isocast_vp8::Decoder,
    /// The alpha stream's decoder, when the track has alpha.
    alpha: Option<isocast_vp8::Decoder>,
    /// The track's colour description (range and matrix win over the
    /// stream's; untagged is Rec. 709 video range).
    colour: Colour,
    /// 4:2:0 (`420v`) or BGRA frames of one size.
    pool: Option<(FramePool, u32, (usize, usize))>,
    /// The vImage conversion for (full range, matrix code).
    conversion: Option<((bool, Matrix), Box<YpCbCrToArgb>)>,
    shown: String,
}

impl Vp8Video {
    pub fn new(alpha: bool, colour: Colour, shown: &str) -> Result<Self> {
        let decoder =
            || isocast_vp8::Decoder::new(THREADS).map_err(|err| anyhow::anyhow!("{shown}: {err}"));
        Ok(Self {
            color: decoder()?,
            alpha: if alpha { Some(decoder()?) } else { None },
            colour,
            pool: None,
            conversion: None,
            shown: shown.to_string(),
        })
    }

    /// Decode one frame (and its alpha frame, if any) into a picture; none
    /// for a frame that isn't shown.
    pub fn decode(
        &mut self,
        data: &[u8],
        alpha: Option<&[u8]>,
    ) -> Result<Option<CFRetained<CVImageBuffer>>> {
        let shown = &self.shown;
        let has_alpha = self.alpha.is_some();
        let frame = self
            .color
            .decode(data)
            .with_context(|| format!("could not decode {shown}'s VP8 video"))?;
        // The alpha stream is decoded whenever it has a frame, even with no
        // picture to show, so its own reference frames stay in step.
        let alpha_frame = match (self.alpha.as_mut(), alpha) {
            (Some(decoder), Some(data)) => decoder
                .decode(data)
                .with_context(|| format!("could not decode {shown}'s VP8 alpha channel"))?,
            _ => None,
        };
        let Some(frame) = frame else {
            return Ok(None);
        };
        let full = self
            .colour
            .range
            .map_or(frame.full_range, |range| range == 2);
        let matrix = Matrix::of(self.colour.matrix);
        let bgra = has_alpha || full;
        let format = if bgra {
            kCVPixelFormatType_32BGRA
        } else {
            kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        };
        let size = (frame.width, frame.height);
        if self
            .pool
            .as_ref()
            .is_none_or(|(_, kind, at)| *kind != format || *at != size)
        {
            let (width, height) = (frame.width as u32, frame.height as u32);
            let pool = if bgra {
                FramePool::new(width, height)?
            } else {
                FramePool::ycbcr(width, height)?
            };
            self.pool = Some((pool, format, size));
        }
        let pool = &mut self.pool.as_mut().expect("set above").0;
        let PixelBuffer(target) = take_waiting(pool)?;
        if bgra {
            if self
                .conversion
                .as_ref()
                .is_none_or(|(key, _)| *key != (full, matrix))
            {
                self.conversion = Some(((full, matrix), YpCbCrToArgb::new(full, matrix)?));
            }
            let conversion = &self.conversion.as_ref().expect("set above").1;
            write_bgra(&frame, alpha_frame.as_ref(), conversion, &target)
                .with_context(|| format!("could not convert {shown}'s VP8 video"))?;
        } else {
            write_420v(&frame, &target)
                .with_context(|| format!("could not convert {shown}'s VP8 video"))?;
        }
        Ok(Some(target))
    }
}

/// A frame from `pool`, waiting briefly for one to come back when the
/// pool's frames are all in flight.
pub fn take_waiting(pool: &mut FramePool) -> Result<PixelBuffer> {
    for _ in 0..200 {
        if let Some(frame) = pool.take()? {
            return Ok(frame);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    bail!("no free frame to decode into")
}

/// A pixel buffer's base address, locked for writing until dropped.
struct Locked<'a>(&'a CVImageBuffer);

impl<'a> Locked<'a> {
    fn new(buffer: &'a CVImageBuffer) -> Result<Self> {
        let status = unsafe { CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags(0)) };
        if status != 0 {
            bail!("could not write a frame (CoreVideo error {status})");
        }
        Ok(Self(buffer))
    }
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        unsafe { CVPixelBufferUnlockBaseAddress(self.0, CVPixelBufferLockFlags(0)) };
    }
}

/// Copy a decoded frame into a `420v` buffer (luma, then interleaved
/// chroma).
fn write_420v(frame: &isocast_vp8::Frame, target: &CVImageBuffer) -> Result<()> {
    let _locked = Locked::new(target)?;
    if CVPixelBufferGetPixelFormatType(target) != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange {
        bail!("unexpected frame format");
    }
    let luma = CVPixelBufferGetBaseAddressOfPlane(target, 0) as *mut u8;
    let chroma = CVPixelBufferGetBaseAddressOfPlane(target, 1) as *mut u8;
    let (luma_stride, chroma_stride) = (
        CVPixelBufferGetBytesPerRowOfPlane(target, 0),
        CVPixelBufferGetBytesPerRowOfPlane(target, 1),
    );
    let (width, height) = (
        frame.width.min(CVPixelBufferGetWidthOfPlane(target, 0)),
        frame.height.min(CVPixelBufferGetHeightOfPlane(target, 0)),
    );
    let (chroma_width, chroma_height) = (
        frame.u.width.min(CVPixelBufferGetWidthOfPlane(target, 1)),
        frame.u.height.min(CVPixelBufferGetHeightOfPlane(target, 1)),
    );
    if luma.is_null() || chroma.is_null() || luma_stride < width || chroma_stride < chroma_width * 2
    {
        bail!("unexpected frame layout");
    }
    for y in 0..height {
        // SAFETY: locked, and each row holds at least `width` bytes.
        let row = unsafe { std::slice::from_raw_parts_mut(luma.add(y * luma_stride), width) };
        row.copy_from_slice(&frame.y.row(y)[..width]);
    }
    for y in 0..chroma_height {
        // SAFETY: locked, and each row holds at least `chroma_width` pairs.
        let row = unsafe {
            std::slice::from_raw_parts_mut(chroma.add(y * chroma_stride), chroma_width * 2)
        };
        let (u, v) = (frame.u.row(y), frame.v.row(y));
        for ((pair, &u), &v) in row.as_chunks_mut::<2>().0.iter_mut().zip(u).zip(v) {
            *pair = [u, v];
        }
    }
    Ok(())
}

/// Convert a decoded frame into a BGRA buffer, with `alpha`'s luma as its
/// alpha channel (opaque without one).
fn write_bgra(
    frame: &isocast_vp8::Frame,
    alpha: Option<&isocast_vp8::Frame>,
    conversion: &YpCbCrToArgb,
    target: &CVImageBuffer,
) -> Result<()> {
    let _locked = Locked::new(target)?;
    let base = CVPixelBufferGetBaseAddress(target) as *mut u8;
    let stride = CVPixelBufferGetBytesPerRow(target);
    let width = frame.width.min(CVPixelBufferGetWidth(target));
    let height = frame.height.min(CVPixelBufferGetHeight(target));
    if CVPixelBufferGetPixelFormatType(target) != kCVPixelFormatType_32BGRA
        || base.is_null()
        || stride < width * 4
    {
        bail!("unexpected frame format");
    }
    // vImage converts 2×2 blocks, so the even part.
    let (even_width, even_height) = (width & !1, height & !1);
    let dest = |w: usize, h: usize| VImageBuffer {
        data: base.cast(),
        height: h,
        width: w,
        row_bytes: stride,
    };
    let plane = |plane: &isocast_vp8::Plane, w: usize, h: usize| VImageBuffer {
        data: plane.data.as_ptr() as *mut c_void,
        height: h,
        width: w,
        row_bytes: plane.stride,
    };
    if even_width > 0 && even_height > 0 {
        let (y, u, v) = (
            plane(&frame.y, even_width, even_height),
            plane(&frame.u, even_width / 2, even_height / 2),
            plane(&frame.v, even_width / 2, even_height / 2),
        );
        let target = dest(even_width, even_height);
        // ARGB channels 3, 2, 1, 0: BGRA in memory.
        let permute = [3u8, 2, 1, 0];
        // SAFETY: every buffer covers `even_width`×`even_height` (chroma
        // half that) within its locked or decoded memory.
        let status = unsafe {
            vImageConvert_420Yp8_Cb8_Cr8ToARGB8888(
                &y,
                &u,
                &v,
                &target,
                &**conversion,
                permute.as_ptr(),
                255,
                0,
            )
        };
        if status != 0 {
            bail!("vImage error {status}");
        }
    }
    // An odd last column (beside vImage's part) and an odd last row are
    // converted one pixel at a time.
    let set = |x: usize, y: usize| {
        let pixel = rgb(frame, conversion, x, y);
        // SAFETY: locked, and (x, y) is inside the buffer.
        unsafe { ptr::copy_nonoverlapping(pixel.as_ptr(), base.add(y * stride + x * 4), 4) };
    };
    if width > even_width {
        for y in 0..even_height {
            set(width - 1, y);
        }
    }
    if height > even_height {
        for x in 0..width {
            set(x, height - 1);
        }
    }
    let Some(alpha) = alpha else {
        return Ok(());
    };
    // The alpha frame's luma, over as much of the picture as it covers
    // (the rest stays opaque).
    let (w, h) = (width.min(alpha.width), height.min(alpha.height));
    let source = plane(&alpha.y, w, h);
    let target = dest(w, h);
    // SAFETY: both buffers cover `w`×`h`; 0x1 is the last byte of each
    // pixel (ARGB's B; BGRA's A).
    let status = unsafe { vImageOverwriteChannels_ARGB8888(&source, &target, &target, 0x1, 0) };
    if status != 0 {
        bail!("vImage error {status}");
    }
    Ok(())
}

/// One pixel converted the way vImage does it (for an odd last column or
/// row, which vImage's 2×2 blocks can't do).
fn rgb(frame: &isocast_vp8::Frame, conversion: &YpCbCrToArgb, x: usize, y: usize) -> [u8; 4] {
    let (luma, cb, cr) = (
        frame.y.row(y)[x],
        frame.u.row(y / 2)[x / 2],
        frame.v.row(y / 2)[x / 2],
    );
    let ((kr, kb), full) = (conversion.coefficients, conversion.full);
    let (luma_scale, chroma_scale, luma_bias) = if full {
        (1.0, 1.0, 0.0)
    } else {
        (255.0 / 219.0, 255.0 / 224.0, 16.0)
    };
    let yy = (f32::from(luma) - luma_bias) * luma_scale;
    let (cb, cr) = (
        (f32::from(cb) - 128.0) * chroma_scale,
        (f32::from(cr) - 128.0) * chroma_scale,
    );
    let kg = 1.0 - kr - kb;
    let r = yy + 2.0 * (1.0 - kr) * cr;
    let b = yy + 2.0 * (1.0 - kb) * cb;
    let g = yy - 2.0 * kr * (1.0 - kr) / kg * cr - 2.0 * kb * (1.0 - kb) / kg * cb;
    let clamp = |v: f32| v.round().clamp(0.0, 255.0) as u8;
    [clamp(b), clamp(g), clamp(r), 255]
}

/// The Y'CbCr matrices the compositor knows (gpu.rs); others are drawn as
/// Rec. 709 there, so here too.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Matrix {
    Bt601,
    Bt709,
    Bt2020,
}

impl Matrix {
    /// From a Matroska (ISO/IEC 23091-4) matrix code; untagged is 709.
    fn of(code: Option<u64>) -> Self {
        match code {
            Some(5 | 6) => Matrix::Bt601,
            Some(9 | 10) => Matrix::Bt2020,
            _ => Matrix::Bt709,
        }
    }

    /// Kr and Kb.
    fn coefficients(self) -> (f32, f32) {
        match self {
            Matrix::Bt601 => (0.299, 0.114),
            Matrix::Bt709 => (0.2126, 0.0722),
            Matrix::Bt2020 => (0.2627, 0.0593),
        }
    }
}

// vImage (Accelerate), which converts Y'CbCr to RGB with NEON.

#[repr(C)]
struct VImageBuffer {
    data: *mut c_void,
    height: usize,
    width: usize,
    row_bytes: usize,
}

#[repr(C)]
struct YpCbCrToArgbMatrix {
    yp: f32,
    cr_r: f32,
    cr_g: f32,
    cb_g: f32,
    cb_b: f32,
}

#[repr(C)]
struct YpCbCrPixelRange {
    yp_bias: i32,
    cbcr_bias: i32,
    yp_range_max: i32,
    cbcr_range_max: i32,
    yp_max: i32,
    yp_min: i32,
    cbcr_max: i32,
    cbcr_min: i32,
}

/// `vImage_YpCbCrToARGB`: vImage's prepared conversion.
#[repr(C, align(16))]
struct VImageConversion {
    opaque: [u8; 128],
}

/// A prepared conversion and what it converts.
struct YpCbCrToArgb {
    conversion: VImageConversion,
    coefficients: (f32, f32),
    full: bool,
}

impl std::ops::Deref for YpCbCrToArgb {
    type Target = VImageConversion;

    fn deref(&self) -> &VImageConversion {
        &self.conversion
    }
}

/// `kvImage420Yp8_Cb8_Cr8`, `kvImageARGB8888`.
const YPCBCR_420YP8_CB8_CR8: i32 = 3;
const ARGB8888: i32 = 0;

#[link(name = "Accelerate", kind = "framework")]
unsafe extern "C" {
    fn vImageConvert_YpCbCrToARGB_GenerateConversion(
        matrix: *const YpCbCrToArgbMatrix,
        range: *const YpCbCrPixelRange,
        out: *mut VImageConversion,
        input: i32,
        output: i32,
        flags: u32,
    ) -> isize;
    fn vImageConvert_420Yp8_Cb8_Cr8ToARGB8888(
        y: *const VImageBuffer,
        cb: *const VImageBuffer,
        cr: *const VImageBuffer,
        dest: *const VImageBuffer,
        info: *const VImageConversion,
        permute: *const u8,
        alpha: u8,
        flags: u32,
    ) -> isize;
    fn vImageOverwriteChannels_ARGB8888(
        new_source: *const VImageBuffer,
        source: *const VImageBuffer,
        dest: *const VImageBuffer,
        mask: u8,
        flags: u32,
    ) -> isize;
}

impl YpCbCrToArgb {
    fn new(full: bool, matrix: Matrix) -> Result<Box<Self>> {
        let (kr, kb) = matrix.coefficients();
        let kg = 1.0 - kr - kb;
        let coefficients = YpCbCrToArgbMatrix {
            yp: 1.0,
            cr_r: 2.0 * (1.0 - kr),
            cr_g: -2.0 * kr * (1.0 - kr) / kg,
            cb_g: -2.0 * kb * (1.0 - kb) / kg,
            cb_b: 2.0 * (1.0 - kb),
        };
        // vImage's documented 8-bit ranges.
        let range = if full {
            YpCbCrPixelRange {
                yp_bias: 0,
                cbcr_bias: 128,
                yp_range_max: 255,
                cbcr_range_max: 255,
                yp_max: 255,
                yp_min: 0,
                cbcr_max: 255,
                cbcr_min: 0,
            }
        } else {
            YpCbCrPixelRange {
                yp_bias: 16,
                cbcr_bias: 128,
                yp_range_max: 235,
                cbcr_range_max: 240,
                yp_max: 255,
                yp_min: 0,
                cbcr_max: 255,
                cbcr_min: 0,
            }
        };
        let mut out = Box::new(Self {
            conversion: VImageConversion { opaque: [0; 128] },
            coefficients: (kr, kb),
            full,
        });
        // SAFETY: plain structs in, vImage fills `out`.
        let status = unsafe {
            vImageConvert_YpCbCrToARGB_GenerateConversion(
                &coefficients,
                &range,
                &mut out.conversion,
                YPCBCR_420YP8_CB8_CR8,
                ARGB8888,
                0,
            )
        };
        if status != 0 {
            bail!("could not set up a colour conversion (vImage error {status})");
        }
        Ok(out)
    }
}
