//! The frames around a failure, side by side: a 4×3 contact sheet of the
//! part of the window around the pointer, with the pointer (red cross),
//! where the edge should be (cyan) and where it was found (yellow).

use std::path::Path;

use anyhow::{Context, Result};

use super::measure::Image;
use super::runner::Recorded;

const COLUMNS: usize = 4;
const ROWS: usize = 3;
const GAP: u32 = 6;

pub fn write(
    path: &Path,
    recorded: &Recorded,
    expected: &[f32],
    found: &[Option<f32>],
    axis: usize,
    centre: usize,
) -> Result<()> {
    let count = COLUMNS * ROWS;
    let first = centre
        .saturating_sub(count / 2 - 1)
        .min(recorded.images.len().saturating_sub(count));
    let tiles: Vec<usize> = (first..(first + count).min(recorded.images.len())).collect();
    let Some(sample) = tiles.first().map(|&i| &recorded.images[i]) else {
        return Ok(());
    };
    let (tw, th) = (sample.image.width, sample.image.height);
    let (w, h) = (
        COLUMNS as u32 * (tw + GAP) + GAP,
        ROWS as u32 * (th + GAP) + GAP,
    );
    let mut out = vec![0u8; (w * h * 4) as usize];
    for px in out.as_chunks_mut::<4>().0 {
        px.copy_from_slice(&[40, 40, 40, 255]);
    }
    for (n, &i) in tiles.iter().enumerate() {
        let crop = &recorded.images[i];
        let (ox, oy) = (
            GAP + (n % COLUMNS) as u32 * (tw + GAP),
            GAP + (n / COLUMNS) as u32 * (th + GAP),
        );
        // The frame the sheet is about gets a red border.
        if i == centre {
            fill(&mut out, w, ox - 3, oy - 3, tw + 6, th + 6, [230, 40, 40]);
        }
        for y in 0..th.min(crop.image.height) {
            let src = (y * crop.image.width * 4) as usize;
            let dst = (((oy + y) * w + ox) * 4) as usize;
            let len = (tw.min(crop.image.width) * 4) as usize;
            out[dst..dst + len].copy_from_slice(&crop.image.rgba[src..src + len]);
        }
        let s = crop.image.scale;
        let local = |points: f32, along: usize| -> i64 {
            (points * s).round() as i64 - if along == 0 { crop.x } else { crop.y }
        };
        let frame = &recorded.frames[i];
        let mark = |out: &mut Vec<u8>, at: i64, colour: [u8; 3]| {
            if axis == 0 && at >= 0 && at < tw as i64 {
                fill(out, w, ox + at as u32, oy, 1, th, colour);
            } else if axis == 1 && at >= 0 && at < th as i64 {
                fill(out, w, ox, oy + at as u32, tw, 1, colour);
            }
        };
        mark(&mut out, local(expected[i], axis), [0, 220, 255]);
        if let Some(f) = found[i] {
            mark(&mut out, local(f, axis), [255, 220, 0]);
        }
        let (px, py) = (local(frame.pointer.0, 0), local(frame.pointer.1, 1));
        cross(&mut out, w, (ox, oy), (tw, th), (px, py));
    }
    let file =
        std::fs::File::create(path).with_context(|| format!("can't write {}", path.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&out)?;
    writer.finish()?;
    Ok(())
}

fn cross(out: &mut [u8], w: u32, (ox, oy): (u32, u32), (tw, th): (u32, u32), (x, y): (i64, i64)) {
    for d in -6i64..=6 {
        for (cx, cy) in [(x + d, y), (x, y + d)] {
            if cx >= 0 && cy >= 0 && cx < tw as i64 && cy < th as i64 {
                fill(out, w, ox + cx as u32, oy + cy as u32, 1, 1, [255, 40, 40]);
            }
        }
    }
}

fn fill(out: &mut [u8], w: u32, x: u32, y: u32, fw: u32, fh: u32, colour: [u8; 3]) {
    for row in y..y + fh {
        for col in x..x + fw {
            let i = ((row * w + col) * 4) as usize;
            if i + 3 < out.len() {
                out[i..i + 3].copy_from_slice(&colour);
            }
        }
    }
}

/// Every frame's crop as a PNG (`--frames`), for a video.
pub fn write_frames(dir: &Path, recorded: &Recorded) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    for (i, crop) in recorded.images.iter().enumerate() {
        write_png(&dir.join(format!("{i:05}.png")), &crop.image)?;
    }
    Ok(())
}

pub fn write_png(path: &Path, image: &Image) -> Result<()> {
    let file =
        std::fs::File::create(path).with_context(|| format!("can't write {}", path.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), image.width, image.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&image.rgba)?;
    writer.finish()?;
    Ok(())
}
