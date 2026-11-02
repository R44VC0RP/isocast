//! `tune`: benchmark the hardware encoders on a setup's files, all at once
//! as in a recording; say whether this Mac keeps up with them, and if it
//! doesn't, find the closest change to the setup that fits.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use isocast_protocol::api::{Candidate, Capacity, SkippedSource, TunedStream, Tuning, Variant};
use isocast_protocol::change::merge_patch;
use isocast_protocol::platform_text::MACHINE;
use isocast_protocol::{
    CanvasId, Codec, Encoder, EncoderQuality, IsoEncoder, PROGRAM, QualityPreset, Setup, SourceKind,
};
use serde_json::{Value, json};

use crate::capacity::{self, Fix, Measurement, Memory, VideoLoad, Workload};
use crate::record::expand_home;
use crate::source::{self, Timeline};
use crate::{FileWriter, FramePool, Platform, Time, VideoTrackSpec, WriterSpec, space};

/// Seconds of video each benchmark encodes per stream before timing starts,
/// so encoder start-up isn't counted.
const WARM_UP_SECONDS: u32 = 1;
/// Seconds of video each benchmark times per stream.
const CONTENT_SECONDS: u32 = 3;
/// How much faster than real time encoders must run on the benchmark's
/// hard content (at most 80% busy) to fit, so they keep up when the Mac is
/// busy. Real screen and camera footage is easier.
const HEADROOM: f64 = 1.25;
/// Under this the encoders fall behind, and a recording drops frames.
const KEEPS_UP: f64 = 1.0;
/// How much faster than the data rate a feasible disk must write.
const DISK_MARGIN: f64 = 2.0;
/// How long a feasible encoder's files must fit on the disk.
const HOURS_ON_DISK: f64 = 1.0;
/// The most changes to the setup benchmarked when it doesn't fit (each
/// takes about five seconds).
const MAX_VARIANTS: usize = 8;
/// A change whose pixel rate predicts less than this (from the setup's own
/// measurement) isn't benchmarked: it would only show again that the
/// encoders can't keep up.
const HOPELESS: f64 = 0.9;
/// How far frames' windows into the texture shift.
const TEXTURE_MARGIN: usize = 64;
/// Distinct frames each stream cycles through. Each one holds a
/// production permit for the whole benchmark, so this stays under the frame
/// budget's production reserve (8 per pool), with room for the encoder.
const CYCLE: usize = 6;
/// The program file sizes tried, by their short side, when the setup
/// doesn't fit.
const PROGRAM_SIDES: [u32; 3] = [1440, 1080, 720];
/// How far each kind of change takes a setup from what was asked, so the
/// closest that fits is found first: keep every file and frame rate before
/// anything else, then quality, then smoothness; dropping a file is last.
const COST_PROGRAM_STEP: u32 = 2;
const COST_QUALITY: u32 = 6;
const COST_CODEC: u32 = 7;
const COST_FRAME_RATE: u32 = 12;
const COST_DROP_ISO: u32 = 20;

/// How a measured speed is judged.
pub fn capacity(realtime_factor: f64) -> Capacity {
    if realtime_factor >= HEADROOM {
        Capacity::Fits
    } else if realtime_factor >= KEEPS_UP {
        Capacity::NoMargin
    } else {
        Capacity::CannotKeepUp
    }
}

/// Benchmark the setup's files with each candidate ISO encoder, all at once
/// as in a recording, and judge whether this Mac keeps up. If the setup
/// doesn't fit, benchmark changes to it, closest first, until one does.
/// With `memory`, keep what was measured there for `start` to check.
pub fn tune<P: Platform>(
    platform: &P,
    setup: &Setup,
    folder: Option<&Path>,
    memory: Option<&Path>,
) -> Result<Tuning> {
    let issues = setup.validate();
    if !issues.is_empty() {
        let lines: Vec<String> = issues
            .iter()
            .map(|i| format!("{}: {}", i.path, i.message))
            .collect();
        bail!("the setup is not valid:\n  {}", lines.join("\n  "));
    }
    let mut formats = Formats::new();
    let (loads, skipped) = setup_loads(platform, setup, &mut formats)?;
    if loads.is_empty() {
        if let Some(first) = skipped.first() {
            bail!(
                "source {:?}: could not open it, and nothing else in the setup records video: {}",
                first.source,
                first.reason
            );
        }
        bail!(
            "the setup records no video: enable recording.program, or list sources in \
             recording.iso.sources"
        );
    }
    let folder = folder
        .map(Path::to_path_buf)
        .unwrap_or_else(|| expand_home(&setup.recording.folder));
    std::fs::create_dir_all(&folder)
        .with_context(|| format!("could not create {}", folder.display()))?;
    // This run's own folder, made new (never one already there, and never
    // through a link): the benchmark writes and then removes only what it
    // made, wherever the folder came from.
    let scratch = crate::space::owned_temp_dir(&folder, ".isocast-tune")?;
    let tuner = Tuner {
        platform,
        setup,
        scratch: scratch.clone(),
        free: 0,
        reserve: setup.recording.keep_free_gb * 1e9,
        disk: 0.0,
        formats,
    };
    let result = tuner.run(&loads, &folder);
    let _ = std::fs::remove_dir_all(&scratch);
    let (mut tuning, measured) = result?;
    // A source that can't be opened (a camera in another mode, unplugged)
    // doesn't stop the rest from being measured: it is named, with why.
    for skip in &skipped {
        tuning.notes.insert(
            0,
            format!(
                "source {:?} couldn't be opened, so its ISO file wasn't measured (the rest \
                 were; with it the encoders have more to do): {}",
                skip.source, skip.reason
            ),
        );
    }
    tuning.skipped = skipped;
    if let Some(path) = memory {
        let fix = tuning.change.as_ref().map(|change| Fix {
            description: tuning.recommendation.clone().unwrap_or_default(),
            change: Some(change.clone()),
        });
        if let Err(err) = remember(platform, setup, path, &measured, fix) {
            tuning.notes.push(format!(
                "could not keep these measurements in {} for `start` to check: {err:#}",
                path.display()
            ));
        }
    }
    Ok(tuning)
}

/// Keep what was measured, each with the fix this run recommended.
fn remember<P: Platform>(
    platform: &P,
    setup: &Setup,
    path: &Path,
    measured: &[(Workload, f64)],
    fix: Option<Fix>,
) -> Result<()> {
    let machine = capacity::machine_key(&platform.machine());
    let now = chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
    let mut memory = Memory::load(path);
    for (workload, realtime_factor) in measured {
        let capacity = capacity(*realtime_factor);
        memory.remember(Measurement {
            measured_at: now.clone(),
            machine: machine.clone(),
            setup: setup.name.clone(),
            workload: workload.clone(),
            realtime_factor: *realtime_factor,
            capacity,
            fix: fix.clone().filter(|_| capacity != Capacity::Fits),
        });
    }
    memory.save(path)
}

/// One file's video as benchmarked.
#[derive(Debug, Clone)]
struct Load {
    /// The file's size and rate.
    stream: TunedStream,
    /// The frames' size: the canvas, for a program file written smaller.
    input: (u32, u32),
    encoder: Encoder,
}

impl Load {
    fn spec(&self, path: PathBuf) -> WriterSpec {
        WriterSpec {
            path,
            container: self.encoder.container,
            video: VideoTrackSpec {
                width: self.stream.width,
                height: self.stream.height,
                fps: self.stream.fps,
                encoder: self.encoder.clone(),
            },
            audio: Vec::new(),
            created: std::time::SystemTime::now(),
        }
    }

    fn pixel_rate(&self) -> f64 {
        f64::from(self.stream.width) * f64::from(self.stream.height) * f64::from(self.stream.fps)
    }
}

fn workload(loads: &[Load]) -> Workload {
    let mut workload: Workload = loads
        .iter()
        .map(|l| VideoLoad::new(l.stream.width, l.stream.height, l.stream.fps, &l.encoder))
        .collect();
    workload.sort();
    workload
}

/// The pixel rate the H.264/HEVC engines encode (ProRes runs on engines of
/// its own), or all of it if everything is ProRes.
fn pixel_load(loads: &[Load]) -> f64 {
    let engines: f64 = loads
        .iter()
        .filter(|l| matches!(l.encoder.codec, Codec::H264 | Codec::Hevc))
        .map(Load::pixel_rate)
        .sum();
    if engines > 0.0 {
        engines
    } else {
        loads.iter().map(Load::pixel_rate).sum()
    }
}

/// The data rate of `loads` by the disk estimate.
fn estimate(loads: &[Load]) -> f64 {
    loads
        .iter()
        .map(|l| space::bytes_per_second(&l.spec(PathBuf::new())))
        .sum()
}

/// ISO sources' formats by id and canvas frame rate (a display or camera
/// follows the canvas's), so each is probed once: or why it couldn't be
/// opened, so it isn't tried again for every change.
type Formats = HashMap<(String, u32), Result<(u32, u32, u32), String>>;

/// The files' video: each ISO source at its device's format, then the
/// program at its file's size, then the vertical program (if a vertical
/// canvas records) at its own.
/// ISO sources that can't be opened are left out, each with why.
fn setup_loads<P: Platform>(
    platform: &P,
    setup: &Setup,
    formats: &mut Formats,
) -> Result<(Vec<Load>, Vec<SkippedSource>)> {
    let mut loads = Vec::new();
    let mut skipped = Vec::new();
    let iso = &setup.recording.iso;
    let ids = if iso.enabled { &iso.sources[..] } else { &[] };
    for id in ids {
        let key = (id.clone(), setup.canvas.fps);
        let format = formats
            .entry(key)
            .or_insert_with(|| iso_format(platform, setup, id).map_err(|e| format!("{e:#}")))
            .clone();
        let (width, height, fps) = match format {
            Ok(format) => format,
            Err(reason) => {
                skipped.push(SkippedSource {
                    source: id.clone(),
                    reason,
                });
                continue;
            }
        };
        let Some(source) = setup.source(id) else {
            continue;
        };
        loads.push(Load {
            stream: TunedStream {
                source: id.clone(),
                width,
                height,
                fps,
            },
            input: (width, height),
            encoder: iso.encoder_for(source),
        });
    }
    // The program shares the encoders, drawn at the canvas size.
    let program = &setup.recording.program;
    if program.enabled {
        let (width, height) = program.size(&setup.canvas);
        loads.push(Load {
            stream: TunedStream {
                source: PROGRAM.to_string(),
                width,
                height,
                fps: setup.canvas.fps,
            },
            input: (setup.canvas.width, setup.canvas.height),
            encoder: program.encoder.clone(),
        });
    }
    // The vertical program too, drawn at the vertical canvas's size and
    // rate.
    for canvas in setup.extra_canvas_ids() {
        let Some(vertical) = setup.for_canvas(canvas) else {
            continue;
        };
        let program = &vertical.recording.program;
        if program.enabled {
            let (width, height) = program.size(&vertical.canvas);
            loads.push(Load {
                stream: TunedStream {
                    source: canvas.program().to_string(),
                    width,
                    height,
                    fps: vertical.canvas.fps,
                },
                input: (vertical.canvas.width, vertical.canvas.height),
                encoder: program.encoder.clone(),
            });
        }
    }
    Ok((loads, skipped))
}

/// An ISO source's size and rate: its device's format (or the test
/// pattern's). A device whose size is only known once it streams (an
/// iPhone screen) is opened briefly to learn it.
fn iso_format<P: Platform>(platform: &P, setup: &Setup, id: &str) -> Result<(u32, u32, u32)> {
    let source = setup
        .source(id)
        .with_context(|| format!("recording.iso.sources names unknown source {id:?}"))?;
    if let SourceKind::TestPattern(pattern) = &source.kind {
        return Ok((pattern.width, pattern.height, pattern.fps));
    }
    let probe = platform
        .probe(id, &source.kind, &setup.canvas)
        .with_context(|| format!("source {id:?}"))?;
    if let Some(format) = probe.and_then(|p| p.video) {
        return Ok((format.width, format.height, format.fps));
    }
    let timeline = Arc::new(Timeline::default());
    timeline.set_start(platform.now());
    let opened = source::open(platform, setup, id, &timeline, &Default::default())
        .with_context(|| format!("source {id:?}: could not open it"))?;
    let video = opened
        .video
        .as_ref()
        .with_context(|| format!("source {id:?} has no picture"))?;
    Ok((video.width, video.height, video.fps))
}

/// A change to the setup to try.
struct Change {
    description: String,
    patch: Value,
    setup: Setup,
    cost: u32,
    changes: usize,
    /// It lowers the quality preset (which the pixel rate doesn't predict).
    quality: bool,
}

/// Changes to `setup` that ask less of the encoders, closest first: a
/// smaller program file, one quality preset lower, half the frame rate, one
/// ISO file fewer, and their combinations. Each is a valid setup.
fn changes(setup: &Setup) -> Vec<Change> {
    // Each kind: (description, patch, cost) choices.
    let mut program = Vec::new();
    let recording = &setup.recording;
    if recording.program.enabled {
        let (canvas_w, canvas_h) = (setup.canvas.width, setup.canvas.height);
        let (width, height) = recording.program.size(&setup.canvas);
        let short = canvas_w.min(canvas_h);
        let even = |side: u32, to: u32| {
            ((f64::from(side) * f64::from(to) / f64::from(short) / 2.0).round() as u32 * 2).max(16)
        };
        for (step, side) in PROGRAM_SIDES
            .iter()
            .filter(|side| **side < width.min(height))
            .take(2)
            .enumerate()
        {
            let (w, h) = (even(canvas_w, *side), even(canvas_h, *side));
            program.push((
                format!("program at {w}×{h}"),
                json!({ "recording": { "program": { "width": w, "height": h } } }),
                COST_PROGRAM_STEP * (step as u32 + 1),
            ));
        }
    }
    let mut quality = Vec::new();
    {
        let iso_used = recording.iso.enabled && !recording.iso.sources.is_empty();
        // Only its codec and quality matter here, not a file's depth.
        let iso_encoder = recording.iso.encoder.at_depth(8);
        let mut patch = serde_json::Map::new();
        let mut names = Vec::new();
        for (key, used, encoder) in [
            (
                "program",
                recording.program.enabled,
                &recording.program.encoder,
            ),
            ("iso", iso_used, &iso_encoder),
        ] {
            if let Some(lower) = lower_quality(encoder).filter(|_| used) {
                let value = serde_json::to_value(lower).unwrap_or(Value::Null);
                patch.insert(key.into(), json!({ "encoder": { "quality": value } }));
                names.push((key, value));
            }
        }
        if !names.is_empty() {
            let text = |value: &Value| match value {
                Value::String(name) => name.replace('_', " "),
                value => value.to_string(),
            };
            let description = if names.len() == 2 && names[0].1 == names[1].1 {
                format!("quality {}", text(&names[0].1))
            } else {
                names
                    .iter()
                    .map(|(key, value)| {
                        let file = if *key == "iso" { "ISO" } else { "program" };
                        format!("{file} quality {}", text(value))
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            quality.push((
                description,
                json!({ "recording": Value::Object(patch) }),
                COST_QUALITY,
            ));
        }
    }
    let mut rate = Vec::new();
    let fps = setup.canvas.fps;
    if fps >= 48 && fps.is_multiple_of(2) {
        rate.push((
            format!("{} fps", fps / 2),
            json!({ "canvas": { "fps": fps / 2 } }),
            COST_FRAME_RATE,
        ));
    }
    let mut drop = Vec::new();
    if recording.iso.enabled {
        for id in &recording.iso.sources {
            let rest: Vec<&String> = recording.iso.sources.iter().filter(|s| *s != id).collect();
            drop.push((
                format!("without the {id} ISO file"),
                json!({ "recording": { "iso": { "sources": rest } } }),
                COST_DROP_ISO,
            ));
        }
    }

    let base = match serde_json::to_value(setup) {
        Ok(value) => value,
        Err(_) => return Vec::new(),
    };
    let none = || vec![None];
    let options = |list: Vec<(String, Value, u32)>| {
        none()
            .into_iter()
            .chain(list.into_iter().map(Some))
            .collect::<Vec<_>>()
    };
    let (program, quality, rate, drop) = (
        options(program),
        options(quality),
        options(rate),
        options(drop),
    );
    let mut out = Vec::new();
    for p in &program {
        for q in &quality {
            for r in &rate {
                for d in &drop {
                    let parts: Vec<&(String, Value, u32)> =
                        [p, q, r, d].into_iter().flatten().collect();
                    if parts.is_empty() {
                        continue;
                    }
                    let patch = parts
                        .iter()
                        .fold(json!({}), |patch, part| merge_patch(&patch, &part.1));
                    let Ok(changed) = Setup::from_value(merge_patch(&base, &patch)) else {
                        continue;
                    };
                    if !changed.validate().is_empty() {
                        continue;
                    }
                    out.push(Change {
                        description: parts
                            .iter()
                            .map(|part| part.0.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                        patch,
                        setup: changed,
                        cost: parts.iter().map(|part| part.2).sum(),
                        changes: parts.len(),
                        quality: q.is_some(),
                    });
                }
            }
        }
    }
    out.sort_by_key(|change| (change.cost, change.changes));
    out
}

/// One preset below `encoder`'s quality, for H.264 and HEVC.
fn lower_quality(encoder: &Encoder) -> Option<EncoderQuality> {
    if !matches!(encoder.codec, Codec::H264 | Codec::Hevc) {
        return None;
    }
    let value = encoder.quality.value();
    [
        QualityPreset::VeryHigh,
        QualityPreset::High,
        QualityPreset::Medium,
        QualityPreset::Low,
    ]
    .into_iter()
    .map(EncoderQuality::Preset)
    .find(|preset| preset.value() < value)
}

/// What tune settled on.
enum Pick {
    /// The setup as it is fits.
    AsIs,
    /// Another ISO encoder (a candidate's index) fits.
    Codec(usize),
    /// A change (a variant's index) fits.
    Variant(usize),
    /// Nothing tried fits with room to spare; this one (the setup as it is
    /// if `None`) keeps up, with no margin.
    NoMargin(Option<usize>),
}

struct Tuner<'a, P: Platform> {
    platform: &'a P,
    setup: &'a Setup,
    scratch: PathBuf,
    free: u64,
    reserve: f64,
    /// The disk's write speed, bytes per second.
    disk: f64,
    formats: Formats,
}

impl<P: Platform> Tuner<'_, P> {
    /// Benchmark `loads` (the setup's files), unless the disk is too full
    /// to try: how many times faster than real time, and the data rate.
    fn measure(&self, loads: &[Load]) -> Result<(f64, f64)> {
        let seconds = f64::from(WARM_UP_SECONDS + CONTENT_SECONDS);
        // Leave room: the benchmark writes its files before deleting them.
        if (self.free as f64) < estimate(loads) * seconds + self.reserve {
            bail!("not benchmarked: not enough free space to try it");
        }
        benchmark(self.platform, loads, &self.scratch)
    }

    /// Why files at `bytes_per_second` don't fit on the disk, if they don't.
    fn disk_problem(&self, bytes_per_second: f64) -> Option<String> {
        let free = self.free as f64;
        let hours = (free - self.reserve).max(0.0) / bytes_per_second.max(1.0) / 3600.0;
        if self.disk < bytes_per_second * DISK_MARGIN {
            Some(format!(
                "needs {}/s, more than half of what the disk writes ({}/s)",
                space::human(bytes_per_second),
                space::human(self.disk)
            ))
        } else if hours < HOURS_ON_DISK {
            Some(format!(
                "only {hours:.1} h fit in {} free",
                space::human(free)
            ))
        } else {
            None
        }
    }

    /// Whether the encoders keep up, and why it doesn't fit, if it doesn't.
    fn judge(&self, realtime_factor: f64, bytes_per_second: f64) -> (Capacity, Option<String>) {
        let capacity = capacity(realtime_factor);
        let reason = match capacity {
            Capacity::CannotKeepUp => Some(format!(
                "can't keep up: {realtime_factor:.2}× real time, so about {:.0}% of frames would \
                 be dropped",
                (1.0 - realtime_factor) * 100.0
            )),
            Capacity::NoMargin => Some(format!(
                "keeps up with no margin: {realtime_factor:.2}× real time; fitting takes \
                 {HEADROOM}×, so a busy Mac doesn't cost frames"
            )),
            Capacity::Fits => self.disk_problem(bytes_per_second),
        };
        (capacity, reason)
    }

    /// Whether it keeps up (with a margin or not) and the disk takes it.
    fn keeps_up(&self, capacity: Capacity, bytes_per_second: f64) -> bool {
        capacity != Capacity::CannotKeepUp && self.disk_problem(bytes_per_second).is_none()
    }

    fn run(mut self, loads: &[Load], folder: &Path) -> Result<(Tuning, Vec<(Workload, f64)>)> {
        let setup = self.setup;
        let disk_before = space::write_speed(folder)?;
        self.free = self.platform.volume(folder)?.free_bytes;
        let current = own_iso_encoder(setup);
        let programs: Vec<String> = setup
            .canvas_ids()
            .into_iter()
            .map(CanvasId::program)
            .collect();
        let program = |source: &str| programs.iter().any(|p| p == source);
        let has_iso = loads.iter().any(|l| !program(&l.stream.source));
        // Each ISO file has its own depth (a source's own may differ):
        // the setup's own files are benchmarked as they are, and
        // `current` names them only if they all share it.
        let uniform = loads
            .iter()
            .filter(|l| !program(&l.stream.source))
            .all(|l| l.encoder == current);
        let encoders = if has_iso {
            iso_encoders(&current, uniform)
        } else {
            // Without ISO files, the ISO encoder makes no difference.
            vec![current.clone()]
        };
        // A candidate is the ISO encoder its change sets: every ISO file at
        // its codec and depth, but a source's own depth stays (the change
        // keeps it). H.264 can't take a source's own 10, so it isn't a
        // choice then.
        let with = |encoder: &Encoder| -> Result<Vec<Load>> {
            loads
                .iter()
                .map(|load| -> Result<Load> {
                    match load.stream.source.as_str() {
                        source if program(source) => Ok(load.clone()),
                        id => {
                            let own = setup.source(id).and_then(|s| s.encoder.bit_depth);
                            if own == Some(10) && encoder.codec == Codec::H264 {
                                bail!(
                                    "sources.{id}.encoder.bit_depth is 10, which h264 can't \
                                     record; set it to 8 or leave it out to consider h264"
                                );
                            }
                            let file = IsoEncoder {
                                codec: encoder.codec,
                                quality: setup.recording.iso.encoder.quality,
                                container: encoder.container,
                                bit_depth: None,
                            };
                            Ok(Load {
                                encoder: file.at_depth(own.unwrap_or(encoder.bit_depth)),
                                ..load.clone()
                            })
                        }
                    }
                })
                .collect()
        };

        // Benchmark first, then judge every result against the same disk
        // reading.
        let mut results = Vec::new();
        for (i, encoder) in encoders.into_iter().enumerate() {
            let (loads, result) = match i {
                0 => (loads.to_vec(), self.measure(loads)),
                _ => match with(&encoder) {
                    Ok(loads) => {
                        let result = self.measure(&loads);
                        (loads, result)
                    }
                    // Not a choice; its reason says why.
                    Err(err) => (loads.to_vec(), Err(err)),
                },
            };
            results.push((encoder, loads, result));
        }
        // A burst test reads low while something else writes to the disk
        // (like an earlier benchmark's files still being flushed); keep the
        // better of two readings.
        let disk_after = space::write_speed(folder)?;
        self.disk = disk_before.max(disk_after);

        let mut measured = Vec::new();
        let mut candidates = Vec::new();
        for (encoder, loads, result) in results {
            match result {
                Ok((realtime_factor, bytes_per_second)) => {
                    let (capacity, reason) = self.judge(realtime_factor, bytes_per_second);
                    measured.push((workload(&loads), realtime_factor));
                    candidates.push(Candidate {
                        encoder,
                        realtime_factor,
                        bytes_per_second,
                        capacity: Some(capacity),
                        feasible: reason.is_none(),
                        reason,
                    });
                }
                // Not every codec takes every size; that one just isn't a
                // choice.
                Err(err) => candidates.push(Candidate {
                    encoder,
                    realtime_factor: 0.0,
                    bytes_per_second: estimate(&loads),
                    capacity: None,
                    feasible: false,
                    reason: Some(format!("{err:#}")),
                }),
            }
        }
        // The setup's own encoder is benchmarked first.
        let own = &candidates[0];
        let own_factor = own.capacity.map(|_| own.realtime_factor);
        let mut notes = Vec::new();
        notes.push(match own.capacity {
            Some(Capacity::Fits) if own.feasible => format!(
                "this {MACHINE} keeps up with the setup: {:.2}× real time on hard content",
                own.realtime_factor
            ),
            Some(Capacity::Fits) => format!(
                "the encoders keep up with the setup ({:.2}× real time on hard content), but it \
                 {}",
                own.realtime_factor,
                own.reason.as_deref().unwrap_or("doesn't fit the disk")
            ),
            Some(Capacity::NoMargin) => format!(
                "this {MACHINE} keeps up with the setup with no margin: {:.2}× real time on hard \
                 content, so anything else using the encoders (a stream, a call, an export) may \
                 cost frames",
                own.realtime_factor
            ),
            Some(Capacity::CannotKeepUp) => format!(
                "this {MACHINE} can't keep up with the setup: the encoders run at {:.2}× real time on \
                 hard content, so a recording would drop about {:.0}% of its frames",
                own.realtime_factor,
                (1.0 - own.realtime_factor) * 100.0
            ),
            None => format!(
                "the setup couldn't be benchmarked: {}",
                own.reason.as_deref().unwrap_or("unknown error")
            ),
        });
        if disk_before.min(disk_after) * 2.0 < self.disk {
            notes.push(format!(
                "the disk's write speed varied ({}/s, then {}/s); something else may be writing \
                 to it",
                space::human(disk_before),
                space::human(disk_after)
            ));
        }

        // When it doesn't fit, search changes, closest first.
        let codec_fit = candidates
            .iter()
            .skip(1)
            .position(|c| c.feasible)
            .map(|i| i + 1);
        let mut variants: Vec<Variant> = Vec::new();
        let mut skipped = 0;
        let mut pick = own.feasible.then_some(Pick::AsIs);
        if pick.is_none() {
            let own_load = pixel_load(loads);
            // How much faster a lower quality preset encodes than its pixel
            // rate predicts, once one has been measured.
            let mut quality_gain = None;
            for change in changes(setup) {
                if change.cost > COST_CODEC
                    && let Some(index) = codec_fit
                {
                    pick = Some(Pick::Codec(index));
                    break;
                }
                if variants.len() >= MAX_VARIANTS {
                    break;
                }
                let Ok((changed, _)) = setup_loads(self.platform, &change.setup, &mut self.formats)
                else {
                    continue;
                };
                if changed.is_empty() {
                    // Nothing left to record.
                    continue;
                }
                // What the pixel rate predicts, from the setup's own speed.
                let predicted = own_factor.map(|own| own * own_load / pixel_load(&changed));
                let learning = change.quality && quality_gain.is_none();
                let gain = if change.quality {
                    quality_gain.unwrap_or(1.0)
                } else {
                    1.0
                };
                let found_keeps_up = variants
                    .iter()
                    .any(|v| self.keeps_up(v.capacity, v.bytes_per_second));
                // Worth benchmarking if it may fit, or may be the closest
                // that keeps up, or shows what a lower preset does.
                let worth = predicted.is_none_or(|p| {
                    let p = p * gain;
                    p >= HOPELESS && (p >= HEADROOM * 0.85 || learning || !found_keeps_up)
                });
                if !worth {
                    skipped += 1;
                    continue;
                }
                let Ok((realtime_factor, bytes_per_second)) = self.measure(&changed) else {
                    continue;
                };
                if change.quality
                    && let Some(predicted) = predicted
                {
                    quality_gain = Some((realtime_factor / predicted).clamp(0.5, 2.0));
                }
                let (capacity, reason) = self.judge(realtime_factor, bytes_per_second);
                measured.push((workload(&changed), realtime_factor));
                let feasible = reason.is_none();
                variants.push(Variant {
                    description: change.description,
                    change: change.patch,
                    streams: changed.iter().map(|l| l.stream.clone()).collect(),
                    realtime_factor,
                    bytes_per_second,
                    capacity,
                    feasible,
                    reason,
                });
                if feasible {
                    pick = Some(Pick::Variant(variants.len() - 1));
                    break;
                }
            }
            if pick.is_none() {
                pick = codec_fit.map(Pick::Codec);
            }
            if pick.is_none() {
                // Nothing fits with room to spare: the closest that keeps up.
                pick = if own
                    .capacity
                    .is_some_and(|c| self.keeps_up(c, own.bytes_per_second))
                {
                    Some(Pick::NoMargin(None))
                } else {
                    variants
                        .iter()
                        .position(|v| self.keeps_up(v.capacity, v.bytes_per_second))
                        .map(|i| Pick::NoMargin(Some(i)))
                };
            }
        }

        // Sources' own depths stay as they are (they were benchmarked so).
        let iso_change = |encoder: &Encoder| {
            json!({ "recording": { "iso": { "encoder": {
                "codec": encoder.codec,
                "bit_depth": encoder.bit_depth,
            } } } })
        };
        let (recommended, recommendation, change) = match &pick {
            Some(Pick::AsIs) => (
                Some(current.clone()),
                format!(
                    "keep the setup as it is ({:.2}× real time)",
                    own.realtime_factor
                ),
                None,
            ),
            Some(Pick::Codec(index)) => {
                let c = &candidates[*index];
                (
                    Some(c.encoder.clone()),
                    format!(
                        "ISO files in {:?} {}-bit ({:.2}× real time)",
                        c.encoder.codec, c.encoder.bit_depth, c.realtime_factor
                    ),
                    Some(iso_change(&c.encoder)),
                )
            }
            Some(Pick::Variant(index) | Pick::NoMargin(Some(index))) => {
                let v = &variants[*index];
                let margin = match v.capacity {
                    Capacity::Fits => "",
                    _ => ", no margin",
                };
                let changed: Setup =
                    Setup::from_value(merge_patch(&serde_json::to_value(setup)?, &v.change))?;
                (
                    Some(own_iso_encoder(&changed)),
                    format!(
                        "{} ({:.2}× real time{margin})",
                        v.description, v.realtime_factor
                    ),
                    Some(v.change.clone()),
                )
            }
            Some(Pick::NoMargin(None)) => (
                Some(current.clone()),
                format!(
                    "keep the setup as it is ({:.2}× real time, no margin)",
                    own.realtime_factor
                ),
                None,
            ),
            None => (None, String::new(), None),
        };
        match &pick {
            Some(Pick::AsIs) | None => {}
            Some(Pick::NoMargin(_)) => notes.push(format!(
                "nothing tried keeps up with room to spare; the closest that keeps up: \
                 {recommendation}"
            )),
            Some(Pick::Codec(_) | Pick::Variant(_)) => {
                notes.push(format!("the closest change that fits: {recommendation}"));
            }
        }
        // A closer change that keeps up, if only just.
        let picked = match pick {
            Some(Pick::Variant(index) | Pick::NoMargin(Some(index))) => Some(index),
            _ => None,
        };
        if let Some(closer) = variants
            .iter()
            .take(picked.unwrap_or(0))
            .find(|v| self.keeps_up(v.capacity, v.bytes_per_second))
        {
            notes.push(format!(
                "closer, but with no margin: {} ({:.2}× real time)",
                closer.description, closer.realtime_factor
            ));
        }
        if pick.is_none() {
            notes.push(
                "nothing tried keeps up (see each candidate's and variant's reason); record \
                 fewer sources, or at lower resolutions or frame rates"
                    .into(),
            );
        }
        if skipped > 0 {
            notes.push(format!(
                "{skipped} other change(s) weren't benchmarked: from their pixel rates (and what \
                 a lower preset did), they would neither fit nor be the closest that keeps up"
            ));
        }
        if let Some(Pick::AsIs) = pick
            && own.realtime_factor < 2.0
        {
            notes.push(format!(
                "the encoders are about {:.0}% busy on hard content, so little room is left for \
                 more sources; ProRes runs on separate engines",
                100.0 / own.realtime_factor
            ));
        }
        if matches!(pick, Some(Pick::AsIs | Pick::Codec(_))) {
            for candidate in candidates
                .iter()
                .filter(|c| c.feasible && Some(&c.encoder) != recommended.as_ref())
            {
                notes.push(format!(
                    "also feasible: {:?} {}-bit at {:.1}× real time, about {}/minute",
                    candidate.encoder.codec,
                    candidate.encoder.bit_depth,
                    candidate.realtime_factor,
                    space::human(candidate.bytes_per_second * 60.0)
                ));
            }
        }
        let tuning = Tuning {
            streams: loads.iter().map(|l| l.stream.clone()).collect(),
            disk_bytes_per_second: self.disk,
            free_bytes: self.free,
            capacity: own.capacity,
            realtime_factor: own.realtime_factor,
            candidates: candidates.clone(),
            variants,
            recommended,
            recommendation: (!recommendation.is_empty()).then_some(recommendation),
            change,
            notes,
            skipped: Vec::new(),
        };
        Ok((tuning, measured))
    }
}

/// The ISO encoder tune names `setup`'s files by: its first ISO source's
/// (which may have a depth of its own), else the ISO encoder at its depth.
fn own_iso_encoder(setup: &Setup) -> Encoder {
    let iso = &setup.recording.iso;
    match iso.sources.iter().find_map(|id| setup.source(id)) {
        Some(source) if iso.enabled => iso.encoder_for(source),
        _ => iso.encoder.at_depth(iso.encoder.depth()),
    }
}

/// The ISO encoders benchmarked: the setup's, then HEVC 8 and 10-bit,
/// H.264 and ProRes, each once, each set for every ISO file. `uniform`:
/// every ISO file of the setup is at `current`, so another the same isn't
/// benchmarked again.
fn iso_encoders(current: &Encoder, uniform: bool) -> Vec<Encoder> {
    let with = |codec, bit_depth| {
        IsoEncoder {
            codec,
            quality: current.quality,
            container: current.container,
            bit_depth: Some(bit_depth),
        }
        .at_depth(bit_depth)
    };
    let encoders = [
        current.clone(),
        with(Codec::Hevc, 8),
        with(Codec::Hevc, 10),
        with(Codec::H264, 8),
        with(Codec::Prores422, 8),
        with(Codec::Prores422Hq, 8),
    ];
    let mut seen = Vec::new();
    encoders
        .into_iter()
        .enumerate()
        .filter(|(i, e)| {
            if *i == 0 && !uniform {
                return true;
            }
            // ProRes ignores bit_depth: it always keeps 10 bits or more.
            let prores = !matches!(e.codec, Codec::H264 | Codec::Hevc);
            let key = (e.codec, if prores { 0 } else { e.bit_depth });
            let new = !seen.contains(&key);
            seen.push(key);
            new
        })
        .map(|(_, e)| e)
        .collect()
}

/// Encode every file's video at once, as fast as the encoders take frames.
/// Returns how many times faster than real time they ran, and their data
/// rate together.
fn benchmark<P: Platform>(platform: &P, loads: &[Load], scratch: &Path) -> Result<(f64, f64)> {
    let barrier = Barrier::new(loads.len());
    let results: Vec<Result<(Duration, u64)>> = thread::scope(|scope| {
        let threads: Vec<_> = loads
            .iter()
            .enumerate()
            .map(|(i, load)| {
                let spec = load.spec(scratch.join(format!("{i}.mov")));
                let barrier = &barrier;
                scope.spawn(move || encode_stream(platform, &spec, load.input, barrier))
            })
            .collect();
        threads
            .into_iter()
            .map(|t| t.join().expect("benchmark thread panicked"))
            .collect()
    });
    let mut slowest = Duration::ZERO;
    let mut bytes = 0;
    for result in results {
        let (elapsed, size) = result?;
        slowest = slowest.max(elapsed);
        bytes += size;
    }
    Ok((
        CONTENT_SECONDS as f64 / slowest.as_secs_f64().max(1e-6),
        bytes as f64 / (WARM_UP_SECONDS + CONTENT_SECONDS) as f64,
    ))
}

/// Encode one stream of moving, hard-to-compress frames of `input` size
/// (scaled to the file's, if it differs). Returns how long the timed part
/// took (after the warm-up, until the encoder took its last frame; frames
/// in flight cancel out at both ends), and the file's size. A failed stream
/// removes its file, which would otherwise make the next candidate's fail
/// with "File exists".
fn encode_stream<P: Platform>(
    platform: &P,
    spec: &WriterSpec,
    input: (u32, u32),
    barrier: &Barrier,
) -> Result<(Duration, u64)> {
    let result = encode_stream_to(platform, spec, input, barrier);
    if result.is_err() {
        let _ = std::fs::remove_file(&spec.path);
    }
    result
}

fn encode_stream_to<P: Platform>(
    platform: &P,
    spec: &WriterSpec,
    input: (u32, u32),
    barrier: &Barrier,
) -> Result<(Duration, u64)> {
    let video = &spec.video;
    let (width, height) = (input.0 as usize, input.1 as usize);
    let prepared = (|| -> Result<_> {
        let texture = Texture::new(width + TEXTURE_MARGIN, height + TEXTURE_MARGIN);
        let mut pool = platform.frame_pool(input.0, input.1)?;
        let writer = platform.create_writer(spec)?;
        // Draw the frames before the clock starts, as a capture hands the
        // encoder finished frames; the encoder cycles through them.
        let mut cycle = Vec::new();
        for k in 0..CYCLE {
            let (dx, dy) = ((k * 7) % TEXTURE_MARGIN, (k * 5) % TEXTURE_MARGIN);
            let frame = pool
                .draw(&mut |pixels, stride| texture.copy(dx, dy, width, height, pixels, stride))?
                .context("could not draw benchmark frames")?;
            cycle.push(frame);
        }
        Ok((writer, cycle))
    })();
    // Every stream waits here, even one that failed, so none waits forever.
    barrier.wait();
    let (mut writer, cycle) = prepared?;
    let warm_up = WARM_UP_SECONDS as u64 * video.fps as u64;
    let frames = warm_up + CONTENT_SECONDS as u64 * video.fps as u64;
    let mut started = Instant::now();
    let base = Time::from_nanos(0);
    for n in 0..frames {
        if n == warm_up {
            started = Instant::now();
        }
        let frame = &cycle[n as usize % CYCLE];
        let pts = base.add_ticks(n, video.fps);
        if n == 0 {
            writer.start(pts)?;
        }
        while !writer.append_video(frame, pts)? {
            thread::sleep(Duration::from_micros(500));
        }
    }
    let elapsed = started.elapsed();
    writer.finish(base.add_ticks(frames, video.fps))?;
    let size = std::fs::metadata(&spec.path).map(|m| m.len()).unwrap_or(0);
    let _ = std::fs::remove_file(&spec.path);
    Ok((elapsed, size))
}

/// Size of the texture's coloured blocks, in pixels.
const BLOCK: usize = 32;
/// Levels of per-pixel noise, about what a camera sensor adds.
const NOISE: u32 = 8;

/// Busy, camera-like content: coloured blocks with fine noise, cut into
/// shifted windows so every frame moves.
pub(crate) struct Texture {
    width: usize,
    pixels: Vec<u8>,
}

impl Texture {
    pub(crate) fn new(width: usize, height: usize) -> Self {
        let mut pixels = vec![0u8; width * height * 4];
        let mut x: u32 = 0x9E37_79B9;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        let blocks_x = width.div_ceil(BLOCK);
        let blocks: Vec<u32> = (0..blocks_x * height.div_ceil(BLOCK))
            .map(|_| next())
            .collect();
        for y in 0..height {
            for px in 0..width {
                let block = blocks[(y / BLOCK) * blocks_x + px / BLOCK].to_le_bytes();
                let noise = (next() % NOISE) as u8;
                let i = (y * width + px) * 4;
                pixels[i] = block[0] / 2 + noise;
                pixels[i + 1] = block[1] / 2 + noise;
                pixels[i + 2] = block[2] / 2 + noise;
                pixels[i + 3] = 255;
            }
        }
        Self { width, pixels }
    }

    /// Copy the `width`×`height` window at (`dx`, `dy`) into a BGRA frame.
    pub(crate) fn copy(
        &self,
        dx: usize,
        dy: usize,
        width: usize,
        height: usize,
        out: &mut [u8],
        stride: usize,
    ) {
        for y in 0..height {
            let from = ((y + dy) * self.width + dx) * 4;
            out[y * stride..y * stride + width * 4]
                .copy_from_slice(&self.pixels[from..from + width * 4]);
        }
    }
}
