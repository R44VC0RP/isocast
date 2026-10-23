//! Bounded CPU frame pools, uploaded to Vulkan for compositing. Rendered
//! outputs keep their texture until the software encoder reads them back.

use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use isocast_engine::gpu::{FrameImporter, Gpu, GpuFrame, GpuPixels};

const MAX_POOL_FRAMES: usize = 32;

#[derive(Clone)]
pub struct Frame(Arc<Storage>);

struct Storage {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    rendered: bool,
    gpu: Mutex<Option<(Gpu, wgpu::Texture)>>,
}

impl Frame {
    pub fn width(&self) -> u32 {
        self.0.width
    }

    pub fn height(&self) -> u32 {
        self.0.height
    }

    fn texture(&self, gpu: &Gpu, target: bool) -> Result<wgpu::Texture> {
        let mut cached = self
            .0
            .gpu
            .lock()
            .map_err(|_| anyhow::anyhow!("Linux frame GPU lock poisoned"))?;
        if self.0.rendered && !target {
            ensure!(
                cached
                    .as_ref()
                    .is_some_and(|(owner, _)| owner.device == gpu.device),
                "rendered Linux frame belongs to an unavailable GPU; wait for a new picture"
            );
        }
        if cached
            .as_ref()
            .is_none_or(|(owner, _)| owner.device != gpu.device)
        {
            // Synthetic source pools outlive a lost device. Recreate their
            // textures for the replacement compositor, never submit a texture
            // from a different device to its queue.
            let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("Linux BGRA frame"),
                size: wgpu::Extent3d {
                    width: self.width(),
                    height: self.height(),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Bgra8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::COPY_SRC
                    | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            *cached = Some((gpu.clone(), texture));
        }
        Ok(cached.as_ref().expect("created texture").1.clone())
    }

    /// Packed BGRA. A CPU source needs no GPU round trip. Render targets
    /// are handed here only after the engine's compositor fence completes.
    pub fn read_bgra(&self) -> Result<Vec<u8>> {
        if !self.0.rendered {
            return Ok(self.0.pixels.clone());
        }
        let (gpu, texture) = self
            .0
            .gpu
            .lock()
            .map_err(|_| anyhow::anyhow!("Linux frame GPU lock poisoned"))?
            .clone()
            .context("frame was never rendered")?;
        let row = self.width() * 4;
        let padded =
            row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Linux software encoder readback"),
            size: u64::from(padded) * u64::from(self.height()),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut commands = gpu.device.create_command_encoder(&Default::default());
        commands.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(self.height()),
                },
            },
            texture.size(),
        );
        let submission = gpu.queue.submit([commands.finish()]);
        let (send, receive) = mpsc::sync_channel(1);
        buffer
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = send.send(result);
            });
        gpu.device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: Some(Duration::from_secs(2)),
            })
            .context("Linux frame readback GPU wait")?;
        receive
            .recv_timeout(Duration::from_secs(2))
            .context("Linux frame readback callback timed out")??;
        let mapped = buffer
            .slice(..)
            .get_mapped_range()
            .context("mapping Linux frame readback")?;
        let mut pixels = Vec::with_capacity(self.width() as usize * self.height() as usize * 4);
        for line in mapped.chunks_exact(padded as usize) {
            pixels.extend_from_slice(&line[..row as usize]);
        }
        drop(mapped);
        buffer.unmap();
        Ok(pixels)
    }
}

pub struct FramePool {
    width: u32,
    height: u32,
    len: usize,
    buffers: Vec<Arc<Storage>>,
}

impl FramePool {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        ensure!(
            width > 0 && height > 0,
            "frame pool: dimensions must be positive"
        );
        // Matches the compositor's portable device limit and also bounds
        // every row-size conversion and allocation below.
        ensure!(
            width <= 8192 && height <= 8192,
            "frame pool: dimensions exceed 8192x8192"
        );
        Ok(Self {
            width,
            height,
            len: width as usize * height as usize * 4,
            buffers: Vec::new(),
        })
    }

    fn free(&mut self) -> Result<Option<&mut Arc<Storage>>> {
        if let Some(index) = self
            .buffers
            .iter()
            .position(|frame| Arc::strong_count(frame) == 1)
        {
            return Ok(Some(&mut self.buffers[index]));
        }
        if self.buffers.len() == MAX_POOL_FRAMES {
            return Ok(None);
        }
        self.buffers.push(Arc::new(Storage {
            width: self.width,
            height: self.height,
            pixels: Vec::new(),
            rendered: false,
            gpu: Mutex::new(None),
        }));
        Ok(self.buffers.last_mut())
    }
}

impl isocast_engine::FramePool for FramePool {
    type Frame = Frame;

    fn draw(&mut self, draw: &mut dyn FnMut(&mut [u8], usize)) -> Result<Option<Frame>> {
        let len = self.len;
        let Some(buffer) = self.free()? else {
            return Ok(None);
        };
        let storage = Arc::get_mut(buffer).context("Linux frame pool: buffer is still shared")?;
        if storage.pixels.is_empty() {
            storage
                .pixels
                .try_reserve_exact(len)
                .context("allocating Linux CPU frame")?;
            storage.pixels.resize(len, 0);
        }
        storage.rendered = false;
        draw(&mut storage.pixels, storage.width as usize * 4);
        Ok(Some(Frame(buffer.clone())))
    }

    fn take(&mut self) -> Result<Option<Frame>> {
        let Some(buffer) = self.free()? else {
            return Ok(None);
        };
        let storage = Arc::get_mut(buffer).context("Linux frame pool: buffer is still shared")?;
        storage.pixels = Vec::new();
        storage.rendered = true;
        Ok(Some(Frame(buffer.clone())))
    }
}

pub struct Importer(pub Gpu);

impl FrameImporter for Importer {
    type Frame = Frame;

    fn import(&mut self, frame: &Frame) -> Result<GpuFrame> {
        let texture = frame.texture(&self.0, false)?;
        if !frame.0.rendered {
            self.0.queue.write_texture(
                texture.as_image_copy(),
                &frame.0.pixels,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(frame.width() * 4),
                    rows_per_image: Some(frame.height()),
                },
                texture.size(),
            );
        }
        Ok(GpuFrame {
            width: frame.width(),
            height: frame.height(),
            pixels: if frame.0.rendered {
                GpuPixels::Premultiplied(texture.clone())
            } else {
                GpuPixels::Rgb(texture.clone())
            },
        })
    }

    fn target(&mut self, frame: &Frame) -> Result<wgpu::Texture> {
        ensure!(
            frame.0.rendered,
            "Linux frame target must come from FramePool::take"
        );
        frame.texture(&self.0, true)
    }

    fn import_cached(&mut self, frame: &Frame) -> Result<Option<GpuFrame>> {
        self.import(frame).map(Some)
    }

    fn target_cached(&mut self, frame: &Frame) -> Result<Option<wgpu::Texture>> {
        self.target(frame).map(Some)
    }

    fn ycbcr_target(&mut self, _frame: &Frame) -> Result<(wgpu::Texture, wgpu::Texture)> {
        bail!("Linux bootstrap uses BGRA targets; YCbCr GPU targets are not supported")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use isocast_engine::FramePool as _;

    #[test]
    fn pool_is_bounded_and_does_not_reuse_live_pixels() {
        let mut pool = FramePool::new(16, 16).unwrap();
        let frames: Vec<_> = (0..MAX_POOL_FRAMES)
            .map(|n| {
                pool.draw(&mut |pixels, _| pixels.fill(n as u8))
                    .unwrap()
                    .unwrap()
            })
            .collect();
        assert!(pool.take().unwrap().is_none());
        for (n, frame) in frames.iter().enumerate() {
            assert!(frame.read_bgra().unwrap().iter().all(|v| *v == n as u8));
        }
        drop(frames);
        assert!(pool.take().unwrap().is_some());
    }

    #[test]
    fn render_targets_do_not_keep_cpu_pixels() {
        let mut pool = FramePool::new(16, 16).unwrap();
        let target = pool.take().unwrap().unwrap();
        assert_eq!(target.0.pixels.capacity(), 0);
        drop(target);
        let source = pool
            .draw(&mut |pixels, _| pixels.fill(42))
            .unwrap()
            .unwrap();
        assert_eq!(source.read_bgra().unwrap(), vec![42; 16 * 16 * 4]);
        drop(source);
        let target = pool.take().unwrap().unwrap();
        assert_eq!(target.0.pixels.capacity(), 0);
    }

    #[test]
    fn vulkan_target_readback_preserves_bgra_and_row_padding() {
        let gpu = Gpu::new().unwrap();
        let mut importer = Importer(gpu.clone());
        let frame = FramePool::new(18, 10).unwrap().take().unwrap().unwrap();
        let texture = importer.target(&frame).unwrap();
        let view = texture.create_view(&Default::default());
        let mut commands = gpu.device.create_command_encoder(&Default::default());
        {
            let _pass = commands.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Linux readback test"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 1.0,
                            g: 0.0,
                            b: 0.0,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
        }
        gpu.queue.submit([commands.finish()]);
        let pixels = frame.read_bgra().unwrap();
        assert_eq!(pixels.len(), 18 * 10 * 4);
        assert!(
            pixels
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| *p == [0, 0, 255, 255])
        );
    }
}
