//! `estimate_capacity`: predict, without benchmarking, whether a setup's
//! files fit this Mac's H.264/HEVC media engines, from per-chip data kept
//! as data (`media_engines.json`), and find the closest changes that fit.
//! `tune` measures; this is instant, so `setup check`, `doctor`, adding an
//! ISO file, first launch and the live overload notice can all ask (#66).

use std::sync::OnceLock;

use isocast_protocol::api::{
    Capacity, CapacityEstimate, CapacitySuggestion, EstimatedOutput, IsoAdmission, Machine,
    OutputKind, SOFTWARE_RENDERER_RESOURCE,
};
use isocast_protocol::change::merge_patch;
use isocast_protocol::platform_text::MACHINE;
use isocast_protocol::{CanvasId, Codec, PROGRAM, Setup, SourceKind};
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
    stream_h264_cost: Option<f64>,
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
    /// Bitrate-limited live H.264 has a different cost from a quality file.
    pub stream_h264_cost: f64,
    /// Live H.264's per-picture cost must not inherit a recording-only fit.
    pub stream_frame_overhead: f64,
    /// An H.264 recording file's cost per pixel and per picture: the
    /// table's, kept as that share of the engines beside a measured HEVC
    /// fit (no H.264 recording cell qualifies a fit of its own).
    pub h264_cost: f64,
    pub h264_frame_overhead: f64,
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
            stream_h264_cost: row.stream_h264_cost.unwrap_or(table.codec_cost.h264),
            stream_frame_overhead: row.frame_overhead_pixels,
            h264_cost: table.codec_cost.h264,
            h264_frame_overhead: row.frame_overhead_pixels,
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
            stream_h264_cost: table.codec_cost.h264,
            stream_frame_overhead: 0.0,
            h264_cost: table.codec_cost.h264,
            h264_frame_overhead: 0.0,
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
                stream_h264_cost: table.codec_cost.h264,
                stream_frame_overhead: 0.0,
                h264_cost: table.codec_cost.h264,
                h264_frame_overhead: 0.0,
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
            stream_h264_cost: table.codec_cost.h264,
            stream_frame_overhead: 0.0,
            h264_cost: table.codec_cost.h264,
            h264_frame_overhead: 0.0,
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
    resources: Vec<isocast_protocol::api::ResourceEstimate>,
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
    /// Streaming on [main, the other canvases (`[vertical]`, `[[canvases]]`)],
    /// independently for a live estimate.
    pub streaming: [bool; 2],
    /// `tune`'s measurements, and this Mac's key in them.
    pub memory: Option<(&'a Memory, &'a str)>,
    /// A recording runs: setup changes wait for the take to end.
    pub recording: bool,
    /// Validated local calibration; absent preserves the per-chip model.
    pub profile: Option<&'a isocast_protocol::machine::MachineProfile>,
}

/// The per-chip table's engines and, from a valid local profile, the
/// measured HEVC fit, also as a mixed model for HEVC beside H.264
/// recording files. Each judged workload (the setup, every suggestion, an
/// added ISO file) picks its own: a change can leave the qualified cells
/// (10-bit, ProRes, oversize) or depend on a size nobody knows (#110).
struct Model {
    table: Chip,
    fit: Option<Chip>,
    mixed: Option<Chip>,
    profile: Option<isocast_protocol::machine::MachineProfile>,
}

impl Model {
    fn new(inputs: &Inputs) -> Self {
        let table = chip(&inputs.machine.chip);
        let profile = inputs.profile.filter(|p| p.valid_for(inputs.machine));
        let fit = profile.and_then(|profile| fitted_chip(&table, profile));
        // The same engines and fit: H.264 recording files keep the table's
        // share of them (scaled with the fit, like live H.264), shared on
        // the table's H.264/HEVC engine topology. Not measured mixed.
        let mixed = fit.as_ref().zip(profile).map(|(fit, profile)| Chip {
            basis: format!(
                "mixed: measured local concurrent HEVC HD/UHD on {} for HEVC files; assumed \
                 table costs for H.264 recording files and live H.264, on the table's shared \
                 H.264/HEVC engines; engine count and overload recovery remain table assumptions",
                profile.measured_at
            ),
            ..fit.clone()
        });
        Model {
            table,
            fit,
            mixed,
            profile: profile.cloned(),
        }
    }

    /// The measured fit for an all-HEVC workload its cells cover, the
    /// mixed model for covered HEVC beside eligible H.264 recording files,
    /// else the table.
    fn chip(&self, setup: &Setup, formats: Formats) -> &Chip {
        if self.fit.is_none() {
            return &self.table;
        }
        match (coverage(setup, formats), &self.fit, &self.mixed) {
            (Coverage::Hevc, Some(fit), _) => fit,
            (Coverage::Mixed, _, Some(mixed)) => mixed,
            _ => &self.table,
        }
    }

    fn judge(&self, setup: &Setup, formats: Formats, streaming: [bool; 2]) -> Judgement {
        let chip = self.chip(setup, formats);
        let mut judged = judge(chip, setup, formats, streaming[0], streaming[1]);
        if judged
            .outputs
            .iter()
            .any(|o| !matches!(o.codec, Codec::Hevc | Codec::H264))
        {
            judged.resources = prores_resources(setup, &judged.outputs, self.profile.as_ref());
            for resource in &judged.resources {
                if let Some(load) = resource.load {
                    judged.load = judged.load.max(load);
                }
            }
            judged.capacity = verdict(judged.load);
            if judged.resources.iter().any(|r| {
                matches!(
                    r.resource.as_str(),
                    "prores" | "recording_disk" | "startup_recovery"
                ) && r.load.is_none()
            }) && judged.capacity == Capacity::Fits
            {
                judged.capacity = Capacity::NoMargin;
            }
            // A bandwidth/ProRes load is not the H.26x native ISO share model.
            // Do not manufacture a numeric ISO-loss forecast for these pools.
            judged.iso_drop = 0.0;
        }
        judged
    }
}

/// Which model a setup's recording files allow.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Coverage {
    /// Something the fit doesn't cover, or no HEVC file it measures.
    Table,
    /// Only HEVC files the fit's cells cover.
    Hevc,
    /// Covered HEVC files and H.264 files with the same limits.
    Mixed,
}

/// Which model covers every recording file of `setup`: HEVC (or, beside
/// at least one HEVC file, H.264) High/8-bit within 3840×2160 and 60 fps,
/// at a known size. A source at an assumed (canvas) size never certifies
/// a measured fit; live streams don't change the choice (their cost stays
/// the table's share either way). Each ISO file is at its own depth.
fn coverage(setup: &Setup, formats: Formats) -> Coverage {
    // These concurrent cells cover HEVC High/8-bit, not another depth or
    // encoder effort; H.264 at the same settings keeps its table cost.
    let eligible = |encoder: &isocast_protocol::Encoder| {
        matches!(encoder.codec, Codec::Hevc | Codec::H264)
            && encoder.effective_bit_depth() == 8
            && encoder.quality.value()
                == isocast_protocol::EncoderQuality::Preset(isocast_protocol::QualityPreset::High)
                    .value()
    };
    let format_covered = |w: u32, h: u32, fps: u32| {
        w.max(h) <= 3840 && u64::from(w) * u64::from(h) <= 3840 * 2160 && fps <= 60
    };
    // (codec, covered) for each recording file.
    let mut files: Vec<(Codec, bool)> = Vec::new();
    let mut program = |setup: &Setup| {
        let program = &setup.recording.program;
        if program.enabled {
            let (w, h) = program.size(&setup.canvas);
            files.push((
                program.encoder.codec,
                eligible(&program.encoder) && format_covered(w, h, setup.canvas.fps),
            ));
        }
    };
    program(setup);
    for canvas in setup.extra_canvas_ids() {
        if let Some(extra) = setup.for_canvas(canvas) {
            program(&extra);
        }
    }
    let iso = &setup.recording.iso;
    if iso.enabled {
        for id in &iso.sources {
            let (w, h, fps, assumed) = iso_format(setup, formats, id);
            let encoder = setup.source(id).map(|source| iso.encoder_for(source));
            files.push((
                iso.encoder.codec,
                encoder.as_ref().is_some_and(eligible) && !assumed && format_covered(w, h, fps),
            ));
        }
    }
    if files.iter().any(|(_, covered)| !covered) {
        return Coverage::Table;
    }
    let has = |codec: Codec| files.iter().any(|(c, _)| *c == codec);
    match (has(Codec::Hevc), has(Codec::H264)) {
        (true, false) => Coverage::Hevc,
        (true, true) => Coverage::Mixed,
        // Nothing measured contributes: exactly the table.
        _ => Coverage::Table,
    }
}

/// Fit pixel work plus per-picture overhead from concurrent HD and UHD
/// measurements. One session alone cannot determine aggregate capacity.
fn fitted_chip(table: &Chip, profile: &isocast_protocol::machine::MachineProfile) -> Option<Chip> {
    let mut chip = table.clone();
    let throughput = |width, fps, sessions| {
        profile
            .encoders
            .iter()
            .filter(|e| {
                e.codec == Codec::Hevc
                    && e.bit_depth == 8
                    && e.width == width
                    && e.fps == fps
                    && e.sessions == sessions
                    && e.certifies_fit()
                    && e.backend == "videotoolbox"
            })
            .map(|e| e.frames_per_second)
            .min_by(f64::total_cmp)
    };
    // At least two sessions per assumed engine. Engine topology is still
    // explicitly from the table until the platform exposes resource pools.
    let sessions = (chip.engines * 2).max(4);
    let (hd, uhd) = throughput(1920, 60, sessions).zip(throughput(3840, 30, sessions))?;
    let hd_two = throughput(1920, 60, 2)?;
    if hd < hd_two {
        return None;
    }
    let hd_pixels = 1920.0 * 1080.0;
    let uhd_pixels = 3840.0 * 2160.0;
    let per_pixel = (uhd.recip() - hd.recip()) / (uhd_pixels - hd_pixels);
    let per_picture = hd.recip() - per_pixel * hd_pixels;
    if per_pixel <= 0.0 || per_picture < 0.0 {
        return None;
    }
    let rate = 1.0 / (per_pixel * f64::from(chip.engines.max(1)));
    let overhead = per_picture / per_pixel;
    if !rate.is_finite() || !overhead.is_finite() || !(1e6..=1e11).contains(&rate) {
        return None;
    }
    // A recording-codec test is not a live H.264 test. Keep the latter's
    // existing normalized cost rather than pretending it was measured.
    // H.264 recording files likewise keep the table's share.
    chip.stream_h264_cost *= rate / chip.rate;
    chip.stream_frame_overhead *= rate / chip.rate;
    chip.h264_cost *= rate / chip.rate;
    chip.h264_frame_overhead *= rate / chip.rate;
    chip.rate = rate;
    chip.frame_overhead = overhead;
    chip.basis = format!(
        "measured: local concurrent HEVC HD/UHD on {}; engine count, live H.264 and overload recovery remain table assumptions",
        profile.measured_at
    );
    Some(chip)
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

/// Whether the setup has a stream section on a canvas other than the main
/// one.
fn has_vertical_stream(setup: &Setup) -> bool {
    setup
        .extra_canvas_ids()
        .into_iter()
        .any(|canvas| setup.stream_of(canvas).is_some())
}

/// Whether streaming streams the main canvas: its `[stream]`, or (with no
/// stream section anywhere, as before the vertical canvas) the stream it
/// would get. A setup that only streams its vertical canvas has none.
fn streams_main(setup: &Setup) -> bool {
    match &setup.stream {
        Some(stream) => {
            stream.enabled
                || stream
                    .destinations
                    .iter()
                    .any(|destination| destination.enabled)
        }
        None => !has_vertical_stream(setup),
    }
}

/// The vertical canvas's encodes (its program file and its stream's
/// distinct formats), from the setup it sees. Its ISO sources are the
/// main canvas's files, so none are counted here.
fn vertical_outputs(
    setup: &Setup,
    streaming: bool,
    push: &mut impl FnMut(String, OutputKind, (u32, u32, u32), Codec, bool),
) {
    // Every canvas besides the main one (#84), in order: each its own
    // program file and stream encodes.
    for canvas in setup.extra_canvas_ids() {
        canvas_outputs(setup, canvas, streaming, push);
    }
}

/// `vertical_outputs` for one canvas: outputs named `<id>-program`,
/// `<id>-stream` and `<id>-stream:<destination>`.
fn canvas_outputs(
    setup: &Setup,
    canvas: CanvasId,
    streaming: bool,
    push: &mut impl FnMut(String, OutputKind, (u32, u32, u32), Codec, bool),
) {
    let Some(vertical) = setup.for_canvas(canvas) else {
        return;
    };
    let fps = vertical.canvas.fps;
    let program = &vertical.recording.program;
    if program.enabled {
        let (w, h) = program.size(&vertical.canvas);
        push(
            canvas.program(),
            OutputKind::Program,
            (w, h, fps),
            program.encoder.codec,
            false,
        );
    }
    if streaming && let Some(stream) = &vertical.stream {
        let size = (vertical.canvas.width, vertical.canvas.height);
        for (i, encode) in stream.encodes(size).iter().enumerate() {
            let name = if i == 0 {
                format!("{canvas}-stream")
            } else {
                format!("{canvas}-stream:{}", encode.ids[0])
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

/// `main_stream` and `vertical_stream` say which canvases stream (each
/// only if it has something to stream).
fn judge(
    chip: &Chip,
    setup: &Setup,
    formats: Formats,
    main_stream: bool,
    vertical_stream: bool,
) -> Judgement {
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
                let (cost, overhead) = match (kind, codec) {
                    (OutputKind::Stream, _) => (chip.stream_h264_cost, chip.stream_frame_overhead),
                    (_, Codec::H264) => (chip.h264_cost, chip.h264_frame_overhead),
                    _ => (cost, chip.frame_overhead),
                };
                pixels * cost + f64::from(fps) * overhead
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
    if main_stream && streams_main(setup) {
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
    vertical_outputs(setup, vertical_stream, &mut push);
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
        resources: Vec::new(),
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

/// ProRes does not consume H.26x pixels, but it is not free. Only full,
/// concurrent cells with observed native selection supply its native budget.
/// CPU/GPU/copy observations are explicitly lower-bound work, not a claim
/// that filters, conversion services or another process have spare capacity.
fn prores_resources(
    setup: &Setup,
    outputs: &[EstimatedOutput],
    profile: Option<&isocast_protocol::machine::MachineProfile>,
) -> Vec<isocast_protocol::api::ResourceEstimate> {
    use isocast_protocol::api::ResourceEstimate;
    use isocast_protocol::machine::{EncoderResource, Evidence, FIT_MIN_SECONDS};
    let prores: Vec<_> = outputs
        .iter()
        .filter(|o| !matches!(o.codec, Codec::Hevc | Codec::H264))
        .collect();
    let hd = 1920.0 * 1080.0;
    let mut native = 0.0;
    let mut process_cpu = 0.0;
    let mut system_cpu = 0.0;
    let mut covered = true;
    let mut cpu_known = true;
    let mut system_cpu_known = true;
    let mut hardware = 0;
    let mut software = 0;
    for output in &prores {
        let cell = profile.and_then(|p| {
            p.encoders
                .iter()
                .filter(|m| {
                    m.codec == output.codec
                        && m.backend == "videotoolbox"
                        && m.resource == EncoderResource::RecordingWriter
                        && m.evidence == Evidence::ProresResource
                        && m.sessions >= 2
                        && m.seconds >= FIT_MIN_SECONDS
                        && m.hardware_accelerated.is_some()
                        && m.width == 1920
                        && m.height == 1080
                })
                .min_by(|a, b| a.frames_per_second.total_cmp(&b.frames_per_second))
        });
        let pixels = f64::from(output.width) * f64::from(output.height);
        let eligible = !output.assumed
            && output.fps <= 60
            && pixels <= 3840.0 * 2160.0
            && output.width.max(output.height) <= 3840;
        if let Some(cell) = cell.filter(|_| eligible) {
            // Scaling up by pixels retains HD's per-picture cost too, a
            // deliberately conservative bound rather than a fitted zero-cost
            // overhead. A full UHD422 control must exist before scaling up.
            let uhd = profile.and_then(|p| {
                p.encoders
                    .iter()
                    .filter(|m| {
                        m.codec == Codec::Prores422
                            && m.evidence == Evidence::ProresResource
                            && m.backend == cell.backend
                            && m.width == 3840
                            && m.height == 2160
                            && m.hardware_accelerated == cell.hardware_accelerated
                    })
                    .min_by(|a, b| a.frames_per_second.total_cmp(&b.frames_per_second))
            });
            let scale = (pixels / hd).max(1.0);
            let mut demand = f64::from(output.fps) * scale / cell.frames_per_second;
            if pixels > hd {
                let reference = profile.and_then(|p| {
                    p.encoders
                        .iter()
                        .filter(|m| {
                            m.codec == Codec::Prores422
                                && m.evidence == Evidence::ProresResource
                                && m.backend == cell.backend
                                && m.width == 1920
                                && m.height == 1080
                                && m.hardware_accelerated == cell.hardware_accelerated
                        })
                        .min_by(|a, b| a.frames_per_second.total_cmp(&b.frames_per_second))
                });
                if let Some((uhd, reference)) = uhd.zip(reference) {
                    // A slow UHD control must veto an optimistic HD scaling.
                    // Other variants use the measured HD variant/reference
                    // ratio; their UHD scaling is still explicitly assumed.
                    demand = demand.max(
                        f64::from(output.fps) * (pixels / (hd * 4.0)) / uhd.frames_per_second
                            * reference.frames_per_second
                            / cell.frames_per_second,
                    );
                } else {
                    covered = false;
                }
            }
            native += demand;
            if cell.hardware_accelerated == Some(true) {
                hardware += 1;
            } else {
                software += 1;
            }
            if let Some(cpu) = cell.process_cpu_seconds_per_frame {
                process_cpu += f64::from(output.fps) * scale * cpu;
            } else {
                cpu_known = false;
            }
            if let Some(cpu) = cell.system_cpu_seconds_per_frame {
                system_cpu += f64::from(output.fps) * scale * cpu;
            } else {
                system_cpu_known = false;
            }
        } else {
            covered = false;
            cpu_known = false;
            system_cpu_known = false;
        }
    }
    let raw_bytes = prores
        .iter()
        .map(|o| f64::from(o.width) * f64::from(o.height) * f64::from(o.fps) * 4.0)
        .sum::<f64>();
    let mut disk_bytes = 0.0;
    for output in outputs.iter().filter(|o| o.kind != OutputKind::Stream) {
        let encoder = if output.kind == OutputKind::Iso {
            // The data rate estimate doesn't depend on the depth.
            setup.recording.iso.encoder.at_depth(8)
        } else if output.output == PROGRAM {
            setup.recording.program.encoder.clone()
        } else {
            output
                .output
                .strip_suffix("-program")
                .and_then(|id| CanvasId::parse(id).ok())
                .and_then(|id| setup.for_canvas(id))
                .map(|s| s.recording.program.encoder)
                .unwrap_or_else(|| isocast_protocol::Encoder {
                    codec: output.codec,
                    ..Default::default()
                })
        };
        disk_bytes += isocast_protocol::encoder_bytes_per_second(
            &encoder,
            output.width,
            output.height,
            output.fps,
            2,
        );
    }
    // Use the recording path on Windows too: its home normally comes from
    // USERPROFILE, not HOME. Otherwise a valid volume calibration is ignored.
    #[cfg(windows)]
    let folder = Some(crate::record::expand_home(&setup.recording.folder));
    #[cfg(not(windows))]
    let folder = if let Some(rest) = setup.recording.folder.strip_prefix("~/") {
        std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(rest))
    } else {
        Some(std::path::PathBuf::from(&setup.recording.folder))
    };
    let disk_capacity = profile
        .and_then(|p| p.disk_device.zip(p.disk_write_bytes_per_second))
        .filter(|(device, _)| {
            folder.as_ref().and_then(|f| crate::space::device(f)) == Some(*device)
        })
        // The verdict keeps its normal 15% margin. This is still a burst,
        // not a sustained guarantee, and never borrowed from another volume.
        .map(|(_, rate)| rate);
    let gpu_seconds = profile.and_then(|p| {
        std::iter::once(setup.clone())
            .chain(
                setup
                    .extra_canvas_ids()
                    .into_iter()
                    .filter_map(|c| setup.for_canvas(c)),
            )
            .filter(|s| s.recording.program.enabled || s.stream.is_some())
            .map(|s| {
                p.graphics
                    .iter()
                    .filter(|g| g.width * g.height >= s.canvas.width * s.canvas.height)
                    .min_by_key(|g| g.width * g.height)
                    .map(|g| g.composite_ms / 1000.0 * f64::from(s.canvas.fps))
            })
            .sum::<Option<f64>>()
    });
    let cpu_capacity = profile
        .filter(|p| p.machine.cores > 0 && system_cpu_known && covered)
        .map(|p| f64::from(p.machine.cores));
    let mut resources = vec![
        ResourceEstimate {
            resource: "prores".into(), unit: "native resource seconds/second".into(),
            demand: native, capacity: covered.then_some(1.0), load: covered.then_some(native),
            basis: format!("{}; {hardware} hardware / {software} software outputs identified from full concurrent native-session measurements. Additive shared-resource model; larger pictures take the worse HD-pixel scaling or measured UHD422 bound. Other variants extrapolate UHD using their HD cost ratio (assumed). Unmeasured work is not free",
                if covered { "measured local ProRes service demand" } else { "unqualified or incomplete ProRes coverage" }),
        },
        ResourceEstimate {
            resource: "system_cpu".into(), unit: "CPU core seconds/second".into(),
            demand: system_cpu, capacity: cpu_capacity, load: cpu_capacity.map(|v| system_cpu / v),
            basis: format!("{} whole-system busy CPU during concurrent native cells, including native services/startup/drain/outside activity; conservative additive scaling, not isolated encoder CPU. Scene/decode work is additional; live CPU pressure is authoritative", if system_cpu_known && covered { "measured" } else { "partial/unknown" }),
        },
        ResourceEstimate {
            resource: "process_cpu".into(), unit: "CPU core seconds/second".into(),
            demand: process_cpu, capacity: None, load: None,
            basis: format!("{}; includes cell startup/drain, excludes native-service CPU and other processes. Not total CPU headroom; live OS/encoder pressure remains authoritative",
                if cpu_known { "measured process-CPU observation" } else { "partial/unknown process-CPU observation" }),
        },
        ResourceEstimate {
            resource: "graphics".into(), unit: "GPU seconds/second".into(),
            demand: gpu_seconds.unwrap_or(0.0), capacity: None, load: None,
            basis: format!("{} simple compositor workload only; scene filters, native ProRes conversion and concurrent GPU work are additional/unqualified", if gpu_seconds.is_some() { "measured" } else { "unmeasured/partial" }),
        },
        ResourceEstimate {
            resource: "frame_bandwidth".into(), unit: "bytes/second".into(),
            demand: raw_bytes, capacity: None, load: None,
            basis: "one BGRA-equivalent input read per ProRes output; conversion/copy passes are additional, not a certified memory-bandwidth budget".into(),
        },
        ResourceEstimate {
            resource: "recording_disk".into(), unit: "bytes/second".into(),
            demand: disk_bytes, capacity: disk_capacity, load: disk_capacity.map(|v| disk_bytes / v),
            basis: if disk_capacity.is_some() {
                "published codec target-rate planning (all recording files); calibrated same-volume 64 MiB burst rate, with the normal 15% fit margin, not a sustained guarantee".into()
            } else { "published codec target-rate planning (all recording files); destination throughput unmeasured or on another volume, run tune there".into() },
        },
    ];
    if software > 0
        && outputs.iter().any(|o| o.kind == OutputKind::Iso)
        && outputs.iter().any(|o| o.protected)
    {
        // AWS software4444: one startup stall set native ISO admission to75%,
        // recovering slowly despite a fitting steady service budget. Never
        // promise a zero-loss start from an unpaced throughput calibration.
        resources.push(ResourceEstimate {
            resource: "startup_recovery".into(), unit: "qualification".into(),
            demand: 0.0, capacity: None, load: None,
            basis: "software ProRes startup/recovery with protected outputs can temporarily shed ISO pictures even below the steady resource budget; zero-loss starts are not qualified by this calibration. Native safety remains authoritative".into(),
        });
    }
    resources
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
    if !judgement.resources.is_empty() {
        let unknown = judgement.resources.iter().any(|r| {
            matches!(
                r.resource.as_str(),
                "prores" | "recording_disk" | "startup_recovery"
            ) && r.load.is_none()
        });
        return format!(
            "ProRes resource estimate: {}; largest modelled load {}; CPU/native-service, graphics and destination-disk observations are listed separately. No numeric ISO-loss forecast; content, filters and outside load require a real tune",
            if unknown {
                "native ProRes, destination or startup/recovery capacity is not fully qualified; do not assume this fits"
            } else {
                match judgement.capacity {
                    Capacity::Fits => "modelled encoder/bandwidth work fits with margin",
                    Capacity::NoMargin => "modelled work has no safe margin",
                    Capacity::CannotKeepUp => "modelled work exceeds a resource budget",
                }
            },
            percent(judgement.load)
        );
    }
    let busiest = percent(judgement.load);
    // Outputs of canvases besides the main one: a protected output that
    // isn't the main program's or stream's.
    let vertical = judgement.outputs.iter().any(|o| {
        o.protected
            && o.output != PROGRAM
            && o.output != "stream"
            && !o.output.starts_with("stream:")
    });
    match judgement.capacity {
        Capacity::Fits => format!(
            "this {MACHINE}'s encoders keep up with these files (the busiest engine at about {busiest})"
        ),
        Capacity::NoMargin => format!(
            "this {MACHINE}'s encoders are predicted to keep up with these files with no margin \
             (the busiest engine at about {busiest}): anything else using them, like a stream \
             or a video call, may cost frames"
        ),
        Capacity::CannotKeepUp => {
            let iso = judgement.outputs.iter().any(|o| !o.protected);
            let priority = if vertical {
                "other canvases' outputs shed next to protect main, which can still drop if its own workload is too high"
            } else {
                "those can also drop if their own workload is too high"
            };
            if iso {
                format!(
                    "this {MACHINE}'s encoders can't keep up with these files (the busiest engine at \
                     about {busiest}): ISO files are shed first (estimated {:.0}% frame loss) \
                     to prioritize program recordings and streams; {priority}",
                    judgement.iso_drop.max(1.0)
                )
            } else {
                let priority = if vertical {
                    "other canvases' outputs shed before main; main can still drop if its own workload is too high"
                } else {
                    "the program and stream will drop frames"
                };
                format!(
                    "this {MACHINE}'s encoders can't keep up with these files (the busiest engine at \
                     about {busiest}): {priority}"
                )
            }
        }
    }
}

/// Automatic quality's ISO frame thinning is the user's choice, never part
/// of a fit: estimates and their suggestions don't count it, and nothing
/// turns it on for them. Native overload shedding is separate.
pub const ISO_THINNING_NOTE: &str = "Allowing ISO frame thinning (Settings › Advanced › \
     Automatic Quality, `governor.allow_iso_thinning`) is your choice, off by default: it lets \
     automatic quality skip ISO pictures under pressure, lost from those files for good, and \
     isn't counted as making these files fit";

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
fn options(setup: &Setup, formats: Formats, streaming: [bool; 2]) -> Vec<Option_> {
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
    if streaming[0] && streams_main(setup) {
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
    for canvas in setup.extra_canvas_ids() {
        out.extend(vertical_options(setup, canvas, streaming[1]));
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

/// A patch of extra canvas `canvas`'s fields: `{"vertical": change}`, or
/// `{"canvases": [{"id": …, …change}]}` (merged by id).
fn canvas_patch(setup: &Setup, canvas: CanvasId, change: Value) -> Option<Value> {
    if canvas.is_vertical() {
        return Some(json!({ "vertical": change }));
    }
    setup.extra_canvas(canvas)?;
    // `[[canvases]]` merges item by item, by id (`change::merge_patch`).
    let mut entry = change;
    entry
        .as_object_mut()?
        .insert("id".into(), canvas.as_str().into());
    Some(json!({ "canvases": [entry] }))
}

/// Changes to an extra canvas's own encodes, as patches of its fields
/// (`vertical`, or its `canvases` entry): its program's codec and size, its
/// stream's size and its frame rate.
fn vertical_options(setup: &Setup, canvas: CanvasId, streaming: bool) -> Vec<Option_> {
    let mut out = Vec::new();
    let Some(vertical) = setup.for_canvas(canvas) else {
        return out;
    };
    let words = match canvas {
        CanvasId::Vertical => "the vertical".to_string(),
        other => format!("canvas {other:?}'s"),
    };
    let canvas_words = match canvas {
        CanvasId::Vertical => "the vertical canvas".to_string(),
        other => format!("canvas {other:?}"),
    };
    let program = &vertical.recording.program;
    if program.enabled {
        let table = table();
        let to = match program.encoder.codec {
            Codec::H264 if table.codec_cost.hevc < table.codec_cost.h264 => Some(("hevc", "HEVC")),
            Codec::Hevc if table.codec_cost.h264 < table.codec_cost.hevc => Some(("h264", "H.264")),
            _ => None,
        };
        if let Some((to, name)) = to {
            out.push(Option_ {
                description: format!("record {words} program as {name}"),
                patch: canvas_patch(
                    setup,
                    canvas,
                    json!({ "recording": { "encoder": { "codec": to } } }),
                ),
                method: None,
                cost: COST_ISO_CODEC,
                target: format!("{canvas}-program-codec"),
                off: false,
            });
        }
        let (cw, ch) = (vertical.canvas.width, vertical.canvas.height);
        let (w, h) = program.size(&vertical.canvas);
        let short = cw.min(ch);
        for (step, side) in SIDES.iter().filter(|s| **s < w.min(h)).take(2).enumerate() {
            let (nw, nh) = (scaled(cw, *side, short), scaled(ch, *side, short));
            out.push(Option_ {
                description: format!("record {words} program at {nw}×{nh}"),
                patch: canvas_patch(
                    setup,
                    canvas,
                    json!({ "recording": { "width": nw, "height": nh } }),
                ),
                method: None,
                cost: COST_PROGRAM_SIZE * (step as u32 + 1),
                target: canvas.program(),
                off: false,
            });
        }
    }
    if streaming && vertical.stream.is_some() {
        let (w, h) = stream_size(&vertical);
        let short = w.min(h);
        if short > 720 {
            let (nw, nh) = (scaled(w, 720, short), scaled(h, 720, short));
            out.push(Option_ {
                description: format!("stream {canvas_words} at {nw}×{nh}"),
                patch: canvas_patch(
                    setup,
                    canvas,
                    json!({ "stream": { "width": nw, "height": nh } }),
                ),
                method: None,
                cost: COST_STREAM_SIZE,
                target: format!("{canvas}-stream"),
                off: false,
            });
        }
    }
    // Only a rate of its own: one it inherits halves with the main canvas's.
    let own = setup.extra_canvas(canvas).and_then(|v| v.fps);
    if let Some(fps) = own
        && fps >= 48
        && fps.is_multiple_of(2)
    {
        out.push(Option_ {
            description: format!("record and stream {canvas_words} at {} fps", fps / 2),
            patch: canvas_patch(setup, canvas, json!({ "fps": fps / 2 })),
            method: None,
            cost: COST_CANVAS_RATE,
            target: format!("{canvas}-canvas"),
            off: false,
        });
    }
    // A canvas-array patch that couldn't be made is no option.
    out.retain(|option| option.patch.is_some());
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
    model: &Model,
    setup: &Setup,
    formats: Formats,
    streaming: [bool; 2],
    options: &[Option_],
    recording: bool,
) -> Vec<Tried> {
    let mut combos: Vec<Vec<&Option_>> = options.iter().map(|o| vec![o]).collect();
    for (i, a) in options.iter().enumerate() {
        for b in &options[i + 1..] {
            // A method (stop the stream) stands alone: a client applies a
            // patch or calls a method, not both.
            // Two `canvases` patches merge by id, but stay single changes
            // so each suggestion names one canvas.
            let both_canvases = |o: &Option_| {
                o.patch
                    .as_ref()
                    .is_some_and(|p| p.get("canvases").is_some())
            };
            let clash = a.method.is_some()
                || b.method.is_some()
                || both_canvases(a) && both_canvases(b)
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
        // `stream_stop` stops the main canvas's streams only.
        let streams = streaming[0] && method != Some("stream_stop");
        let judgement = model.judge(&changed, formats, [streams, streaming[1]]);
        if judgement.resources.is_empty()
            && judgement.outputs.iter().all(|o| o.pixels_per_second == 0.0)
            && !judgement.outputs.is_empty()
        {
            continue;
        }
        let touches = |key: &str| patch.as_ref().is_some_and(|p| p.get(key).is_some());
        // A canvas's rate holds while it streams.
        let vertical_rate = patch.as_ref().is_some_and(|p| {
            p.get("vertical").is_some_and(|v| v.get("fps").is_some())
                || p.get("canvases")
                    .is_some_and(|c| serde_json::to_value(&setup.canvases).ok().as_ref() != Some(c))
        });
        let after_take = recording && patch.is_some()
            || (touches("canvas") && streaming[0] || vertical_rate && streaming[1])
                && method.is_none();
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
    model: &Model,
    setup: &Setup,
    formats: Formats,
    streaming: [bool; 2],
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
    let full = model.judge(&added, formats, streaming);
    if full.capacity != Capacity::CannotKeepUp
        && !full.resources.iter().any(|r| {
            matches!(
                r.resource.as_str(),
                "prores" | "recording_disk" | "startup_recovery"
            ) && r.load.is_none()
        })
    {
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
    let why = if !full.resources.is_empty() {
        summary(&full)
    } else {
        format!(
            "at full size, this {MACHINE}'s encoders can't keep up with it beside the other files (the \
         busiest engine at about {})",
            percent(full.load)
        )
    };
    // Only this source changes: the rest of the setup is the user's.
    let options = source_options(&added, formats, id);
    let mut tried = try_options(model, &added, formats, streaming, &options, false);
    tried.sort_by_key(|a| (a.0, a.1));
    if let Some((_, _, fit, _)) = tried.into_iter().find(|t| {
        t.2.capacity == Capacity::Fits
            || full.resources.is_empty() && t.2.capacity != Capacity::CannotKeepUp
    }) {
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
    let model = Model::new(inputs);
    let setup = inputs.setup;
    let chip = model.chip(setup, inputs.formats);
    let mut judgement = model.judge(setup, inputs.formats, inputs.streaming);
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
            &model,
            setup,
            inputs.formats,
            inputs.streaming,
            &options,
            inputs.recording,
        ))
    };
    let admission = match add_iso {
        Some(id) => Some(admission(
            &model,
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
        streaming: inputs.streaming.iter().any(|on| *on),
        iso_drop_percent: judgement.iso_drop,
        outputs: judgement.outputs,
        resources: judgement.resources,
        suggestions,
        admission,
        measured,
    })
}

/// Conservative zero-loss reference across hosted Windows samples (#128).
/// The highest single passing profile proved unstable across hosts/takes;
/// this is a qualified workload reference, not this machine's capacity.
/// Twenty seconds each headless and with two paused previews: H.264 program
/// only, two same-sized test-pattern sources and a tone, no ISO or stream.
/// The same controls also held 36 fps: tested frame-rate reserve, not capacity.
const SOFTWARE_REFERENCE: (u32, u32, u32) = (320, 180, 30);
const SOFTWARE_REFERENCE_HOST: &str = "four-vCPU hosted AMD EPYC and Intel Xeon runners, Windows Server 24H2, WARP and Media Foundation software H.264";

/// Whether this process's graphics adapter is a software one, opening (or
/// reusing) the shared GPU on `platform` as the live engine does, so it is
/// known before an estimate or a take even when no preview has run. A failed
/// device open is not evidence of a hardware adapter.
pub fn software_adapter<P: crate::Platform>(platform: &P) -> Result<bool, String> {
    crate::gpu::Gpu::for_platform(platform)
        .map(|_| crate::governor::is_software_renderer())
        .map_err(|error| format!("could not identify the graphics adapter: {error:#}"))
}

/// The estimate on a software graphics adapter (#128): compositing, preview
/// and frame conversion run on the CPU, so the hardware encoder tables say
/// nothing. Judged only against the assumed advisory reference: within it
/// is no margin (never fits), above it frames are expected to drop, and
/// streams, ISO files and other canvases are not qualified at all.
pub fn software(inputs: &Inputs, add_iso: Option<&str>) -> Result<CapacityEstimate, String> {
    let setup = inputs.setup;
    let (ref_w, ref_h, ref_fps) = SOFTWARE_REFERENCE;
    let reference = format!("{ref_w}×{ref_h} at {ref_fps} fps");
    let within = |w: u32, h: u32, fps: u32| {
        w.max(h) <= ref_w.max(ref_h) && w.min(h) <= ref_w.min(ref_h) && fps <= ref_fps
    };
    // The outputs (which encodes run, at what size), not engine costs.
    let mut outputs = judge(
        &chip(""),
        setup,
        inputs.formats,
        inputs.streaming[0],
        inputs.streaming[1],
    )
    .outputs;
    for output in &mut outputs {
        output.pixels_per_second =
            f64::from(output.width) * f64::from(output.height) * f64::from(output.fps);
    }
    let canvas = &setup.canvas;
    let demand = f64::from(canvas.width) * f64::from(canvas.height) * f64::from(canvas.fps);
    let above = !within(canvas.width, canvas.height, canvas.fps);
    let extra: Vec<&str> = outputs
        .iter()
        .filter(|o| o.kind != OutputKind::Program || o.output != PROGRAM)
        .map(|o| o.output.as_str())
        .collect();
    let larger: Vec<String> = setup
        .sources
        .iter()
        .filter(|s| s.kind.has_video())
        .filter_map(|s| {
            let (w, h, fps, assumed) = iso_format(setup, inputs.formats, &s.id);
            (!assumed && !within(w, h, fps)).then(|| format!("{} ({w}×{h}, {fps} fps)", s.id))
        })
        .collect();
    let basis = format!(
        "assumed: software graphics adapter; advisory program-only reference {reference}, \
         tested on {SOFTWARE_REFERENCE_HOST} with two same-sized test-pattern sources and a tone, \
         one H.264 program recording, no ISO or stream; zero drops for 20 seconds headless \
         and 20 seconds with two previews paused; the same controls also held 36 fps. \
         Local capacity is unmeasured; streams, \
         ISO files, other codecs and other canvases are not qualified"
    );
    let mut summary = if above {
        format!(
            "software graphics adapter ({}): the {}×{} at {} fps canvas is above the assumed \
             advisory reference of {reference} for a program recording only, so expect dropped \
             frames; lower the canvas and its sources to {reference} or less (a smaller output \
             encode alone doesn't reduce the compositing work)",
            inputs.machine.chip, canvas.width, canvas.height, canvas.fps
        )
    } else {
        format!(
            "software graphics adapter ({}): compositing and preview run on the CPU; the {}×{} \
             at {} fps canvas is within the assumed advisory reference of {reference} for a \
             program recording only, which is not a capacity guarantee: other load on this \
             machine can still cost frames",
            inputs.machine.chip, canvas.width, canvas.height, canvas.fps
        )
    };
    summary.push_str(&format!(
        "; reference tested on {SOFTWARE_REFERENCE_HOST} with two same-sized test-pattern \
         sources, a tone and one H.264 program recording; local capacity is unmeasured"
    ));
    if !extra.is_empty() {
        summary.push_str(&format!(
            "; additional outputs ({}) are not qualified on a software adapter and may drop frames",
            extra.join(", ")
        ));
    }
    if !larger.is_empty() {
        summary.push_str(&format!(
            "; sources above the reference ({}) are not qualified: lower them too",
            larger.join(", ")
        ));
    }
    // Every way the setup leaves the reference's sizes has advice, so no
    // suggestion and no extra output means within it
    // ([`software_within_reference`]).
    let lower = if above {
        Some(format!(
            "lower the canvas and its sources to {reference} or less, recording the program only"
        ))
    } else if !larger.is_empty() {
        Some(format!(
            "lower the sources above the reference ({}) to {reference} or less",
            larger.join(", ")
        ))
    } else {
        None
    };
    let suggestions: Vec<_> = lower
        .map(|description| CapacitySuggestion {
            description,
            change: None,
            method: None,
            load: 0.0,
            capacity: Capacity::NoMargin,
            after_take: inputs.recording,
        })
        .into_iter()
        .collect();
    let admission = match add_iso {
        Some(id) => {
            let source = setup
                .source(id)
                .filter(|s| s.kind.has_video())
                .ok_or_else(|| {
                    let ids: Vec<&str> = setup
                        .sources
                        .iter()
                        .filter(|s| s.kind.has_video())
                        .map(|s| s.id.as_str())
                        .collect();
                    format!(
                        "add_iso: no video source {id:?}; video sources: {}",
                        ids.join(", ")
                    )
                })?;
            let iso = &setup.recording.iso;
            let on = iso.enabled && iso.sources.contains(&source.id);
            Some(IsoAdmission {
                source: id.into(),
                on,
                as_is: on,
                change: None,
                description: if on {
                    format!("{id}'s ISO file is already on")
                } else {
                    format!("{id}'s ISO file stays off")
                },
                reason: (!on).then(|| {
                    format!(
                        "ISO files are not qualified on a software graphics adapter; the \
                         {reference} reference covers only a single H.264 program recording, \
                         and local capacity remains unmeasured"
                    )
                }),
                width: None,
                height: None,
                fps: None,
            })
        }
        None => None,
    };
    Ok(CapacityEstimate {
        chip: inputs.machine.chip.clone(),
        encode_engines: 0,
        engine_pixels_per_second: 0.0,
        basis: basis.clone(),
        summary,
        workload_key: workload_key(&outputs),
        // No qualified resource load; the resource explicitly reports unknown.
        load: 0.0,
        // This machine is unmeasured. Exceeding a reference is a warning,
        // not proof of its local maximum throughput.
        capacity: Capacity::NoMargin,
        streaming: inputs.streaming.iter().any(|on| *on),
        iso_drop_percent: 0.0,
        outputs,
        resources: vec![isocast_protocol::api::ResourceEstimate {
            resource: SOFTWARE_RENDERER_RESOURCE.into(),
            unit: "pixels_per_second".into(),
            demand,
            capacity: None,
            load: None,
            basis,
        }],
        suggestions,
        admission,
        measured: None,
    })
}

/// Whether `estimate` is a software graphics adapter's ([`software`]) for
/// a setup within the advisory reference: the main program recording only,
/// canvas and sources no larger than the reference. Still no margin and
/// local capacity unknown; above it or unqualified outputs is `false`.
pub fn software_within_reference(estimate: &CapacityEstimate) -> bool {
    let [output] = estimate.outputs.as_slice() else {
        return false;
    };
    let (width, height, fps) = SOFTWARE_REFERENCE;
    estimate
        .resources
        .iter()
        .any(|r| r.resource == SOFTWARE_RENDERER_RESOURCE)
        && estimate.suggestions.is_empty()
        && output.kind == OutputKind::Program
        && output.output == PROGRAM
        && output.codec == Codec::H264
        && output.width.max(output.height) <= width
        && output.width.min(output.height) <= height
        && output.fps <= fps
}

/// Each ISO source's format, and `add_iso`'s (the ISO file asked to be
/// turned on), by probing it without opening it (a screen's size, a
/// camera's best format), but those `known` already; sources it can't
/// probe are left out.
pub fn probed_formats<P: crate::Platform>(
    platform: &P,
    setup: &Setup,
    add_iso: Option<&str>,
    known: &dyn Fn(&str) -> bool,
) -> std::collections::HashMap<String, (u32, u32, u32)> {
    let mut formats = std::collections::HashMap::new();
    let iso = &setup.recording.iso;
    let ids = iso
        .sources
        .iter()
        .filter(|_| iso.enabled)
        .map(String::as_str)
        .chain(add_iso);
    for id in ids.filter(|id| !known(id)) {
        let Some(source) = setup.source(id).filter(|_| !formats.contains_key(id)) else {
            continue;
        };
        if let Ok(Some(probe)) = platform.probe(id, &source.kind, &setup.canvas)
            && let Some(video) = probe.video
        {
            formats.insert(id.to_string(), (video.width, video.height, video.fps));
        }
    }
    formats
}

/// The `doctor` and `setup check` line: whether `setup`'s files fit this
/// Mac's encoders (recording, and with its stream if it has one), with the
/// closest change that fits.
pub fn check(estimate: &CapacityEstimate) -> Option<isocast_protocol::api::Check> {
    use isocast_protocol::api::{Check, CheckStatus};
    if estimate.outputs.is_empty() {
        return None;
    }
    let status = match estimate.capacity {
        Capacity::Fits => CheckStatus::Ok,
        _ => CheckStatus::Warning,
    };
    let mut detail = estimate.summary.clone();
    if estimate.streaming {
        detail.push_str(" (including configured streams)");
    }
    let software = estimate
        .resources
        .iter()
        .any(|r| r.resource == SOFTWARE_RENDERER_RESOURCE);
    if software {
        // No engine count to show: the CPU composites and converts.
        detail.push_str(&format!(
            " [{}, software graphics adapter; assumed advisory reference, not measured]",
            estimate.chip
        ));
    } else {
        detail.push_str(&format!(
            " [{}, {} engine(s); {}]",
            estimate.chip,
            estimate.encode_engines,
            estimate.basis.split(':').next().unwrap_or("")
        ));
    }
    if !software
        && estimate.capacity != Capacity::Fits
        && estimate.outputs.iter().any(|o| o.kind == OutputKind::Iso)
    {
        detail.push_str(&format!(". {ISO_THINNING_NOTE}"));
    }
    let fix = estimate.suggestions.first().map(|s| match &s.change {
        Some(change) => format!("{}: isocast plan --patch '{change}'", s.description),
        None => s.description.clone(),
    });
    Some(Check {
        name: if software {
            "software graphics capacity"
        } else {
            "encoder capacity"
        }
        .into(),
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
                streaming: [streaming; 2],
                memory: None,
                recording: false,
                profile: None,
            },
            None,
        )
        .unwrap()
    }

    fn run_software(setup: &Setup, add_iso: Option<&str>) -> CapacityEstimate {
        let m = machine("Microsoft Basic Render Driver");
        let none = |_: &str| None;
        software(
            &Inputs {
                machine: &m,
                setup,
                formats: &none,
                streaming: [false; 2],
                memory: None,
                recording: false,
                profile: None,
            },
            add_iso,
        )
        .unwrap()
    }

    /// A program-only setup at `w`×`h`, `fps`, its one source as large.
    fn program_only(w: u32, h: u32, fps: u32) -> Setup {
        let setup = Setup::from_value(json!({
            "version": 1,
            "name": "software",
            "canvas": { "width": w, "height": h, "fps": fps },
            "sources": [{ "id": "bars", "kind": "test_pattern", "width": w, "height": h, "fps": fps }],
            "scenes": [{ "id": "main", "items": [{ "source": "bars", "rect": { "x": 0, "y": 0, "width": w, "height": h } }] }],
            "recording": { "folder": "/tmp", "program": { "encoder": { "codec": "h264" } } }
        }))
        .unwrap();
        assert_eq!(setup.validate(), Vec::new());
        setup
    }

    #[test]
    fn software_adapter_never_fits_and_warns_above_the_reference() {
        // Within the assumed reference: no margin, never "fits", and the
        // capacity stays unknown rather than a number.
        let small = run_software(&program_only(320, 180, 30), None);
        assert_eq!(small.capacity, Capacity::NoMargin);
        assert!(small.basis.starts_with("assumed:"), "{}", small.basis);
        assert!(
            small.summary.contains("not a capacity guarantee"),
            "{}",
            small.summary
        );
        assert!(small.suggestions.is_empty());
        assert!(software_within_reference(&small));
        assert!(small.basis.contains("320×180 at 30 fps"));
        for (width, height) in [(426, 240), (480, 270)] {
            let edge = run_software(&program_only(width, height, 30), None);
            assert!(edge.summary.contains("expect dropped frames"));
            assert!(!software_within_reference(&edge));
        }
        let mut other = small.clone();
        other.outputs[0].codec = Codec::Hevc;
        assert!(!software_within_reference(&other));
        other.outputs[0].codec = Codec::H264;
        other.outputs[0].width = 1920;
        assert!(!software_within_reference(&other));
        other.outputs.clear();
        assert!(!software_within_reference(&other));
        let resource = &small.resources[0];
        assert_eq!((resource.capacity, resource.load), (None, None));
        let check = check(&small).unwrap();
        assert_eq!(check.status, isocast_protocol::api::CheckStatus::Warning);
        assert!(
            check.detail.contains("software graphics adapter"),
            "{}",
            check.detail
        );
        assert!(!check.detail.contains("engine(s)"), "{}", check.detail);

        // Above it: frames expected to drop, naming the canvas and the
        // advice to lower the canvas and sources, not just the encode.
        let large = run_software(&program_only(1920, 1080, 60), None);
        assert_eq!(large.capacity, Capacity::NoMargin);
        assert!(
            large.summary.contains("1920×1080 at 60 fps"),
            "{}",
            large.summary
        );
        assert!(
            large.summary.contains("expect dropped frames"),
            "{}",
            large.summary
        );
        assert!(
            large.summary.contains("bars (1920×1080, 60 fps)"),
            "{}",
            large.summary
        );
        assert!(
            large.suggestions[0]
                .description
                .contains("canvas and its sources")
        );
        assert!(!software_within_reference(&large));

        // A canvas within it composing a larger source is not qualified.
        let mut value = serde_json::to_value(program_only(320, 180, 30)).unwrap();
        value["sources"][0]["width"] = json!(1920);
        value["sources"][0]["height"] = json!(1080);
        let wide = run_software(&Setup::from_value(value).unwrap(), None);
        assert!(
            wide.suggestions[0]
                .description
                .contains("bars (1920×1080, 30 fps)"),
            "{:?}",
            wide.suggestions
        );
        assert!(!software_within_reference(&wide));

        // A higher rate alone is above the reference too.
        assert_eq!(
            run_software(&program_only(320, 180, 60), None).capacity,
            Capacity::NoMargin
        );
    }

    #[test]
    fn software_adapter_does_not_qualify_iso_files_or_streams() {
        let setup = soak(&[("a", 1280, 720)], true);
        let estimate = run_software(&setup, None);
        assert!(
            estimate
                .summary
                .contains("additional outputs (a) are not qualified"),
            "{}",
            estimate.summary
        );
        assert!(!software_within_reference(&estimate));
        let program = program_only(1280, 720, 30);
        let admitted = run_software(&program, Some("bars")).admission.unwrap();
        assert!(!admitted.on);
        assert!(admitted.reason.unwrap().contains("not qualified"));
        let m = machine("Microsoft Basic Render Driver");
        let none = |_: &str| None;
        let inputs = Inputs {
            machine: &m,
            setup: &program,
            formats: &none,
            streaming: [false; 2],
            memory: None,
            recording: false,
            profile: None,
        };
        assert!(
            software(&inputs, Some("nope"))
                .unwrap_err()
                .contains("bars")
        );
    }

    #[test]
    fn only_ten_bit_iso_files_leave_the_measured_hevc_fit() {
        let mut value = serde_json::to_value(soak(&[("cam", 1920, 1080)], false)).unwrap();
        value["sources"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "id": "screen", "kind": "display", "display": "main" }));
        value["recording"]["iso"]["sources"] = json!(["cam", "screen"]);
        let formats = |id: &str| (id == "screen").then_some((1920, 1080, 60));
        let coverage_of = |value: &serde_json::Value| {
            coverage(&Setup::from_value(value.clone()).unwrap(), &formats)
        };
        // The fit measures 8-bit HEVC: screens left to the default (8)
        // are covered; a screen's own 10-bit file isn't, nor a set 10.
        assert_eq!(coverage_of(&value), Coverage::Hevc);
        let screen = value["sources"].as_array().unwrap().len() - 1;
        value["sources"][screen]["encoder"] = json!({ "bit_depth": 10 });
        assert_eq!(coverage_of(&value), Coverage::Table);
        // Its own 8 under a set 10 is, but the camera's 10 isn't.
        value["sources"][screen]["encoder"] = json!({ "bit_depth": 8 });
        value["recording"]["iso"]["encoder"]["bit_depth"] = json!(10);
        assert_eq!(coverage_of(&value), Coverage::Table);
        value["recording"]["iso"]["encoder"]["bit_depth"] = json!(8);
        assert_eq!(coverage_of(&value), Coverage::Hevc);
    }

    #[test]
    fn table_parses_and_chips_resolve() {
        assert_eq!(chip("Apple M2").engines, 1);
        assert_eq!(chip("Apple M2 Max").engines, 2);
        assert_eq!(chip("Apple M3 Ultra").engines, 4);
        assert!(chip("Apple M9 Pro").basis.starts_with("assumed"));
        assert!(chip("Intel Core i9").basis.starts_with("assumed"));
    }

    #[test]
    fn prores_is_a_separate_finite_resource_not_free_h26x_work() {
        use isocast_protocol::machine::{MEASUREMENT_VERSION, MachineProfile, PROFILE_SCHEMA};
        let m = machine("Apple M2");
        let mut setup = soak(&[("screen", 1920, 1080)], false);
        setup.recording.iso.encoder.codec = Codec::Prores422;
        setup.recording.program.encoder.codec = Codec::Prores422;
        let none = |_: &str| None;
        let profile: MachineProfile = serde_json::from_value(json!({
            "schema_version": PROFILE_SCHEMA, "measurement_version": MEASUREMENT_VERSION,
            "app_version": env!("CARGO_PKG_VERSION"), "app_build":"test", "measured_at":"test",
            "machine":m,"seconds":3.0,"graphics":[],"skipped":[],"complete":true,"notes":[],
            "disk_device":crate::space::device(std::path::Path::new("/tmp")),
            "disk_write_bytes_per_second":5e9,
            "encoders":[{"codec":"prores422","bit_depth":8,"width":1920,"height":1080,
                "fps":60,"sessions":2,"frames":300,"seconds":1.0,"frames_per_second":300.0,
                "resource":"recording_writer","backend":"videotoolbox","evidence":"prores_resource",
                "rate_control":"constant_quality","quality":70,"hardware_accelerated":true,
                "process_cpu_seconds_per_frame":0.0001}]
        }))
        .unwrap();
        let inputs = Inputs {
            machine: &m,
            setup: &setup,
            formats: &none,
            streaming: [false; 2],
            memory: None,
            recording: false,
            profile: Some(&profile),
        };
        let estimate = super::estimate(&inputs, None).unwrap();
        assert_eq!(estimate.capacity, Capacity::Fits);
        assert_eq!(estimate.resources[0].load, Some(0.4));
        assert!(estimate.outputs.iter().all(|o| o.pixels_per_second == 0.0));
        let mut software = profile.clone();
        software.encoders[0].hardware_accelerated = Some(false);
        let software_estimate = super::estimate(
            &Inputs {
                profile: Some(&software),
                ..inputs
            },
            None,
        )
        .unwrap();
        assert_eq!(software_estimate.capacity, Capacity::NoMargin);
        assert!(
            software_estimate
                .resources
                .iter()
                .any(|r| r.resource == "startup_recovery")
        );
        assert_eq!(run(&setup, false).capacity, Capacity::NoMargin);
        let mut not_recording = setup.clone();
        not_recording.recording.iso.enabled = false;
        not_recording.recording.iso.sources.clear();
        let unqualified = super::estimate(
            &Inputs {
                setup: &not_recording,
                profile: None,
                ..inputs
            },
            Some("screen"),
        )
        .unwrap();
        assert!(!unqualified.admission.unwrap().on);
        let mut other_disk = profile.clone();
        other_disk.disk_device = None;
        assert_eq!(
            super::estimate(
                &Inputs {
                    profile: Some(&other_disk),
                    ..inputs
                },
                None
            )
            .unwrap()
            .capacity,
            Capacity::NoMargin
        );
        let other = super::estimate(
            &Inputs {
                setup: &not_recording,
                profile: Some(&other_disk),
                ..inputs
            },
            Some("screen"),
        )
        .unwrap();
        assert!(!other.admission.unwrap().on);
        let mut slow_uhd = profile.clone();
        slow_uhd.encoders[0].frames = 600;
        slow_uhd.encoders[0].frames_per_second = 600.0;
        let mut uhd = slow_uhd.encoders[0].clone();
        uhd.width = 3840;
        uhd.height = 2160;
        uhd.frames = 40;
        uhd.frames_per_second = 40.0;
        slow_uhd.encoders.push(uhd);
        let mut big = not_recording.clone();
        big.canvas.width = 3840;
        big.canvas.height = 2160;
        let result = super::estimate(
            &Inputs {
                setup: &big,
                profile: Some(&slow_uhd),
                ..inputs
            },
            None,
        )
        .unwrap();
        assert_eq!(result.resources[0].load, Some(1.5));
        assert_eq!(result.capacity, Capacity::CannotKeepUp);
    }

    /// The base M2's measured results (docs/archive/2026-10-22/notes.md, #66): fits,
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
            streaming: [false; 2],
            memory: None,
            recording: false,
            profile: None,
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

    #[test]
    fn calibration_covers_canvas_files_without_changing_assumed_stream_cost() {
        use isocast_protocol::machine::{MEASUREMENT_VERSION, MachineProfile, PROFILE_SCHEMA};
        let m = machine("Apple M2");
        let cells = [
            (1920, 1080, 60, 2, 200),
            (1920, 1080, 60, 4, 240),
            (3840, 2160, 30, 4, 100),
        ]
        .into_iter()
        .map(|(width, height, fps, sessions, frames)| {
            json!({ "resource": "recording_writer", "backend": "videotoolbox",
                    "evidence": "fit", "rate_control": "constant_quality", "quality": 70,
                    "codec": "hevc", "bit_depth": 8, "width": width, "height": height,
                    "fps": fps, "sessions": sessions, "frames": frames, "seconds": 1.0,
                    "frames_per_second": frames })
        })
        .collect::<Vec<_>>();
        let profile: MachineProfile = serde_json::from_value(json!({
            "schema_version": PROFILE_SCHEMA, "measurement_version": MEASUREMENT_VERSION,
            "app_version": env!("CARGO_PKG_VERSION"), "app_build": "test", "measured_at": "test",
            "machine": m, "seconds": 3.0, "encoders": cells, "graphics": [], "notes": [],
            "skipped": [], "complete": true
        }))
        .unwrap();
        let setup = soak(&[("screen", 1920, 1080)], true);
        let mut value = serde_json::to_value(&setup).unwrap();
        value["canvases"] = json!([{ "id": "shorts", "width": 1080, "height": 1920,
            "fps": 30, "recording": { "encoder": { "codec": "hevc" } } }]);
        let setup = Setup::from_value(value.clone()).unwrap();
        let none = |_: &str| None;
        let inputs = Inputs {
            machine: &m,
            setup: &setup,
            formats: &none,
            streaming: [true; 2],
            memory: None,
            recording: false,
            profile: Some(&profile),
        };
        let measured = estimate(&inputs, None).unwrap();
        let fallback = estimate(
            &Inputs {
                profile: None,
                ..inputs
            },
            None,
        )
        .unwrap();
        assert!(measured.basis.starts_with("measured:"));
        let shorts = measured
            .outputs
            .iter()
            .find(|o| o.output == "shorts-program")
            .unwrap();
        assert_eq!((shorts.width, shorts.height, shorts.fps), (1080, 1920, 30));
        let stream_cost = |e: &CapacityEstimate| {
            e.outputs
                .iter()
                .find(|o| o.output == "stream")
                .unwrap()
                .pixels_per_second
                / e.engine_pixels_per_second
        };
        assert!((stream_cost(&measured) - stream_cost(&fallback)).abs() < 1e-10);
        // H.264 beside covered HEVC is the labelled mixed model; an
        // uncovered file on any canvas retains the table, even though the
        // main canvas itself is covered.
        for (encoder, prefix) in [
            (json!({ "codec": "h264" }), "mixed:"),
            (
                json!({ "codec": "hevc", "bit_depth": 10 }),
                fallback.basis.as_str(),
            ),
            (
                json!({ "codec": "h264", "bit_depth": 10 }),
                fallback.basis.as_str(),
            ),
        ] {
            value["canvases"][0]["recording"]["encoder"] = encoder;
            let setup = Setup::from_value(value.clone()).unwrap();
            let basis = estimate(
                &Inputs {
                    setup: &setup,
                    ..inputs
                },
                None,
            )
            .unwrap()
            .basis;
            assert!(basis.starts_with(prefix), "{basis}");
        }
        // Each judged workload picks its own model: a covered setup's
        // H.264, 10-bit or oversized candidate, or an ISO file at a size
        // nobody knows (a screen not yet probed), changes it.
        let model = Model::new(&inputs);
        let basis = |setup: &Setup, formats: Formats| model.chip(setup, formats).basis.clone();
        let with = |base: &Setup, edit: &dyn Fn(&mut Value)| {
            let mut value = serde_json::to_value(base).unwrap();
            edit(&mut value);
            Setup::from_value(value).unwrap()
        };
        assert!(basis(&setup, &none).starts_with("measured:"));
        let h264 = with(&setup, &|v: &mut Value| {
            v["recording"]["iso"]["encoder"]["codec"] = json!("h264");
        });
        assert!(basis(&h264, &none).starts_with("mixed:"));
        let deep = with(&setup, &|v: &mut Value| {
            v["recording"]["iso"]["encoder"]["bit_depth"] = json!(10);
        });
        assert_eq!(basis(&deep, &none), fallback.basis);
        let add_desk = |v: &mut Value| {
            v["sources"]
                .as_array_mut()
                .unwrap()
                .push(json!({ "id": "desk", "kind": "display", "display": "main" }));
            v["recording"]["iso"]["sources"] = json!(["screen", "desk"]);
        };
        let screen = with(&setup, &add_desk);
        let h264_screen = with(&h264, &add_desk);
        let probed = |id: &str| (id == "desk").then_some((1920u32, 1080u32, 60u32));
        let oversize = |id: &str| (id == "desk").then_some((5120u32, 2880u32, 60u32));
        assert_eq!(basis(&screen, &none), fallback.basis);
        assert_eq!(basis(&h264_screen, &none), fallback.basis);
        assert!(basis(&screen, &probed).starts_with("measured:"));
        assert!(basis(&h264_screen, &probed).starts_with("mixed:"));
        assert_eq!(basis(&screen, &oversize), fallback.basis);
        assert_eq!(basis(&h264_screen, &oversize), fallback.basis);

        // Mixed: every H.264 output (recording file and live) asks exactly
        // the table's share of the engines; the HEVC files take the fit's.
        let share = |e: &CapacityEstimate| {
            e.outputs
                .iter()
                .map(|o| {
                    (
                        o.output.clone(),
                        (o.codec, o.pixels_per_second / e.engine_pixels_per_second),
                    )
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let mixed = estimate(
            &Inputs {
                setup: &h264,
                ..inputs
            },
            None,
        )
        .unwrap();
        let table = estimate(
            &Inputs {
                setup: &h264,
                profile: None,
                ..inputs
            },
            None,
        )
        .unwrap();
        assert!(mixed.basis.starts_with("mixed:"), "{}", mixed.basis);
        let (mixed_share, table_share) = (share(&mixed), share(&table));
        assert_eq!(
            mixed_share.keys().collect::<Vec<_>>(),
            table_share.keys().collect::<Vec<_>>()
        );
        assert!(mixed_share.contains_key("screen") && mixed_share.contains_key("stream"));
        for (output, (codec, load)) in &mixed_share {
            let table_load = table_share[output].1;
            if *codec == Codec::H264 {
                assert!((load - table_load).abs() < 1e-12, "{output}");
            } else {
                assert!((load - table_load).abs() > 1e-3, "{output}");
            }
        }

        // H.264 alone has nothing measured: exactly the table.
        let only = with(&h264, &|v: &mut Value| {
            v["recording"]["program"]["encoder"]["codec"] = json!("h264");
            v["canvases"][0]["recording"]["encoder"]["codec"] = json!("h264");
        });
        let measured_only = estimate(
            &Inputs {
                setup: &only,
                ..inputs
            },
            None,
        )
        .unwrap();
        let table_only = estimate(
            &Inputs {
                setup: &only,
                profile: None,
                ..inputs
            },
            None,
        )
        .unwrap();
        assert_eq!(measured_only.basis, table_only.basis);
        assert_eq!(measured_only.load, table_only.load);
        assert_eq!(share(&measured_only), share(&table_only));
    }
}
