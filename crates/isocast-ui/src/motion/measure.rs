//! What a frame showed (where the dragged thing is: in the interface's
//! state, in the box drawn, in the pixels), and the verdict on a run.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};

use super::runner::{Recorded, Runner};
use super::scenario::Run;

/// One frame of a run. Positions are in window points along the tracked
/// axis (x for column splitters and the canvas, y for the row splitter).
#[derive(Debug, Clone, Serialize)]
pub struct Frame {
    pub index: usize,
    pub t: f64,
    pub pointer: (f32, f32),
    pub pressed: bool,
    /// Where the button went down and up during this frame, if it did.
    pub press: Option<(f32, f32)>,
    pub release: Option<(f32, f32)>,
    /// The edge in the interface's state (layout, or the newest setup).
    pub model: Option<f32>,
    /// Canvas only: the box drawn for the frame shown.
    pub shown: Option<f32>,
    /// The edge found in the frame's pixels.
    pub pixel: Option<f32>,
    /// Canvas only: the drawn (test) outline found in the pixels.
    pub outline: Option<f32>,
    /// The interface's own work for the frame (inputs, timers, updates,
    /// drawing commands), and the frame finished: that plus the GPU's
    /// drawing and the read back. Wall time; not checked on CI.
    pub cpu_ms: f64,
    pub frame_ms: f64,
    pub gpu_ms: f64,
    pub engine_wait_ms: f64,
    pub late: bool,
    pub redraws_requested: u64,
    /// Calls to the engine made during the frame, and their JSON size.
    pub calls: u64,
    pub call_bytes: u64,
    /// Engine frames that arrived during the frame.
    pub engine_frames: u64,
    /// Setup revisions, counted from the replay's start.
    pub picture_revision: Option<i64>,
    pub setup_revision: i64,
    pub black_picture: bool,
    pub blank_window: bool,
    /// 16 px tiles that went back to how they were two frames before.
    pub aba_tiles: u32,
    /// The first of them (top-left, window points).
    pub aba_at: Vec<[u32; 2]>,
    /// The whole frame's pixels, hashed (two lockstep runs must match).
    pub frame_hash: String,
    /// The engine frame's size, and the size the preview last asked for.
    pub picture_size: Option<(u32, u32)>,
    pub asked_size: Option<(u32, u32)>,
}

/// A frame's pixels (RGBA8), or a part of them.
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    pub rgba: Vec<u8>,
}

/// Where a crop came from, in the frame's pixels.
pub struct Crop {
    pub image: Image,
    pub x: i64,
    pub y: i64,
}

impl Image {
    pub fn at_px(&self, x: i64, y: i64) -> Option<[u8; 3]> {
        if x < 0 || y < 0 || x >= self.width as i64 || y >= self.height as i64 {
            return None;
        }
        let i = (y as usize * self.width as usize + x as usize) * 4;
        Some([self.rgba[i], self.rgba[i + 1], self.rgba[i + 2]])
    }

    pub fn hash(&self) -> String {
        let mut h = 0xcbf29ce484222325u64;
        for b in &self.rgba {
            h = (h ^ *b as u64).wrapping_mul(0x100000001b3);
        }
        format!("{h:016x}")
    }

    pub fn tile_hashes(&self, tile: u32) -> Vec<u64> {
        let tile = ((tile as f32 * self.scale) as u32).max(1);
        let (cols, rows) = (self.width.div_ceil(tile), self.height.div_ceil(tile));
        let mut hashes = vec![0xcbf29ce484222325u64; (cols * rows) as usize];
        for y in 0..self.height {
            let row = (y / tile) * cols;
            let line =
                &self.rgba[(y * self.width * 4) as usize..((y + 1) * self.width * 4) as usize];
            for (x, px) in line.as_chunks::<4>().0.iter().enumerate() {
                let h = &mut hashes[(row + x as u32 / tile) as usize];
                let v = u32::from_le_bytes([px[0], px[1], px[2], 0]) as u64;
                *h = (*h ^ v).wrapping_mul(0x100000001b3);
            }
        }
        hashes
    }

    /// How much of `rect` (window points) is black.
    pub fn black_fraction(&self, rect: [f32; 4]) -> f64 {
        let s = self.scale;
        let (x0, y0) = ((rect[0] * s) as i64, (rect[1] * s) as i64);
        let (x1, y1) = (
            ((rect[0] + rect[2]) * s) as i64,
            ((rect[1] + rect[3]) * s) as i64,
        );
        let (mut black, mut all) = (0u64, 0u64);
        for y in (y0..y1).step_by(4) {
            for x in (x0..x1).step_by(4) {
                if let Some(c) = self.at_px(x, y) {
                    all += 1;
                    if c.iter().map(|v| *v as u32).sum::<u32>() < 12 {
                        black += 1;
                    }
                }
            }
        }
        if all == 0 {
            0.0
        } else {
            black as f64 / all as f64
        }
    }

    /// How much of the window is its most common colour (a blank frame).
    pub fn uniform_fraction(&self) -> f64 {
        let mut counts = std::collections::HashMap::<[u8; 3], u64>::new();
        let mut all = 0;
        for y in (0..self.height as i64).step_by(8) {
            for x in (0..self.width as i64).step_by(8) {
                if let Some(c) = self.at_px(x, y) {
                    *counts.entry(c).or_default() += 1;
                    all += 1;
                }
            }
        }
        counts.values().max().copied().unwrap_or(0) as f64 / all.max(1) as f64
    }

    /// `size` points around `centre`, for the contact sheet.
    pub fn crop_around(&self, centre: (f32, f32), size: (f32, f32)) -> Crop {
        let s = self.scale;
        let (w, h) = (
            ((size.0 * s) as u32).min(self.width),
            ((size.1 * s) as u32).min(self.height),
        );
        let x = ((centre.0 * s) as i64 - w as i64 / 2).clamp(0, (self.width - w) as i64);
        let y = ((centre.1 * s) as i64 - h as i64 / 2).clamp(0, (self.height - h) as i64);
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for row in y..y + h as i64 {
            let start = ((row as u32 * self.width + x as u32) * 4) as usize;
            rgba.extend_from_slice(&self.rgba[start..start + (w * 4) as usize]);
        }
        Crop {
            image: Image {
                width: w,
                height: h,
                scale: s,
                rgba,
            },
            x,
            y,
        }
    }

    fn whole(self) -> Crop {
        Crop {
            image: self,
            x: 0,
            y: 0,
        }
    }
}

fn near(a: [u8; 3], b: [u8; 3], tolerance: u8) -> bool {
    a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= tolerance)
}

fn magenta(c: [u8; 3]) -> bool {
    c[0] > 190 && c[1] < 90 && c[2] > 190
}

/// The offscreen test picture's item: verdigris, or its lighter checker.
fn verdigris(c: [u8; 3]) -> bool {
    near(c, crate::picture::ITEM_RGB, 6) || near(c, [86, 169, 150], 6)
}

fn green(c: [u8; 3]) -> bool {
    c[1] > 190 && c[0] < 110 && c[2] < 110
}

pub struct Probe {
    pub model: Option<f32>,
    pub shown: Option<f32>,
    pub pixel: Option<f32>,
    pub outline: Option<f32>,
}

/// What is dragged, and how to find it in a frame.
pub enum Tracker {
    /// A column splitter: its 1px hairline (Theme.line), followed from
    /// frame to frame along a row through the docks.
    Column {
        n: usize,
        scan_y: Option<f32>,
        last: Option<f32>,
    },
    /// The row splitter above the docks: the edge's profile down a column,
    /// taken at rest, matched in each frame.
    Row {
        scan_x: Option<f32>,
        template: Option<Vec<[u8; 3]>>,
        last: Option<f32>,
    },
    /// The selected item on the canvas: its right edge (`corner`, resized
    /// by the bottom-right handle) or left edge (moved), in the picture
    /// (the setup's magenta block) and in the box drawn.
    Canvas {
        corner: bool,
        /// The engine's picture (the magenta block) or the test picture
        /// (verdigris).
        engine: bool,
        scan_y: Option<f32>,
    },
    None,
}

const TEMPLATE: i64 = 8;

impl Tracker {
    pub fn new(target: &str, engine: bool) -> Result<Self> {
        let (kind, arg) = target.split_once(':').unwrap_or((target, ""));
        Ok(match kind {
            "splitter" => Tracker::Column {
                n: arg.parse().unwrap_or(0),
                scan_y: None,
                last: None,
            },
            "row-splitter" => Tracker::Row {
                scan_x: None,
                template: None,
                last: None,
            },
            "canvas-corner" => Tracker::Canvas {
                corner: true,
                engine,
                scan_y: None,
            },
            "canvas-move" => Tracker::Canvas {
                corner: false,
                engine,
                scan_y: None,
            },
            "none" => Tracker::None,
            other => bail!(
                "target {other:?} isn't one of splitter:N, row-splitter, canvas-corner, canvas-move, none"
            ),
        })
    }

    /// The line the pixels are searched along (window points: a row's y,
    /// or for the row splitter a column's x), once found.
    pub fn scan_line(&self) -> Option<f32> {
        match self {
            Tracker::Column { scan_y, .. } => *scan_y,
            Tracker::Row { scan_x, .. } => *scan_x,
            Tracker::Canvas { scan_y, .. } => *scan_y,
            Tracker::None => None,
        }
    }

    /// The axis tracked: 0 is x, 1 is y.
    pub fn axis(&self) -> usize {
        matches!(self, Tracker::Row { .. }) as usize
    }

    /// Where the pointer takes hold of the target.
    pub fn grab(&self, r: &Runner) -> Option<(f32, f32)> {
        match self {
            Tracker::Column { n, .. } => {
                let [x, y, _, h] = r.element("ColumnSplitter", *n)?;
                Some((x + 0.5, y + h / 2.0))
            }
            Tracker::Row { .. } => {
                let [x, y, w, _] = r.element("RowSplitter", 0)?;
                Some((x + w / 2.0, y))
            }
            Tracker::Canvas { corner, .. } => {
                let [x, y, w, h] = r.canvas_box(false);
                if w <= 0.0 {
                    return None;
                }
                Some(if *corner {
                    r.canvas_to_window(x + w, y + h)
                } else {
                    r.canvas_to_window(x + w / 2.0, y + h / 2.0)
                })
            }
            Tracker::None => {
                let app = r.app();
                Some((
                    app.get_preview_width() / 2.0,
                    app.get_preview_height() / 2.0,
                ))
            }
        }
    }

    /// The window area (points) the dragged thing covers, when it is a
    /// drawn object: the item's box as set and as shown. Its own texture
    /// moving (a checker) can repeat a tile two frames apart, which isn't
    /// flicker; whether the object itself keeps up is the gap's business.
    pub fn moving_region(&self, r: &Runner) -> Option<[f32; 4]> {
        let Tracker::Canvas { .. } = self else {
            return None;
        };
        let to_window = |[x, y, w, h]: [f32; 4]| {
            let (x0, y0) = r.canvas_to_window(x, y);
            let (x1, y1) = r.canvas_to_window(x + w, y + h);
            [x0, y0, x1, y1]
        };
        let (a, b) = (
            to_window(r.canvas_box(false)),
            to_window(r.canvas_box(true)),
        );
        Some([
            a[0].min(b[0]),
            a[1].min(b[1]),
            a[2].max(b[2]),
            a[3].max(b[3]),
        ])
    }

    pub fn probe(&mut self, r: &Runner, image: &Image) -> Probe {
        let mut probe = Probe {
            model: None,
            shown: None,
            pixel: None,
            outline: None,
        };
        match self {
            Tracker::Column { n, scan_y, last } => {
                let Some([x, y, _, h]) = r.element("ColumnSplitter", *n) else {
                    return probe;
                };
                probe.model = Some(x);
                let scan = *scan_y.get_or_insert(y + h / 2.0);
                let line = r.theme_line();
                let from = last.unwrap_or(x);
                // The nearest hairline to where it was: a column of the
                // line's colour 9 points tall.
                let is_line = |px: i64| {
                    let y0 = (scan * image.scale) as i64;
                    (-4..=4).all(|dy| {
                        image
                            .at_px(px, y0 + (dy as f32 * image.scale) as i64)
                            .is_some_and(|c| near(c, line, 2))
                    })
                };
                let start = (from * image.scale).round() as i64;
                let reach = (240.0 * image.scale) as i64;
                for d in 0..reach {
                    for px in [start - d, start + d] {
                        if is_line(px) {
                            let found = px as f32 / image.scale;
                            probe.pixel = Some(found);
                            *last = Some(found);
                            return probe;
                        }
                    }
                }
            }
            Tracker::Row {
                scan_x,
                template,
                last,
            } => {
                let Some([x, y, w, _]) = r.element("RowSplitter", 0) else {
                    return probe;
                };
                probe.model = Some(y);
                let scan = *scan_x.get_or_insert(x + w / 4.0);
                let col = (scan * image.scale) as i64;
                let profile = |centre: i64| -> Option<Vec<[u8; 3]>> {
                    (-TEMPLATE..=TEMPLATE)
                        .map(|d| image.at_px(col, centre + d))
                        .collect()
                };
                let Some(template) = template else {
                    *template = profile((y * image.scale).round() as i64);
                    probe.pixel = Some(y);
                    *last = Some(y);
                    return probe;
                };
                let start = (last.unwrap_or(y) * image.scale).round() as i64;
                let reach = (200.0 * image.scale) as i64;
                let mut best: Option<(u64, i64)> = None;
                for d in 0..reach {
                    for centre in [start - d, start + d] {
                        let Some(p) = profile(centre) else { continue };
                        let cost: u64 = p
                            .iter()
                            .zip(template.iter())
                            .map(|(a, b)| {
                                a.iter()
                                    .zip(b)
                                    .map(|(x, y)| x.abs_diff(*y) as u64)
                                    .sum::<u64>()
                            })
                            .sum();
                        // Strictly better only: ties go to the nearest.
                        if best.is_none_or(|(c, _)| cost < c) {
                            best = Some((cost, centre));
                        }
                    }
                    if best.is_some_and(|(c, _)| c == 0) {
                        break;
                    }
                }
                if let Some((cost, centre)) = best
                    && cost < 40 * (2 * TEMPLATE as u64 + 1)
                {
                    let found = centre as f32 / image.scale;
                    probe.pixel = Some(found);
                    *last = Some(found);
                }
            }
            Tracker::Canvas {
                corner,
                engine,
                scan_y,
            } => {
                let [bx, by, bw, _] = r.canvas_box(false);
                let [sx, sy, sw, sh] = r.canvas_box(true);
                let edge = |x: f32, w: f32| if *corner { x + w } else { x };
                probe.model = Some(r.canvas_to_window(edge(bx, bw), by).0);
                probe.shown = Some(r.canvas_to_window(edge(sx, sw), sy).0);
                // A row through the item where the frame shows it, clear
                // of the handles, each frame (the item may move up or down).
                let scan = r.canvas_to_window(sx, sy + sh * 0.3).1;
                *scan_y = Some(scan);
                let anchor = r.canvas_to_window(sx + sw * 0.5, sy).0;
                let step = if *corner { 1 } else { -1 };
                let is_item = if *engine { magenta } else { verdigris };
                let y = (scan * image.scale) as i64;
                let mut px = (anchor * image.scale) as i64;
                if !image.at_px(px, y).is_some_and(is_item) {
                    return probe;
                }
                while image.at_px(px + step, y).is_some_and(is_item) {
                    px += step;
                }
                // The edge between the last block pixel and the next.
                let picture_edge = if *corner { px + 1 } else { px };
                probe.pixel = Some(picture_edge as f32 / image.scale);
                let reach = (40.0 * image.scale) as i64;
                probe.outline = (1..reach)
                    .map(|d| picture_edge + step * d)
                    .find(|p| image.at_px(*p, y).is_some_and(green))
                    .map(|p| p as f32 / image.scale);
            }
            Tracker::None => {}
        }
        probe
    }
}

fn percentile(values: &[f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    let i = ((v.len() - 1) as f64 * p).round() as usize;
    v[i]
}

fn round(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// How a series of edge positions followed the pointer. Positions and
/// gaps are in window points.
#[derive(Debug, Serialize)]
pub struct Follow {
    pub frames: usize,
    /// How far the edge travelled per point the pointer travelled over the
    /// gesture, from the rest frame (1 follows, late or not; none if the
    /// pointer didn't move).
    pub gain: Option<f64>,
    /// The delay that best lines the edge up with the pointer.
    pub lag_frames: usize,
    pub gap_pt_median: f64,
    pub gap_pt_p95: f64,
    pub gap_pt_max: f64,
    /// The gap left once the lag is taken out (jitter, wrong place).
    pub residual_pt_p95: f64,
    pub jump_back_frames: u64,
    pub stall_frames: u64,
    pub worst_frame: Option<usize>,
    pub first_jump_back: Option<usize>,
    pub first_stall: Option<usize>,
}

/// `series` against `expected` over `frames` (indices into both). `pixel`
/// is one device pixel in points: an edge drawn on the pixel grid moves in
/// whole pixels, so a jump-back is a step against the pointer of more than
/// a pixel, and a stall is the pointer having moved more than 1.5 pixels
/// since the edge last moved while it didn't.
fn follow(
    series: &[Option<f32>],
    expected: &[f32],
    frames: &[usize],
    pixel: f64,
) -> Option<Follow> {
    let pairs: Vec<(usize, f64, f64)> = frames
        .iter()
        .filter_map(|&i| series[i].map(|s| (i, s as f64, expected[i] as f64)))
        .collect();
    if pairs.len() < 3 {
        return None;
    }
    let shifted = |s: usize| -> Vec<f64> {
        pairs
            .iter()
            .filter(|(i, _, _)| *i >= s)
            .map(|(i, v, _)| (v - expected[i - s] as f64).abs())
            .collect()
    };
    let mut lag = 0;
    let mut best = f64::INFINITY;
    for s in 0..=30 {
        let gaps = shifted(s);
        if gaps.len() < pairs.len() / 2 {
            break;
        }
        let mean = gaps.iter().sum::<f64>() / gaps.len() as f64;
        // A longer lag has to explain the motion clearly better.
        if mean < best - 0.05 {
            best = mean;
            lag = s;
        }
    }
    // Gain: how far the edge travelled per point the pointer travelled,
    // over the gesture from the rest frame before it (the sum of each
    // frame's movement, so a late edge that goes the whole way, out and
    // back, has gain 1, and one that goes half as far 0.5). None when the
    // pointer didn't move.
    let mut path: Vec<(f64, f64)> = Vec::with_capacity(pairs.len() + 1);
    if let Some(r) = frames[0].checked_sub(1)
        && let Some(v) = series[r]
    {
        path.push((v as f64, expected[r] as f64));
    }
    path.extend(pairs.iter().map(|(_, v, e)| (*v, *e)));
    let (edge_travel, pointer_travel) = path.windows(2).fold((0.0, 0.0), |(a, b), w| {
        (a + (w[1].0 - w[0].0).abs(), b + (w[1].1 - w[0].1).abs())
    });
    let gain = (pointer_travel > 0.0).then(|| edge_travel / pointer_travel);
    // Jumps and stalls are judged against the pointer delayed by the lag
    // when the edge goes about as far as the pointer, else against the
    // pointer itself (a delay fitted to a half-speed or frozen edge
    // means nothing there). The lag itself is always reported and checked.
    let used = if gain.is_some_and(|g| (g - 1.0).abs() < 0.2) {
        lag
    } else {
        0
    };
    let gaps: Vec<f64> = pairs.iter().map(|(_, v, e)| (v - e).abs()).collect();
    let worst = pairs
        .iter()
        .zip(&gaps)
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map(|((i, _, _), _)| *i);
    let (mut back, mut stall, mut first_back, mut first_stall) = (0, 0, None, None);
    let mut anchor: Option<f64> = None;
    for w in pairs.windows(2) {
        let ((i0, v0, _), (i1, v1, _)) = (w[0], w[1]);
        if i1 != i0 + 1 || i0 < used {
            anchor = None;
            continue;
        }
        let (e0, e1) = (expected[i0 - used] as f64, expected[i1 - used] as f64);
        let since = *anchor.get_or_insert(e0);
        let (want, got) = (e1 - e0, v1 - v0);
        if got.abs() >= 0.5 * pixel {
            if want.abs() > 0.5 * pixel && got * want.signum() < -(pixel + 0.01) {
                back += 1;
                first_back.get_or_insert(i1);
            }
            anchor = Some(e1);
        } else if (e1 - since).abs() > 1.5 * pixel {
            stall += 1;
            first_stall.get_or_insert(i1);
        }
    }
    Some(Follow {
        frames: pairs.len(),
        gain: gain.map(round),
        lag_frames: lag,
        gap_pt_median: round(percentile(&gaps, 0.5)),
        gap_pt_p95: round(percentile(&gaps, 0.95)),
        gap_pt_max: round(percentile(&gaps, 1.0)),
        residual_pt_p95: round(percentile(&shifted(used), 0.95)),
        jump_back_frames: back,
        stall_frames: stall,
        worst_frame: worst,
        first_jump_back: first_back,
        first_stall,
    })
}

/// Where the edge should be in each frame, and the frames each gesture's
/// measurements cover. Frame 0 is drawn before any input (the rest). At a
/// press the edge is wherever it was in the frame before; from then until
/// the frame with the release (inclusive, at the release's position) it
/// moves as far as the pointer has moved from where it was pressed; after
/// the release it stays. Each gesture's frames run on a third of a second
/// past its release, for the edge to settle.
fn expectations(
    frames: &[Frame],
    axis: usize,
    fps: u32,
) -> Result<(Vec<f32>, Vec<usize>, Option<usize>)> {
    let pos = |p: (f32, f32)| if axis == 0 { p.0 } else { p.1 };
    let rest = frames.first().and_then(|f| f.model);
    let mut expected = vec![rest.unwrap_or(0.0); frames.len()];
    let mut active = Vec::new();
    let mut first_press = None;
    let mut gesture: Option<(f32, f32, usize)> = None; // edge0, press, start
    let mut hold = rest.unwrap_or(0.0);
    let tail = fps as usize / 3;
    for (i, f) in frames.iter().enumerate() {
        if let Some(p) = f.press {
            if i == 0 {
                bail!("frame 0 must be drawn before any input");
            }
            let edge0 = frames[i - 1]
                .model
                .context("the target wasn't found in the frame before a press")?;
            gesture = Some((edge0, pos(p), i));
            first_press.get_or_insert(i);
        }
        // In the frame of the release, where it was released (the pointer
        // may hover on after it in the same frame).
        let at = f.release.filter(|_| gesture.is_some()).unwrap_or(f.pointer);
        expected[i] = match gesture {
            Some((edge0, press, _)) => edge0 + pos(at) - press,
            None => hold,
        };
        if f.release.is_some()
            && let Some((_, _, start)) = gesture.take()
        {
            hold = expected[i];
            active.extend(start..=(i + tail).min(frames.len() - 1));
        }
    }
    if let Some((_, _, start)) = gesture {
        active.extend(start..frames.len());
    }
    active.sort_unstable();
    active.dedup();
    Ok((expected, active, first_press))
}

/// The run's verdict and numbers, and the contact sheet when it failed.
pub fn analyse(run: &Run, recorded: &Recorded, axis: usize, out: &Path) -> Result<Value> {
    let frames = &recorded.frames;
    let pos = |f: &Frame| if axis == 0 { f.pointer.0 } else { f.pointer.1 };
    let (expected, active, first_press) = expectations(frames, axis, run.fps)?;
    let pixel_pt = 1.0 / run.scale as f64;

    // Pixels and boxes sit at a fixed offset from the layout at rest (a
    // hairline's own pixel, rounding): measured in the frame before the
    // first press, then taken out.
    let offsets = first_press
        .map(|p| &frames[p - 1])
        .map_or((0.0, 0.0), |rest| {
            (
                rest.pixel.zip(rest.model).map_or(0.0, |(a, b)| a - b),
                rest.outline.zip(rest.model).map_or(0.0, |(a, b)| a - b),
            )
        });
    let correct = |v: Option<f32>, offset: f32| v.map(|v| v - offset);
    let pixel: Vec<Option<f32>> = frames.iter().map(|f| correct(f.pixel, offsets.0)).collect();
    let model: Vec<Option<f32>> = frames.iter().map(|f| f.model).collect();
    let shown: Vec<Option<f32>> = frames.iter().map(|f| f.shown).collect();
    let outline: Vec<Option<f32>> = frames
        .iter()
        .map(|f| correct(f.outline, offsets.1))
        .collect();
    let on_screen = follow(&pixel, &expected, &active, pixel_pt);
    let in_state = follow(&model, &expected, &active, pixel_pt);
    let drawn_box = follow(&shown, &expected, &active, pixel_pt);
    let drawn_outline = follow(&outline, &expected, &active, pixel_pt);
    let tracked = run.target != "none";
    let untracked: Vec<usize> = active
        .iter()
        .copied()
        .filter(|&i| pixel[i].is_none())
        .collect();
    let unknown_state = active.iter().filter(|&&i| model[i].is_none()).count();

    // A place that changed and changed back while the pointer kept going
    // one way (where it turns, positions repeat by design; around a press
    // or release the interface changes state: readouts, hover).
    let edges: Vec<usize> = frames
        .iter()
        .filter(|f| f.press.is_some() || f.release.is_some())
        .map(|f| f.index)
        .collect();
    let turning = |i: usize| {
        if edges.iter().any(|&e| e.abs_diff(i) <= 2) {
            return true;
        }
        let lo = i.saturating_sub(3);
        let hi = (i + 3).min(frames.len() - 1);
        let mut sign = 0.0f32;
        for j in lo + 1..=hi {
            let d = pos(&frames[j]) - pos(&frames[j - 1]);
            if d.abs() > 0.01 {
                if sign != 0.0 && d.signum() != sign {
                    return true;
                }
                sign = d.signum();
            }
        }
        false
    };
    let flicker: Vec<usize> = frames
        .iter()
        .filter(|f| f.index >= 2 && f.aba_tiles > 0 && !turning(f.index))
        .map(|f| f.index)
        .collect();
    let black = frames.iter().filter(|f| f.black_picture).count() as u64;
    let blank = frames.iter().filter(|f| f.blank_window).count() as u64;
    let cpu: Vec<f64> = frames.iter().map(|f| f.cpu_ms).collect();
    let finished: Vec<f64> = frames.iter().map(|f| f.frame_ms).collect();
    let late = frames.iter().filter(|f| f.late).count() as u64;
    let gesture_frames: Vec<&Frame> = active.iter().map(|&i| &frames[i]).collect();
    let calls: u64 = gesture_frames.iter().map(|f| f.calls).sum();
    let bytes: u64 = gesture_frames.iter().map(|f| f.call_bytes).sum();
    let engine_frames: u64 = gesture_frames.iter().map(|f| f.engine_frames).sum();
    let seconds = gesture_frames.len() as f64 / run.fps as f64;

    // The verdict. A measurement that couldn't be made fails: a tracked
    // target missing from any frame of a gesture, no gesture at all, or
    // (lockstep) the engine not catching up.
    let th = &run.thresholds;
    let mut failures = Vec::new();
    let mut check = |name: &str, value: f64, limit: Option<f64>| {
        if let Some(limit) = limit
            && value > limit
        {
            failures.push(json!({ "check": name, "value": round(value), "limit": limit }));
        }
    };
    if tracked {
        check(
            "no_gesture",
            f64::from(u8::from(active.is_empty())),
            Some(0.0),
        );
        check("untracked_frames", untracked.len() as f64, Some(0.0));
        check("unknown_state_frames", unknown_state as f64, Some(0.0));
    }
    check(
        "engine_sync_timeouts",
        recorded.sync_timeouts as f64,
        Some(0.0),
    );
    if let Some(f) = &on_screen {
        check("gap_pt_p95", f.gap_pt_p95, th.gap_pt_p95);
        check("gap_pt_max", f.gap_pt_max, th.gap_pt_max);
        check("lag_frames", f.lag_frames as f64, th.lag_frames);
        if let (Some(gain), Some(tolerance)) = (f.gain, th.gain_tolerance) {
            check("gain_off_by", (gain - 1.0).abs(), Some(tolerance));
        }
        check(
            "jump_back_frames",
            f.jump_back_frames as f64,
            th.jump_back_frames.map(|v| v as f64),
        );
        check(
            "stall_frames",
            f.stall_frames as f64,
            th.stall_frames.map(|v| v as f64),
        );
    }
    check(
        "black_frames",
        (black + blank) as f64,
        th.black_frames.map(|v| v as f64),
    );
    check(
        "flicker_frames",
        flicker.len() as f64,
        th.flicker_frames.map(|v| v as f64),
    );
    check("frame_ms_max", percentile(&finished, 1.0), th.frame_ms_max);
    check("late_frames", late as f64, th.late_frames.map(|v| v as f64));
    let pass = failures.is_empty();
    let failing: Vec<&str> = failures
        .iter()
        .filter_map(|f| f["check"].as_str())
        .collect();
    let missing_reasons: Vec<&String> = run
        .expect_failing
        .iter()
        .filter(|c| !failing.contains(&c.as_str()))
        .collect();
    // With expect_failing, exactly those checks: a known issue that gets a
    // new failure, or loses one (fixed), no longer counts as expected.
    let unexpected_failures: Vec<&str> = if run.expect_failing.is_empty() {
        Vec::new()
    } else {
        failing
            .iter()
            .copied()
            .filter(|c| !run.expect_failing.iter().any(|e| e == c))
            .collect()
    };
    let as_expected = run.expect.as_deref().map(|e| {
        (e == "pass") == pass && missing_reasons.is_empty() && unexpected_failures.is_empty()
    });

    // The frames around the worst moment, as a contact sheet.
    let worst = untracked
        .first()
        .copied()
        .or(on_screen
            .as_ref()
            .and_then(|f| f.first_jump_back.or(f.worst_frame)))
        .or(flicker.first().copied())
        .or(frames
            .iter()
            .position(|f| f.black_picture || f.blank_window));
    let sheet = match worst {
        Some(w) if !recorded.images.is_empty() => {
            let path = out.join("sheet.png");
            super::sheet::write(&path, recorded, &expected, &pixel, axis, w)?;
            Some((path.display().to_string(), w))
        }
        _ => None,
    };

    // Everything here follows the wall clock: it differs from run to run
    // and isn't part of the lockstep comparison.
    let wall = json!({
        "gpu": recorded.gpu,
        "cpu_ms_p50": round(percentile(&cpu, 0.5)),
        "cpu_ms_p95": round(percentile(&cpu, 0.95)),
        "cpu_ms_max": round(percentile(&cpu, 1.0)),
        "frame_ms_p50": round(percentile(&finished, 0.5)),
        "frame_ms_p95": round(percentile(&finished, 0.95)),
        "frame_ms_max": round(percentile(&finished, 1.0)),
        "late_frames": late,
        // Slint asks again for each engine frame that arrives: wall.
        "redraws_requested": frames.iter().map(|f| f.redraws_requested).sum::<u64>(),
        "engine_wait_ms_p95": round(percentile(&frames.iter().map(|f| f.engine_wait_ms).collect::<Vec<_>>(), 0.95)),
        "calls_per_s": if seconds > 0.0 { round(calls as f64 / seconds) } else { 0.0 },
        "engine_frames_received_per_s": if seconds > 0.0 { round(engine_frames as f64 / seconds) } else { 0.0 },
        "preview": {
            "received": recorded.preview.received,
            "shown": recorded.preview.shown,
            "waited_for_layout": recorded.preview.waited_for_layout,
            "shown_without_layout": recorded.preview.shown_without_layout,
            "longest_gap_ms": round(recorded.preview.longest_gap.as_secs_f64() * 1000.0),
        },
    });
    let engine = json!({
        "calls_during_gesture": calls,
        "bytes_during_gesture": bytes,
        "most_calls_in_a_frame": gesture_frames.iter().map(|f| f.calls).max().unwrap_or(0),
        "edits_sent": recorded.edits_sent,
        "most_edits_in_flight": recorded.most_in_flight,
        "stale_edits": recorded.stale_applies,
        "by_method": recorded.calls.by_method.iter().map(|(k, (n, b))| (k.clone(), json!({ "calls": n, "bytes": b }))).collect::<serde_json::Map<_, _>>(),
    });
    Ok(json!({
        "id": run.id(),
        "invocation": std::env::var("MOTION_INVOCATION").ok(),
        "scenario": run.name,
        "variant": run.variant,
        "description": run.description,
        "pass": pass,
        "expect": run.expect,
        "inject": run.inject,
        "as_expected": as_expected,
        "expect_failing": run.expect_failing,
        "expected_failures_missing": missing_reasons,
        "unexpected_failures": unexpected_failures,
        "failures": failures,
        "target": run.target,
        "clock": if run.lockstep { "lockstep" } else { "realtime" },
        "picture": if run.engine_picture { "engine" } else { "test" },
        "window": { "points": [run.size.0, run.size.1], "scale": run.scale, "pixels": [recorded.size.0, recorded.size.1] },
        "fps": run.fps,
        "frames": frames.len(),
        "gesture_frames": active.len(),
        "tracked_frames": active.len() - untracked.len(),
        "first_untracked": untracked.first(),
        "engine_sync_timeouts": recorded.sync_timeouts,
        "rest_offsets_pt": [round(offsets.0 as f64), round(offsets.1 as f64)],
        "scan_line": recorded.scan_line,
        "on_screen": on_screen,
        "in_state": in_state,
        "drawn_box": drawn_box,
        "drawn_outline": drawn_outline,
        "glitches": {
            "black_picture_frames": black,
            "blank_window_frames": blank,
            "flicker_frames": flicker.len(),
            "first_flicker": flicker.first(),
        },
        "engine": engine,
        "recording": recorded.recording_started,
        "sheet": sheet.as_ref().map(|(p, w)| json!({ "path": p, "centre_frame": w })),
        "wall": wall,
    }))
}

impl From<Image> for Crop {
    fn from(image: Image) -> Self {
        image.whole()
    }
}
