//! The GPU: frames shown to it without copying, and drawn with it.

use std::future::Future;
use std::pin::pin;
use std::task::{Context as TaskContext, Poll, Waker};

use anyhow::{Context, Result};

use crate::render::{TexelKind, Texels};

/// The GPU device frames are imported into and drawn with. Clones share
/// the device and its queue.
#[derive(Clone)]
pub struct Gpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

impl Gpu {
    pub fn new() -> Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .context("no GPU found")?;
        // 10-bit frames arrive as 16-bit planes.
        let required_features = adapter.features() & wgpu::Features::TEXTURE_FORMAT_16BIT_NORM;
        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("isocast"),
            required_features,
            ..Default::default()
        }))
        .context("could not open the GPU")?;
        Ok(Self { device, queue })
    }
}

/// Run a wgpu future to completion; on native back ends they are ready at
/// once.
fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = TaskContext::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
        std::thread::yield_now();
    }
}

/// A video frame shown to the GPU as textures, without copying it. The
/// textures keep the frame alive until the GPU is done with them (except
/// those from [`FrameImporter::import_cached`]).
#[derive(Clone)]
pub struct GpuFrame {
    pub width: u32,
    pub height: u32,
    pub pixels: GpuPixels,
}

impl GpuFrame {
    /// What its draw records need to know about its texels.
    pub(crate) fn texels(&self) -> Texels {
        Texels {
            width: self.width,
            height: self.height,
            kind: match &self.pixels {
                GpuPixels::Rgb(_) => TexelKind::Rgb,
                GpuPixels::Premultiplied(_) => TexelKind::Premultiplied,
                GpuPixels::YCbCr { coding, .. } => TexelKind::YCbCr(*coding),
            },
        }
    }

    /// The textures a draw of it samples (the same twice for one plane).
    pub(crate) fn planes(&self) -> (&wgpu::Texture, &wgpu::Texture) {
        match &self.pixels {
            GpuPixels::Rgb(texture) | GpuPixels::Premultiplied(texture) => (texture, texture),
            GpuPixels::YCbCr { luma, chroma, .. } => (luma, chroma),
        }
    }
}

#[derive(Clone)]
pub enum GpuPixels {
    /// RGB in one plane (any channel order the texture format names).
    Rgb(wgpu::Texture),
    /// RGB with premultiplied alpha, like a canvas a scene was drawn on.
    Premultiplied(wgpu::Texture),
    /// 4:2:0 Y'CbCr: a full-size luma plane and a half-size plane of
    /// interleaved Cb and Cr.
    YCbCr {
        luma: wgpu::Texture,
        chroma: wgpu::Texture,
        coding: YCbCr,
    },
}

/// How Y'CbCr samples encode colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct YCbCr {
    pub matrix: Matrix,
    /// Full range (0–255 for 8 bits) rather than video range (16–235).
    pub full_range: bool,
    /// Bits per sample, kept in the high bits of 8- or 16-bit texels.
    pub bits: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Matrix {
    Bt601,
    Bt709,
    Bt2020,
}

/// Shows a platform's frames to the GPU without copying them.
pub trait FrameImporter: Send {
    type Frame;

    fn import(&mut self, frame: &Self::Frame) -> Result<GpuFrame>;

    /// Show an 8-bit BGRA frame (from a frame pool) to the GPU as a
    /// `Bgra8Unorm` texture it can draw into.
    fn target(&mut self, frame: &Self::Frame) -> Result<wgpu::Texture>;

    /// Show an 8-bit 4:2:0 Y'CbCr frame (from a Y'CbCr frame pool) to the
    /// GPU as textures it can draw into: its luma plane (`R8Unorm`) and its
    /// half-size plane of interleaved Cb and Cr (`Rg8Unorm`).
    fn ycbcr_target(&mut self, frame: &Self::Frame) -> Result<(wgpu::Texture, wgpu::Texture)>;

    /// [`FrameImporter::import`], but with textures kept for the frame's
    /// surface and given again for every frame in it: capture and decoder
    /// pools recycle a few surfaces, so after the first second every frame
    /// finds its textures (and the compositor their bind groups). The
    /// textures don't keep the frame alive: the caller holds the frame until
    /// the GPU has drawn from it. `None` if the platform can't (then use
    /// `import`).
    fn import_cached(&mut self, frame: &Self::Frame) -> Result<Option<GpuFrame>> {
        let _ = frame;
        Ok(None)
    }

    /// [`FrameImporter::ycbcr_target`], with the textures kept for the
    /// frame's surface in the same way; `None` if the platform can't.
    fn ycbcr_target_cached(
        &mut self,
        frame: &Self::Frame,
    ) -> Result<Option<(wgpu::Texture, wgpu::Texture)>> {
        let _ = frame;
        Ok(None)
    }
}
