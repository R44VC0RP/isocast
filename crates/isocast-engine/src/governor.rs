//! Runtime quality ceilings. The existing encoder-pressure controller stays
//! authoritative; these slower limits never change file formats or audio.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use isocast_protocol::{
    CanvasId,
    governor::{GovernorSettings, GovernorStatus},
};
use parking_lot::Mutex;

static ENABLED: AtomicBool = AtomicBool::new(false);
static ACTIVE: AtomicBool = AtomicBool::new(false);
static MAIN_ACTIVE: AtomicBool = AtomicBool::new(false);
static VERTICAL_ACTIVE: AtomicBool = AtomicBool::new(false);
static PREVIEW: AtomicU32 = AtomicU32::new(0);
static MONITOR_DEMAND: AtomicU32 = AtomicU32::new(0);
static ISO: AtomicU32 = AtomicU32::new(100);
static ISO_ALLOWED: AtomicBool = AtomicBool::new(false);
// A separate, bounded emergency overlay cannot be overwritten by a slower
// quality-policy update, and cannot freeze ISO video for an entire episode.
static HOLD_UNTIL: AtomicU64 = AtomicU64::new(0);
static HOLD_ARMED: AtomicBool = AtomicBool::new(true);
static VERTICAL: AtomicU32 = AtomicU32::new(100);
static STREAM: AtomicU32 = AtomicU32::new(100);
static STREAM_TARGETS: AtomicU32 = AtomicU32::new(0);
static STREAM_APPLIED: [AtomicU32; 2] = [AtomicU32::new(100), AtomicU32::new(100)];
static RATE_FAILED: [AtomicBool; 2] = [AtomicBool::new(false), AtomicBool::new(false)];
static ISO_PINNED: AtomicBool = AtomicBool::new(false);
static UNPINNED_ISO: AtomicBool = AtomicBool::new(false);
static LOSSES: AtomicU64 = AtomicU64::new(0);
static NOTICES: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct Shared {
    settings: GovernorSettings,
    pins: HashMap<String, Weak<AtomicBool>>,
    status: GovernorStatus,
    rate_error: Option<String>,
    previews: Vec<Weak<PreviewState>>,
}
fn shared() -> &'static Mutex<Shared> {
    static VALUE: OnceLock<Mutex<Shared>> = OnceLock::new();
    VALUE.get_or_init(|| Mutex::new(Shared::default()))
}

pub fn status() -> GovernorStatus {
    let mut status = shared().lock().status.clone();
    status.native_iso_percent = crate::record::pressure::snapshot().1;
    status
}

pub fn clock_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH
        .get_or_init(Instant::now)
        .elapsed()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

pub fn monitor_fps(requested: u32) -> u32 {
    MONITOR_DEMAND.fetch_max(requested, Ordering::Relaxed);
    let cap = PREVIEW.load(Ordering::Relaxed);
    if ENABLED.load(Ordering::Relaxed) && cap > 0 {
        requested.min(cap)
    } else {
        requested
    }
}

/// Registered once per ISO runner, not looked up or locked per picture.
pub(crate) fn iso_pin(source: &str) -> Arc<AtomicBool> {
    let mut state = shared().lock();
    if let Some(pin) = state.pins.get(source).and_then(Weak::upgrade) {
        return pin;
    }
    let pin = Arc::new(AtomicBool::new(state.settings.iso_pinned(source)));
    if !pin.load(Ordering::Relaxed) && !UNPINNED_ISO.swap(true, Ordering::Relaxed) {
        HOLD_ARMED.store(true, Ordering::Relaxed);
    }
    state.pins.insert(source.into(), Arc::downgrade(&pin));
    pin
}
pub(crate) fn iso_ceiling(pin: &AtomicBool) -> u32 {
    if ENABLED.load(Ordering::Relaxed)
        && ACTIVE.load(Ordering::Relaxed)
        && ISO_ALLOWED.load(Ordering::Relaxed)
        && !pin.load(Ordering::Relaxed)
    {
        if clock_ms() < HOLD_UNTIL.load(Ordering::Relaxed) {
            0
        } else {
            ISO.load(Ordering::Relaxed) * 10
        }
    } else {
        1000
    }
}
pub fn vertical_ceiling() -> u32 {
    if ENABLED.load(Ordering::Relaxed) && MAIN_ACTIVE.load(Ordering::Relaxed) {
        VERTICAL.load(Ordering::Relaxed) * 10
    } else {
        1000
    }
}
pub fn stream_bitrate(canvas: CanvasId, intended_kbps: u32) -> u32 {
    let index = usize::from(!canvas.is_main());
    if !ENABLED.load(Ordering::Relaxed)
        || RATE_FAILED[index].load(Ordering::Acquire)
        || STREAM_TARGETS.load(Ordering::Relaxed) & (1 << index) == 0
    {
        return intended_kbps;
    }
    (intended_kbps.saturating_mul(STREAM.load(Ordering::Relaxed)) / 100).max(500.min(intended_kbps))
}
/// Called on the encoder owner. No blocking lock in the failure path.
pub fn stream_rate_result(canvas: CanvasId, intended: u32, applied: u32, error: Option<String>) {
    let index = usize::from(!canvas.is_main());
    STREAM_APPLIED[index].store(
        applied.saturating_mul(100) / intended.max(1),
        Ordering::Relaxed,
    );
    if let Some(error) = error {
        RATE_FAILED[index].store(true, Ordering::Release);
        if let Some(mut state) = shared().try_lock() {
            state.rate_error = Some(error);
        }
    }
    NOTICES.fetch_add(1, Ordering::Release);
}

#[derive(Default)]
struct PreviewState {
    failed: AtomicBool,
    error: Mutex<Option<String>>,
}

/// Per-encoder failure ownership. An unrelated healthy monitor cannot clear
/// this error, and ending the affected feed removes it without blocking on
/// global governor state from the media worker's destructor.
pub(crate) struct PreviewRate(Option<Arc<PreviewState>>);

impl PreviewRate {
    pub fn new() -> Self {
        let state = Arc::new(PreviewState::default());
        shared().lock().previews.push(Arc::downgrade(&state));
        Self(Some(state))
    }

    pub fn result(&self, error: Option<String>) {
        let state = self.0.as_ref().expect("live monitor notice");
        state.failed.store(error.is_some(), Ordering::Relaxed);
        if let Some(mut message) = state.error.try_lock() {
            *message = error;
        }
        NOTICES.fetch_add(1, Ordering::Release);
    }
}

impl Drop for PreviewRate {
    fn drop(&mut self) {
        drop(self.0.take());
        NOTICES.fetch_add(1, Ordering::Release);
    }
}

/// Immediate three-second hold, once per pressure episode. This is not a
/// permanent quality step; native admission takes over when the hold ends.
pub fn protected_loss(canvas: CanvasId, count: u64) {
    let active = if canvas.is_main() {
        &MAIN_ACTIVE
    } else {
        &VERTICAL_ACTIVE
    };
    if count == 0 || !ENABLED.load(Ordering::Relaxed) || !active.load(Ordering::Relaxed) {
        return;
    }
    // Producers subtract intentional admission refusals before publishing.
    // The current ceiling cannot classify work decided before restoration.
    LOSSES.fetch_add(count, Ordering::Relaxed);
    request_hold();
}

fn request_hold() {
    if ISO_ALLOWED.load(Ordering::Relaxed)
        && !ISO_PINNED.load(Ordering::Relaxed)
        && UNPINNED_ISO.load(Ordering::Relaxed)
        && HOLD_ARMED
            .compare_exchange(true, false, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        HOLD_UNTIL.store(clock_ms().saturating_add(3000), Ordering::Relaxed);
    }
}

/// Fresh observations from the existing native headroom sampler. Ratios are
/// native completion latency / its learned backlog limit, not utilization.
#[derive(Debug, Clone, Default)]
pub struct Headroom {
    pub age_ms: f64,
    pub ready: bool,
    pub encoder_threatened: bool,
    pub pending_frames: u64,
    pub recovery_ratio: f64,
    pub cpu_percent: Option<f64>,
    pub thermal_state: u32,
}

#[derive(Default)]
pub struct Sample {
    pub active: bool,
    pub main_protected: bool,
    pub vertical_protected: bool,
    pub paused: bool,
    pub progressed: bool,
    pub protected_loss: bool,
    pub encoder_pressure: bool,
    pub native_iso_percent: u32,
    pub headroom: Option<Headroom>,
    pub gpu_late: bool,
    pub cpu_late: bool,
    pub gpu_headroom: bool,
    /// None means the disk probe is stale, not that the disk recovered.
    pub disk_behind: Option<bool>,
    pub upload_congestion: f64,
    pub stream_canvases: u32,
    pub congested_canvases: u32,
    pub has_isos: bool,
    pub has_monitors: bool,
    pub has_vertical: bool,
    pub has_stream: bool,
}

#[derive(Clone, Copy, Default, PartialEq)]
struct Limits {
    preview: u32,
    iso: u32,
    vertical: u32,
    stream: u32,
}
impl Limits {
    fn full() -> Self {
        Self {
            preview: 0,
            iso: 100,
            vertical: 100,
            stream: 100,
        }
    }
    fn read() -> Self {
        Self {
            preview: PREVIEW.load(Ordering::Relaxed),
            iso: ISO.load(Ordering::Relaxed),
            vertical: VERTICAL.load(Ordering::Relaxed),
            stream: STREAM.load(Ordering::Relaxed),
        }
    }
    fn apply(self) {
        PREVIEW.store(self.preview, Ordering::Relaxed);
        ISO.store(self.iso, Ordering::Relaxed);
        VERTICAL.store(self.vertical, Ordering::Relaxed);
        STREAM.store(self.stream, Ordering::Relaxed);
    }
}

pub struct Controller {
    previous: Limits,
    losses: u64,
    pressure_since: Option<Instant>,
    quiet_since: Option<Instant>,
    last_step: Instant,
    restore_after: Instant,
    last_restore: Option<Instant>,
    last_rate_step: Instant,
    disk_bad: bool,
    monitor_fps: u32,
    monitor_seen: Instant,
    held: bool,
    hold_cause: Option<&'static str>,
    last_loss: Option<Instant>,
    episode_quiet: Option<Instant>,
    steps: u64,
    notices: u64,
    recovering: bool,
}
impl Default for Controller {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            previous: Limits::full(),
            losses: LOSSES.load(Ordering::Relaxed),
            pressure_since: None,
            quiet_since: None,
            last_step: now - Duration::from_secs(5),
            restore_after: now,
            last_restore: None,
            last_rate_step: now - Duration::from_secs(1),
            disk_bad: false,
            monitor_fps: 0,
            monitor_seen: now,
            held: false,
            hold_cause: None,
            last_loss: None,
            episode_quiet: None,
            steps: 0,
            notices: NOTICES.load(Ordering::Acquire),
            recovering: false,
        }
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        ENABLED.store(false, Ordering::Relaxed);
        ACTIVE.store(false, Ordering::Relaxed);
        MAIN_ACTIVE.store(false, Ordering::Relaxed);
        VERTICAL_ACTIVE.store(false, Ordering::Relaxed);
        HOLD_UNTIL.store(0, Ordering::Relaxed);
        Limits::full().apply();
        shared().lock().status = GovernorStatus {
            enabled: false,
            reason: "Automatic quality controller stopped; native protection remains active".into(),
            steps: self.steps,
            ..Default::default()
        };
    }
}

impl Controller {
    /// No policy/status work is needed on an unchanged, pressure-free tick.
    /// Media producers and optional retune owners publish cheap wake signals.
    pub fn needs_poll(&self) -> bool {
        self.recovering
            || LOSSES.load(Ordering::Relaxed) != self.losses
            || NOTICES.load(Ordering::Acquire) != self.notices
    }

    /// Only an atomic exchange and local fields; no engine/status/policy
    /// locks or allocations while a healthy workload keeps running.
    pub fn observe_monitors(&mut self) {
        let now = Instant::now();
        let demand = MONITOR_DEMAND.swap(0, Ordering::Relaxed);
        if demand > 0
            && (demand >= self.monitor_fps
                || now.duration_since(self.monitor_seen) >= Duration::from_secs(1))
        {
            self.monitor_fps = demand;
            self.monitor_seen = now;
        } else if now.duration_since(self.monitor_seen) >= Duration::from_secs(3) {
            self.monitor_fps = 0;
        }
    }

    pub fn tick(&mut self, settings: &GovernorSettings, sample: &Sample) -> Option<GovernorStatus> {
        let now = Instant::now();
        self.notices = NOTICES.load(Ordering::Acquire);
        ISO_ALLOWED.store(settings.allow_iso_thinning, Ordering::Relaxed);
        if !settings.allow_iso_thinning {
            HOLD_UNTIL.store(0, Ordering::Relaxed);
        }
        let mut state = shared().lock();
        state.settings = settings.clone();
        let mut unpinned_iso = false;
        let mut pinned_iso = false;
        state.pins.retain(|source, weak| {
            let Some(pin) = weak.upgrade() else {
                return false;
            };
            let pinned = settings.iso_pinned(source);
            pin.store(pinned, Ordering::Relaxed);
            unpinned_iso |= !pinned;
            pinned_iso |= pinned && settings.allow_iso_thinning;
            true
        });
        if unpinned_iso && !UNPINNED_ISO.swap(unpinned_iso, Ordering::Relaxed) {
            HOLD_ARMED.store(true, Ordering::Relaxed);
        } else if !unpinned_iso {
            UNPINNED_ISO.store(false, Ordering::Relaxed);
        }
        ISO_PINNED.store(
            !settings.allow_iso_thinning || settings.pinned("recording.iso.fps"),
            Ordering::Relaxed,
        );
        ACTIVE.store(sample.active && !sample.paused, Ordering::Relaxed);
        MAIN_ACTIVE.store(sample.main_protected, Ordering::Relaxed);
        VERTICAL_ACTIVE.store(sample.vertical_protected, Ordering::Relaxed);
        ENABLED.store(settings.enabled, Ordering::Relaxed);
        let mut limits = Limits::read();
        if !sample.has_stream {
            limits.stream = 100;
            STREAM_TARGETS.store(0, Ordering::Relaxed);
            for applied in &STREAM_APPLIED {
                applied.store(100, Ordering::Relaxed);
            }
            for failed in &RATE_FAILED {
                failed.store(false, Ordering::Relaxed);
            }
            state.rate_error = None;
        }
        for i in 0..2 {
            if sample.stream_canvases & (1 << i) == 0 {
                STREAM_APPLIED[i].store(100, Ordering::Relaxed);
                RATE_FAILED[i].store(false, Ordering::Relaxed);
            }
        }
        if settings.pinned("preview.fps") {
            limits.preview = 0;
        }
        if !unpinned_iso {
            limits.iso = 100;
        }
        if settings.pinned("vertical.fps") || !sample.main_protected {
            limits.vertical = 100;
        }
        if settings.pinned("stream.video_bitrate_kbps") {
            limits.stream = 100;
        }
        let health = sample.headroom.as_ref().filter(|h| h.age_ms <= 500.0);
        if let Some(behind) = sample.disk_behind {
            self.disk_bad = behind;
        }
        let losses = LOSSES.load(Ordering::Relaxed);
        let production_loss = sample.protected_loss || losses != self.losses;
        if production_loss {
            self.last_loss = Some(now);
        }
        self.losses = losses;
        let recent_loss = self
            .last_loss
            .is_some_and(|at| now.duration_since(at) < Duration::from_secs(5));
        let upload = sample.upload_congestion >= 0.25;
        let retunable = (0..2).any(|i| {
            sample.congested_canvases & (1 << i) != 0 && !RATE_FAILED[i].load(Ordering::Acquire)
        });
        let cause = if self.disk_bad {
            Some("disk")
        } else if health.is_some_and(|h| h.thermal_state >= 2)
            && (sample.cpu_late
                || sample.gpu_late
                || sample.encoder_pressure
                || sample.protected_loss)
        {
            Some("thermal")
        } else if recent_loss
            && sample.cpu_late
            && health.is_some_and(|h| h.cpu_percent.is_some_and(|p| p >= 90.0))
        {
            Some("CPU")
        } else if sample.gpu_late {
            Some("GPU")
        } else if sample.encoder_pressure || health.is_some_and(|h| h.encoder_threatened) {
            Some("encoder")
        } else if upload {
            Some("upload")
        } else {
            None
        };
        let emergency = production_loss
            || health.is_some_and(|h| h.encoder_threatened && h.pending_frames >= 8);
        let cause = cause.or(emergency.then_some("output"));
        if settings.enabled && sample.active && !sample.paused && emergency && unpinned_iso {
            request_hold();
        }
        let held = settings.enabled
            && sample.active
            && !sample.paused
            && unpinned_iso
            && clock_ms() < HOLD_UNTIL.load(Ordering::Relaxed);
        if !held
            && !emergency
            && !sample.encoder_pressure
            && !health.is_some_and(|h| h.encoder_threatened)
        {
            if now.duration_since(*self.episode_quiet.get_or_insert(now)) >= Duration::from_secs(5)
            {
                HOLD_ARMED.store(true, Ordering::Relaxed);
            }
        } else {
            self.episode_quiet = None;
        }
        let mut reason = state.status.reason.clone();
        let mut pinned = false;
        if !settings.enabled || !sample.active {
            limits = Limits::full();
            self.pressure_since = None;
            self.quiet_since = None;
            self.last_restore = None;
            self.restore_after = now;
            self.last_loss = None;
            self.episode_quiet = None;
            self.disk_bad = false;
            HOLD_UNTIL.store(0, Ordering::Relaxed);
            HOLD_ARMED.store(true, Ordering::Relaxed);
            STREAM_TARGETS.store(0, Ordering::Relaxed);
            reason = if settings.enabled {
                "At requested quality"
            } else {
                "Automatic quality is off; native encoder safety remains active"
            }
            .into();
        } else if sample.paused {
            self.quiet_since = None;
            self.pressure_since = None;
            reason =
                "Automatic quality: outputs paused or connecting; holding current limits".into();
        } else if held {
            self.quiet_since = None;
            if !self.held {
                self.hold_cause = cause;
            }
            if self.last_restore.is_some() {
                self.restore_after = now + Duration::from_secs(120);
            }
            self.last_restore = None;
            reason = format!(
                "Automatic quality: {} threatens a protected output; brief ISO hold (at most 3 s, audio stays whole)",
                cause.unwrap_or("video loss")
            );
        } else if self.held {
            // A CPU/GPU/disk hold that stopped production losses can leave a
            // proportional ceiling. Encoder-only admission already has its
            // own fast feedback: do not add a second sticky ISO controller.
            if settings.allow_iso_thinning
                && unpinned_iso
                && matches!(self.hold_cause, Some("CPU" | "GPU" | "disk" | "thermal"))
                && !production_loss
                && sample.progressed
                && self
                    .last_loss
                    .is_none_or(|at| now.duration_since(at) >= Duration::from_secs(1))
            {
                limits.iso = (limits.iso / 2).max(25);
            }
            reason = format!(
                "Automatic quality: emergency hold ended; unpinned ISO ceiling {}%, native admission remains authoritative",
                limits.iso
            );
            self.hold_cause = None;
        } else if let Some(cause) = cause {
            let sustained = now.duration_since(*self.pressure_since.get_or_insert(now))
                >= Duration::from_secs(2);
            if sustained && now.duration_since(self.last_step) >= Duration::from_secs(5) {
                let preview_step = if limits.preview == 0 && self.monitor_fps > 30 {
                    Some(30)
                } else if limits.preview != 15 && self.monitor_fps > 15 {
                    Some(15)
                } else {
                    None
                };
                if sample.has_monitors
                    && !matches!(cause, "disk" | "upload")
                    && !settings.pinned("preview.fps")
                    && let Some(preview_step) = preview_step
                {
                    limits.preview = preview_step;
                    reason = format!(
                        "Automatic quality: {cause} pressure; preview limited to {} fps",
                        limits.preview
                    );
                } else if !matches!(cause, "upload" | "encoder" | "output")
                    && sample.has_isos
                    && unpinned_iso
                    && limits.iso > 25
                {
                    limits.iso = limits.iso.saturating_sub(25).max(25);
                    reason = format!(
                        "Automatic quality: {cause} pressure; unpinned ISO video limited to {}%",
                        limits.iso
                    );
                } else if upload
                    && sample.has_stream
                    && !settings.pinned("stream.video_bitrate_kbps")
                    && limits.stream > 25
                    && retunable
                {
                    limits.stream = limits.stream.saturating_sub(25).max(25);
                    reason = format!(
                        "Automatic quality: upload queue is behind; requesting {}% stream bitrate",
                        limits.stream
                    );
                } else if sample.has_vertical
                    && sample.main_protected
                    && !settings.pinned("vertical.fps")
                    && limits.vertical > 25
                    && cause == "encoder"
                {
                    limits.vertical -= 25;
                    reason = format!(
                        "Automatic quality: encoder pressure remains; vertical video limited to {}%",
                        limits.vertical
                    );
                } else {
                    pinned = (sample.has_monitors
                        && settings.pinned("preview.fps")
                        && !matches!(cause, "disk" | "upload"))
                        || (sample.has_isos
                            && pinned_iso
                            && !matches!(cause, "upload" | "encoder" | "output"))
                        || (sample.has_vertical
                            && settings.pinned("vertical.fps")
                            && cause == "encoder")
                        || (sample.has_stream
                            && settings.pinned("stream.video_bitrate_kbps")
                            && upload);
                    reason = format!(
                        "Automatic quality: {cause} pressure remains; {}",
                        if pinned {
                            "kept settings cannot be lowered"
                        } else {
                            "no further safe live reduction; size/codec/software changes require a qualified new session"
                        }
                    );
                }
            }
            if self.last_restore.is_some() && (sustained || production_loss) {
                self.restore_after = now + Duration::from_secs(120);
                self.last_restore = None;
            }
        } else {
            self.pressure_since = None;
        }
        // Upload-only congestion must not prevent video-work ceilings from
        // recovering. A single native stall event is not a new 30 s blackout:
        // require actual production loss or an unhealthy measured budget.
        let video_limited = limits.preview > 0 || limits.iso < 100 || limits.vertical < 100;
        let healthy = !held
            && !production_loss
            && !self.disk_bad
            && sample.progressed
            && sample.gpu_headroom
            && (video_limited || sample.upload_congestion < 0.1)
            && health.is_none_or(|h| {
                h.ready
                    && h.recovery_ratio < 0.7
                    && h.cpu_percent.is_none_or(|p| p < 70.0)
                    && h.thermal_state < 2
            });
        if settings.enabled && sample.active && !sample.paused {
            if healthy {
                let quiet = *self.quiet_since.get_or_insert(now);
                if limits != Limits::full()
                    && now >= self.restore_after
                    && now.duration_since(quiet) >= Duration::from_secs(30)
                {
                    if limits.vertical < 100 {
                        limits.vertical = (limits.vertical + 25).min(100);
                    } else if limits.iso < 100 {
                        limits.iso = (limits.iso + 25).min(100);
                    } else if limits.preview > 0 {
                        limits.preview = if limits.preview == 15 { 30 } else { 0 };
                    } else if sample.upload_congestion < 0.1 {
                        limits.stream = (limits.stream + 25).min(100);
                    }
                    self.quiet_since = Some(now);
                    self.last_restore = Some(now);
                    reason =
                        "Automatic quality: 30 seconds of headroom; restoring one quality step"
                            .into();
                } else if limits == Limits::full() && cause.is_none() {
                    reason = "At requested quality".into();
                    if now.duration_since(quiet) >= Duration::from_secs(30) {
                        self.last_restore = None;
                    }
                }
            } else {
                self.quiet_since = None;
            }
        }
        // A stream queue has only one second before it sheds. Do not spend
        // the ordinary ladder's delays on controls that cannot fix upload.
        if settings.enabled
            && sample.active
            && !sample.paused
            && sample.has_stream
            && sample.upload_congestion >= 0.5
            && limits.stream > 25
            && !settings.pinned("stream.video_bitrate_kbps")
            && retunable
            && now.duration_since(self.last_rate_step) >= Duration::from_secs(1)
        {
            limits.stream = (limits.stream / 2).max(25);
            self.last_rate_step = now;
            reason = format!(
                "Automatic quality: upload queue near its limit; requesting {}% bitrate, unpinned ISO ceiling {}%",
                limits.stream,
                if held { 0 } else { limits.iso }
            );
        }
        let rate_error = (sample.has_stream && RATE_FAILED.iter().any(|failed| failed.load(Ordering::Acquire)))
            .then(|| state.rate_error.clone().unwrap_or_else(|| "A stream encoder refused retuning; its last acknowledged target is shown in stream status. Restart that stream to retry".into()));
        let mut preview_error = None;
        state.previews.retain(|weak| {
            let Some(preview) = weak.upgrade() else { return false };
            if preview_error.is_none() && preview.failed.load(Ordering::Relaxed) {
                preview_error = Some(preview.error.lock().clone().unwrap_or_else(||
                    "A preview encoder refused a rate change; the feed continues at its last acknowledged encoder setting".into()));
            }
            true
        });
        let rate_error = rate_error.or(preview_error);
        if limits.stream < self.previous.stream {
            STREAM_TARGETS.fetch_or(sample.congested_canvases, Ordering::Relaxed);
        }
        if limits.stream == 100 {
            STREAM_TARGETS.store(0, Ordering::Relaxed);
        }
        limits.apply();
        if limits != self.previous || held != self.held {
            self.steps += 1;
            self.last_step = now;
        }
        self.previous = limits;
        self.held = held;
        let mut next = GovernorStatus {
            enabled: settings.enabled,
            bottleneck: cause.map(str::to_string),
            reason,
            steps: self.steps,
            preview_fps: (limits.preview > 0).then_some(limits.preview),
            iso_percent: if held { 0 } else { limits.iso },
            vertical_percent: limits.vertical,
            stream_bitrate_percent: if sample.has_stream {
                (0..2)
                    .filter(|i| sample.stream_canvases & (1 << i) != 0)
                    .map(|i| STREAM_APPLIED[i].load(Ordering::Relaxed))
                    .min()
                    .unwrap_or(100)
            } else {
                100
            },
            native_iso_percent: sample.native_iso_percent,
            recovery_seconds: self
                .quiet_since
                .filter(|_| limits != Limits::full())
                .map(|since| {
                    (30.0 - now.duration_since(since).as_secs_f64())
                        .max(0.0)
                        .max(
                            self.restore_after
                                .saturating_duration_since(now)
                                .as_secs_f64(),
                        )
                }),
            limited_by_pins: pinned,
            error: rate_error,
        };
        if !sample.active || !settings.enabled {
            next.bottleneck = None;
        }
        let changed = next.steps != state.status.steps
            || next.reason != state.status.reason
            || next.enabled != state.status.enabled
            || next.bottleneck != state.status.bottleneck
            || next.stream_bitrate_percent != state.status.stream_bitrate_percent;
        let changed = changed || next.error != state.status.error;
        state.status = next.clone();
        self.recovering = settings.enabled
            && sample.active
            && (limits != Limits::full()
                || held
                || cause.is_some()
                || !HOLD_ARMED.load(Ordering::Relaxed)
                || self.last_restore.is_some());
        changed.then_some(next)
    }
}
