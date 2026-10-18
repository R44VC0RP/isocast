//! An ISO file's frames on their way to its encoder, when they can't go as
//! captured: run through the source's filters (when the setup asks for
//! `recording.iso.filtered`), or not in 4:2:0 Y'CbCr. Both are drawn into
//! 8-bit 4:2:0 frames of the file's size on the GPU.
//!
//! H.264 and HEVC encoders take only Y'CbCr. Handed BGRA (test patterns,
//! colours, text, images), VideoToolbox converts each frame itself inside
//! its encoder service, and with 16 or more such files at once that
//! conversion deadlocked every file of the recording (S.2 stress,
//! 2026-10-13). Converted here, the encoders get what the program's do.

use anyhow::Result;

use super::SourceFilters;
use crate::filter::Filters;
use crate::gpu::{FrameImporter, Gpu, GpuPixels, InFlight, PendingFrame};
use crate::render::{Layer, Renderer};
use crate::ycbcr::ToYCbCr;
use crate::{FramePool, Platform, Time};

pub(super) struct IsoFrames<P: Platform> {
    id: String,
    gpu: Gpu,
    importer: P::Importer,
    to_ycbcr: ToYCbCr,
    /// Frames of the file's size, handed to its writer. The pool is
    /// bounded, so a stalled encoder drops frames here instead of holding
    /// ever more memory.
    pool: P::FramePool,
    size: (u32, u32),
    /// Every source's filters (which can change while recording), if the
    /// file records its source filtered.
    source_filters: Option<SourceFilters>,
    /// What filtered or rescaled frames are drawn with, made on first use.
    drawing: Option<Drawing>,
    /// When the engine's clock started (mask animations on `clock`).
    epoch: Time,
    in_flight: InFlight,
}

impl<P: Platform> Drop for IsoFrames<P> {
    fn drop(&mut self) {
        let _ = self.in_flight.drain();
    }
}

/// Filters and a picture of the file's size to draw frames on.
struct Drawing {
    renderer: Renderer,
    filters: Filters,
    picture: wgpu::Texture,
    /// What the conversion reads: `picture`.
    picture_bound: wgpu::BindGroup,
}

impl<P: Platform> IsoFrames<P> {
    pub(super) fn device_lost(&self) -> bool {
        self.gpu.is_lost()
    }

    /// Frames of source `id` drawn at `size` with `gpu`, through its
    /// filters if `source_filters` is given.
    pub(super) fn new(
        platform: &P,
        gpu: Gpu,
        id: &str,
        size: (u32, u32),
        source_filters: Option<SourceFilters>,
        epoch: Time,
    ) -> Result<Self> {
        let importer = platform.frame_importer(&gpu)?;
        let pool = platform.ycbcr_frame_pool(size.0, size.1)?;
        let to_ycbcr = ToYCbCr::new(&gpu);
        Ok(Self {
            id: id.to_string(),
            gpu,
            importer,
            to_ycbcr,
            pool,
            size,
            source_filters,
            drawing: None,
            epoch,
            in_flight: InFlight::default(),
        })
    }

    /// `frame`, taken for `at`, through the source's filters if the file
    /// records them, at the file's size, as 4:2:0 Y'CbCr. `None` if every
    /// frame is still in use downstream.
    pub(super) fn draw(
        &mut self,
        frame: &P::Frame,
        at: Time,
    ) -> Result<Option<PendingFrame<P::Frame>>> {
        if !self.in_flight.available()? {
            return Ok(None);
        }
        let Some(out) = self.pool.take()? else {
            return Ok(None);
        };
        let (luma, chroma) = self.importer.ycbcr_target(&out)?;
        let imported = self.importer.import(frame)?;
        let filters = (self.source_filters.as_ref())
            .map(|all| all.lock().get(&self.id).cloned().unwrap_or_default());
        let direct = match &imported.pixels {
            GpuPixels::Rgb(texture)
                if filters.is_none() && (imported.width, imported.height) == self.size =>
            {
                Some(self.to_ycbcr.bind(&self.gpu, texture))
            }
            _ => None,
        };
        let bound = match direct {
            Some(bound) => bound,
            None => {
                let (gpu, to_ycbcr, size) = (&self.gpu, &self.to_ycbcr, self.size);
                let epoch = self.epoch;
                let drawing = self.drawing.get_or_insert_with(|| {
                    let renderer = Renderer::new(gpu, wgpu::TextureFormat::Bgra8Unorm);
                    let mut filters = Filters::new(gpu, renderer.format(), true);
                    filters.set_epoch(epoch);
                    let picture = crate::scene::canvas_texture(gpu, renderer.format(), size);
                    let picture_bound = to_ycbcr.bind(gpu, &picture);
                    Drawing {
                        renderer,
                        filters,
                        picture,
                        picture_bound,
                    }
                });
                let filtered = match &filters {
                    Some(filters) => drawing.filters.apply(
                        gpu,
                        &drawing.renderer,
                        &self.id,
                        imported,
                        filters,
                        at,
                    ),
                    None => imported,
                };
                let layer = Layer::full(&filtered, size.0, size.1);
                drawing
                    .renderer
                    .render(gpu, &drawing.picture, &[layer], size);
                drawing.picture_bound.clone()
            }
        };
        let view = |texture: &wgpu::Texture| texture.create_view(&Default::default());
        let mut encoder = self.gpu.device.create_command_encoder(&Default::default());
        self.to_ycbcr
            .encode(&mut encoder, &bound, &view(&luma), &view(&chroma));
        let done = self.in_flight.submit(&self.gpu, encoder.finish());
        Ok(Some(PendingFrame::new(out, done)))
    }

    pub(super) fn drain(&mut self) -> Result<()> {
        self.in_flight.drain()
    }
}
