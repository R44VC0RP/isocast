//! Showing CoreVideo frames to the GPU without copying: each plane of a
//! frame's IOSurface becomes a Metal texture through a CVMetalTextureCache,
//! wrapped as a wgpu texture. The compositor's imports instead keep a
//! texture per surface (pools recycle a few), made of the IOSurface itself,
//! so a frame in a surface seen before costs no new objects.

use std::collections::HashMap;
use std::ptr::{NonNull, null_mut};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use isocast_engine::gpu::{FrameImporter, Gpu, GpuFrame, GpuPixels, Matrix, YCbCr};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::ProtocolObject;
use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString};
use objc2_core_video::{
    CVMetalTexture, CVMetalTextureCache, CVMetalTextureGetTexture, CVPixelBufferGetHeight,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferGetIOSurface, CVPixelBufferGetPixelFormatType,
    CVPixelBufferGetWidth, CVPixelBufferGetWidthOfPlane, kCVImageBufferYCbCrMatrix_ITU_R_601_4,
    kCVImageBufferYCbCrMatrix_ITU_R_2020, kCVImageBufferYCbCrMatrixKey, kCVMetalTextureUsage,
    kCVPixelFormatType_32BGRA, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
    kCVPixelFormatType_420YpCbCr10BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange, kCVReturnSuccess,
};
use objc2_metal::{
    MTLDevice, MTLPixelFormat, MTLTexture, MTLTextureDescriptor, MTLTextureType, MTLTextureUsage,
};
use wgpu::hal;

use crate::frames::PixelBuffer;

/// Once this many surfaces' textures are kept, those unused for
/// [`SURFACE_TTL`] go. A kept texture keeps its surface's memory (12 MB
/// for 4K 4:2:0) after its pool lets go of it, but the working set is
/// larger than it looks: the flagship (two video decoders and the
/// program's frame pool) cycles through more than 32 surfaces, and a cap
/// of 32 made every frame miss (5.0e, undone).
const KEPT_SURFACES: usize = 16;
const SURFACE_TTL: Duration = Duration::from_secs(2);
/// The most kept, however recently used.
const MAX_SURFACES: usize = 64;

pub struct MetalImporter {
    device: wgpu::Device,
    metal: Retained<ProtocolObject<dyn MTLDevice>>,
    cache: CFRetained<CVMetalTextureCache>,
    /// Textures made of frames' surfaces, by IOSurface id.
    surfaces: HashMap<u32, Surface>,
}

/// How often a kept surface's Y'CbCr matrix is read from its frame again
/// (reading a frame's attachments costs more than the rest of an import).
const MATRIX_EVERY: u32 = 120;

/// The textures made of one IOSurface's planes.
struct Surface {
    /// What they were made for: the surface's size and pixel format, and
    /// whether the GPU draws into them.
    key: (usize, usize, u32, bool),
    planes: Vec<wgpu::Texture>,
    /// The Y'CbCr matrix its frames are tagged with.
    matrix: Matrix,
    /// Frames shown in it since its matrix was read.
    uses: u32,
    used: Instant,
}

// SAFETY: CVMetalTextureCache may be used from any thread; the importer is
// used by one thread at a time.
unsafe impl Send for MetalImporter {}

impl MetalImporter {
    pub fn new(gpu: &Gpu) -> Result<Self> {
        let metal = {
            let device = unsafe { gpu.device.as_hal::<hal::api::Metal>() }
                .context("the GPU is not a Metal device")?;
            device.raw_device().clone()
        };
        let mut cache = null_mut();
        let status = unsafe {
            CVMetalTextureCache::create(None, None, &metal, None, NonNull::from(&mut cache))
        };
        let Some(cache) = NonNull::new(cache).filter(|_| status == kCVReturnSuccess) else {
            bail!("could not create a Metal texture cache (CoreVideo error {status})");
        };
        Ok(Self {
            device: gpu.device.clone(),
            metal,
            cache: unsafe { CFRetained::from_raw(cache) },
            surfaces: HashMap::new(),
        })
    }

    /// The textures of `frame`'s planes, made of its IOSurface the first
    /// time a frame comes in that surface and kept (render targets if
    /// `target`), with the surface's size, format and matrix. `None` if it
    /// has no IOSurface or isn't BGRA or 4:2:0 Y'CbCr. The size and format
    /// are the surface's own (cheap to read, unlike the frame's).
    fn surface(&mut self, frame: &PixelBuffer, target: bool) -> Result<Option<&Surface>> {
        let buffer = &frame.0;
        let Some(surface) = CVPixelBufferGetIOSurface(Some(buffer)) else {
            return Ok(None);
        };
        let id = surface.id();
        let key = (
            surface.width(),
            surface.height(),
            surface.pixel_format(),
            target,
        );
        let now = Instant::now();
        if self.surfaces.get(&id).is_none_or(|kept| kept.key != key) {
            let bgra = [wgpu::TextureFormat::Bgra8Unorm];
            let ycbcr;
            let formats: &[wgpu::TextureFormat] = match key.2 {
                format if format == kCVPixelFormatType_32BGRA => &bgra,
                format => match ycbcr_format(format) {
                    Some((bits, _)) => {
                        ycbcr = plane_formats(bits);
                        &ycbcr
                    }
                    None => return Ok(None),
                },
            };
            self.forget_old(now);
            let mut planes = Vec::with_capacity(formats.len());
            for (plane, &format) in formats.iter().enumerate() {
                let (width, height) = match formats.len() {
                    1 => (key.0, key.1),
                    _ => (
                        CVPixelBufferGetWidthOfPlane(buffer, plane),
                        CVPixelBufferGetHeightOfPlane(buffer, plane),
                    ),
                };
                let descriptor = plane_descriptor(format, width, height, target)?;
                // Reads (and for a target, draws into) the surface's own
                // memory; keeps the surface alive, but not the frame in it.
                let raw = self
                    .metal
                    .newTextureWithDescriptor_iosurface_plane(&descriptor, &surface, plane)
                    .with_context(|| {
                        format!("Metal could not make a texture of plane {plane} of a frame")
                    })?;
                planes.push(self.wrap_metal(raw, format, width, height, target));
            }
            self.surfaces.insert(
                id,
                Surface {
                    key,
                    planes,
                    matrix: matrix(frame),
                    uses: 0,
                    used: now,
                },
            );
        }
        let kept = self.surfaces.get_mut(&id).expect("kept above");
        kept.used = now;
        kept.uses += 1;
        if kept.uses >= MATRIX_EVERY {
            kept.uses = 0;
            kept.matrix = matrix(frame);
        }
        Ok(Some(kept))
    }

    /// Let go of the textures of surfaces unused for a while (a source
    /// closed, a pool shrank), once many are kept.
    fn forget_old(&mut self, now: Instant) {
        if self.surfaces.len() < KEPT_SURFACES {
            return;
        }
        self.surfaces
            .retain(|_, kept| now.duration_since(kept.used) < SURFACE_TTL);
        while self.surfaces.len() >= MAX_SURFACES {
            let oldest = self
                .surfaces
                .iter()
                .min_by_key(|(_, kept)| kept.used)
                .map(|(id, _)| *id)
                .expect("not empty");
            self.surfaces.remove(&oldest);
        }
    }

    /// A Metal texture of this device (of `format`, `width` × `height`) as
    /// a wgpu texture the GPU reads, and draws into if `target`.
    fn wrap_metal(
        &self,
        raw: Retained<ProtocolObject<dyn MTLTexture>>,
        format: wgpu::TextureFormat,
        width: usize,
        height: usize,
        target: bool,
    ) -> wgpu::Texture {
        let (width, height) = (width as u32, height as u32);
        let hal_texture = unsafe {
            hal::metal::Device::texture_from_raw(
                raw,
                format,
                MTLTextureType::Type2D,
                1,
                1,
                hal::CopyExtent {
                    width,
                    height,
                    depth: 1,
                },
                None,
            )
        };
        // SAFETY: `raw` is a texture of this device of `format` and size.
        unsafe { wrap(&self.device, hal_texture, format, width, height, target) }
    }

    /// Plane `plane` of `frame` as a wgpu texture that keeps the frame
    /// alive until the GPU is done with it; one the GPU can also draw into
    /// if `target`.
    fn plane(
        &self,
        frame: &PixelBuffer,
        plane: usize,
        format: wgpu::TextureFormat,
        width: usize,
        height: usize,
        target: bool,
    ) -> Result<wgpu::Texture> {
        let metal_format = metal_format(format)?;
        let usage = if target {
            MTLTextureUsage::ShaderRead | MTLTextureUsage::RenderTarget
        } else {
            MTLTextureUsage::ShaderRead
        };
        let usage = CFNumber::new_isize(usage.0 as isize);
        let attributes = unsafe {
            CFDictionary::<CFString, CFNumber>::from_slices(&[kCVMetalTextureUsage], &[&*usage])
        };
        let mut texture = null_mut();
        let status = unsafe {
            CVMetalTextureCache::create_texture_from_image(
                None,
                &self.cache,
                &frame.0,
                Some(attributes.as_opaque()),
                metal_format,
                width,
                height,
                plane,
                NonNull::from(&mut texture),
            )
        };
        let Some(texture) = NonNull::new(texture).filter(|_| status == kCVReturnSuccess) else {
            bail!("could not show plane {plane} of a frame to the GPU (CoreVideo error {status})");
        };
        let texture: CFRetained<CVMetalTexture> = unsafe { CFRetained::from_raw(texture) };
        let raw = CVMetalTextureGetTexture(&texture).context("the frame has no Metal texture")?;
        let keep = Keep(texture, frame.clone());
        let (width, height) = (width as u32, height as u32);
        let hal_texture = unsafe {
            hal::metal::Device::texture_from_raw(
                raw,
                format,
                MTLTextureType::Type2D,
                1,
                1,
                hal::CopyExtent {
                    width,
                    height,
                    depth: 1,
                },
                Some(Box::new(move || drop(keep))),
            )
        };
        Ok(unsafe { wrap(&self.device, hal_texture, format, width, height, target) })
    }
}

/// What a texture of one plane of a frame's IOSurface is made with.
fn plane_descriptor(
    format: wgpu::TextureFormat,
    width: usize,
    height: usize,
    target: bool,
) -> Result<Retained<MTLTextureDescriptor>> {
    let descriptor = unsafe {
        MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            metal_format(format)?,
            width,
            height,
            false,
        )
    };
    descriptor.setUsage(match target {
        true => MTLTextureUsage::ShaderRead | MTLTextureUsage::RenderTarget,
        false => MTLTextureUsage::ShaderRead,
    });
    Ok(descriptor)
}

/// The Metal pixel format of a wgpu texture format frames are shown as.
fn metal_format(format: wgpu::TextureFormat) -> Result<MTLPixelFormat> {
    Ok(match format {
        wgpu::TextureFormat::Bgra8Unorm => MTLPixelFormat::BGRA8Unorm,
        wgpu::TextureFormat::R8Unorm => MTLPixelFormat::R8Unorm,
        wgpu::TextureFormat::Rg8Unorm => MTLPixelFormat::RG8Unorm,
        wgpu::TextureFormat::R16Unorm => MTLPixelFormat::R16Unorm,
        wgpu::TextureFormat::Rg16Unorm => MTLPixelFormat::RG16Unorm,
        other => bail!("no Metal format for {other:?}"),
    })
}

/// `hal_texture` as a wgpu texture of `device`: one the GPU reads, and
/// draws into if `target`.
///
/// # Safety
///
/// `hal_texture` must be a texture of `device`'s Metal device, of `format`
/// and `width` × `height`.
unsafe fn wrap(
    device: &wgpu::Device,
    hal_texture: hal::metal::Texture,
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
    target: bool,
) -> wgpu::Texture {
    unsafe {
        device.create_texture_from_hal::<hal::api::Metal>(
            hal_texture,
            &wgpu::TextureDescriptor {
                label: Some("frame"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: if target {
                    wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING
                } else {
                    wgpu::TextureUsages::TEXTURE_BINDING
                },
                view_formats: &[],
            },
            if target {
                wgpu::TextureUses::COLOR_TARGET
            } else {
                wgpu::TextureUses::RESOURCE
            },
        )
    }
}

/// What an imported texture holds on to: its Metal texture and the frame
/// whose memory it uses.
struct Keep(
    #[allow(dead_code)] CFRetained<CVMetalTexture>,
    #[allow(dead_code)] PixelBuffer,
);

// SAFETY: both are reference counted atomically and only released here.
unsafe impl Send for Keep {}
unsafe impl Sync for Keep {}

// The importer runs on long-lived engine threads (the compositor, the
// encoders) that have no autorelease pool of their own, and making a Metal
// texture for a frame autoreleases its texture descriptor and more: each
// call drains a pool of its own, or they would pile up until the engine
// stops.
impl FrameImporter for MetalImporter {
    type Frame = PixelBuffer;

    fn import(&mut self, frame: &PixelBuffer) -> Result<GpuFrame> {
        autoreleasepool(|_| self.import_frame(frame))
    }

    fn target(&mut self, frame: &PixelBuffer) -> Result<wgpu::Texture> {
        autoreleasepool(|_| self.bgra_target(frame))
    }

    fn target_cached(&mut self, frame: &PixelBuffer) -> Result<Option<wgpu::Texture>> {
        autoreleasepool(|_| {
            Ok(self
                .surface(frame, true)?
                .filter(|surface| surface.key.2 == kCVPixelFormatType_32BGRA)
                .map(|surface| surface.planes[0].clone()))
        })
    }

    fn import_cached(&mut self, frame: &PixelBuffer) -> Result<Option<GpuFrame>> {
        autoreleasepool(|_| self.import_kept(frame))
    }

    fn ycbcr_target_cached(
        &mut self,
        frame: &PixelBuffer,
    ) -> Result<Option<(wgpu::Texture, wgpu::Texture)>> {
        autoreleasepool(|_| self.ycbcr_kept(frame))
    }

    fn ycbcr_target(&mut self, frame: &PixelBuffer) -> Result<(wgpu::Texture, wgpu::Texture)> {
        autoreleasepool(|_| self.ycbcr_planes(frame))
    }
}

impl MetalImporter {
    fn import_frame(&mut self, frame: &PixelBuffer) -> Result<GpuFrame> {
        // Let go of textures for frames nothing uses any more.
        self.cache.flush(0);
        let buffer = &frame.0;
        let width = CVPixelBufferGetWidth(buffer);
        let height = CVPixelBufferGetHeight(buffer);
        let pixel_format = CVPixelBufferGetPixelFormatType(buffer);
        let pixels = if pixel_format == kCVPixelFormatType_32BGRA {
            let format = wgpu::TextureFormat::Bgra8Unorm;
            GpuPixels::Rgb(self.plane(frame, 0, format, width, height, false)?)
        } else {
            let Some((bits, full_range)) = ycbcr_format(pixel_format) else {
                bail!(
                    "frames in pixel format {:?} can't be drawn yet",
                    String::from_utf8_lossy(&pixel_format.to_be_bytes())
                );
            };
            let [luma, chroma] = plane_formats(bits);
            let size = |plane| {
                (
                    CVPixelBufferGetWidthOfPlane(buffer, plane),
                    CVPixelBufferGetHeightOfPlane(buffer, plane),
                )
            };
            let ((lw, lh), (cw, ch)) = (size(0), size(1));
            GpuPixels::YCbCr {
                luma: self.plane(frame, 0, luma, lw, lh, false)?,
                chroma: self.plane(frame, 1, chroma, cw, ch, false)?,
                coding: YCbCr {
                    matrix: matrix(frame),
                    full_range,
                    bits,
                },
            }
        };
        Ok(GpuFrame {
            width: width as u32,
            height: height as u32,
            pixels,
        })
    }

    fn bgra_target(&mut self, frame: &PixelBuffer) -> Result<wgpu::Texture> {
        let buffer = &frame.0;
        if CVPixelBufferGetPixelFormatType(buffer) != kCVPixelFormatType_32BGRA {
            bail!("the GPU only draws into BGRA frames");
        }
        let (width, height) = (
            CVPixelBufferGetWidth(buffer),
            CVPixelBufferGetHeight(buffer),
        );
        self.plane(
            frame,
            0,
            wgpu::TextureFormat::Bgra8Unorm,
            width,
            height,
            true,
        )
    }

    fn import_kept(&mut self, frame: &PixelBuffer) -> Result<Option<GpuFrame>> {
        let Some(kept) = self.surface(frame, false)? else {
            return Ok(None);
        };
        let (width, height, pixel_format, _) = kept.key;
        let pixels = match ycbcr_format(pixel_format) {
            None => GpuPixels::Rgb(kept.planes[0].clone()),
            Some((bits, full_range)) => GpuPixels::YCbCr {
                luma: kept.planes[0].clone(),
                chroma: kept.planes[1].clone(),
                coding: YCbCr {
                    matrix: kept.matrix,
                    full_range,
                    bits,
                },
            },
        };
        Ok(Some(GpuFrame {
            width: width as u32,
            height: height as u32,
            pixels,
        }))
    }

    fn ycbcr_kept(
        &mut self,
        frame: &PixelBuffer,
    ) -> Result<Option<(wgpu::Texture, wgpu::Texture)>> {
        if CVPixelBufferGetPixelFormatType(&frame.0)
            != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        {
            bail!("the GPU only converts into 8-bit 4:2:0 video-range frames");
        }
        Ok(self
            .surface(frame, true)?
            .map(|kept| (kept.planes[0].clone(), kept.planes[1].clone())))
    }

    fn ycbcr_planes(&mut self, frame: &PixelBuffer) -> Result<(wgpu::Texture, wgpu::Texture)> {
        // Let go of textures for frames nothing uses any more.
        self.cache.flush(0);
        let buffer = &frame.0;
        if CVPixelBufferGetPixelFormatType(buffer)
            != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        {
            bail!("the GPU only converts into 8-bit 4:2:0 video-range frames");
        }
        let size = |plane| {
            (
                CVPixelBufferGetWidthOfPlane(buffer, plane),
                CVPixelBufferGetHeightOfPlane(buffer, plane),
            )
        };
        let ((lw, lh), (cw, ch)) = (size(0), size(1));
        Ok((
            self.plane(frame, 0, wgpu::TextureFormat::R8Unorm, lw, lh, true)?,
            self.plane(frame, 1, wgpu::TextureFormat::Rg8Unorm, cw, ch, true)?,
        ))
    }
}

/// The bits per sample and whether full range of a 4:2:0 Y'CbCr pixel
/// format frames are drawn from; `None` for others.
fn ycbcr_format(pixel_format: u32) -> Option<(u32, bool)> {
    [
        (kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, 8, false),
        (kCVPixelFormatType_420YpCbCr8BiPlanarFullRange, 8, true),
        (kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange, 10, false),
        (kCVPixelFormatType_420YpCbCr10BiPlanarFullRange, 10, true),
    ]
    .into_iter()
    .find(|known| known.0 == pixel_format)
    .map(|(_, bits, full_range)| (bits, full_range))
}

/// The texture formats of a 4:2:0 frame's luma and chroma planes.
fn plane_formats(bits: u32) -> [wgpu::TextureFormat; 2] {
    match bits {
        8 => [wgpu::TextureFormat::R8Unorm, wgpu::TextureFormat::Rg8Unorm],
        _ => [
            wgpu::TextureFormat::R16Unorm,
            wgpu::TextureFormat::Rg16Unorm,
        ],
    }
}

/// The Y'CbCr matrix a frame is tagged with; BT.709 if untagged.
fn matrix(frame: &PixelBuffer) -> Matrix {
    let key = unsafe { kCVImageBufferYCbCrMatrixKey };
    let Some(value) = (unsafe { frame.0.attachment(key, null_mut()) }) else {
        return Matrix::Bt709;
    };
    let Ok(value) = value.downcast::<CFString>() else {
        return Matrix::Bt709;
    };
    if *value == *unsafe { kCVImageBufferYCbCrMatrix_ITU_R_601_4 } {
        Matrix::Bt601
    } else if *value == *unsafe { kCVImageBufferYCbCrMatrix_ITU_R_2020 } {
        Matrix::Bt2020
    } else {
        Matrix::Bt709
    }
}
