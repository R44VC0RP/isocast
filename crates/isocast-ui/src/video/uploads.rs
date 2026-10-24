//! RGBA pixels (a picture decoded to pixels, or the engine's JPEG stills)
//! into surfaces of this process, for a window whose pictures are native
//! layers: they show frames, not Slint images.

#[cfg(windows)]
use anyhow::Context;
use anyhow::Result;
use isocast_client::shared::{LocalRing, SharedFrame, Surface};
#[cfg(windows)]
use slint::wgpu_30::wgpu;

use super::Picture;

/// Shown, the one before, one on its way, and one to fill.
const SLOTS: usize = 4;

#[derive(Default)]
pub struct Uploads {
    ring: Option<LocalRing>,
    /// Each slot's surface as a texture to write (Windows: written on the
    /// renderer's queue).
    #[cfg(windows)]
    textures: Vec<wgpu::Texture>,
}

impl Uploads {
    /// `pixels` as a frame (none: every slot is still in use; skip this
    /// picture).
    pub fn frame(
        &mut self,
        pixels: &Picture,
        revision: Option<u64>,
    ) -> Result<Option<SharedFrame>> {
        let (width, height) = (pixels.width(), pixels.height());
        let fits = self.ring.as_ref().is_some_and(|ring| {
            let surface = &ring.surfaces()[0];
            (surface.width(), surface.height()) == (width, height)
        });
        if !fits {
            self.ring = None;
            self.make(width, height)?;
        }
        let ring = self.ring.as_ref().unwrap();
        let slot = match ring.take() {
            Ok(Some(slot)) => slot,
            Ok(None) => return Ok(None),
            Err(error) => {
                self.ring = None;
                return Err(error);
            }
        };
        // Given back if the write fails.
        let frame = ring.frame(slot, isocast_client::shared::host_now(), revision);
        #[cfg(windows)]
        {
            let (_, queue) = isocast_client::shared::rendering_gpu()
                .context("no renderer registered for shared pictures")?;
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.textures[slot as usize],
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                pixels.as_bytes(),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(width * 4),
                    rows_per_image: Some(height),
                },
                wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
            );
            // Staged writes run at the next submit: now, before the layers
            // read the surface on the same queue.
            queue.submit([]);
        }
        #[cfg(target_os = "macos")]
        write(frame.surface(), pixels)?;
        Ok(Some(frame))
    }

    fn make(&mut self, width: u32, height: u32) -> Result<()> {
        #[cfg(windows)]
        {
            let (device, _) = isocast_client::shared::rendering_gpu()
                .context("no renderer registered for shared pictures")?;
            let surfaces = (0..SLOTS)
                .map(|_| Surface::rgba_on(&device, width, height))
                .collect::<Result<Vec<_>>>()?;
            self.textures = surfaces
                .iter()
                .map(|surface| surface.texture(&device, wgpu::TextureUsages::COPY_DST))
                .collect::<Result<Vec<_>>>()?;
            self.ring = Some(LocalRing::new(surfaces)?);
        }
        #[cfg(target_os = "macos")]
        {
            let surfaces = (0..SLOTS)
                .map(|_| Surface::rgba(width, height))
                .collect::<Result<Vec<_>>>()?;
            self.ring = Some(LocalRing::new(surfaces)?);
        }
        Ok(())
    }
}

/// `pixels` into `surface`'s memory (the same size, RGBA).
#[cfg(target_os = "macos")]
fn write(surface: &Surface, pixels: &Picture) -> Result<()> {
    use objc2_io_surface::IOSurfaceLockOptions;
    let surface = surface.io_surface();
    let none = IOSurfaceLockOptions(0);
    // SAFETY: locked for the CPU while written; nothing else writes it.
    unsafe {
        anyhow::ensure!(
            surface.lock(none, std::ptr::null_mut()) == 0,
            "could not lock a picture surface"
        );
        let base = surface.base_address().as_ptr().cast::<u8>();
        let stride = surface.bytes_per_row();
        let row = pixels.width() as usize * 4;
        for (y, line) in pixels.as_bytes().chunks_exact(row).enumerate() {
            std::ptr::copy_nonoverlapping(line.as_ptr(), base.add(y * stride), row);
        }
        surface.unlock(none, std::ptr::null_mut());
    }
    Ok(())
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn pixels_land_in_a_surface_frame_and_slots_come_back() {
        let mut picture = Picture::new(2, 2);
        picture
            .make_mut_bytes()
            .copy_from_slice(&[1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255]);
        let mut uploads = Uploads::default();
        let frame = uploads.frame(&picture, Some(3)).unwrap().unwrap();
        assert_eq!(
            (frame.width(), frame.height(), frame.revision),
            (2, 2, Some(3))
        );
        let surface = frame.surface().io_surface();
        let stride = surface.bytes_per_row();
        let base = surface.base_address().as_ptr().cast::<u8>();
        // SAFETY: a 2×2 RGBA surface nothing else writes.
        let rows = unsafe {
            [
                std::slice::from_raw_parts(base, 8).to_vec(),
                std::slice::from_raw_parts(base.add(stride), 8).to_vec(),
            ]
        };
        assert_eq!(rows.concat(), picture.as_bytes());
        // Every slot taken: then none until one is dropped.
        let second = uploads.frame(&picture, None).unwrap().unwrap();
        let third = uploads.frame(&picture, None).unwrap().unwrap();
        let fourth = uploads.frame(&picture, None).unwrap().unwrap();
        assert!(uploads.frame(&picture, None).unwrap().is_none());
        drop(frame);
        assert!(uploads.frame(&picture, None).unwrap().is_some());
        drop((second, third, fourth));
    }
}
