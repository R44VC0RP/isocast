//! What `tune` measured on this Mac, kept between runs, so `start` can say
//! when a recording asks more of the hardware encoders than they managed
//! (OBS says "encoding overloaded" mid-take; this says it up front).

use std::path::{Path, PathBuf};

use isocast_protocol::api::{Capacity, Machine};
use isocast_protocol::platform_text::MACHINE;
use isocast_protocol::{Codec, Encoder, Setup};
use serde::{Deserialize, Serialize};

/// Measurements kept; the oldest go first.
const KEPT: usize = 64;

/// One file's video as the encoders see it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct VideoLoad {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub codec: Codec,
    pub bit_depth: u8,
    /// VideoToolbox quality, 1–100; 0 for ProRes, which ignores it.
    pub quality: u8,
}

impl VideoLoad {
    pub fn new(width: u32, height: u32, fps: u32, encoder: &Encoder) -> Self {
        let prores = !matches!(encoder.codec, Codec::H264 | Codec::Hevc);
        Self {
            width,
            height,
            fps,
            codec: encoder.codec,
            bit_depth: if prores {
                0
            } else {
                encoder.effective_bit_depth()
            },
            quality: if prores { 0 } else { encoder.quality.value() },
        }
    }

    fn pixel_rate(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height) * u64::from(self.fps)
    }

    /// Whether encoding `self` is at least as much work as `other`.
    fn covers(&self, other: &Self) -> bool {
        self.codec == other.codec
            && self.bit_depth >= other.bit_depth
            && self.quality >= other.quality
            && self.width * self.height >= other.width * other.height
            && self.fps >= other.fps
    }
}

/// Every file's video of a recording, sorted: two recordings with the same
/// workload cost the encoders the same.
pub type Workload = Vec<VideoLoad>;

/// The workload of recording `setup`, given each ISO source's size and
/// rate (`None` if one isn't known): its ISO files (each at its own
/// depth), its program and, with a vertical canvas that records, the
/// vertical program at that canvas's size and rate.
pub fn workload(
    setup: &Setup,
    iso_format: impl Fn(&str) -> Option<(u32, u32, u32)>,
) -> Option<Workload> {
    let iso = &setup.recording.iso;
    let mut loads = Vec::new();
    if iso.enabled {
        for id in &iso.sources {
            let (width, height, fps) = iso_format(id)?;
            let encoder = iso.encoder_for(setup.source(id)?);
            loads.push(VideoLoad::new(width, height, fps, &encoder));
        }
    }
    let program = &setup.recording.program;
    if program.enabled {
        let (width, height) = program.size(&setup.canvas);
        loads.push(VideoLoad::new(
            width,
            height,
            setup.canvas.fps,
            &program.encoder,
        ));
    }
    // Every other canvas that records (`[vertical]`, `[[canvases]]`), at
    // its own size and rate.
    for vertical in setup
        .extra_canvas_ids()
        .into_iter()
        .filter_map(|canvas| setup.for_canvas(canvas))
    {
        let program = &vertical.recording.program;
        if program.enabled {
            let (width, height) = program.size(&vertical.canvas);
            loads.push(VideoLoad::new(
                width,
                height,
                vertical.canvas.fps,
                &program.encoder,
            ));
        }
    }
    loads.sort();
    Some(loads)
}

/// Which Mac (and macOS) a measurement holds for.
pub fn machine_key(machine: &Machine) -> String {
    format!("{}, {} cores, {}", machine.chip, machine.cores, machine.os)
}

/// What tune recommended, for a start note to name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fix {
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Measurement {
    /// Local time, RFC 3339.
    pub measured_at: String,
    pub machine: String,
    /// The name of the setup tune was run on.
    pub setup: String,
    pub workload: Workload,
    pub realtime_factor: f64,
    pub capacity: Capacity,
    /// What that run of tune recommended, when this didn't fit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<Fix>,
}

/// The measurements file.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Memory {
    #[serde(default)]
    pub measurements: Vec<Measurement>,
}

/// Where tune's measurements are kept: beside the live setup's file
/// (`setup.toml` → `setup.tune.json`), so an isolated engine keeps its own.
pub fn memory_path(setup_file: &Path) -> PathBuf {
    setup_file.with_extension("tune.json")
}

/// The machine calibration beside a tune-memory file. Named recording
/// profiles share it; isolated setup files do not.
pub fn profile_path(memory: &Path) -> PathBuf {
    memory.with_extension("").with_extension("machine.json")
}

pub fn load_profile(
    path: &Path,
    machine: &Machine,
    app_build: &str,
) -> Option<isocast_protocol::machine::MachineProfile> {
    // A profile is tiny; refuse oversized/corrupt files instead of growing
    // the daemon or letting invalid timings affect admission.
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(256 * 1024 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > 256 * 1024 {
        return None;
    }
    let profile: isocast_protocol::machine::MachineProfile = serde_json::from_slice(&bytes).ok()?;
    (profile.app_build == app_build && profile.valid_for(machine)).then_some(profile)
}

impl Memory {
    /// The measurements in `path`; none if it is missing or unreadable.
    pub fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Write to `path` in one step (a crash leaves the old file or the new
    /// one, never half of one).
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let temporary = path.with_extension("tune.json.tmp");
        std::fs::write(&temporary, serde_json::to_vec_pretty(self)?)?;
        isocast_protocol::fs::replace(&temporary, path)?;
        Ok(())
    }

    /// Keep `measurement`, replacing an older one of the same workload on
    /// the same Mac.
    pub fn remember(&mut self, measurement: Measurement) {
        self.measurements
            .retain(|m| m.machine != measurement.machine || m.workload != measurement.workload);
        self.measurements.push(measurement);
        let extra = self.measurements.len().saturating_sub(KEPT);
        self.measurements.drain(..extra);
    }

    /// What tune last measured for `workload` on `machine`, or for a
    /// smaller workload that already couldn't keep up.
    pub fn judge(&self, machine: &str, workload: &Workload) -> Option<Judged<'_>> {
        let here = self.measurements.iter().filter(|m| m.machine == machine);
        if let Some(same) = here.clone().rev().find(|m| m.workload == *workload) {
            return Some(Judged::Measured(same));
        }
        // The slowest of them says the most.
        here.filter(|m| m.capacity == Capacity::CannotKeepUp)
            .filter(|m| covers(workload, &m.workload))
            .min_by(|a, b| a.realtime_factor.total_cmp(&b.realtime_factor))
            .map(Judged::Exceeds)
    }
}

pub enum Judged<'a> {
    /// Tune measured this workload.
    Measured(&'a Measurement),
    /// Tune measured a smaller workload that couldn't keep up.
    Exceeds(&'a Measurement),
}

/// Whether `bigger` holds, file for file, at least the work of each of
/// `smaller`'s files.
fn covers(bigger: &[VideoLoad], smaller: &[VideoLoad]) -> bool {
    let mut free: Vec<&VideoLoad> = bigger.iter().collect();
    // The largest first, each taking the smallest file that covers it.
    let mut wanted: Vec<&VideoLoad> = smaller.iter().collect();
    wanted.sort_by_key(|load| std::cmp::Reverse(load.pixel_rate()));
    for load in wanted {
        let Some(index) = free
            .iter()
            .enumerate()
            .filter(|(_, candidate)| candidate.covers(load))
            .min_by_key(|(_, candidate)| candidate.pixel_rate())
            .map(|(index, _)| index)
        else {
            return false;
        };
        free.swap_remove(index);
    }
    true
}

/// A warning for `start` if recording `workload` can't keep up on this
/// Mac by what tune measured: what it measured, and the fix it named.
pub fn start_warning(memory: &Path, machine: &str, workload: &Workload) -> Option<String> {
    let memory = Memory::load(memory);
    let (measured, exact) = match memory.judge(machine, workload)? {
        Judged::Measured(m) if m.capacity == Capacity::CannotKeepUp => (m, true),
        Judged::Measured(_) => return None,
        Judged::Exceeds(m) => (m, false),
    };
    let when = measured
        .measured_at
        .get(..16)
        .unwrap_or(&measured.measured_at)
        .replace('T', " ");
    let short = (1.0 - measured.realtime_factor).clamp(0.0, 1.0) * 100.0;
    let mut text = if exact {
        format!(
            "this {MACHINE}'s encoders can't keep up with this recording: `isocast tune` measured \
             {:.2}× real time for its files on {when}, so expect about {short:.0}% of frames \
             dropped",
            measured.realtime_factor
        )
    } else {
        format!(
            "this recording asks more of the encoders than `isocast tune` measured on {when} \
             for {:?}, whose files already ran at only {:.2}× real time, so expect dropped \
             frames",
            measured.setup, measured.realtime_factor
        )
    };
    match measured.fix.as_ref().filter(|_| exact) {
        Some(Fix {
            description,
            change: Some(change),
        }) => text.push_str(&format!(
            "; tune recommended {description}: isocast plan --patch '{change}'"
        )),
        Some(Fix {
            description,
            change: None,
        }) => text.push_str(&format!("; tune recommended {description}")),
        None => text.push_str("; run `isocast tune` for the closest change that fits"),
    }
    Some(text)
}
