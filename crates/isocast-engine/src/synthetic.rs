//! Synthetic sources that need no devices or permissions.
//!
//! Both are driven by the session clock: the test pattern flashes and the
//! tone beeps for [`PULSE`] at every whole second since the session started,
//! so files recorded from them can be checked for sync.

use std::f64::consts::TAU;
use std::time::Duration;

use crate::SAMPLE_RATE;

/// How long each flash and beep lasts.
pub const PULSE: Duration = Duration::from_millis(100);

/// 75% colour bars, as BGRA.
const BARS: [[u8; 4]; 8] = [
    [191, 191, 191, 255], // white
    [0, 191, 191, 255],   // yellow
    [191, 191, 0, 255],   // cyan
    [0, 191, 0, 255],     // green
    [191, 0, 191, 255],   // magenta
    [0, 0, 191, 255],     // red
    [191, 0, 0, 255],     // blue
    [0, 0, 0, 255],       // black
];
const WHITE: [u8; 4] = [255, 255, 255, 255];
const BLACK: [u8; 4] = [0, 0, 0, 255];

/// Seconds a bar takes to scroll across the whole frame.
const SCROLL_SECONDS: usize = 4;

/// Moving colour bars with the source label, frame number and session time
/// burnt in, and a band along the bottom that flashes white on every pulse.
pub struct TestPattern {
    label: String,
    width: usize,
    height: usize,
    fps: u32,
    /// Two widths of bars, so any scrolled row is one contiguous slice.
    bars: Vec<u8>,
    /// Size of one font pixel.
    scale: usize,
}

impl TestPattern {
    pub fn new(label: &str, width: u32, height: u32, fps: u32) -> Self {
        let (width, height) = (width as usize, height as usize);
        let mut bars = Vec::with_capacity(width * 2 * 4);
        for x in 0..width * 2 {
            bars.extend_from_slice(&BARS[(x % width) * BARS.len() / width]);
        }
        Self {
            label: label.to_uppercase(),
            width,
            height,
            fps,
            bars,
            scale: (height / 120).max(1),
        }
    }

    /// Draw frame number `frame`, shown `since_start` after the session
    /// started, into a BGRA buffer with `stride` bytes per row.
    pub fn draw(&self, frame: u64, since_start: Duration, pixels: &mut [u8], stride: usize) {
        let (w, h, s) = (self.width, self.height, self.scale);

        let shift = (frame as usize * w / (SCROLL_SECONDS * self.fps as usize)) % w;
        let row = &self.bars[shift * 4..(shift + w) * 4];
        for y in 0..h {
            pixels[y * stride..y * stride + w * 4].copy_from_slice(row);
        }

        let flash = since_start.subsec_nanos() < PULSE.as_nanos() as u32;
        let band = h / 4;
        let color = if flash { WHITE } else { BLACK };
        fill_rect(pixels, stride, (0, h - band), (w, band), color);

        let total_ms = since_start.as_millis();
        let lines = [
            self.label.clone(),
            format!("FRAME {frame:06}"),
            format!(
                "T+{:02}:{:02}:{:02}.{:03}",
                total_ms / 3_600_000,
                total_ms / 60_000 % 60,
                total_ms / 1000 % 60,
                total_ms % 1000
            ),
        ];
        let longest = lines.iter().map(|l| l.len()).max().unwrap_or(0);
        let (pad, advance, line_height) = (2 * s, 6 * s, 9 * s);
        let origin = 4 * s;
        let box_size = (
            longest * advance + 2 * pad - s,
            lines.len() * line_height + 2 * pad - 2 * s,
        );
        fill_rect(pixels, stride, (origin, origin), box_size, BLACK);
        for (i, line) in lines.iter().enumerate() {
            let y = origin + pad + i * line_height;
            for (j, c) in line.chars().enumerate() {
                self.draw_glyph(pixels, stride, (origin + pad + j * advance, y), c);
            }
        }
    }

    fn draw_glyph(&self, pixels: &mut [u8], stride: usize, (x, y): (usize, usize), c: char) {
        let s = self.scale;
        for (r, bits) in glyph(c).iter().enumerate() {
            for col in 0..5 {
                if bits & (0b10000 >> col) != 0 {
                    fill_rect(pixels, stride, (x + col * s, y + r * s), (s, s), WHITE);
                }
            }
        }
    }
}

/// Fill a rectangle, clipped to the frame.
fn fill_rect(
    pixels: &mut [u8],
    stride: usize,
    (x, y): (usize, usize),
    (w, h): (usize, usize),
    color: [u8; 4],
) {
    let frame_width = stride / 4;
    let frame_height = pixels.len() / stride;
    let (x_end, y_end) = ((x + w).min(frame_width), (y + h).min(frame_height));
    if x >= x_end || y >= y_end {
        return;
    }
    let first = y * stride + x * 4..y * stride + x_end * 4;
    for px in pixels[first.clone()].as_chunks_mut::<4>().0 {
        px.copy_from_slice(&color);
    }
    for row in y + 1..y_end {
        pixels.copy_within(first.clone(), row * stride + x * 4);
    }
}

/// A 5×7 bitmap glyph; each row's low five bits are its pixels, left to
/// right. Unknown characters draw as `?`.
fn glyph(c: char) -> [u8; 7] {
    match c {
        '0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        '1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        '2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        '3' => [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
        '4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        '5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        '6' => [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        '7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        '9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
        'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'B' => [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E],
        'C' => [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
        'D' => [0x1C, 0x12, 0x11, 0x11, 0x11, 0x12, 0x1C],
        'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        'F' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
        'G' => [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F],
        'H' => [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'I' => [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
        'J' => [0x07, 0x02, 0x02, 0x02, 0x02, 0x12, 0x0C],
        'K' => [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
        'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
        'M' => [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
        'N' => [0x11, 0x11, 0x19, 0x15, 0x13, 0x11, 0x11],
        'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'P' => [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
        'Q' => [0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D],
        'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
        'S' => [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
        'T' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04],
        'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x15, 0x0A],
        'X' => [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11],
        'Y' => [0x11, 0x11, 0x11, 0x0A, 0x04, 0x04, 0x04],
        'Z' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F],
        ':' => [0x00, 0x0C, 0x0C, 0x00, 0x0C, 0x0C, 0x00],
        '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C],
        '-' => [0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00],
        '+' => [0x00, 0x04, 0x04, 0x1F, 0x04, 0x04, 0x00],
        ' ' => [0; 7],
        _ => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x00, 0x04],
    }
}

/// Stereo silence with a sine beep for [`PULSE`] at every whole second.
pub struct Tone {
    frequency: f64,
}

impl Tone {
    pub const CHANNELS: u32 = 2;

    pub fn new(frequency: u32) -> Self {
        Self {
            frequency: frequency as f64,
        }
    }

    /// Fill `out` with interleaved samples, the first being sample number
    /// `first` since the session started.
    pub fn fill(&self, first: u64, out: &mut [f32]) {
        let rate = SAMPLE_RATE as u64;
        let pulse = rate * PULSE.as_millis() as u64 / 1000;
        for (i, frame) in out
            .as_chunks_mut::<{ Self::CHANNELS as usize }>()
            .0
            .iter_mut()
            .enumerate()
        {
            let into_second = (first + i as u64) % rate;
            let value = if into_second < pulse {
                let t = into_second as f64 / rate as f64;
                ((TAU * self.frequency * t).sin() * 0.5) as f32
            } else {
                0.0
            };
            frame.fill(value);
        }
    }
}
