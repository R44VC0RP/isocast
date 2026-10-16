//! `estimate_capacity`: predict, without benchmarking, whether a setup's
//! files fit this Mac's H.264/HEVC media engines, from per-chip data kept
//! as data (`media_engines.json`), and find the closest changes that fit.
//! `tune` measures; this is instant, so `setup check`, `doctor`, adding an
//! ISO file, first launch and the live overload notice can all ask (#66).

use std::sync::OnceLock;

use isocast_protocol::api::{
    Capacity, CapacityEstimate, CapacitySuggestion, EstimatedOutput, IsoAdmission, Machine,
    OutputKind,
};
use isocast_protocol::change::merge_patch;
use isocast_protocol::{Codec, PROGRAM, Setup, SourceKind};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::capacity::{self, Judged, Memory};

/// The short sides a camera or test pattern is offered at, largest first.
const SIDES: [u32; 3] = [1440, 1080, 720];
/// How many suggestions an estimate lists.
const SUGGESTIONS: usize = 3;
/// How far each kind of change takes a setup from what was asked (as in
/// `tune`): sizes before frame rates, keeping every file before anything
/// else; turning an ISO file off is last.
const COST_PROGRAM_SIZE: u32 = 2;
const COST_STREAM_SIZE: u32 = 3;
const COST_ISO_SIZE: u32 = 4;
const COST_ISO_CODEC: u32 = 7;
const COST_ISO_RATE: u32 = 8;
const COST_CANVAS_RATE: u32 = 12;
const COST_STREAM_STOP: u32 = 15;
const COST_ISO_OFF: u32 = 20;

#[derive(Debug, Deserialize)]
struct Table {
    fits_below: f64,
    codec_cost: CodecCost,
    chips: Vec<ChipRow>,
    generations: Vec<GenerationRow>,
    tiers: Vec<TierRow>,
    unknown: Rates,
}

/// What an H.264 or HEVC picture costs (its `basis` documents them).
#[derive(Debug, Deserialize)]
struct CodecCost {
    hevc: f64,
    h264: f64,
}

#[derive(Debug, Deserialize)]
struct ChipRow {
    chip: String,
    encode_engines: u32,
    engine_pixels_per_second: f64,
    #[serde(default)]
    frame_overhead_pixels: f64,
    overload_iso_efficiency: Option<f64>,
    basis: String,
}

#[derive(Debug, Deserialize)]
struct GenerationRow {
    generation: String,
    engine_pixels_per_second: f64,
}

#[derive(Debug, Deserialize)]
struct TierRow {
    tier: String,
    encode_engines: u32,
}

#[derive(Debug, Deserialize)]
struct Rates {
    encode_engines: u32,
    engine_pixels_per_second: f64,
}

fn table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str(include_str!("media_engines.json"))
            .expect("media_engines.json is valid (checked by its test)")
    })
}

/// A chip's encode engines, and where the numbers come from.
#[derive(Debug, Clone, PartialEq)]
pub struct Chip {
    pub name: String,
    pub engines: u32,
    pub rate: f64,
    /// Per-picture session overhead, in HEVC-equivalent pixels.
    pub frame_overhead: f64,
    /// Fraction of the theoretical ISO budget retained under overload:
    /// native pressure pauses and gradual recovery leave some capacity idle.
    pub overload_iso_efficiency: f64,
    pub basis: String,
}

/// The engines of `chip` (as macOS names it, like "Apple M2 Pro"): its own
/// measured row, else its generation's rate per engine and its tier's
/// engine count, else the newest generation's, else a cautious guess.
pub fn chip(name: &str) -> Chip {
    let table = table();
    if let Some(row) = table.chips.iter().find(|row| row.chip == name) {
        return Chip {
            name: name.into(),
            engines: row.encode_engines,
            rate: row.engine_pixels_per_second,
            frame_overhead: row.frame_overhead_pixels,
            overload_iso_efficiency: row.overload_iso_efficiency.unwrap_or(1.0),
            basis: row.basis.clone(),
        };
    }
    let rest = name.strip_prefix("Apple ").unwrap_or(name);
    let mut words = rest.splitn(2, ' ');
    let generation = words.next().unwrap_or_default();
    let tier = words.next().unwrap_or_default().trim();
    let engines = table
        .tiers
        .iter()
        .find(|row| row.tier == tier)
        .map(|row| row.encode_engines);
    let known = table
        .generations
        .iter()
        .find(|row| row.generation == generation);
    let apple = generation.starts_with('M') && generation[1..].parse::<u32>().is_ok();
    match (known, engines) {
        (Some(row), Some(engines)) => Chip {
            name: name.into(),
            engines,
            rate: row.engine_pixels_per_second,
            frame_overhead: 0.0,
            overload_iso_efficiency: 1.0,
            basis: format!(
                "assumed: {engines} encode engine(s) as on other {} chips, each as fast as \
                 measured on other {generation} chips; not measured on this chip",
                if tier.is_empty() { "base" } else { tier }
            ),
        },
        (None, Some(engines)) if apple => {
            let newest = table
                .generations
                .last()
                .map_or(table.unknown.engine_pixels_per_second, |r| {
                    r.engine_pixels_per_second
                });
            Chip {
                name: name.into(),
                engines,
                rate: newest,
                frame_overhead: 0.0,
                overload_iso_efficiency: 1.0,
                basis: format!(
                    "assumed: a newer chip than any measured, taken to be as fast as the newest \
                     measured generation, with {engines} encode engine(s)"
                ),
            }
        }
        _ => Chip {
            name: name.into(),
            engines: table.unknown.encode_engines,
            rate: table.unknown.engine_pixels_per_second,
            frame_overhead: 0.0,
            overload_iso_efficiency: 1.0,
            basis: "assumed: a chip with no measured data, judged cautiously".into(),
        },
    }
}

/// What a setup's files ask of the engines.
#[derive(Debug, Clone)]
struct Judgement {
    outputs: Vec<EstimatedOutput>,
    load: f64,
    capacity: Capacity,
    iso_drop: f64,
}

/// A source's (width, height, fps) while it runs, if known.
pub type Formats<'a> = &'a dyn Fn(&str) -> Option<(u32, u32, u32)>;

/// What to judge.
pub struct Inputs<'a> {
    pub machine: &'a Machine,
    pub setup: &'a Setup,
    /// Running (or probed) sources' formats; a camera's or test pattern's
    /// own fields in the setup win.
    pub formats: Formats<'a>,
    pub streaming: bool,
    /// `tune`'s measurements, and this Mac's key in them.
    pub memory: Option<(&'a Memory, &'a str)>,
    /// A recording runs: setup changes wait for the take to end.
    pub recording: bool,
}

/// An ISO source's size and rate: its own fields (a camera's size and
/// rate if set, a test pattern's), else what it runs at, else the
/// canvas's (assumed).
fn iso_format(setup: &Setup, formats: Formats, id: &str) -> (u32, u32, u32, bool) {
    let live = formats(id);
    let canvas = (setup.canvas.width, setup.canvas.height, setup.canvas.fps);
    match setup.source(id).map(|s| &s.kind) {
        Some(SourceKind::TestPattern(p)) => (p.width, p.height, p.fps, false),
        Some(SourceKind::Camera(c)) => {
            let (w, h, f) = live.unwrap_or(canvas);
            let size = match (c.width, c.height) {
                (Some(w), Some(h)) => Some((w, h)),
                _ => None,
            };
            let (w, h) = size.unwrap_or((w, h));
            let fps = c.fps.unwrap_or(f);
            (
                w,
                h,
                fps,
                live.is_none() && (size.is_none() || c.fps.is_none()),
            )
        }
        _ => match live {
            Some((w, h, f)) => (w, h, f, false),
            None => (canvas.0, canvas.1, canvas.2, true),
        },
    }
}

fn codec_cost(codec: Codec) -> Option<f64> {
    let table = table();
    match codec {
        Codec::Hevc => Some(table.codec_cost.hevc),
        Codec::H264 => Some(table.codec_cost.h264),
        // ProRes runs on engines of its own.
        _ => None,
    }
}

fn judge(chip: &Chip, setup: &Setup, formats: Formats, streaming: bool) -> Judgement {
    let mut outputs = Vec::new();
    let mut push = |output: String,
                    kind: OutputKind,
                    (width, height, fps): (u32, u32, u32),
                    codec: Codec,
                    assumed: bool| {
        let pixels = f64::from(width) * f64::from(height) * f64::from(fps);
        outputs.push(EstimatedOutput {
            output,
            kind,
            width,
            height,
            fps,
            codec,
            protected: kind != OutputKind::Iso,
            pixels_per_second: codec_cost(codec).map_or(0.0, |cost| {
                pixels * cost + f64::from(fps) * chip.frame_overhead
            }),
            assumed,
        });
    };
    let recording = &setup.recording;
    let fps = setup.canvas.fps;
    if recording.program.enabled {
        let (w, h) = recording.program.size(&setup.canvas);
        push(
            PROGRAM.into(),
            OutputKind::Program,
            (w, h, fps),
            recording.program.encoder.codec,
            false,
        );
    }
    if streaming {
        // One encode per distinct format among the enabled destinations
        // (#73): the main one's is `stream`, others `stream:<id>`.
        let canvas = (setup.canvas.width, setup.canvas.height);
        let encodes = setup
            .stream
            .as_ref()
            .map(|s| s.encodes(canvas))
            .unwrap_or_default();
        if encodes.len() <= 1 {
            let (w, h) = encodes
                .first()
                .map_or_else(|| stream_size(setup), |e| (e.width, e.height));
            push(
                "stream".into(),
                OutputKind::Stream,
                (w, h, fps),
                Codec::H264,
                false,
            );
        } else {
            for (i, encode) in encodes.iter().enumerate() {
                let name = if i == 0 {
                    "stream".to_string()
                } else {
                    format!("stream:{}", encode.ids[0])
                };
                push(
                    name,
                    OutputKind::Stream,
                    (encode.width, encode.height, fps),
                    Codec::H264,
                    false,
                );
            }
        }
    }
    if recording.iso.enabled {
        for id in &recording.iso.sources {
            let (w, h, f, assumed) = iso_format(setup, formats, id);
            push(
                id.clone(),
                OutputKind::Iso,
                (w, h, f),
                recording.iso.encoder.codec,
                assumed,
            );
        }
    }
    // Each encoder lands on the least busy engine, the largest first (the
    // writers start them in that order, #41).
    let mut sizes: Vec<f64> = outputs
        .iter()
        .map(|o| o.pixels_per_second)
        .filter(|p| *p > 0.0)
        .collect();
    sizes.sort_by(|a, b| b.total_cmp(a));
    let mut engines = vec![0.0f64; chip.engines.max(1) as usize];
    for size in sizes {
        if let Some(least) = engines.iter_mut().min_by(|a, b| a.total_cmp(b)) {
            *least += size;
        }
    }
    let busiest = engines.iter().copied().fold(0.0, f64::max);
    let load = busiest / chip.rate;
    let excess: f64 = engines.iter().map(|e| (e - chip.rate).max(0.0)).sum();
    let iso: f64 = outputs
        .iter()
        .filter(|o| !o.protected)
        .map(|o| o.pixels_per_second)
        .sum();
    let iso_drop = if excess <= 0.0 {
        0.0
    } else if iso > 0.0 {
        let available = (1.0 - excess / iso).max(0.0);
        (1.0 - available * chip.overload_iso_efficiency) * 100.0
    } else {
        100.0
    };
    Judgement {
        outputs,
        load,
        capacity: verdict(load),
        iso_drop,
    }
}

/// The stream's size: its own, else the canvas scaled to fit 1080p.
fn stream_size(setup: &Setup) -> (u32, u32) {
    let canvas = (setup.canvas.width, setup.canvas.height);
    match &setup.stream {
        Some(settings) => settings.output_size(canvas),
        None => {
            let short = canvas.0.min(canvas.1);
            if short <= 1080 {
                canvas
            } else {
                (scaled(canvas.0, 1080, short), scaled(canvas.1, 1080, short))
            }
        }
    }
}

fn verdict(load: f64) -> Capacity {
    if load <= table().fits_below {
        Capacity::Fits
    } else if load <= 1.0 {
        Capacity::NoMargin
    } else {
        Capacity::CannotKeepUp
    }
}

/// The same for setups that ask the same of the encoders.
fn workload_key(outputs: &[EstimatedOutput]) -> String {
    let mut parts: Vec<String> = outputs
        .iter()
        .map(|o| {
            format!(
                "{:?}:{}x{}@{}:{:?}",
                o.kind, o.width, o.height, o.fps, o.codec
            )
        })
        .collect();
    parts.sort();
    // FNV-1a: stable across runs and builds.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in parts.join(",").bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn percent(load: f64) -> String {
    format!("{:.0}%", load * 100.0)
}

fn summary(judgement: &Judgement) -> String {
    let busiest = percent(judgement.load);
    match judgement.capacity {
        Capacity::Fits => format!(
            "this Mac's encoders keep up with these files (the busiest engine at about {busiest})"
        ),
        Capacity::NoMargin => format!(
            "this Mac's encoders keep up with these files with no margin (the busiest engine \
             at about {busiest}): anything else using them, like a stream or a video call, may \
             cost ISO frames"
        ),
        Capacity::CannotKeepUp => {
            let iso = judgement.outputs.iter().any(|o| !o.protected);
            if iso {
                format!(
                    "this Mac's encoders can't keep up with these files (the busiest engine at \
                     about {busiest}): ISO files will drop at least {:.0}% of their frames so \
                     the program and stream stay whole",
                    judgement.iso_drop.max(1.0)
                )
            } else {
                format!(
                    "this Mac's encoders can't keep up with these files (the busiest engine at \
                     about {busiest}): the program and stream will drop frames"
                )
            }
        }
    }
}

/// One change to try.
#[derive(Debug, Clone)]
struct Option_ {
    description: String,
    patch: Option<Value>,
    method: Option<&'static str>,
    cost: u32,
    /// What it changes: a source id, or "program", "stream", "iso", "canvas".
    target: String,
    off: bool,
}

/// An even side scaled like `side` is to `short`.
fn scaled(side: u32, to: u32, short: u32) -> u32 {
    ((f64::from(side) * f64::from(to) / f64::from(short.max(1)) / 2.0).round() as u32 * 2).max(16)
}

/// Smaller sizes and a lower rate for source `id` (a camera or test
/// pattern; a screen or window records at its own size).
fn source_options(setup: &Setup, formats: Formats, id: &str) -> Vec<Option_> {
    let Some(source) = setup.source(id) else {
        return Vec::new();
    };
    if !matches!(
        source.kind,
        SourceKind::Camera(_) | SourceKind::TestPattern(_)
    ) {
        return Vec::new();
    }
    let (w, h, fps, _) = iso_format(setup, formats, id);
    let short = w.min(h);
    let mut out = Vec::new();
    for (step, side) in SIDES.iter().filter(|s| **s < short).take(2).enumerate() {
        let (nw, nh) = (scaled(w, *side, short), scaled(h, *side, short));
        out.push(Option_ {
            description: format!("capture {id} at {nw}×{nh}"),
            patch: Some(json!({ "sources": [{ "id": id, "width": nw, "height": nh }] })),
            method: None,
            cost: COST_ISO_SIZE + step as u32,
            target: id.into(),
            off: false,
        });
    }
    if fps >= 48 {
        out.push(Option_ {
            description: format!("capture {id} at {} fps", fps / 2),
            patch: Some(json!({ "sources": [{ "id": id, "fps": fps / 2 }] })),
            method: None,
            cost: COST_ISO_RATE,
            target: id.into(),
            off: false,
        });
    }
    out
}

/// Recording `key`'s files (`iso` or `program`) with the cheaper of HEVC
/// and H.264, if they use the other.
fn codec_option(key: &str, what: &str, codec: Codec) -> Option<Option_> {
    let table = table();
    let (to, name) = match codec {
        Codec::H264 if table.codec_cost.hevc < table.codec_cost.h264 => ("hevc", "HEVC"),
        Codec::Hevc if table.codec_cost.h264 < table.codec_cost.hevc => ("h264", "H.264"),
        _ => return None,
    };
    Some(Option_ {
        description: format!("record {what} as {name}"),
        patch: Some(json!({ "recording": { key: { "encoder": { "codec": to } } } })),
        method: None,
        cost: COST_ISO_CODEC,
        target: format!("{key}-codec"),
        off: false,
    })
}

/// Every single change that asks less of the encoders.
fn options(setup: &Setup, formats: Formats, streaming: bool) -> Vec<Option_> {
    let recording = &setup.recording;
    let mut out = Vec::new();
    let iso_on = recording.iso.enabled && !recording.iso.sources.is_empty();
    if iso_on {
        for id in &recording.iso.sources {
            out.extend(source_options(setup, formats, id));
            let rest: Vec<&String> = recording.iso.sources.iter().filter(|s| *s != id).collect();
            out.push(Option_ {
                description: format!("turn off {id}'s ISO file"),
                patch: Some(json!({ "recording": { "iso": { "sources": rest } } })),
                method: None,
                cost: COST_ISO_OFF,
                target: id.clone(),
                off: true,
            });
        }
        out.extend(codec_option(
            "iso",
            "ISO files",
            recording.iso.encoder.codec,
        ));
    }
    if recording.program.enabled {
        out.extend(codec_option(
            PROGRAM,
            "the program",
            recording.program.encoder.codec,
        ));
    }
    if recording.program.enabled {
        let (cw, ch) = (setup.canvas.width, setup.canvas.height);
        let (w, h) = recording.program.size(&setup.canvas);
        let short = cw.min(ch);
        for (step, side) in SIDES.iter().filter(|s| **s < w.min(h)).take(2).enumerate() {
            let (nw, nh) = (scaled(cw, *side, short), scaled(ch, *side, short));
            out.push(Option_ {
                description: format!("record the program at {nw}×{nh}"),
                patch: Some(json!({ "recording": { "program": { "width": nw, "height": nh } } })),
                method: None,
                cost: COST_PROGRAM_SIZE * (step as u32 + 1),
                target: PROGRAM.into(),
                off: false,
            });
        }
    }
    if streaming {
        if setup.stream.is_some() {
            let (w, h) = stream_size(setup);
            let short = w.min(h);
            if short > 720 {
                let (nw, nh) = (scaled(w, 720, short), scaled(h, 720, short));
                out.push(Option_ {
                    description: format!("stream at {nw}×{nh}"),
                    patch: Some(json!({ "stream": { "width": nw, "height": nh } })),
                    method: None,
                    cost: COST_STREAM_SIZE,
                    target: "stream".into(),
                    off: false,
                });
            }
        }
        out.push(Option_ {
            description: "stop the stream".into(),
            patch: None,
            method: Some("stream_stop"),
            cost: COST_STREAM_STOP,
            target: "stream".into(),
            off: true,
        });
    }
    let fps = setup.canvas.fps;
    if fps >= 48 && fps.is_multiple_of(2) {
        out.push(Option_ {
            description: format!("record and stream at {} fps", fps / 2),
            patch: Some(json!({ "canvas": { "fps": fps / 2 } })),
            method: None,
            cost: COST_CANVAS_RATE,
            target: "canvas".into(),
            off: false,
        });
    }
    out
}

/// `setup` with `patch`, if that adds no problems to it.
fn patched(setup: &Setup, patch: &Value) -> Option<Setup> {
    let base = serde_json::to_value(setup).ok()?;
    let changed = Setup::from_value(merge_patch(&base, patch)).ok()?;
    (changed.validate().len() <= setup.validate().len()).then_some(changed)
}

/// A tried combination: (cost, changes, suggestion, what it changes).
type Tried = (u32, usize, CapacitySuggestion, Vec<String>);

/// Singles and pairs of `options`, judged.
fn try_options(
    chip: &Chip,
    setup: &Setup,
    formats: Formats,
    streaming: bool,
    options: &[Option_],
    recording: bool,
) -> Vec<Tried> {
    let mut combos: Vec<Vec<&Option_>> = options.iter().map(|o| vec![o]).collect();
    for (i, a) in options.iter().enumerate() {
        for b in &options[i + 1..] {
            // A method (stop the stream) stands alone: a client applies a
            // patch or calls a method, not both.
            let clash = a.method.is_some()
                || b.method.is_some()
                || a.target == b.target && (a.off || b.off || a.description == b.description);
            // Two sizes of one thing aren't a pair.
            let same_kind = a.target == b.target
                && a.description.split(" at ").next() == b.description.split(" at ").next()
                && a.description.contains('×') == b.description.contains('×');
            if !clash && !same_kind {
                combos.push(vec![a, b]);
            }
        }
    }
    let mut tried = Vec::new();
    for combo in combos {
        let mut patch: Option<Value> = None;
        let mut method = None;
        for option in &combo {
            if let Some(part) = &option.patch {
                patch = Some(match patch {
                    Some(p) => merge_patch(&p, part),
                    None => part.clone(),
                });
            }
            if option.method.is_some() {
                method = option.method;
            }
        }
        let changed = match &patch {
            Some(p) => match patched(setup, p) {
                Some(s) => s,
                None => continue,
            },
            None => setup.clone(),
        };
        let streams = streaming && method != Some("stream_stop");
        let judgement = judge(chip, &changed, formats, streams);
        if judgement.outputs.iter().all(|o| o.pixels_per_second == 0.0)
            && !judgement.outputs.is_empty()
        {
            continue;
        }
        let touches = |key: &str| patch.as_ref().is_some_and(|p| p.get(key).is_some());
        let after_take =
            recording && patch.is_some() || touches("canvas") && streaming && method.is_none();
        tried.push((
            combo.iter().map(|o| o.cost).sum(),
            combo.len(),
            CapacitySuggestion {
                description: combo
                    .iter()
                    .map(|o| o.description.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                change: patch,
                method: method.map(Into::into),
                load: judgement.load,
                capacity: judgement.capacity,
                after_take,
            },
            combo.iter().map(|o| o.target.clone()).collect(),
        ));
    }
    tried
}

/// The closest changes that fit; if none, those with no margin; if none
/// keeps up, those that come closest.
fn pick(mut tried: Vec<Tried>) -> Vec<CapacitySuggestion> {
    tried.sort_by_key(|a| (a.0, a.1));
    for wanted in [Capacity::Fits, Capacity::NoMargin] {
        // Different ways, not one change with each of the others.
        let mut used: Vec<&str> = Vec::new();
        let mut found = Vec::new();
        for t in tried.iter().filter(|t| t.2.capacity == wanted) {
            if found.len() == SUGGESTIONS {
                break;
            }
            if t.3.iter().any(|part| used.contains(&part.as_str())) {
                continue;
            }
            used.extend(t.3.iter().map(String::as_str));
            found.push(t.2.clone());
        }
        if !found.is_empty() {
            return found;
        }
    }
    tried.sort_by(|a, b| a.2.load.total_cmp(&b.2.load));
    tried.into_iter().take(2).map(|t| t.2).collect()
}

/// How to turn on `id`'s ISO file so the setup keeps up: as it is, else
/// with the source smaller or slower, else not at all.
fn admission(
    chip: &Chip,
    setup: &Setup,
    formats: Formats,
    streaming: bool,
    id: &str,
) -> Result<IsoAdmission, String> {
    let source = setup.source(id).ok_or_else(|| {
        let ids: Vec<&str> = setup
            .sources
            .iter()
            .filter(|s| s.kind.has_video())
            .map(|s| s.id.as_str())
            .collect();
        format!(
            "add_iso: no source {id:?}; video sources: {}",
            ids.join(", ")
        )
    })?;
    if !source.kind.has_video() {
        return Err(format!("add_iso: source {id:?} has no picture to record"));
    }
    let iso = &setup.recording.iso;
    if iso.enabled && iso.sources.iter().any(|s| s == id) {
        return Ok(IsoAdmission {
            source: id.into(),
            on: true,
            as_is: true,
            change: None,
            description: format!("{id}'s ISO file is already on"),
            reason: None,
            width: None,
            height: None,
            fps: None,
        });
    }
    let mut sources = iso.sources.clone();
    sources.push(id.into());
    let add = json!({ "recording": { "iso": { "enabled": true, "sources": sources } } });
    let Some(added) = patched(setup, &add) else {
        return Err(format!(
            "add_iso: turning on {id:?}'s ISO file leaves an invalid setup"
        ));
    };
    let full = judge(chip, &added, formats, streaming);
    if full.capacity != Capacity::CannotKeepUp {
        let (w, h, fps, _) = iso_format(&added, formats, id);
        return Ok(IsoAdmission {
            source: id.into(),
            on: true,
            as_is: true,
            change: Some(add),
            description: format!("{id}'s ISO file"),
            reason: None,
            width: Some(w),
            height: Some(h),
            fps: Some(fps),
        });
    }
    let why = format!(
        "at full size, this Mac's encoders can't keep up with it beside the other files (the \
         busiest engine at about {})",
        percent(full.load)
    );
    // Only this source changes: the rest of the setup is the user's.
    let options = source_options(&added, formats, id);
    let mut tried = try_options(chip, &added, formats, streaming, &options, false);
    tried.sort_by_key(|a| (a.0, a.1));
    if let Some((_, _, fit, _)) = tried
        .into_iter()
        .find(|t| t.2.capacity != Capacity::CannotKeepUp)
    {
        let change = fit.change.map(|p| merge_patch(&add, &p));
        let (w, h, fps, _) = change
            .as_ref()
            .and_then(|c| patched(setup, c))
            .map_or((0, 0, 0, true), |s| iso_format(&s, formats, id));
        return Ok(IsoAdmission {
            source: id.into(),
            on: true,
            as_is: false,
            change,
            description: format!("{id}'s ISO file at {w}×{h}, {fps} fps"),
            reason: Some(why),
            width: Some(w),
            height: Some(h),
            fps: Some(fps),
        });
    }
    Ok(IsoAdmission {
        source: id.into(),
        on: false,
        as_is: false,
        change: None,
        description: format!("{id}'s ISO file stays off"),
        reason: Some(if options.is_empty() {
            format!("{why}, and a screen or window records at its own size")
        } else {
            format!("{why}, even smaller and slower")
        }),
        width: None,
        height: None,
        fps: None,
    })
}

/// Predict whether `inputs.setup`'s files fit, with the closest changes
/// that fit, and with `add_iso`, how to turn that ISO file on.
pub fn estimate(inputs: &Inputs, add_iso: Option<&str>) -> Result<CapacityEstimate, String> {
    let chip = chip(&inputs.machine.chip);
    let setup = inputs.setup;
    let mut judgement = judge(&chip, setup, inputs.formats, inputs.streaming);
    let mut measured = None;
    if let Some((memory, machine)) = inputs.memory {
        let workload = capacity::workload(setup, |id| {
            let (w, h, f, _) = iso_format(setup, inputs.formats, id);
            Some((w, h, f))
        });
        if let Some(judged) = workload.as_ref().and_then(|w| memory.judge(machine, w)) {
            let (m, exact) = match judged {
                Judged::Measured(m) => (m, true),
                Judged::Exceeds(m) => (m, false),
            };
            let when = m.measured_at.get(..10).unwrap_or(&m.measured_at);
            measured = Some(if exact {
                format!(
                    "`isocast tune` measured {:.2}× real time for these files on hard content on \
                     {when} ({:?})",
                    m.realtime_factor, m.capacity
                )
            } else {
                format!(
                    "`isocast tune` measured smaller files ({:?}) at only {:.2}× real time on \
                     {when}",
                    m.setup, m.realtime_factor
                )
            });
            if m.capacity == Capacity::CannotKeepUp {
                judgement.capacity = Capacity::CannotKeepUp;
            }
        }
    }
    let suggestions = if judgement.capacity == Capacity::Fits {
        Vec::new()
    } else {
        let options = options(setup, inputs.formats, inputs.streaming);
        pick(try_options(
            &chip,
            setup,
            inputs.formats,
            inputs.streaming,
            &options,
            inputs.recording,
        ))
    };
    let admission = match add_iso {
        Some(id) => Some(admission(
            &chip,
            setup,
            inputs.formats,
            inputs.streaming,
            id,
        )?),
        None => None,
    };
    Ok(CapacityEstimate {
        chip: chip.name.clone(),
        encode_engines: chip.engines,
        engine_pixels_per_second: chip.rate,
        basis: chip.basis.clone(),
        summary: summary(&judgement),
        workload_key: workload_key(&judgement.outputs),
        load: judgement.load,
        capacity: judgement.capacity,
        streaming: inputs.streaming,
        iso_drop_percent: judgement.iso_drop,
        outputs: judgement.outputs,
        suggestions,
        admission,
        measured,
    })
}

/// Each ISO source's format by probing it without opening it (a screen's
/// size, a camera's best format), but those `known` already; sources it
/// can't probe are left out.
pub fn probed_formats<P: crate::Platform>(
    platform: &P,
    setup: &Setup,
    known: &dyn Fn(&str) -> bool,
) -> std::collections::HashMap<String, (u32, u32, u32)> {
    let mut formats = std::collections::HashMap::new();
    let iso = &setup.recording.iso;
    for id in iso.sources.iter().filter(|id| iso.enabled && !known(id)) {
        let Some(source) = setup.source(id) else {
            continue;
        };
        if let Ok(Some(probe)) = platform.probe(&source.kind, &setup.canvas)
            && let Some(video) = probe.video
        {
            formats.insert(id.clone(), (video.width, video.height, video.fps));
        }
    }
    formats
}

/// The `doctor` and `setup check` line: whether `setup`'s files fit this
/// Mac's encoders (recording, and with its stream if it has one), with the
/// closest change that fits.
pub fn check<P: crate::Platform>(
    platform: &P,
    setup: &Setup,
    memory: Option<(&Memory, &str)>,
) -> Option<isocast_protocol::api::Check> {
    use isocast_protocol::api::{Check, CheckStatus};
    let has_video = setup.recording.program.enabled
        || (setup.recording.iso.enabled && !setup.recording.iso.sources.is_empty());
    if !has_video {
        return None;
    }
    let machine = platform.machine();
    let formats = probed_formats(platform, setup, &|_| false);
    let lookup = |id: &str| formats.get(id).copied();
    let run = |streaming| {
        estimate(
            &Inputs {
                machine: &machine,
                setup,
                formats: &lookup,
                streaming,
                memory,
                recording: false,
            },
            None,
        )
        .ok()
    };
    let recording = run(false)?;
    let streaming = setup.stream.as_ref().and_then(|_| run(true));
    let worst = match &streaming {
        Some(s) if s.load > recording.load => s,
        _ => &recording,
    };
    let status = match worst.capacity {
        Capacity::Fits => CheckStatus::Ok,
        _ => CheckStatus::Warning,
    };
    let mut detail = recording.summary.clone();
    if let Some(s) = streaming
        .as_ref()
        .filter(|s| s.capacity != recording.capacity)
    {
        detail.push_str(&format!(
            "; with the stream on, {}",
            s.summary.trim_start_matches("this Mac's encoders ")
        ));
    }
    detail.push_str(&format!(
        " [{}, {} engine(s); {}]",
        worst.chip,
        worst.encode_engines,
        worst.basis.split(':').next().unwrap_or("")
    ));
    let fix = worst.suggestions.first().map(|s| match &s.change {
        Some(change) => format!("{}: isocast plan --patch '{change}'", s.description),
        None => s.description.clone(),
    });
    Some(Check {
        name: "encoder capacity".into(),
        status,
        detail,
        fix,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(chip: &str) -> Machine {
        Machine {
            chip: chip.into(),
            memory_gb: 24.0,
            cores: 8,
            os: "macOS 26.7".into(),
            hardware_encoders: Vec::new(),
        }
    }

    /// The hardware soak's setup on a base M2: test patterns as the ISO
    /// sources, a 1080p60 program, and a stream.
    fn soak(isos: &[(&str, u32, u32)], stream: bool) -> Setup {
        let mut sources = Vec::new();
        for (id, w, h) in isos {
            sources.push(
                json!({ "id": id, "kind": "test_pattern", "width": w, "height": h, "fps": 60 }),
            );
        }
        let ids: Vec<&str> = isos.iter().map(|i| i.0).collect();
        let mut value = json!({
            "version": 1,
            "name": "soak",
            "canvas": { "width": 1920, "height": 1080, "fps": 60 },
            "sources": sources,
            "scenes": [{ "id": "main", "items": [{ "source": ids[0], "rect": { "x": 0, "y": 0, "width": 1920, "height": 1080 } }] }],
            "recording": {
                "folder": "/tmp",
                "program": { "encoder": { "codec": "hevc" } },
                "iso": { "enabled": true, "sources": ids, "encoder": { "codec": "hevc" } }
            }
        });
        if stream {
            value["stream"] = json!({ "service": "custom", "server": "rtmp://127.0.0.1/live" });
        }
        let setup = Setup::from_value(value).unwrap();
        assert_eq!(setup.validate(), Vec::new());
        setup
    }

    fn run(setup: &Setup, streaming: bool) -> CapacityEstimate {
        let m = machine("Apple M2");
        let none = |_: &str| None;
        estimate(
            &Inputs {
                machine: &m,
                setup,
                formats: &none,
                streaming,
                memory: None,
                recording: false,
            },
            None,
        )
        .unwrap()
    }

    #[test]
    fn table_parses_and_chips_resolve() {
        assert_eq!(chip("Apple M2").engines, 1);
        assert_eq!(chip("Apple M2 Max").engines, 2);
        assert_eq!(chip("Apple M3 Ultra").engines, 4);
        assert!(chip("Apple M9 Pro").basis.starts_with("assumed"));
        assert!(chip("Intel Core i9").basis.starts_with("assumed"));
    }

    /// The base M2's measured results (docs/notes.md, #66): fits,
    /// borderline and over.
    #[test]
    fn base_m2_matches_measurements() {
        let one = soak(&[("screen", 3840, 2160)], true);
        assert_eq!(run(&one, false).capacity, Capacity::Fits);
        let two = soak(&[("screen", 3840, 2160), ("facecam", 1920, 1080)], true);
        assert_eq!(run(&two, false).capacity, Capacity::NoMargin);
        let over = run(&two, true);
        assert_eq!(over.capacity, Capacity::CannotKeepUp);
        let first = &over.suggestions[0];
        assert_eq!(first.capacity, Capacity::Fits, "{:?}", over.suggestions);
        // Equal aggregate pixels do not imply equal native work: six HD
        // sessions exceed the M2 where a 4K plus two HD sessions still fit.
        let hd = [
            ("a", 1920, 1080),
            ("b", 1920, 1080),
            ("c", 1920, 1080),
            ("d", 1920, 1080),
            ("e", 1920, 1080),
            ("f", 1920, 1080),
            ("g", 1920, 1080),
            ("h", 1920, 1080),
        ];
        assert_ne!(
            run(&soak(&hd[..4], false), false).capacity,
            Capacity::CannotKeepUp
        );
        let five = run(&soak(&hd[..5], false), false);
        assert_eq!(five.capacity, Capacity::CannotKeepUp);
        assert!((45.0..60.0).contains(&five.iso_drop_percent));
        let eight = run(&soak(&hd, true), true);
        assert!((70.0..90.0).contains(&eight.iso_drop_percent));
    }

    /// H.264 ISO files cost about twice HEVC's on the M2 (tune), so a
    /// setup recording them that way is offered HEVC.
    #[test]
    fn h264_isos_are_offered_hevc() {
        let setup = soak(&[("screen", 3840, 2160), ("facecam", 1920, 1080)], false);
        let mut value = serde_json::to_value(&setup).unwrap();
        value["recording"]["iso"]["encoder"]["codec"] = json!("h264");
        let setup = Setup::from_value(value).unwrap();
        let estimate = run(&setup, false);
        assert_eq!(estimate.capacity, Capacity::CannotKeepUp);
        assert!(
            estimate
                .suggestions
                .iter()
                .any(|s| s.description.contains("record ISO files as HEVC")),
            "{:?}",
            estimate.suggestions
        );
    }

    #[test]
    fn admission_shrinks_or_refuses() {
        let setup = soak(&[("screen", 3840, 2160), ("facecam", 1920, 1080)], true);
        let mut value = serde_json::to_value(&setup).unwrap();
        value["sources"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "id": "cam2", "kind": "test_pattern", "width": 1920, "height": 1080, "fps": 60 }));
        let setup = Setup::from_value(value).unwrap();
        let m = machine("Apple M2");
        let none = |_: &str| None;
        let inputs = Inputs {
            machine: &m,
            setup: &setup,
            formats: &none,
            streaming: false,
            memory: None,
            recording: false,
        };
        let admitted = estimate(&inputs, Some("cam2")).unwrap().admission.unwrap();
        assert!(!admitted.on && !admitted.as_is, "{admitted:?}");
        assert!(estimate(&inputs, Some("nope")).is_err());
        let mut value = serde_json::to_value(&setup).unwrap();
        value["sources"][1]["width"] = json!(1280);
        value["sources"][1]["height"] = json!(720);
        let smaller = Setup::from_value(value).unwrap();
        let inputs = Inputs {
            setup: &smaller,
            ..inputs
        };
        let admitted = estimate(&inputs, Some("cam2")).unwrap().admission.unwrap();
        assert!(admitted.on && !admitted.as_is, "{admitted:?}");
    }
}
