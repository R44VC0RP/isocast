//! The silent machine measurement (T.1, #110; docs/self-tuning.md): what
//! this machine's hardware encoders, GPU, memory and scratch disk do with
//! synthetic pictures, as evidence for capacity estimates.
//!
//! It runs only inside the daemon's disposable helper, which the daemon
//! kills after 30 seconds and never runs beside an output, so this module
//! only keeps a cooperative deadline: it stops starting new work before
//! [`DEADLINE`] and says which cells it left unmeasured. It opens no device,
//! window or network connection, asks for no permission, and writes only
//! small files inside `scratch`, each removed when its cell ends.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Barrier;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use isocast_protocol::machine::{
    EncoderMeasurement, EncoderResource, Evidence, FIT_MIN_SECONDS, GraphicsMeasurement,
    MEASUREMENT_VERSION, MachineProfile, PROFILE_SCHEMA, RateControlMode, SkippedMeasurement,
};
use isocast_protocol::{Codec, Container, Crop, Encoder, EncoderQuality, QualityPreset};

use crate::gpu::{FrameImporter, Gpu, GpuFrame};
use crate::render::{Layer, Rect, Renderer};
use crate::tune::Texture;
use crate::ycbcr::ToYCbCr;
use crate::{
    FileWriter, FramePool, LiveVideoSpec, Platform, Time, VideoEncoder, VideoTrackSpec, WriterSpec,
    space,
};

/// When the measurement stops starting new work, from its start. The
/// daemon's own 30-second limit (startup and teardown included) is the
/// hard stop.
const DEADLINE: Duration = Duration::from_secs(25);
/// Slack kept below the deadline when deciding whether a cell still fits.
const SLACK: Duration = Duration::from_millis(500);
/// Further time diagnostic cells leave unused, so a slow last cell, frame
/// release and the profile's write stay well inside the helper's watchdog.
const DIAGNOSTIC_RESERVE: Duration = Duration::from_millis(1000);

/// Each encoder session encodes this long before its timing starts, so
/// session start-up and the first pictures' latency aren't counted.
const WARM_UP: Duration = Duration::from_millis(250);
/// How long completed encoder pictures are counted. Finalization is
/// required afterwards, but is not steady-state encoder work.
const WINDOW: Duration = Duration::from_millis(600);
/// What opening a session and finishing its file add to a cell's time, as
/// planned for the deadline.
const OPEN_AND_FINISH: Duration = Duration::from_millis(600);
/// The gap the macOS writer leaves between hardware sessions' first
/// pictures (#41), as planned for the deadline.
const SESSION_GAP: Duration = Duration::from_millis(25);
/// The most frames one encoder session is given.
const MAX_FRAMES: u64 = 1000;
/// The most bytes one session's scratch file may grow to, by the protocol's
/// generous estimate of the encoder's data rate (it bounds ProRes at 4K to
/// a few dozen frames).
const MAX_FILE_BYTES: f64 = 96.0 * 1e6;
/// A cell timed over fewer completed frames than this isn't reported.
const MIN_FRAMES: u64 = 8;
/// How long a session may refuse the same frame before the cell fails.
const ACCEPT_STALL: Duration = Duration::from_secs(2);
/// The live H.264 cell's target: the stream default (6000 kb/s constant
/// bitrate, a keyframe every 2 s).
const LIVE_BITRATE_KBPS: u32 = 6000;
const LIVE_KEYFRAME_SECONDS: u32 = 2;
/// Longest kept failure reason, so a native error can't bloat the profile.
const MAX_REASON: usize = 300;
/// Free space the scratch volume must keep beyond what the measurement
/// writes.
const FREE_RESERVE: u64 = 2_000_000_000;

/// How far the synthetic frames' windows into the texture shift.
const TEXTURE_MARGIN: usize = 64;
/// Distinct synthetic frames per size, shared by every session and the GPU
/// measurement at that size: 8 HD frames are 66 MB, 6 UHD frames 199 MB.
const CYCLE_HD: usize = 8;
const CYCLE_UHD: usize = 6;

/// Frames drawn and converted before the GPU's timing starts (shader and
/// pipeline start-up).
const GPU_WARM_UP: u32 = 3;
/// The most timed GPU frames per size.
const GPU_MAX_FRAMES: u32 = 30;
/// How long the GPU is timed per size.
const GPU_TIME: Duration = Duration::from_millis(1000);
/// How long one GPU submission may take before the measurement fails.
const GPU_WAIT: Duration = Duration::from_secs(2);

/// The memory copy's buffer size (each of source and destination), at most
/// 1/64 of physical memory.
const MEMORY_COPY_BYTES: usize = 64 << 20;
const MEMORY_TIME: Duration = Duration::from_millis(250);
const MEMORY_MAX_PASSES: usize = 32;

/// The scratch disk write: this many bytes, written and synced.
const DISK_BYTES: usize = 64 << 20;
const DISK_CHUNK: usize = 4 << 20;

const HD: (u32, u32) = (1920, 1080);
const UHD: (u32, u32) = (3840, 2160);

/// Measure this machine with synthetic pictures, writing only inside
/// `scratch` (created if missing; what this writes is removed). Takes about
/// [`DEADLINE`]; cells that didn't fit, failed or aren't supported are left
/// out with a note each, never reported as zero.
pub fn measure<P: Platform>(platform: &P, scratch: &Path) -> Result<MachineProfile> {
    let started = Instant::now();
    let deadline = started + DEADLINE;
    let measured_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    fs::create_dir_all(scratch)
        .with_context(|| format!("could not create {}", scratch.display()))?;
    let mut notes = Vec::new();
    notes.push(match platform.calibration_priority() {
        Some(priority) => format!(
            "measured at low priority ({priority}) so interactive work comes first; a busy \
             machine reads lower"
        ),
        None => "this platform could not lower the measurement's priority".into(),
    });
    let machine = platform.machine();

    let memory_copy_bytes_per_second = memory_copy(machine.memory_gb * f64::from(1u32 << 30));
    if memory_copy_bytes_per_second.is_none() {
        notes.push("memory copy not measured: no time was measured".into());
    }
    let disk_write_bytes_per_second = match disk_write(platform, scratch) {
        Ok((rate, note)) => {
            notes.push(note);
            Some(rate)
        }
        Err(err) => {
            notes.push(format!("disk write not measured: {err:#}"));
            None
        }
    };

    let cells = cells();
    // The GPU comes first but never takes the required encoder cells' time.
    let required_time = cells
        .iter()
        .filter(|cell| cell.required())
        .map(Cell::planned)
        .sum::<Duration>()
        + SLACK;
    let mut cycles: Cycles<P::Frame> = Vec::new();
    let graphics = measure_graphics(platform, &mut cycles, deadline, required_time, &mut notes);

    notes.push(format!(
        "encoders ({backend}): synthetic camera-like BGRA frames; completed-picture counter \
         deltas over a steady interval after a 0.25 s warm-up. Recording cells use the file \
         writer at the high preset's constant quality; files are finished and checked \
         separately, so queued submissions and finalization latency do not count. The live \
         cell uses the stream's real-time H.264 encoder at {LIVE_BITRATE_KBPS} kb/s constant \
         bitrate, counting pictures it returned. Required cells (the capacity fit's concurrent \
         HEVC cells, which need full {FIT_MIN_SECONDS} s windows, then 4K60 HEVC 10-bit and \
         live H.264) run before diagnostic cells; only fit cells may back capacity",
        backend = backend()
    ));
    let mut encoders = Vec::new();
    let mut skipped = Vec::new();
    // Paths, codecs and depths whose single session couldn't start.
    let mut unusable = Vec::new();
    for (index, cell) in cells.iter().enumerate() {
        let mut skip = |reason: String| {
            skipped.push(SkippedMeasurement {
                label: cell.label(),
                required: cell.required(),
                reason: truncate(reason),
            })
        };
        if unusable.contains(&cell.family()) {
            skip("a session of the same path, codec and depth could not start".into());
            continue;
        }
        let reserve = if cell.required() {
            SLACK
        } else {
            SLACK + DIAGNOSTIC_RESERVE
        };
        if Instant::now() + cell.planned() + reserve > deadline {
            skip("out of time: the 30-second calibration limit".into());
            continue;
        }
        let frames = match frames_for(platform, &mut cycles, (cell.width, cell.height)) {
            Ok(frames) => frames,
            Err(err) => {
                skip(format!("no synthetic frames: {err}"));
                continue;
            }
        };
        let result = match cell.resource {
            EncoderResource::RecordingWriter => encode_cell(platform, cell, frames, scratch, index),
            EncoderResource::LiveEncoder => live_cell(platform, cell, frames),
        };
        match result {
            Ok(measurement) => encoders.push(measurement),
            Err(err) => {
                if cell.sessions == 1 && err.downcast_ref::<NotStarted>().is_some() {
                    unusable.push(cell.family());
                }
                skip(format!("failed: {err:#}"));
            }
        }
    }
    drop(cycles);

    Ok(MachineProfile {
        schema_version: PROFILE_SCHEMA,
        measurement_version: MEASUREMENT_VERSION,
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        app_build: String::new(), // the daemon helper stamps its build before saving
        measured_at,
        machine,
        seconds: started.elapsed().as_secs_f64(),
        encoders,
        graphics,
        memory_copy_bytes_per_second,
        disk_write_bytes_per_second,
        disk_device: space::device(scratch),
        complete: skipped.is_empty(),
        skipped,
        notes,
    })
}

/// One encoder measurement to make: `sessions` concurrent sessions of one
/// path, codec, depth, size and rate.
#[derive(Debug, Clone, Copy)]
struct Cell {
    resource: EncoderResource,
    priority: Priority,
    codec: Codec,
    bit_depth: u8,
    width: u32,
    height: u32,
    fps: u32,
    sessions: u32,
}

/// The order cells run in: every required cell before any diagnostic one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Priority {
    /// A concurrent HEVC cell the capacity fit uses (full windows required).
    Fit,
    ProresResource,
    /// Required coverage that is still only an observation.
    Required,
    Diagnostic,
}

/// A session of this path, codec and depth could not start; later cells of
/// the same family are skipped rather than retried.
#[derive(Debug)]
struct NotStarted;

impl std::fmt::Display for NotStarted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the encoder session could not start")
    }
}

impl Cell {
    fn label(&self) -> String {
        let sessions = if self.sessions > 1 {
            format!(" ×{}", self.sessions)
        } else {
            String::new()
        };
        // ProRes ignores bit_depth (always 10 bits or more).
        let depth = if self.is_prores() {
            String::new()
        } else {
            format!(" {}-bit", self.bit_depth)
        };
        let (path, rate) = match self.resource {
            EncoderResource::RecordingWriter => {
                ("recording", format!("constant quality {}", self.quality()))
            }
            EncoderResource::LiveEncoder => {
                ("live", format!("{LIVE_BITRATE_KBPS} kb/s constant bitrate"))
            }
        };
        format!(
            "{path} {:?}{depth} {}×{} {} fps{sessions}, {rate}",
            self.codec, self.width, self.height, self.fps
        )
    }

    fn required(&self) -> bool {
        self.priority != Priority::Diagnostic
    }

    fn is_prores(&self) -> bool {
        !matches!(self.codec, Codec::H264 | Codec::Hevc)
    }

    fn family(&self) -> (EncoderResource, Codec, u8) {
        (self.resource, self.codec, self.bit_depth)
    }

    fn quality(&self) -> u8 {
        EncoderQuality::Preset(QualityPreset::High).value()
    }

    /// How long the cell is planned to take, for the deadline.
    fn planned(&self) -> Duration {
        WARM_UP + WINDOW + OPEN_AND_FINISH + SESSION_GAP * self.sessions
    }

    /// Its labels for a measurement: only fit cells are fit evidence.
    fn measurement(&self, frames: u64, seconds: f64, dropped: Option<u64>) -> EncoderMeasurement {
        let recording = self.resource == EncoderResource::RecordingWriter;
        EncoderMeasurement {
            resource: self.resource,
            backend: backend().into(),
            evidence: if self.priority == Priority::Fit {
                Evidence::Fit
            } else if self.priority == Priority::ProresResource {
                Evidence::ProresResource
            } else {
                Evidence::Diagnostic
            },
            rate_control: if recording {
                RateControlMode::ConstantQuality
            } else {
                RateControlMode::ConstantBitrate
            },
            quality: recording.then(|| self.quality()),
            bitrate_kbps: (!recording).then_some(LIVE_BITRATE_KBPS),
            codec: self.codec,
            bit_depth: self.bit_depth,
            width: self.width,
            height: self.height,
            fps: self.fps,
            sessions: self.sessions,
            frames,
            seconds,
            frames_per_second: frames as f64 / seconds,
            dropped,
            hardware_accelerated: None,
            process_cpu_seconds_per_frame: None,
            system_cpu_seconds_per_frame: None,
        }
    }

    fn encoder(&self) -> Encoder {
        Encoder {
            codec: self.codec,
            quality: EncoderQuality::Preset(QualityPreset::High),
            container: Container::Mov,
            bit_depth: self.bit_depth,
        }
    }

    /// The most frames one session gets: [`MAX_FRAMES`], fewer if its file
    /// would grow past [`MAX_FILE_BYTES`].
    fn max_frames(&self) -> u64 {
        let per_frame = self.file_bytes_per_second() / f64::from(self.fps.max(1));
        let budget = if self.priority == Priority::ProresResource {
            512.0 * 1e6
        } else {
            MAX_FILE_BYTES
        };
        let by_size = (budget / per_frame.max(1.0)).floor() as u64;
        by_size.clamp(MIN_FRAMES * 2, MAX_FRAMES)
    }

    fn file_bytes_per_second(&self) -> f64 {
        isocast_protocol::encoder_bytes_per_second(
            &self.encoder(),
            self.width,
            self.height,
            self.fps,
            0,
        )
    }

    fn spec(&self, path: PathBuf) -> WriterSpec {
        WriterSpec {
            path,
            container: Container::Mov,
            video: VideoTrackSpec {
                width: self.width,
                height: self.height,
                fps: self.fps,
                encoder: self.encoder(),
            },
            audio: Vec::new(),
            created: std::time::SystemTime::now(),
        }
    }
}

/// The native encoder backend measurements name. The platform trait has
/// no backend identity yet; macOS is the only calibration backend.
fn backend() -> &'static str {
    if cfg!(target_os = "macos") {
        "videotoolbox"
    } else {
        "unknown"
    }
}

fn truncate(mut reason: String) -> String {
    if reason.len() > MAX_REASON {
        let mut end = MAX_REASON;
        while !reason.is_char_boundary(end) {
            end -= 1;
        }
        reason.truncate(end);
        reason.push('…');
    }
    reason
}

/// The encoder cells in the order they run, so a run that runs out of time
/// keeps what matters and says which cells it skipped: the concurrent HEVC
/// cells the fit uses (including its two-session cross-check), then the
/// other required cells (4K60 HEVC 10-bit, live H.264), then diagnostic
/// cells, one of each codec and ProRes variant first.
fn cells() -> Vec<Cell> {
    let cell = |priority, resource, codec, bit_depth, (width, height): (u32, u32), fps| Cell {
        resource,
        priority,
        codec,
        bit_depth,
        width,
        height,
        fps,
        sessions: 1,
    };
    let recording = |priority, codec, bit_depth, size, fps| {
        cell(
            priority,
            EncoderResource::RecordingWriter,
            codec,
            bit_depth,
            size,
            fps,
        )
    };
    let diagnostic =
        |codec, bit_depth, size, fps| recording(Priority::Diagnostic, codec, bit_depth, size, fps);
    let mut cells = Vec::new();
    for (size, fps, sessions) in [(HD, 60, 2), (HD, 60, 4), (UHD, 30, 4)] {
        cells.push(Cell {
            sessions,
            ..recording(Priority::Fit, Codec::Hevc, 8, size, fps)
        });
    }
    cells.push(recording(Priority::Required, Codec::Hevc, 10, UHD, 60));
    cells.push(cell(
        Priority::Required,
        EncoderResource::LiveEncoder,
        Codec::H264,
        8,
        HD,
        60,
    ));
    // Native resources, not free H.26x pixels: full concurrent windows and
    // explicit HW/SW selection precede single-session diagnostics.
    for codec in [
        Codec::Prores422,
        Codec::Prores422Proxy,
        Codec::Prores422Lt,
        Codec::Prores422Hq,
        Codec::Prores4444,
    ] {
        cells.push(Cell {
            sessions: 2,
            ..recording(Priority::ProresResource, codec, 8, HD, 60)
        });
    }
    cells.push(Cell {
        sessions: 2,
        ..recording(Priority::ProresResource, Codec::Prores422, 8, UHD, 30)
    });
    // ProRes ignores bit_depth (always 10 bits or more); 8 as in setups.
    for (codec, bit_depth) in [
        (Codec::H264, 8),
        (Codec::Prores422, 8),
        (Codec::Prores422Hq, 8),
        (Codec::Prores4444, 8),
        (Codec::Prores422Lt, 8),
        (Codec::Prores422Proxy, 8),
        (Codec::Hevc, 10),
    ] {
        cells.push(diagnostic(codec, bit_depth, HD, 60));
    }
    for (codec, bit_depth) in [(Codec::Hevc, 8), (Codec::H264, 8), (Codec::Prores422, 8)] {
        cells.push(diagnostic(codec, bit_depth, UHD, 60));
    }
    for (codec, bit_depth) in [
        (Codec::Hevc, 8),
        (Codec::H264, 8),
        (Codec::Prores422, 8),
        (Codec::Hevc, 10),
    ] {
        cells.push(diagnostic(codec, bit_depth, UHD, 30));
        cells.push(diagnostic(codec, bit_depth, HD, 30));
    }
    cells.push(diagnostic(Codec::Hevc, 8, HD, 60));
    cells
}

/// Run `cell`'s sessions at once: each prepares its writer, all start
/// together, and each encodes for [`WARM_UP`] then [`WINDOW`] as fast
/// as it accepts frames, then finishes its file. The result is the frames
/// all sessions completed in their timed parts over the longest timed part
/// (the aggregate throughput of `sessions` concurrent sessions).
fn encode_cell<P: Platform>(
    platform: &P,
    cell: &Cell,
    frames: &[P::Frame],
    scratch: &Path,
    index: usize,
) -> Result<EncoderMeasurement> {
    let sessions = cell.sessions as usize;
    let max_frames = cell.max_frames();
    let needed = (cell.file_bytes_per_second() / f64::from(cell.fps.max(1))
        * max_frames as f64
        * sessions as f64) as u64;
    let free = platform.volume(scratch)?.free_bytes;
    if free < needed.saturating_add(FREE_RESERVE) {
        bail!(
            "not enough free space for its scratch files ({} free)",
            space::human(free as f64)
        );
    }
    let paths: Vec<PathBuf> = (0..sessions)
        .map(|session| scratch.join(format!("encode-{index}-{session}.mov")))
        .collect();
    let barrier = Barrier::new(sessions);
    let cpu_before = platform.process_cpu_seconds();
    let system_cpu_before = platform.system_cpu_seconds();
    let results: Vec<Result<(u64, Duration, Option<bool>)>> = thread::scope(|scope| {
        let threads: Vec<_> = paths
            .iter()
            .map(|path| {
                let spec = cell.spec(path.clone());
                let frames = frames.to_vec();
                let barrier = &barrier;
                scope.spawn(move || {
                    let _ = platform.calibration_priority();
                    encode_session(platform, &spec, &frames, max_frames, barrier)
                })
            })
            .collect();
        threads
            .into_iter()
            .map(|t| {
                t.join()
                    .unwrap_or_else(|_| Err(anyhow!("an encoder session panicked")))
            })
            .collect()
    });
    for path in &paths {
        let _ = fs::remove_file(path);
    }
    let mut frames_done = 0;
    let mut longest = Duration::ZERO;
    let mut shortest = Duration::MAX;
    let mut backends = Vec::new();
    for result in results {
        let (completed, elapsed, hardware) = result?;
        backends.push(hardware);
        frames_done += completed;
        longest = longest.max(elapsed);
        shortest = shortest.min(elapsed);
    }
    // A fit cell is fit evidence only over full windows in every session;
    // a shorter one is not demoted to a diagnostic, it is not reported.
    if matches!(cell.priority, Priority::Fit | Priority::ProresResource)
        && shortest.as_secs_f64() < FIT_MIN_SECONDS
    {
        bail!(
            "a session was timed for only {:.2} s, shorter than the {FIT_MIN_SECONDS} s a fit \
             measurement needs",
            shortest.as_secs_f64()
        );
    }
    let mut measurement = finished(cell, frames_done, longest, None)?;
    measurement.hardware_accelerated = backends
        .first()
        .copied()
        .flatten()
        .filter(|first| backends.iter().all(|b| *b == Some(*first)));
    if cell.priority == Priority::ProresResource && measurement.hardware_accelerated.is_none() {
        bail!("native ProRes hardware/software selection is unknown or differs between sessions");
    }
    measurement.process_cpu_seconds_per_frame = cpu_before
        .zip(platform.process_cpu_seconds())
        .filter(|(before, after)| after >= before)
        .map(|(before, after)| (after - before) / frames_done.max(1) as f64);
    measurement.system_cpu_seconds_per_frame = system_cpu_before
        .zip(platform.system_cpu_seconds())
        .filter(|(before, after)| after > before)
        .map(|(before, after)| (after - before) / frames_done.max(1) as f64);
    Ok(measurement)
}

/// `cell`'s measurement of `frames` completed over `timed`, if enough
/// frames completed to report it.
fn finished(
    cell: &Cell,
    frames: u64,
    timed: Duration,
    dropped: Option<u64>,
) -> Result<EncoderMeasurement> {
    let seconds = timed.as_secs_f64();
    if frames < MIN_FRAMES * cell.sessions as u64 || seconds <= 0.0 {
        bail!("only {frames} frames completed in its timed part, too few to report");
    }
    if !(frames as f64 / seconds).is_finite() {
        bail!("its timing was not a finite number");
    }
    Ok(cell.measurement(frames, seconds, dropped))
}

/// The live H.264 cell: the stream's real-time encoder at its default
/// bitrate, given frames as fast as it takes them, counting the pictures
/// it returned (and those it dropped) over the timed interval.
fn live_cell<P: Platform>(
    platform: &P,
    cell: &Cell,
    frames: &[P::Frame],
) -> Result<EncoderMeasurement> {
    if frames.is_empty() {
        bail!("no synthetic frames");
    }
    let fps = cell.fps;
    let mut encoder = platform
        .create_video_encoder(&LiveVideoSpec {
            width: cell.width,
            height: cell.height,
            fps,
            bitrate_kbps: LIVE_BITRATE_KBPS,
            constant_bitrate: true,
            // As a stream: keyframes asked for on the grid, the encoder's
            // own limit only a backstop at twice the interval.
            keyframe_interval: 2 * fps * LIVE_KEYFRAME_SECONDS,
            low_latency: false,
        })
        .context(NotStarted)?;
    let keyframe_every = u64::from(fps * LIVE_KEYFRAME_SECONDS);
    let start = Instant::now();
    let warm_end = start + WARM_UP;
    let window_end = warm_end + WINDOW;
    let warm_cap = MAX_FRAMES / 4;
    let base = Time::from_nanos(0);
    let mut received: u64 = 0;
    let mut n: u64 = 0;
    let mut timed_from: Option<(Instant, u64, u64)> = None;
    loop {
        let now = Instant::now();
        if timed_from.is_none() && (now >= warm_end || n >= warm_cap) {
            timed_from = Some((now, received, encoder.dropped()));
        }
        if timed_from.is_some() && (now >= window_end || n >= MAX_FRAMES) {
            break;
        }
        let frame = &frames[(n % frames.len() as u64) as usize];
        // Waits a bounded time when its submissions are full, then drops.
        encoder.encode(
            frame,
            base.add_ticks(n, fps),
            n.is_multiple_of(keyframe_every),
        )?;
        n += 1;
        while encoder.receive()?.is_some() {
            received += 1;
        }
    }
    let done = Instant::now();
    let (from, before, dropped_before) = timed_from.expect("set before the loop ends");
    let dropped = encoder.dropped().saturating_sub(dropped_before);
    drop(encoder);
    finished(
        cell,
        received.saturating_sub(before),
        done.duration_since(from),
        Some(dropped),
    )
}

/// One session of a cell: deltas of completed pictures and elapsed time
/// after warm-up, followed by successful finalization outside that interval.
fn encode_session<P: Platform>(
    platform: &P,
    spec: &WriterSpec,
    frames: &[P::Frame],
    max_frames: u64,
    barrier: &Barrier,
) -> Result<(u64, Duration, Option<bool>)> {
    let prepared = platform.create_writer(spec).context(NotStarted);
    // Every session waits here, even one that failed, so none waits forever.
    barrier.wait();
    let mut writer = prepared?;
    if frames.is_empty() {
        writer.cancel();
        bail!("no synthetic frames");
    }
    let fps = spec.video.fps;
    let start = Instant::now();
    let warm_end = start + WARM_UP;
    let window_end = warm_end + WINDOW;
    // A session capped by file size still keeps most of its frames timed.
    let warm_cap = (max_frames / 4).max(1);
    let base = Time::from_nanos(0);
    writer.start(base)?;
    let mut n: u64 = 0;
    let mut timed_from: Option<(Instant, u64)> = None;
    loop {
        let now = Instant::now();
        if timed_from.is_none() && (now >= warm_end || n >= warm_cap) {
            let completed = writer
                .completed_video_frames()
                .context("backend does not expose completed video frames")?;
            timed_from = Some((now, completed));
        }
        if timed_from.is_some() && (now >= window_end || n >= max_frames) {
            break;
        }
        let frame = &frames[(n % frames.len() as u64) as usize];
        let pts = base.add_ticks(n, fps);
        let stall = Instant::now() + ACCEPT_STALL;
        while !writer.append_video(frame, pts)? {
            if Instant::now() >= stall {
                bail!(
                    "the encoder accepted no frame for {} s",
                    ACCEPT_STALL.as_secs()
                );
            }
            thread::sleep(Duration::from_micros(500));
        }
        n += 1;
    }
    let done = Instant::now();
    let completed = writer
        .completed_video_frames()
        .context("backend does not expose completed video frames")?;
    // Do not include native drain or fsync time in a steady-state encoder
    // measurement. Still require successful finalization before accepting it.
    let hardware = writer.hardware_accelerated();
    writer.finish(base.add_ticks(n, fps))?;
    let (from, before) = timed_from.expect("set before the loop ends");
    Ok((
        completed.saturating_sub(before),
        done.duration_since(from),
        hardware,
    ))
}

/// Synthetic frames by size, drawn once (or why they couldn't be).
type Cycles<F> = Vec<((u32, u32), std::result::Result<Vec<F>, String>)>;

/// The synthetic frames at `size`, drawn on first use.
fn frames_for<'a, P: Platform>(
    platform: &P,
    cycles: &'a mut Cycles<P::Frame>,
    size: (u32, u32),
) -> std::result::Result<&'a [P::Frame], String> {
    if !cycles.iter().any(|(s, _)| *s == size) {
        let count = if size.0 * size.1 > HD.0 * HD.1 {
            CYCLE_UHD
        } else {
            CYCLE_HD
        };
        let drawn = synthetic_frames(platform, size, count).map_err(|err| format!("{err:#}"));
        cycles.push((size, drawn));
    }
    let (_, drawn) = cycles
        .iter()
        .find(|(s, _)| *s == size)
        .expect("drawn above");
    drawn.as_deref().map_err(Clone::clone)
}

/// `count` BGRA frames of busy, camera-like content (tune's texture), each
/// a shifted window so every frame moves.
fn synthetic_frames<P: Platform>(
    platform: &P,
    (width, height): (u32, u32),
    count: usize,
) -> Result<Vec<P::Frame>> {
    let (w, h) = (width as usize, height as usize);
    let texture = Texture::new(w + TEXTURE_MARGIN, h + TEXTURE_MARGIN);
    let mut pool = platform.frame_pool(width, height)?;
    let mut frames = Vec::with_capacity(count);
    for k in 0..count {
        let (dx, dy) = ((k * 7) % TEXTURE_MARGIN, (k * 5) % TEXTURE_MARGIN);
        let frame = pool
            .draw(&mut |pixels, stride| texture.copy(dx, dy, w, h, pixels, stride))?
            .context("the frame pool had no free frame for the synthetic pictures")?;
        frames.push(frame);
    }
    Ok(frames)
}

/// The GPU's composition and Y'CbCr conversion at HD and UHD, through the
/// compositor's own renderer, conversion and the platform's frame import,
/// each waited for until the GPU finished it. Failures become notes.
fn measure_graphics<P: Platform>(
    platform: &P,
    cycles: &mut Cycles<P::Frame>,
    deadline: Instant,
    reserve: Duration,
    notes: &mut Vec<String>,
) -> Vec<GraphicsMeasurement> {
    let mut out = Vec::new();
    let gpu = match Gpu::new() {
        Ok(gpu) => gpu,
        Err(err) => {
            notes.push(format!("GPU not measured: {err:#}"));
            return out;
        }
    };
    let renderer = Renderer::new(&gpu, wgpu::TextureFormat::Bgra8Unorm);
    let to_ycbcr = ToYCbCr::new(&gpu);
    for size in [HD, UHD] {
        // Leave the required encoder cells, which come next, their time.
        if Instant::now() + GPU_TIME + reserve > deadline {
            notes.push(format!(
                "GPU at {}×{} not measured: out of time",
                size.0, size.1
            ));
            continue;
        }
        let frames = match frames_for(platform, cycles, size) {
            Ok(frames) => frames,
            Err(err) => {
                notes.push(format!(
                    "GPU at {}×{} not measured: no synthetic frames: {err}",
                    size.0, size.1
                ));
                continue;
            }
        };
        let until = Instant::now() + GPU_TIME;
        match graphics_at(platform, &gpu, &renderer, &to_ycbcr, frames, size, until) {
            Ok(measurement) => out.push(measurement),
            Err(err) => notes.push(format!(
                "GPU at {}×{} not measured: {err:#}",
                size.0, size.1
            )),
        }
    }
    if !out.is_empty() {
        notes.push(
            "GPU: composite_ms is drawing a full-frame picture and three picture-in-picture \
             layers onto a BGRA canvas, conversion_ms converting the canvas to 4:2:0 Y'CbCr; \
             each per frame until the GPU finished, after start-up frames"
                .into(),
        );
    }
    out
}

fn graphics_at<P: Platform>(
    platform: &P,
    gpu: &Gpu,
    renderer: &Renderer,
    to_ycbcr: &ToYCbCr,
    frames: &[P::Frame],
    (width, height): (u32, u32),
    until: Instant,
) -> Result<GraphicsMeasurement> {
    let mut importer = platform.frame_importer(gpu)?;
    let imported: Vec<GpuFrame> = frames
        .iter()
        .map(|frame| importer.import(frame))
        .collect::<Result<_>>()?;
    if imported.is_empty() {
        bail!("no synthetic frames");
    }
    let canvas = crate::scene::canvas_texture(gpu, renderer.format(), (width, height));
    let bound = to_ycbcr.bind(gpu, &canvas);
    let mut pool = platform.ycbcr_frame_pool(width, height)?;
    let target = pool
        .take()?
        .context("the Y'CbCr frame pool had no free frame")?;
    let (luma, chroma) = importer.ycbcr_target(&target)?;
    let luma = luma.create_view(&Default::default());
    let chroma = chroma.create_view(&Default::default());
    let (w, h) = (width as f32, height as f32);
    let mut composite = Duration::ZERO;
    let mut conversion = Duration::ZERO;
    let mut timed: u32 = 0;
    for i in 0..GPU_WARM_UP + GPU_MAX_FRAMES {
        if i >= GPU_WARM_UP && Instant::now() >= until {
            break;
        }
        let k = i as usize;
        let count = imported.len();
        let mut layers = vec![Layer::full(&imported[k % count], width, height)];
        for j in 0..3 {
            let dest = Rect {
                x: w * (0.04 + 0.32 * j as f32),
                y: h * 0.64,
                width: w * 0.28,
                height: h * 0.28,
            };
            layers.push(Layer::rect(
                &imported[(k + j + 1) % count],
                Crop::default(),
                dest,
                0.9,
            ));
        }
        let drawing = Instant::now();
        let submission = renderer.render(gpu, &canvas, &layers, (width, height));
        wait(gpu, submission)?;
        let converting = Instant::now();
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        to_ycbcr.encode(&mut encoder, &bound, &luma, &chroma);
        let submission = gpu.queue.submit([encoder.finish()]);
        wait(gpu, submission)?;
        let converted = Instant::now();
        if i >= GPU_WARM_UP {
            composite += converting - drawing;
            conversion += converted - converting;
            timed += 1;
        }
    }
    if timed == 0 {
        bail!("no frame was timed");
    }
    let ms = |total: Duration| total.as_secs_f64() * 1000.0 / f64::from(timed);
    Ok(GraphicsMeasurement {
        width,
        height,
        frames: timed,
        composite_ms: ms(composite),
        conversion_ms: ms(conversion),
    })
}

/// Wait until the GPU finished `submission`.
fn wait(gpu: &Gpu, submission: wgpu::SubmissionIndex) -> Result<()> {
    let _status = gpu
        .device
        .poll(wgpu::PollType::Wait {
            submission_index: Some(submission),
            timeout: Some(GPU_WAIT),
        })
        .context("the GPU did not finish in time")?;
    Ok(())
}

/// Bytes per second copied between two buffers of at most
/// [`MEMORY_COPY_BYTES`] (and 1/64 of `physical` bytes), for about
/// [`MEMORY_TIME`].
fn memory_copy(physical: f64) -> Option<f64> {
    let mut size = MEMORY_COPY_BYTES;
    if physical > 0.0 {
        size = size.min((physical / 64.0) as usize).max(1 << 20);
    }
    let source: Vec<u8> = (0..size)
        .map(|i| (i as u32).wrapping_mul(0x9E37_79B9).to_le_bytes()[3])
        .collect();
    let mut dest = vec![0u8; size];
    // Touch every page before timing.
    dest.copy_from_slice(&source);
    let started = Instant::now();
    let mut passes = 0;
    while passes < MEMORY_MAX_PASSES && started.elapsed() < MEMORY_TIME {
        dest.copy_from_slice(std::hint::black_box(&source));
        std::hint::black_box(&mut dest);
        passes += 1;
    }
    let elapsed = started.elapsed().as_secs_f64();
    let rate = (size * passes) as f64 / elapsed;
    (elapsed > 0.0 && rate.is_finite() && rate > 0.0).then_some(rate)
}

/// Bytes per second written to `scratch` and synced ([`DISK_BYTES`]), and
/// a note naming the volume.
fn disk_write<P: Platform>(platform: &P, scratch: &Path) -> Result<(f64, String)> {
    let volume = platform.volume(scratch)?;
    if volume.free_bytes < DISK_BYTES as u64 + FREE_RESERVE {
        bail!(
            "only {} free on the scratch volume",
            space::human(volume.free_bytes as f64)
        );
    }
    // Bytes that don't compress, in case the file system compresses.
    let mut chunk = vec![0u8; DISK_CHUNK];
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for byte in chunk.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *byte = x as u8;
    }
    let path = scratch.join("disk-write.bin");
    let started = Instant::now();
    let result = (|| -> std::io::Result<()> {
        let mut file = File::create(&path)?;
        for _ in 0..DISK_BYTES / DISK_CHUNK {
            file.write_all(&chunk)?;
        }
        file.sync_all()
    })();
    let elapsed = started.elapsed().as_secs_f64();
    let _ = fs::remove_file(&path);
    result.with_context(|| format!("could not write {}", path.display()))?;
    let rate = DISK_BYTES as f64 / elapsed;
    if !(elapsed > 0.0 && rate.is_finite()) {
        bail!("no time was measured");
    }
    let network = if volume.network {
        ", a network share"
    } else {
        ""
    };
    Ok((
        rate,
        format!(
            "disk write: {} MB written and synced on the scratch volume ({}{network}), a short \
             burst; the recording folder's own volume and free space are checked separately",
            DISK_BYTES >> 20,
            volume.file_system
        ),
    ))
}
