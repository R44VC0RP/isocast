//! Image files decoded to straight-alpha RGBA, for image and slide show
//! sources: PNG, JPEG (turned upright), GIF (every frame of an animation),
//! WebP, BMP, TGA and SVG.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use image::{AnimationDecoder, ImageDecoder, RgbaImage};
use resvg::{tiny_skia, usvg};

/// A picture in straight-alpha RGBA.
#[derive(Clone)]
pub(crate) struct Picture {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) rgba: Vec<u8>,
}

impl Picture {
    fn from_image(image: RgbaImage) -> Self {
        Self {
            width: image.width(),
            height: image.height(),
            rgba: image.into_raw(),
        }
    }

    /// Fitted inside `width`×`height` without changing its shape, centred,
    /// transparent around it.
    pub(crate) fn fit(&self, width: u32, height: u32) -> Picture {
        if (self.width, self.height) == (width, height) {
            return self.clone();
        }
        let scale = (width as f64 / self.width as f64).min(height as f64 / self.height as f64);
        let w = ((self.width as f64 * scale).round() as u32).clamp(1, width);
        let h = ((self.height as f64 * scale).round() as u32).clamp(1, height);
        let source = RgbaImage::from_raw(self.width, self.height, self.rgba.clone())
            .expect("sized to match");
        let resized = image::imageops::resize(&source, w, h, image::imageops::FilterType::Triangle);
        let mut out = RgbaImage::new(width, height);
        image::imageops::overlay(
            &mut out,
            &resized,
            i64::from((width - w) / 2),
            i64::from((height - h) / 2),
        );
        Picture::from_image(out)
    }

    /// `self` faded `t` (0 to 1) of the way to `other`, the same size.
    pub(crate) fn blend(&self, other: &Picture, t: f32) -> Picture {
        let rgba = self
            .rgba
            .iter()
            .zip(&other.rgba)
            .map(|(&a, &b)| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8)
            .collect();
        Picture {
            width: self.width,
            height: self.height,
            rgba,
        }
    }

    /// Copy into a BGRA frame with `stride` bytes per row.
    pub(crate) fn write_bgra(&self, out: &mut [u8], stride: usize) {
        let width = self.width as usize;
        for (y, row) in self.rgba.chunks_exact(width * 4).enumerate() {
            let line = &mut out[y * stride..y * stride + width * 4];
            for (to, from) in line
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(row.as_chunks::<4>().0)
            {
                *to = [from[2], from[1], from[0], from[3]];
            }
        }
    }
}

/// The frames of an image file, each with how long it shows: one for a
/// still image, several for an animated GIF. `size` is the size to draw an
/// SVG at.
pub(crate) fn load(path: &Path, size: Option<(u32, u32)>) -> Result<Vec<(Picture, Duration)>> {
    let shown = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("svg" | "svgz") => {
            let data = std::fs::read(path).with_context(|| shown(&"could not read it"))?;
            let mut options = usvg::Options {
                resources_dir: path.parent().map(Into::into),
                ..Default::default()
            };
            options.fontdb_mut().load_system_fonts();
            let tree =
                usvg::Tree::from_data(&data, &options).map_err(|e| anyhow::anyhow!(shown(&e)))?;
            let natural = tree.size();
            let (width, height) = size.unwrap_or((
                natural.width().ceil() as u32,
                natural.height().ceil() as u32,
            ));
            let mut pixmap = tiny_skia::Pixmap::new(width.max(1), height.max(1))
                .with_context(|| shown(&"it has no size"))?;
            let transform = tiny_skia::Transform::from_scale(
                width as f32 / natural.width(),
                height as f32 / natural.height(),
            );
            resvg::render(&tree, transform, &mut pixmap.as_mut());
            let picture = Picture {
                width: pixmap.width(),
                height: pixmap.height(),
                rgba: pixmap.take_demultiplied(),
            };
            Ok(vec![(picture, Duration::ZERO)])
        }
        Some("gif") => {
            let file = File::open(path).with_context(|| shown(&"could not open it"))?;
            let decoder = image::codecs::gif::GifDecoder::new(BufReader::new(file))
                .map_err(|e| anyhow::anyhow!(shown(&e)))?;
            let frames = decoder
                .into_frames()
                .collect_frames()
                .map_err(|e| anyhow::anyhow!(shown(&e)))?;
            if frames.is_empty() {
                bail!(shown(&"it has no frames"));
            }
            Ok(frames
                .into_iter()
                .map(|frame| {
                    let (numerator, denominator) = frame.delay().numer_denom_ms();
                    // Browsers show very short GIF delays as 100 ms.
                    let ms = (numerator / denominator.max(1)).max(20);
                    let delay = if ms <= 10 { 100 } else { ms };
                    let picture = Picture::from_image(frame.into_buffer());
                    (picture, Duration::from_millis(delay.into()))
                })
                .collect())
        }
        _ => {
            let reader = image::ImageReader::open(path)
                .with_context(|| shown(&"could not open it"))?
                .with_guessed_format()
                .with_context(|| shown(&"could not read it"))?;
            let mut decoder = reader
                .into_decoder()
                .map_err(|e| anyhow::anyhow!(shown(&e)))?;
            let orientation = decoder
                .orientation()
                .map_err(|e| anyhow::anyhow!(shown(&e)))?;
            let mut image = image::DynamicImage::from_decoder(decoder)
                .map_err(|e| anyhow::anyhow!(shown(&e)))?;
            image.apply_orientation(orientation);
            Ok(vec![(
                Picture::from_image(image.into_rgba8()),
                Duration::ZERO,
            )])
        }
    }
}
