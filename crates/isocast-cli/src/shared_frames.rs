//! `isocast frames --shared`: watch a target through GPU surfaces shared
//! with the daemon (`shared_frames`), and with `--check` prove the pictures
//! arrive whole: every frame is read back through this process's GPU from
//! the shared surface itself.

use std::collections::BTreeMap;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use isocast_client::shared::{SharedFrame, SharedFrames};
use isocast_engine::gpu::Gpu;
use isocast_protocol::api::{Applied, ApplyParams, Live, Method, SharedFramesParams};
use isocast_protocol::change::Change;
use serde_json::json;

use crate::client::Client;

/// Sample points across and down each frame for the black and undrawn
/// checks.
const GRID: u32 = 8;
/// Every channel at or under this is black (video-range black is 16 before
/// conversion, 0 after).
const BLACK: u8 = 24;
/// How far an expected pixel's channels may be off.
const TOLERANCE: u8 = 12;

/// What `frames --shared` was asked.
pub struct Options {
    pub target: String,
    pub max_width: Option<u32>,
    pub max_height: Option<u32>,
    pub fps: Option<u32>,
    pub slots: Option<u32>,
    pub filtered: bool,
    pub seconds: f64,
    pub check: bool,
    pub expect: Vec<String>,
    pub touch: bool,
    pub json: bool,
}

/// A pixel a frame should have: where, as fractions of the width and
/// height, and its colour.
#[derive(Debug)]
struct Expected {
    spec: String,
    at: (f64, f64),
    rgb: [u8; 3],
}

pub fn run(options: Options) -> Result<ExitCode> {
    let expected = options
        .expect
        .iter()
        .map(|spec| parse_expected(spec))
        .collect::<Result<Vec<_>>>()?;
    let params = SharedFramesParams {
        target: options.target.clone(),
        max_width: options.max_width,
        max_height: options.max_height,
        fps: options.fps,
        slots: options.slots,
        filtered: options.filtered,
    };
    let client = Client::connect_or_launch()?;
    #[cfg(windows)]
    let client = {
        let mut client = client;
        if options.check {
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
                backends: wgpu::Backends::DX12,
                ..wgpu::InstanceDescriptor::new_without_display_handle()
            });
            // Pin the process-wide selection before Gpu::new (or its cache)
            // is used. Query on this connection before it becomes a stream.
            isocast_client::shared::adapter(&mut client, &instance)?;
        }
        client
    };
    let mut frames = client.shared_frames(params)?;
    // The engine's clock now, as of its reply: frames are timed on it.
    // (Linux engines keep a clock of their own; elsewhere it's the host's.)
    let (engine_time, connected) = (frames.info().host_time, Instant::now());
    let engine_now = || {
        if cfg!(target_os = "linux") {
            engine_time + connected.elapsed().as_nanos() as u64
        } else {
            isocast_surfaces::host_now()
        }
    };
    // Linux's shared frames are in memory: checked without a GPU.
    let gpu = if options.check && !cfg!(target_os = "linux") {
        Some(Gpu::new().context("this process could not open the GPU to read frames back")?)
    } else {
        None
    };
    if let Some(gpu) = &gpu {
        isocast_client::shared::register_gpu(&gpu.device, &gpu.queue)?;
    }
    let started = Instant::now();
    let length = Duration::try_from_secs_f64(options.seconds.max(0.0)).unwrap_or(Duration::ZERO);
    let mut stats = Stats::default();
    let mut touched: Option<(u64, f64)> = None;
    let mut ended = None;
    // The frame shown last, kept until the next has been read, as a
    // monitor keeps it until the next is on screen.
    let mut showing: Option<SharedFrame> = None;
    while started.elapsed() < length {
        if options.touch && touched.is_none() && started.elapsed() >= length / 2 {
            let revision = touch()?;
            touched = Some((revision, started.elapsed().as_secs_f64()));
        }
        let left = length.saturating_sub(started.elapsed());
        let frame = match frames.next(Some(left.min(Duration::from_millis(250)))) {
            Ok(Some(frame)) => frame,
            Ok(None) => continue,
            Err(err) => {
                ended = Some(format!("{err:#}"));
                break;
            }
        };
        let at = started.elapsed().as_secs_f64();
        stats.frame(&frame, at, engine_now());
        if let Some(gpu) = &gpu {
            let points = grid_points(frame.width(), frame.height(), &expected);
            let texture = frame.texture(&gpu.device)?;
            let pixels = read_pixels(gpu, &texture, &points)?;
            stats.check(&frame, &pixels, &expected, at);
        }
        #[cfg(target_os = "linux")]
        if gpu.is_none() && options.check {
            let points = grid_points(frame.width(), frame.height(), &expected);
            let pixels = read_memory(&frame, &points);
            stats.check(&frame, &pixels, &expected, at);
        }
        showing = Some(frame);
    }
    drop(showing);
    let elapsed = started.elapsed().as_secs_f64().max(0.001);
    report(
        &options, &frames, &stats, &expected, touched, ended, elapsed,
    )
}

/// Apply an empty live change: the setup stays as it is and its revision
/// goes up. Returns the new revision.
fn touch() -> Result<u64> {
    let mut client = Client::connect_or_launch()?;
    let live: Live = client.call(Method::Get(None))?;
    let applied: Applied = client.call(Method::Apply(Box::new(ApplyParams {
        revision: live.revision,
        change: Change::Patch(json!({})),
        live: true,
    })))?;
    Ok(applied.revision)
}

#[derive(Default)]
struct Stats {
    count: u64,
    first: Option<f64>,
    last: Option<f64>,
    /// How long after its time each frame arrived, in milliseconds.
    lags: Vec<f64>,
    /// Between arrivals, in milliseconds.
    intervals: Vec<f64>,
    /// Rings seen: generation, size, when.
    rings: Vec<(u64, u32, u32, f64)>,
    /// Revisions seen, and when first.
    revisions: Vec<(Option<u64>, f64)>,
    checked: u64,
    black: Vec<f64>,
    undrawn: Vec<f64>,
    /// Per expected pixel: frames matching, and the last colour read.
    matched: Vec<(u64, [u8; 4])>,
    /// Per expected pixel, by the setup revision frames showed: frames
    /// matching and frames read.
    by_revision: Vec<BTreeMap<Option<u64>, (u64, u64)>>,
    centre: Option<[u8; 4]>,
}

impl Stats {
    fn frame(&mut self, frame: &SharedFrame, at: f64, now: u64) {
        self.count += 1;
        self.first.get_or_insert(at);
        if let Some(last) = self.last {
            self.intervals.push((at - last) * 1000.0);
        }
        self.last = Some(at);
        self.lags.push(now.saturating_sub(frame.time) as f64 / 1e6);
        let (generation, _) = frame.slot();
        if self.rings.last().is_none_or(|ring| ring.0 != generation) {
            self.rings
                .push((generation, frame.width(), frame.height(), at));
        }
        if self
            .revisions
            .last()
            .is_none_or(|(revision, _)| *revision != frame.revision)
        {
            self.revisions.push((frame.revision, at));
        }
    }

    fn check(&mut self, frame: &SharedFrame, pixels: &[[u8; 4]], expected: &[Expected], at: f64) {
        self.checked += 1;
        let grid = (GRID * GRID) as usize;
        let samples = &pixels[..grid];
        if samples.iter().any(|p| p[3] != 255) {
            self.undrawn.push(at);
        } else if samples.iter().all(|p| p[..3].iter().all(|c| *c <= BLACK)) {
            self.black.push(at);
        }
        self.centre = Some(pixels[grid]);
        self.matched.resize(expected.len(), (0, [0; 4]));
        self.by_revision.resize(expected.len(), BTreeMap::new());
        for (index, want) in expected.iter().enumerate() {
            let got = pixels[grid + 1 + index];
            let close = want
                .rgb
                .iter()
                .zip(got)
                .all(|(w, g)| w.abs_diff(g) <= TOLERANCE);
            let entry = &mut self.matched[index];
            entry.0 += u64::from(close);
            entry.1 = got;
            let counts = self.by_revision[index].entry(frame.revision).or_default();
            counts.0 += u64::from(close);
            counts.1 += 1;
        }
    }
}

/// The grid's points, the centre, then each expected pixel's.
fn grid_points(width: u32, height: u32, expected: &[Expected]) -> Vec<(u32, u32)> {
    let at = |fx: f64, fy: f64| {
        (
            ((fx * f64::from(width)) as u32).min(width - 1),
            ((fy * f64::from(height)) as u32).min(height - 1),
        )
    };
    let mut points = Vec::new();
    for row in 0..GRID {
        for column in 0..GRID {
            let f = |i: u32| (f64::from(i) + 0.5) / f64::from(GRID);
            points.push(at(f(column), f(row)));
        }
    }
    points.push(at(0.5, 0.5));
    points.extend(expected.iter().map(|e| at(e.at.0, e.at.1)));
    points
}

/// Single pixels of a frame in shared memory (Linux), as RGBA.
#[cfg(target_os = "linux")]
fn read_memory(frame: &SharedFrame, points: &[(u32, u32)]) -> Vec<[u8; 4]> {
    let pixels = frame.surface().pixels();
    let width = frame.width() as usize;
    points
        .iter()
        .map(|&(x, y)| {
            let at = (y as usize * width + x as usize) * 4;
            pixels
                .get(at..at + 4)
                .map_or([0; 4], |p| [p[0], p[1], p[2], p[3]])
        })
        .collect()
}

/// Read single pixels of `texture` back through the GPU, as RGBA.
fn read_pixels(gpu: &Gpu, texture: &wgpu::Texture, points: &[(u32, u32)]) -> Result<Vec<[u8; 4]>> {
    let row = u64::from(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: row * points.len() as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    for (index, &(x, y)) in points.iter().enumerate() {
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: row * index as u64,
                    bytes_per_row: Some(row as u32),
                    rows_per_image: Some(1),
                },
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
    }
    let submission = gpu.queue.submit([encoder.finish()]);
    let slice = buffer.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    gpu.device
        .poll(wgpu::PollType::Wait {
            submission_index: Some(submission),
            timeout: Some(Duration::from_secs(2)),
        })
        .context("the GPU did not finish reading the frame back")?;
    rx.recv()
        .context("the GPU did not return the pixels")?
        .context("could not read the pixels back")?;
    let mapped = slice
        .get_mapped_range()
        .context("could not read the pixels back")?;
    let pixels = (0..points.len())
        .map(|index| {
            let at = row as usize * index;
            [mapped[at], mapped[at + 1], mapped[at + 2], mapped[at + 3]]
        })
        .collect();
    drop(mapped);
    buffer.unmap();
    Ok(pixels)
}

/// `X,Y=#RRGGBB`: fractions of the width and height, and a colour.
fn parse_expected(spec: &str) -> Result<Expected> {
    let usage = "expect: give X,Y=#RRGGBB, with X and Y fractions of the picture's width and \
                 height from 0 to 1 (like 0.5,0.5=#bfbfbf for its centre)";
    let (place, colour) = spec
        .split_once('=')
        .with_context(|| format!("{usage}; got {spec:?}"))?;
    let (x, y) = place
        .split_once(',')
        .with_context(|| format!("{usage}; got {spec:?}"))?;
    let fraction = |text: &str| -> Result<f64> {
        let value: f64 = text
            .trim()
            .parse()
            .with_context(|| format!("{usage}; got {spec:?}"))?;
        if !(0.0..=1.0).contains(&value) {
            bail!("{usage}; {value} is outside 0 to 1");
        }
        Ok(value)
    };
    let hex = colour.trim().trim_start_matches('#');
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("{usage}; {colour:?} is not a #RRGGBB colour");
    }
    let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0);
    Ok(Expected {
        spec: spec.to_string(),
        at: (fraction(x)?, fraction(y)?),
        rgb: [channel(0), channel(2), channel(4)],
    })
}

fn report(
    options: &Options,
    frames: &SharedFrames,
    stats: &Stats,
    expected: &[Expected],
    touched: Option<(u64, f64)>,
    ended: Option<String>,
    elapsed: f64,
) -> Result<ExitCode> {
    let info = frames.info();
    let quantile = |values: &[f64], q: f64| {
        let mut sorted = values.to_vec();
        sorted.sort_by(f64::total_cmp);
        sorted
            .get(((sorted.len() as f64 - 1.0) * q).round() as usize)
            .copied()
    };
    let spread = |values: &[f64]| {
        json!({
            "median": quantile(values, 0.5),
            "p90": quantile(values, 0.9),
            "p99": quantile(values, 0.99),
            "max": quantile(values, 1.0),
        })
    };
    // The rate from the first frame on (the ring's set-up isn't counted).
    let fps = match (stats.first, stats.last) {
        (Some(first), Some(last)) if stats.count > 1 && last > first => {
            (stats.count - 1) as f64 / (last - first)
        }
        _ => stats.count as f64 / elapsed,
    };
    let (size, last_ring) = match stats.rings.last() {
        Some(&(_, width, height, _)) => ((width, height), Some(stats.rings.len())),
        None => ((0, 0), None),
    };
    // A touch passes when a frame after it shows its revision.
    let touch_ok = touched.map(|(revision, at)| {
        stats
            .revisions
            .iter()
            .find(|(seen, seen_at)| *seen == Some(revision) && *seen_at >= at)
            .map(|(_, seen_at)| (seen_at - at) * 1000.0)
    });
    let mut problems = Vec::new();
    if stats.count == 0 {
        problems.push(match &ended {
            Some(why) => format!("no frames arrived: {why}"),
            None => "no frames arrived".to_string(),
        });
    }
    if options.check {
        if !stats.undrawn.is_empty() {
            problems.push(format!(
                "{} frames had pixels the daemon never drew (alpha below 255)",
                stats.undrawn.len()
            ));
        }
        if !stats.black.is_empty() {
            problems.push(format!("{} frames were black", stats.black.len()));
        }
        for (want, (matched, got)) in expected.iter().zip(&stats.matched) {
            if *matched == 0 {
                problems.push(format!(
                    "{}: no frame had that colour (last read #{:02x}{:02x}{:02x})",
                    want.spec, got[0], got[1], got[2]
                ));
            }
        }
        if let Some(None) = touch_ok {
            problems.push("no frame showed the revision of the empty change".into());
        }
    }
    if options.json {
        let value = json!({
            "target": info.target,
            "transport": info.transport,
            "service": info.service,
            "format": info.format,
            "slots": info.slots,
            "width": size.0,
            "height": size.1,
            "frames": stats.count,
            "seconds": elapsed,
            "fps": fps,
            "lag_ms": spread(&stats.lags),
            "interval_ms": spread(&stats.intervals),
            "rings": stats.rings.iter().map(|(generation, width, height, at)| json!({
                "generation": generation, "width": width, "height": height, "at_s": at,
            })).collect::<Vec<_>>(),
            "revisions": stats.revisions.iter().map(|(revision, at)| json!({
                "revision": revision, "at_s": at,
            })).collect::<Vec<_>>(),
            "touch": touched.map(|(revision, at)| json!({
                "revision": revision,
                "at_s": at,
                "shown_after_ms": touch_ok.flatten(),
            })),
            "check": options.check.then(|| json!({
                "frames_read_back": stats.checked,
                "samples_per_frame": GRID * GRID,
                "black_frames": stats.black.len(),
                "black_at_s": stats.black,
                "undrawn_frames": stats.undrawn.len(),
                "centre_rgba": stats.centre,
                "expected": expected.iter().enumerate().map(|(index, want)| {
                    let (matched, got) = stats.matched.get(index).copied().unwrap_or_default();
                    let by_revision: Vec<_> = stats
                        .by_revision
                        .get(index)
                        .into_iter()
                        .flatten()
                        .map(|(revision, (matched, read))| json!({
                            "revision": revision, "frames_matching": matched, "frames": read,
                        }))
                        .collect();
                    json!({
                        "spec": want.spec,
                        "frames_matching": matched,
                        "last_rgba": got,
                        "by_revision": by_revision,
                    })
                }).collect::<Vec<_>>(),
            })),
            "ended": ended,
            "problems": problems,
            "ok": problems.is_empty(),
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!(
            "Received {} frames of {} through {} shared {}×{} surfaces ({}), {:.1} fps",
            stats.count, info.target, info.slots, size.0, size.1, info.transport, fps
        );
        if let (Some(median), Some(p90), Some(max)) = (
            quantile(&stats.lags, 0.5),
            quantile(&stats.lags, 0.9),
            quantile(&stats.lags, 1.0),
        ) {
            println!(
                "Frames arrived {median:.1} ms after their time (median; 90% within {p90:.1} ms, \
                 max {max:.1} ms)"
            );
        }
        if let Some(rings) = last_ring.filter(|rings| *rings > 1) {
            println!("Rings: {rings} (the size changed)");
        }
        let revisions: Vec<String> = stats
            .revisions
            .iter()
            .map(|(revision, at)| match revision {
                Some(revision) => format!("{revision} at {at:.2} s"),
                None => format!("none at {at:.2} s"),
            })
            .collect();
        println!("Setup revisions shown: {}", revisions.join(", "));
        if let Some((revision, at)) = touched {
            match touch_ok.flatten() {
                Some(ms) => println!(
                    "An empty change at {at:.2} s made revision {revision}; frames showed it {ms:.0} ms later"
                ),
                None => println!(
                    "An empty change at {at:.2} s made revision {revision}; no frame showed it"
                ),
            }
        }
        if options.check {
            println!(
                "Read back {} frames {} ({} pixels each): {} black, {} not drawn",
                stats.checked,
                if cfg!(target_os = "linux") {
                    "from shared memory"
                } else {
                    "through this process's GPU"
                },
                GRID * GRID,
                stats.black.len(),
                stats.undrawn.len()
            );
            if let Some(c) = stats.centre {
                println!(
                    "Centre pixel of the last frame: #{:02x}{:02x}{:02x} (alpha {})",
                    c[0], c[1], c[2], c[3]
                );
            }
            for (index, (want, (matched, got))) in expected.iter().zip(&stats.matched).enumerate() {
                let by_revision: Vec<String> = stats.by_revision[index]
                    .iter()
                    .map(|(revision, (matched, read))| match revision {
                        Some(revision) => format!("revision {revision}: {matched} of {read}"),
                        None => format!("{matched} of {read}"),
                    })
                    .collect();
                println!(
                    "{}: {matched} of {} frames matched (last read #{:02x}{:02x}{:02x}; {})",
                    want.spec,
                    stats.checked,
                    got[0],
                    got[1],
                    got[2],
                    by_revision.join(", ")
                );
            }
        }
        if let Some(why) = &ended {
            println!("The stream ended: {why}");
        }
        for problem in &problems {
            println!("Problem: {problem}");
        }
    }
    Ok(if problems.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

#[cfg(test)]
mod tests {
    use super::parse_expected;

    #[test]
    fn expected_pixels_read_as_written() {
        let expected = parse_expected("0.25,0.5=#2050c0").unwrap();
        assert_eq!(expected.at, (0.25, 0.5));
        assert_eq!(expected.rgb, [0x20, 0x50, 0xc0]);
        let err = parse_expected("0.5=#fff").unwrap_err().to_string();
        assert!(err.contains("X,Y=#RRGGBB"), "{err}");
        let err = parse_expected("2,0=#ffffff").unwrap_err().to_string();
        assert!(err.contains("outside 0 to 1"), "{err}");
    }
}
