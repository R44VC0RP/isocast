//! Endpoint clock rate reference and pre-wrap session splice for Windows
//! process-loopback capture (`VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK`).
//!
//! Windows's virtual process-loopback device writes endpoint-clocked PCM into
//! an internal 200 ms (`9,600`-frame) circular buffer without SRC while reading
//! it on a 10.000000 ms QPC timer (`+100,000` QPC ticks per 480 frames).
//! 1. `EndpointClockMonitor` follows the target's active render endpoint (or
//!    the default render endpoint for `ClockTarget::DefaultRender`), samples a
//!    passive 200 ms shared loopback `IAudioClock` every 100 ms, and publishes
//!    the QPC-per-frame step atomically so `RateFollower` resamples to QPC.
//! 2. Before cumulative `|qpc_frames - endpoint_frames|` reaches 15 ms (`720`
//!    frames, well inside Windows's ~35-40 ms initial wrap margin), the worker
//!    opens a second process-loopback `Session` in the background. `SpliceBuffer`
//!    aligns the two streams to an exact sample boundary (or across verified
//!    silence) and retires the old session off-thread before its OS ring wraps.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result, bail};
use isocast_engine::{SAMPLE_RATE, Time};
use windows::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
    IAudioCaptureClient, IAudioClient, IAudioClock, IMMDeviceEnumerator, eRender,
};
use windows::Win32::System::Threading::WaitForMultipleObjects;

use super::application::descendant_pids;
use super::{ComAllocation, Session, State, default_id, enumerator, resolve};
use crate::audio_apps::{self, AppEndpoint};
use crate::native::{Apartment, Event};

/// 3,000,000 sub-nanosecond units per nanosecond makes 1 / 48,000 s an exact
/// integer (`62,500,000,000` units/frame), avoiding any truncation bias at 0 ppm.
pub(super) const UNIT_PER_NS: i128 = 3_000_000;
pub(super) const NOMINAL_FRAME_UNITS: u64 =
    (1_000_000_000 * UNIT_PER_NS as u64) / SAMPLE_RATE as u64;

/// Trigger a background session splice at 15 ms (`720` frames) of accumulated
/// endpoint-vs-QPC drift, well inside Windows's ~35-40 ms wrap margin.
pub(super) const SPLICE_THRESHOLD_FRAMES: f64 = (SAMPLE_RATE as f64) * 0.015;
/// If standby splices repeatedly fail and drift reaches 30 ms (`1,440` frames),
/// warn once and report an honest gap before the OS 200 ms ring can wrap.
pub(super) const UNREPORTED_WRAP_THRESHOLD_FRAMES: f64 = (SAMPLE_RATE as f64) * 0.030;
const SPLICE_RETRY_COOLDOWN_FRAMES: u64 = (SAMPLE_RATE as u64) * 5;

const OLD_HIST_FRAMES: usize = 12_000; // 250 ms stereo history
pub(super) const NEW_BUF_CAP_FRAMES: usize = 7_200; // 150 ms rolling standby window
const MIN_OVERLAP_FRAMES: usize = 3_840; // 80 ms standby collection before match
const SILENT_FALLBACK_FRAMES: usize = (SAMPLE_RATE as usize) * 3; // 3 s continuous silence before silent splice
const MAX_WAIT_FRAMES: usize = (SAMPLE_RATE as usize) * 4; // 4 s hard ceiling before fallback/failure
const MAX_LEAD_FRAMES: usize = 1_440; // 30 ms lead margin kept inside old_hist
const TEMPLATE_FRAMES: usize = 960; // 20 ms full verification window
const COARSE_FRAMES: usize = 128; // 2.67 ms high-contrast coarse scan window
const SILENCE_PEAK: f32 = 1e-5;
const MAX_NORM_SAD: f32 = 0.02;

const POLL_MS: u32 = 100;
const ENDPOINT_SCAN_TICKS: u32 = 5;
const MULTIPLE_ENDPOINT_STREAK: u32 = 3;
const PASSIVE_BUFFER_100NS: i64 = 2_000_000;
const HISTORY: usize = 64;
const MAX_RATE_PPM: f64 = 200e-6;
const MAX_STEP_ANOMALY_SEC: f64 = 0.002;
const MIN_SPAN_SEC: f64 = 0.4;

pub(super) struct MtaSession(pub(super) Session);

// SAFETY: process-loopback `Session`s have `_notifications: None` and hold only
// MTA-activated `IAudioClient` / `IAudioCaptureClient` interfaces plus an owned
// kernel `Event`. Both `clock_worker` and `application_worker` enter the process
// MTA via `Apartment::new()` (`CoIncrementMTAUsage` keeps the MTA alive).
unsafe impl Send for MtaSession {}

pub(super) type ProcessSessionOpener = Arc<dyn Fn() -> Result<MtaSession> + Send + Sync>;

/// Endpoint-selection mode for a synthetic-QPC process-loopback stream.
#[derive(Clone, Debug)]
pub(crate) enum ClockTarget {
    /// Follow the default `eConsole` render endpoint (for exclude-mode process
    /// loopback, where there is no single captured application).
    #[allow(dead_code)]
    DefaultRender,
    /// Follow the active render endpoint of a captured application's process
    /// tree, falling back to the default `eConsole` render endpoint when idle.
    App { root_pid: u32, identity: String },
}

#[derive(Clone, Copy, Default)]
struct ClockSample {
    pos: u64,
    qpc_100ns: u64,
}

/// Sliding linear-regression estimator for `(qpc_dt / audio_dt) - 1.0`.
pub(super) struct EndpointRateEstimator {
    clock_freq: u64,
    ring: [ClockSample; HISTORY],
    start: usize,
    len: usize,
    delta: f64,
}

impl EndpointRateEstimator {
    pub(super) fn new(clock_freq: u64) -> Self {
        Self {
            clock_freq,
            ring: [ClockSample::default(); HISTORY],
            start: 0,
            len: 0,
            delta: 0.0,
        }
    }

    #[cfg(test)]
    pub(super) fn delta(&self) -> f64 {
        self.delta
    }

    pub(super) fn frame_units(&self) -> u64 {
        ((NOMINAL_FRAME_UNITS as f64) * (1.0 + self.delta)).round() as u64
    }

    fn oldest(&self) -> ClockSample {
        self.ring[self.start]
    }

    fn newest(&self) -> ClockSample {
        self.ring[(self.start + self.len - 1) % HISTORY]
    }

    fn push_sample(&mut self, sample: ClockSample) {
        if self.len < HISTORY {
            self.ring[(self.start + self.len) % HISTORY] = sample;
            self.len += 1;
        } else {
            self.ring[self.start] = sample;
            self.start = (self.start + 1) % HISTORY;
        }
    }

    pub(super) fn observe(&mut self, pos: u64, qpc_100ns: u64) -> f64 {
        if self.clock_freq == 0 || qpc_100ns == 0 {
            return self.delta;
        }
        if self.len > 0 {
            let prev = self.newest();
            if qpc_100ns <= prev.qpc_100ns || pos < prev.pos {
                self.start = 0;
                self.len = 0;
            } else if pos == prev.pos {
                if qpc_100ns - prev.qpc_100ns > 20_000 {
                    self.start = 0;
                    self.len = 0;
                }
                return self.delta;
            } else {
                let dt_audio = (pos - prev.pos) as f64 / self.clock_freq as f64;
                let dt_qpc = (qpc_100ns - prev.qpc_100ns) as f64 * 1e-7;
                if (dt_qpc - dt_audio).abs() > MAX_STEP_ANOMALY_SEC || dt_qpc > 1.0 {
                    self.ring[0] = ClockSample { pos, qpc_100ns };
                    self.start = 0;
                    self.len = 1;
                    return self.delta;
                }
            }
        }
        self.push_sample(ClockSample { pos, qpc_100ns });
        if self.len >= 5 {
            let base = self.oldest();
            let span_audio = (pos - base.pos) as f64 / self.clock_freq as f64;
            if span_audio >= MIN_SPAN_SEC {
                let n = self.len as f64;
                let (mut sum_x, mut sum_y) = (0.0, 0.0);
                for i in 0..self.len {
                    let s = self.ring[(self.start + i) % HISTORY];
                    let x = (s.pos - base.pos) as f64 / self.clock_freq as f64;
                    let q = (s.qpc_100ns - base.qpc_100ns) as f64 * 1e-7;
                    sum_x += x;
                    sum_y += q - x;
                }
                let (mean_x, mean_y) = (sum_x / n, sum_y / n);
                let (mut sxx, mut sxy) = (0.0, 0.0);
                for i in 0..self.len {
                    let s = self.ring[(self.start + i) % HISTORY];
                    let x = (s.pos - base.pos) as f64 / self.clock_freq as f64;
                    let y = (s.qpc_100ns - base.qpc_100ns) as f64 * 1e-7 - x;
                    let dx = x - mean_x;
                    sxx += dx * dx;
                    sxy += dx * (y - mean_y);
                }
                if sxx > 1e-12 {
                    self.delta = (sxy / sxx).clamp(-MAX_RATE_PPM, MAX_RATE_PPM);
                }
            }
        }
        self.delta
    }
}

/// Allocation-free packet timestamp generator for the real-time packet thread.
pub(super) struct ProcessTimeline {
    units: Option<i128>,
    prev_frames: u32,
    drift_frames: f64,
    frames_since_attempt: u64,
    unspliced_warned: bool,
}

impl ProcessTimeline {
    pub(super) fn new() -> Self {
        Self {
            units: None,
            prev_frames: 0,
            drift_frames: 0.0,
            frames_since_attempt: SPLICE_RETRY_COOLDOWN_FRAMES,
            unspliced_warned: false,
        }
    }

    pub(super) fn packet_time(
        &mut self,
        raw: Time,
        frames: u32,
        gap: u64,
        frame_units: u64,
    ) -> Time {
        let current = match self.units {
            None => {
                let start = i128::from(raw.as_nanos()) * UNIT_PER_NS;
                self.units = Some(start);
                start
            }
            Some(previous) => {
                let advance = u64::from(self.prev_frames).saturating_add(gap);
                let next = previous + i128::from(advance) * i128::from(frame_units);
                self.units = Some(next);
                next
            }
        };
        self.prev_frames = frames;
        let ratio_minus_one = (frame_units as f64 / NOMINAL_FRAME_UNITS as f64) - 1.0;
        self.drift_frames += f64::from(frames) * ratio_minus_one;
        self.frames_since_attempt = self.frames_since_attempt.saturating_add(u64::from(frames));
        Time::from_nanos((current / UNIT_PER_NS) as i64)
    }

    pub(super) fn drift_frames(&self) -> f64 {
        self.drift_frames
    }

    pub(super) fn should_request_splice(&self) -> bool {
        self.drift_frames.abs() >= SPLICE_THRESHOLD_FRAMES
            && self.frames_since_attempt >= SPLICE_RETRY_COOLDOWN_FRAMES
    }

    pub(super) fn check_unspliced_drift_limit(&mut self) -> bool {
        if !self.unspliced_warned && self.drift_frames.abs() >= UNREPORTED_WRAP_THRESHOLD_FRAMES {
            self.unspliced_warned = true;
            true
        } else {
            false
        }
    }

    pub(super) fn mark_attempt(&mut self) {
        self.frames_since_attempt = 0;
    }

    pub(super) fn reset_drift(&mut self) {
        self.drift_frames = 0.0;
        self.frames_since_attempt = 0;
        self.unspliced_warned = false;
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum SpliceDecision {
    NeedMoreFrames,
    Matched {
        next_new_frame: usize,
        lag_frames: i64,
        silent: bool,
    },
    Failed {
        best_norm_sad: f32,
    },
}

/// Align `new_stereo` (`M_n` interleaved stereo frames from the standby session)
/// against `old_stereo` (`M_o` interleaved stereo frames ending at the last
/// frame emitted by the active session). `expected_drift_frames` is
/// `round(qpc_frames - endpoint_frames)` accumulated on the active session.
pub(super) fn align_splice(
    old_stereo: &[f32],
    new_stereo: &[f32],
    expected_drift_frames: i64,
    allow_silent: bool,
    at_limit: bool,
) -> SpliceDecision {
    let m_o = old_stereo.len() / 2;
    let m_n = new_stereo.len() / 2;
    if m_n < MIN_OVERLAP_FRAMES || m_o < m_n + TEMPLATE_FRAMES {
        return if at_limit {
            SpliceDecision::Failed { best_norm_sad: 1.0 }
        } else {
            SpliceDecision::NeedMoreFrames
        };
    }
    let k_exp = (m_o as i64) - (m_n as i64) - expected_drift_frames;
    let max_new = new_stereo.iter().fold(0.0_f32, |a, &s| a.max(s.abs()));
    let tail_start = m_o.saturating_sub(m_n + MAX_LEAD_FRAMES);
    let max_old_tail = old_stereo[tail_start * 2..]
        .iter()
        .fold(0.0_f32, |a, &s| a.max(s.abs()));

    // Both streams are silent across the overlap window: wait up to 3 s for an
    // audible feature so intermittent speech/beats/chirps lock with 0-sample
    // error instead of +/-5 ms Windows session-startup quantum jitter.
    if max_new <= SILENCE_PEAK && max_old_tail <= SILENCE_PEAK {
        if !allow_silent && !at_limit {
            return SpliceDecision::NeedMoreFrames;
        }
        let k = k_exp.clamp(0, (m_o - 1) as i64) as usize;
        let next_new_frame = m_o - k;
        return SpliceDecision::Matched {
            next_new_frame,
            lag_frames: (next_new_frame as i64) - (m_n as i64),
            silent: true,
        };
    }

    // Choose the 20 ms template inside the portion of `new_stereo` guaranteed to
    // have already arrived in `old_stereo` even if `new` leads `old` by up to 30 ms.
    let usable_new = m_n.saturating_sub(MAX_LEAD_FRAMES);
    if usable_new < TEMPLATE_FRAMES {
        return SpliceDecision::NeedMoreFrames;
    }
    let mut best_t_off = 0;
    let mut best_l1 = 0.0_f32;
    let step = 64;
    let mut off = 0;
    while off + TEMPLATE_FRAMES <= usable_new {
        let l1: f32 = new_stereo[off * 2..(off + TEMPLATE_FRAMES) * 2]
            .iter()
            .map(|s| s.abs())
            .sum();
        if l1 > best_l1 {
            best_l1 = l1;
            best_t_off = off;
        }
        if off + TEMPLATE_FRAMES == usable_new {
            break;
        }
        off = (off + step).min(usable_new - TEMPLATE_FRAMES);
    }

    let min_template_l1 = (TEMPLATE_FRAMES * 2) as f32 * SILENCE_PEAK;
    if best_l1 <= min_template_l1 {
        // Audio is just entering the boundary tail; wait a packet or two for it
        // to enter the usable template region.
        if !at_limit {
            return SpliceDecision::NeedMoreFrames;
        }
        if max_new <= SILENCE_PEAK && max_old_tail <= SILENCE_PEAK {
            let k = k_exp.clamp(0, (m_o - 1) as i64) as usize;
            let next_new_frame = m_o - k;
            return SpliceDecision::Matched {
                next_new_frame,
                lag_frames: (next_new_frame as i64) - (m_n as i64),
                silent: true,
            };
        }
        return SpliceDecision::Failed { best_norm_sad: 1.0 };
    }

    let template = &new_stereo[best_t_off * 2..(best_t_off + TEMPLATE_FRAMES) * 2];
    // Pick the sharpest 128-frame sub-block inside `template` for Stage 1 scan.
    let mut coarse_off = 0;
    let mut best_diff = -1.0_f32;
    let mut c_off = 0;
    while c_off + COARSE_FRAMES <= TEMPLATE_FRAMES {
        let mut diff = 0.0_f32;
        let sub = &template[c_off * 2..(c_off + COARSE_FRAMES) * 2];
        for i in 2..sub.len() {
            diff += (sub[i] - sub[i - 2]).abs();
        }
        if diff > best_diff {
            best_diff = diff;
            coarse_off = c_off;
        }
        c_off += 32;
    }
    let coarse = &template[coarse_off * 2..(coarse_off + COARSE_FRAMES) * 2];
    let max_k = m_o - TEMPLATE_FRAMES;

    // Stage 1: exact-integer scan with the 128-frame sub-block, then Stage 2
    // full 960-frame verification on every candidate near the Stage 1 floor.
    let mut coarse_min = f32::MAX;
    let mut best_k = 0;
    // First pass finds coarse_min without allocating.
    for k in 0..=max_k {
        let slice = &old_stereo[(k + coarse_off) * 2..(k + coarse_off + COARSE_FRAMES) * 2];
        let mut sad = 0.0_f32;
        for i in 0..coarse.len() {
            sad += (slice[i] - coarse[i]).abs();
            if sad > coarse_min * 4.0 + 1e-4 {
                break;
            }
        }
        if sad < coarse_min {
            coarse_min = sad;
            best_k = k;
        }
    }

    let coarse_gate = (coarse_min * 4.0)
        .max(best_l1 * (COARSE_FRAMES as f32 / TEMPLATE_FRAMES as f32) * MAX_NORM_SAD);
    let k_target = k_exp + best_t_off as i64;
    let mut full_min = f32::MAX;
    let mut best_overlap: Option<f32> = None;
    let tie_tol = best_l1 * 1e-5 + 1e-7;
    for k in 0..=max_k {
        let slice_c = &old_stereo[(k + coarse_off) * 2..(k + coarse_off + COARSE_FRAMES) * 2];
        let mut c_sad = 0.0_f32;
        for i in 0..coarse.len() {
            c_sad += (slice_c[i] - coarse[i]).abs();
            if c_sad > coarse_gate {
                break;
            }
        }
        if c_sad > coarse_gate {
            continue;
        }
        let slice_f = &old_stereo[k * 2..(k + TEMPLATE_FRAMES) * 2];
        let mut sad = 0.0_f32;
        let cutoff = full_min + tie_tol;
        for i in 0..template.len() {
            sad += (slice_f[i] - template[i]).abs();
            if sad > cutoff {
                break;
            }
        }
        if sad + tie_tol < full_min {
            full_min = sad;
            best_k = k;
            best_overlap = None;
        } else if (sad - full_min).abs() <= tie_tol {
            let cur_ov = *best_overlap.get_or_insert_with(|| {
                overlap_mean_sad(
                    old_stereo,
                    new_stereo,
                    best_k as isize - best_t_off as isize,
                )
            });
            let cand_ov =
                overlap_mean_sad(old_stereo, new_stereo, k as isize - best_t_off as isize);
            let ov_tol = 1e-6;
            if cand_ov + ov_tol < cur_ov
                || ((cand_ov - cur_ov).abs() <= ov_tol
                    && (k as i64 - k_target).abs() < (best_k as i64 - k_target).abs())
            {
                full_min = full_min.min(sad);
                best_k = k;
                best_overlap = Some(cand_ov);
            }
        }
    }

    let norm_sad = full_min / best_l1;
    if norm_sad > MAX_NORM_SAD {
        return SpliceDecision::Failed {
            best_norm_sad: norm_sad,
        };
    }
    let next_new_frame = m_o + best_t_off - best_k;
    SpliceDecision::Matched {
        next_new_frame,
        lag_frames: (next_new_frame as i64) - (m_n as i64),
        silent: false,
    }
}

/// Mean per-sample absolute error across the entire shared overlap of `old_stereo`
/// and `new_stereo` for a periodic tie candidate (`frame j` of `new` aligned to
/// `frame j + shift` of `old`), preferring candidates whose energy onset/offset
/// matches outside the 20 ms steady-state template.
fn overlap_mean_sad(old_stereo: &[f32], new_stereo: &[f32], shift: isize) -> f32 {
    let m_o = (old_stereo.len() / 2) as isize;
    let m_n = (new_stereo.len() / 2) as isize;
    let new_begin = (-shift).max(0);
    let new_end = (m_o - shift).min(m_n);
    if new_end <= new_begin {
        return f32::MAX;
    }
    let count = (new_end - new_begin) as usize;
    let old_begin = (new_begin + shift) as usize;
    let n_slice = &new_stereo[new_begin as usize * 2..new_end as usize * 2];
    let o_slice = &old_stereo[old_begin * 2..(old_begin + count) * 2];
    let mut sad = 0.0_f32;
    for i in 0..n_slice.len() {
        sad += (o_slice[i] - n_slice[i]).abs();
    }
    sad / (count * 2) as f32
}

/// Pre-allocated rolling overlap buffers for zero-allocation packet-thread splicing.
pub(super) struct SpliceBuffer {
    old_ring: Vec<f32>,
    old_head: usize,
    old_len: usize,
    old_flat: Vec<f32>,
    new_ring: Vec<f32>,
    new_head: usize,
    new_len: usize,
    new_flat: Vec<f32>,
    standby_total_frames: usize,
    skip_frames: usize,
}

impl SpliceBuffer {
    pub(super) fn new() -> Self {
        Self {
            old_ring: vec![0.0; OLD_HIST_FRAMES * 2],
            old_head: 0,
            old_len: 0,
            old_flat: vec![0.0; OLD_HIST_FRAMES * 2],
            new_ring: vec![0.0; NEW_BUF_CAP_FRAMES * 2],
            new_head: 0,
            new_len: 0,
            new_flat: vec![0.0; NEW_BUF_CAP_FRAMES * 2],
            standby_total_frames: 0,
            skip_frames: 0,
        }
    }

    /// Trim any duplicate post-splice frames from an `active` packet and record
    /// the surviving stereo samples in the rolling history ring.
    pub(super) fn accept_active<'a>(&mut self, stereo: &'a [f32]) -> &'a [f32] {
        let frames = stereo.len() / 2;
        if self.skip_frames > 0 {
            let drop_n = self.skip_frames.min(frames);
            self.skip_frames -= drop_n;
            let rem = &stereo[drop_n * 2..];
            self.push_history(rem);
            return rem;
        }
        self.push_history(stereo);
        stereo
    }

    fn push_history(&mut self, stereo: &[f32]) {
        for &[left, right] in stereo.as_chunks::<2>().0 {
            self.old_ring[self.old_head * 2] = left;
            self.old_ring[self.old_head * 2 + 1] = right;
            self.old_head = (self.old_head + 1) % OLD_HIST_FRAMES;
            if self.old_len < OLD_HIST_FRAMES {
                self.old_len += 1;
            }
        }
    }

    pub(super) fn reset_standby(&mut self) {
        self.new_head = 0;
        self.new_len = 0;
        self.standby_total_frames = 0;
    }

    pub(super) fn push_standby(&mut self, stereo: &[f32]) -> bool {
        let frames = stereo.len() / 2;
        if self.standby_total_frames + frames > MAX_WAIT_FRAMES + NEW_BUF_CAP_FRAMES {
            self.reset_standby();
            return false;
        }
        for &[left, right] in stereo.as_chunks::<2>().0 {
            self.new_ring[self.new_head * 2] = left;
            self.new_ring[self.new_head * 2 + 1] = right;
            self.new_head = (self.new_head + 1) % NEW_BUF_CAP_FRAMES;
            if self.new_len < NEW_BUF_CAP_FRAMES {
                self.new_len += 1;
            }
        }
        self.standby_total_frames += frames;
        true
    }

    pub(super) fn try_align(&mut self, expected_drift_frames: i64) -> SpliceDecision {
        let m_n = self.new_len;
        if m_n < MIN_OVERLAP_FRAMES {
            return SpliceDecision::NeedMoreFrames;
        }
        let m_o = self.old_len;
        let o_start = (self.old_head + OLD_HIST_FRAMES - m_o) % OLD_HIST_FRAMES;
        let o_first = (OLD_HIST_FRAMES - o_start).min(m_o);
        self.old_flat[..o_first * 2]
            .copy_from_slice(&self.old_ring[o_start * 2..(o_start + o_first) * 2]);
        if o_first < m_o {
            self.old_flat[o_first * 2..m_o * 2]
                .copy_from_slice(&self.old_ring[..(m_o - o_first) * 2]);
        }
        let n_start = (self.new_head + NEW_BUF_CAP_FRAMES - m_n) % NEW_BUF_CAP_FRAMES;
        let n_first = (NEW_BUF_CAP_FRAMES - n_start).min(m_n);
        self.new_flat[..n_first * 2]
            .copy_from_slice(&self.new_ring[n_start * 2..(n_start + n_first) * 2]);
        if n_first < m_n {
            self.new_flat[n_first * 2..m_n * 2]
                .copy_from_slice(&self.new_ring[..(m_n - n_first) * 2]);
        }
        align_splice(
            &self.old_flat[..m_o * 2],
            &self.new_flat[..m_n * 2],
            expected_drift_frames,
            self.standby_total_frames >= SILENT_FALLBACK_FRAMES,
            self.standby_total_frames >= MAX_WAIT_FRAMES,
        )
    }

    /// Commit a matched splice: populates `extra_out` with any un-emitted frames
    /// when `standby` leads `active`, or sets `skip_frames` when `standby` lags,
    /// and updates the rolling history accordingly.
    pub(super) fn commit_matched(&mut self, next_new_frame: usize, extra_out: &mut Vec<f32>) {
        extra_out.clear();
        let m_n = self.new_len;
        if next_new_frame < m_n {
            extra_out.extend_from_slice(&self.new_flat[next_new_frame * 2..m_n * 2]);
            self.push_history(extra_out);
            self.skip_frames = 0;
        } else {
            self.skip_frames = next_new_frame - m_n;
        }
        self.reset_standby();
    }
}

pub(super) enum StandbyFailure {
    BufferOverflow,
    SessionEnded(String),
}

pub(super) enum WorkerCommand {
    OpenStandby,
    DropSession {
        session: MtaSession,
        reason: StandbyFailure,
    },
    Retire {
        session: MtaSession,
        telemetry: SpliceTelemetry,
    },
    WarnUnsplicedDrift {
        drift_frames: f64,
        ppm: f64,
    },
}

pub(super) enum SpliceTelemetry {
    Matched {
        number: u64,
        silent: bool,
        lag_frames: i64,
        drift_before: f64,
        ppm: f64,
    },
    Fallback {
        number: u64,
        best_norm_sad: f32,
        drift_before: f64,
    },
}

struct PassiveEndpointClock {
    id: String,
    client: IAudioClient,
    capture: IAudioCaptureClient,
    estimator: EndpointRateEstimator,
}

impl PassiveEndpointClock {
    fn open(enumerator: &IMMDeviceEnumerator, id: &str) -> Result<Self> {
        let identity = resolve(enumerator, eRender, id)?;
        let client: IAudioClient = unsafe {
            identity
                .raw
                .Activate(windows::Win32::System::Com::CLSCTX_ALL, None)
        }
        .with_context(|| format!("activate passive clock on {:?}", identity.name))?;
        let mix = ComAllocation(
            unsafe { client.GetMixFormat() }
                .with_context(|| format!("query mix format on {:?}", identity.name))?,
        );
        if mix.0.is_null() {
            bail!("null mix format on {:?}", identity.name);
        }
        let sample_rate = unsafe { mix.0.read_unaligned() }.nSamplesPerSec;
        if sample_rate == 0 {
            bail!("zero mix sample rate on {:?}", identity.name);
        }
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                PASSIVE_BUFFER_100NS,
                0,
                mix.0,
                None,
            )
        }
        .with_context(|| format!("initialize passive loopback clock on {:?}", identity.name))?;
        let capture: IAudioCaptureClient = unsafe { client.GetService() }?;
        let _clock: IAudioClock = unsafe { client.GetService() }?;
        unsafe { client.Start() }?;
        isocast_diagnostics::diagnostic!("info";
            "Windows application audio: following render endpoint {:?} ({})",
            identity.name, identity.id
        );
        Ok(Self {
            id: identity.id,
            client,
            capture,
            estimator: EndpointRateEstimator::new(u64::from(sample_rate)),
        })
    }

    fn poll(&mut self) -> Result<u64> {
        // Read the endpoint's exact hardware DMA (position, QPC) pairs from
        // loopback packets (~12 us jitter, unlike IAudioClock::GetPosition's
        // 10 ms quantum quantization on loopback clients).
        let mut latest: Option<(u64, u64)> = None;
        for _ in 0..64 {
            let next = unsafe { self.capture.GetNextPacketSize() }?;
            if next == 0 {
                break;
            }
            let mut data = std::ptr::null_mut();
            let (mut frames, mut flags, mut pos, mut qpc) = (0u32, 0u32, 0u64, 0u64);
            unsafe {
                self.capture.GetBuffer(
                    &mut data,
                    &mut frames,
                    &mut flags,
                    Some(&mut pos),
                    Some(&mut qpc),
                )
            }?;
            unsafe { self.capture.ReleaseBuffer(frames) }?;
            if flags & (AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32) == 0 && qpc != 0 {
                latest = Some((pos, qpc));
            }
        }
        if let Some((pos, qpc_100ns)) = latest {
            self.estimator.observe(pos, qpc_100ns);
        }
        Ok(self.estimator.frame_units())
    }
}

impl Drop for PassiveEndpointClock {
    fn drop(&mut self) {
        let _ = unsafe { self.client.Stop() };
    }
}

pub(super) struct SharedProcessClock {
    frame_units: AtomicU64,
    standby_in_flight: AtomicBool,
    cmd_tx: mpsc::SyncSender<WorkerCommand>,
    cmd_wake: Arc<Event>,
    standby_rx: Mutex<mpsc::Receiver<Option<MtaSession>>>,
    standby_ready: AtomicBool,
}

impl SharedProcessClock {
    pub(super) fn frame_units(&self) -> u64 {
        self.frame_units.load(Ordering::Relaxed)
    }

    pub(super) fn ppm(&self) -> f64 {
        ((self.frame_units() as f64 / NOMINAL_FRAME_UNITS as f64) - 1.0) * 1e6
    }

    pub(super) fn request_standby(&self) -> bool {
        if self
            .standby_in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }
        if self.cmd_tx.try_send(WorkerCommand::OpenStandby).is_ok() {
            self.cmd_wake.signal();
            true
        } else {
            self.standby_in_flight.store(false, Ordering::Release);
            false
        }
    }

    pub(super) fn poll_standby(&self) -> Option<Option<MtaSession>> {
        if !self.standby_ready.swap(false, Ordering::AcqRel) {
            return None;
        }
        let got = self.standby_rx.lock().unwrap().try_recv().ok();
        if got.is_some() {
            self.standby_in_flight.store(false, Ordering::Release);
        }
        got
    }

    pub(super) fn drop_session(&self, session: Session, reason: StandbyFailure) {
        if self
            .cmd_tx
            .try_send(WorkerCommand::DropSession {
                session: MtaSession(session),
                reason,
            })
            .is_ok()
        {
            self.cmd_wake.signal();
        }
    }

    pub(super) fn retire_session(&self, session: Session, telemetry: SpliceTelemetry) {
        if self
            .cmd_tx
            .try_send(WorkerCommand::Retire {
                session: MtaSession(session),
                telemetry,
            })
            .is_ok()
        {
            self.cmd_wake.signal();
        }
    }

    pub(super) fn warn_unspliced_drift(&self, drift_frames: f64, ppm: f64) {
        if self
            .cmd_tx
            .try_send(WorkerCommand::WarnUnsplicedDrift { drift_frames, ppm })
            .is_ok()
        {
            self.cmd_wake.signal();
        }
    }
}

pub(super) struct ProcessClock {
    shared: Arc<SharedProcessClock>,
    stop: Arc<Event>,
    worker: Option<JoinHandle<()>>,
}

impl ProcessClock {
    pub(super) fn start(
        target: ClockTarget,
        opener: Option<ProcessSessionOpener>,
        state: Arc<State>,
        strict_start: bool,
    ) -> Result<Self> {
        let stop = Arc::new(Event::new()?);
        let cmd_wake = Arc::new(Event::new()?);
        let (cmd_tx, cmd_rx) = mpsc::sync_channel(16);
        let (standby_tx, standby_rx) = mpsc::sync_channel(1);
        let shared = Arc::new(SharedProcessClock {
            frame_units: AtomicU64::new(NOMINAL_FRAME_UNITS),
            standby_in_flight: AtomicBool::new(false),
            cmd_tx,
            cmd_wake: cmd_wake.clone(),
            standby_rx: Mutex::new(standby_rx),
            standby_ready: AtomicBool::new(false),
        });
        let (tx, rx) = mpsc::sync_channel(1);
        let worker_shared = shared.clone();
        let worker_stop = stop.clone();
        let worker = thread::Builder::new()
            .name("Windows app-audio clock".into())
            .spawn(move || {
                let _ = clock_worker(
                    target,
                    opener,
                    worker_shared,
                    worker_stop,
                    cmd_wake,
                    cmd_rx,
                    standby_tx,
                    state,
                    strict_start,
                    tx,
                );
            })?;
        match rx.recv() {
            Ok(Ok(())) => Ok(Self {
                shared,
                stop,
                worker: Some(worker),
            }),
            Ok(Err(err)) => {
                stop.signal();
                let _ = worker.join();
                Err(err)
            }
            Err(_) => {
                stop.signal();
                let _ = worker.join();
                bail!("Windows application-audio clock worker exited during startup")
            }
        }
    }

    pub(super) fn shared(&self) -> &Arc<SharedProcessClock> {
        &self.shared
    }
}

impl Drop for ProcessClock {
    fn drop(&mut self) {
        self.stop.signal();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DesiredEndpoint {
    Single(Option<String>),
    Multiple {
        message: String,
        default_id: Option<String>,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct EndpointDecision {
    switch_to: Option<Option<String>>,
    degraded_multiple: Option<String>,
}

/// Endpoint selection state machine: refuses dual-output playback only at
/// initial start (`strict_start`), and degrades mid-take (or across app
/// restarts) after `MULTIPLE_ENDPOINT_STREAK` scans by following the default
/// render endpoint, counting the fallback, and warning once per source.
struct EndpointTracker {
    multiple_streak: u32,
    in_dual_output: bool,
}

impl EndpointTracker {
    fn new() -> Self {
        Self {
            multiple_streak: 0,
            in_dual_output: false,
        }
    }

    fn startup(
        &mut self,
        strict_start: bool,
        initial: DesiredEndpoint,
    ) -> Result<EndpointDecision> {
        match initial {
            DesiredEndpoint::Single(id) => Ok(EndpointDecision {
                switch_to: Some(id),
                degraded_multiple: None,
            }),
            DesiredEndpoint::Multiple {
                message,
                default_id,
            } => {
                if strict_start {
                    bail!("{message}");
                }
                self.multiple_streak = MULTIPLE_ENDPOINT_STREAK;
                self.in_dual_output = true;
                Ok(EndpointDecision {
                    switch_to: Some(default_id),
                    degraded_multiple: Some(message),
                })
            }
        }
    }

    fn observe(&mut self, scan: DesiredEndpoint) -> EndpointDecision {
        match scan {
            DesiredEndpoint::Single(id) => {
                self.multiple_streak = 0;
                self.in_dual_output = false;
                EndpointDecision {
                    switch_to: Some(id),
                    degraded_multiple: None,
                }
            }
            DesiredEndpoint::Multiple {
                message,
                default_id,
            } => {
                self.multiple_streak = self.multiple_streak.saturating_add(1);
                if self.multiple_streak < MULTIPLE_ENDPOINT_STREAK {
                    return EndpointDecision {
                        switch_to: None,
                        degraded_multiple: None,
                    };
                }
                let entered = !self.in_dual_output;
                self.in_dual_output = true;
                EndpointDecision {
                    switch_to: Some(default_id),
                    degraded_multiple: entered.then_some(message),
                }
            }
        }
    }
}

fn query_desired_endpoint(
    enumerator: &IMMDeviceEnumerator,
    target: &ClockTarget,
) -> Result<DesiredEndpoint> {
    match target {
        ClockTarget::DefaultRender => Ok(DesiredEndpoint::Single(default_id(enumerator, eRender)?)),
        ClockTarget::App { root_pid, identity } => {
            let mut tree = descendant_pids(*root_pid);
            if !tree.contains(root_pid) {
                tree.push(*root_pid);
            }
            match audio_apps::app_endpoint(identity, &tree)? {
                AppEndpoint::Default => {
                    Ok(DesiredEndpoint::Single(default_id(enumerator, eRender)?))
                }
                AppEndpoint::Single { id, .. } => Ok(DesiredEndpoint::Single(Some(id))),
                AppEndpoint::Multiple(endpoints) => Ok(DesiredEndpoint::Multiple {
                    message: audio_apps::format_multiple_endpoints(identity, &endpoints),
                    default_id: default_id(enumerator, eRender).ok().flatten(),
                }),
            }
        }
    }
}

fn apply_endpoint_decision(
    enumerator: &IMMDeviceEnumerator,
    passive: &mut Option<PassiveEndpointClock>,
    shared: &SharedProcessClock,
    state: &State,
    clock_error_warned: &mut bool,
    decision: EndpointDecision,
) {
    if let Some(message) = decision.degraded_multiple {
        state.splice_fallbacks.fetch_add(1, Ordering::Relaxed);
        if !state.dual_output_warned.swap(true, Ordering::Relaxed) {
            isocast_diagnostics::diagnostic!("warn";
                "Windows application audio: {message}; degrading to default render endpoint clock while multiple outputs are active"
            );
        }
    }
    let Some(desired) = decision.switch_to else {
        return;
    };
    if passive.as_ref().map(|p| p.id.as_str()) == desired.as_deref() {
        return;
    }
    *passive = None;
    match desired {
        Some(id) => match PassiveEndpointClock::open(enumerator, &id) {
            Ok(clock) => {
                *clock_error_warned = false;
                *passive = Some(clock);
            }
            Err(err) => {
                state.clock_failures.fetch_add(1, Ordering::Relaxed);
                if !*clock_error_warned {
                    *clock_error_warned = true;
                    isocast_diagnostics::diagnostic!("warn";
                        "Windows application audio: passive render endpoint clock open failed: {err:#}"
                    );
                }
                shared
                    .frame_units
                    .store(NOMINAL_FRAME_UNITS, Ordering::Relaxed);
            }
        },
        None => {
            shared
                .frame_units
                .store(NOMINAL_FRAME_UNITS, Ordering::Relaxed);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn clock_worker(
    target: ClockTarget,
    opener: Option<ProcessSessionOpener>,
    shared: Arc<SharedProcessClock>,
    stop: Arc<Event>,
    cmd_wake: Arc<Event>,
    cmd_rx: mpsc::Receiver<WorkerCommand>,
    standby_tx: mpsc::SyncSender<Option<MtaSession>>,
    state: Arc<State>,
    strict_start: bool,
    ready: mpsc::SyncSender<Result<()>>,
) -> Result<()> {
    let _apartment = match Apartment::new() {
        Ok(apt) => apt,
        Err(err) => {
            let _ = ready.send(Err(err));
            return Ok(());
        }
    };
    let enumerator = match enumerator() {
        Ok(e) => e,
        Err(err) => {
            let _ = ready.send(Err(err));
            return Ok(());
        }
    };
    let mut passive: Option<PassiveEndpointClock> = None;
    let mut tracker = EndpointTracker::new();
    let mut clock_error_warned = false;
    match query_desired_endpoint(&enumerator, &target) {
        Ok(initial) => match tracker.startup(strict_start, initial) {
            Ok(decision) => apply_endpoint_decision(
                &enumerator,
                &mut passive,
                &shared,
                &state,
                &mut clock_error_warned,
                decision,
            ),
            Err(err) => {
                let _ = ready.send(Err(err));
                return Ok(());
            }
        },
        Err(err) => {
            state.clock_failures.fetch_add(1, Ordering::Relaxed);
            clock_error_warned = true;
            isocast_diagnostics::diagnostic!("warn";
                "Windows application audio: could not query render endpoint clock at startup: {err:#}"
            );
        }
    }
    if ready.send(Ok(())).is_err() {
        return Ok(());
    }

    let handles = [stop.handle(), cmd_wake.handle()];
    let mut tick: u32 = 0;
    loop {
        if state.stopping.load(Ordering::Acquire) {
            return Ok(());
        }
        let wait = unsafe { WaitForMultipleObjects(&handles, false, POLL_MS) };
        if wait == WAIT_OBJECT_0 {
            return Ok(());
        }
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                WorkerCommand::OpenStandby => {
                    let opened = opener.as_ref().and_then(|open| match open() {
                        Ok(session) => Some(session),
                        Err(err) => {
                            state.standby_failures.fetch_add(1, Ordering::Relaxed);
                            isocast_diagnostics::diagnostic!("warn";
                                "Windows application audio: standby session open failed: {err:#}"
                            );
                            None
                        }
                    });
                    if standby_tx.send(opened).is_ok() {
                        shared.standby_ready.store(true, Ordering::Release);
                        state.data.signal();
                    }
                }
                WorkerCommand::DropSession { session, reason } => {
                    drop(session);
                    match reason {
                        StandbyFailure::BufferOverflow => {
                            isocast_diagnostics::diagnostic!("warn";
                                "Windows application audio: standby session overflowed alignment window; will retry before OS ring wrap"
                            );
                        }
                        StandbyFailure::SessionEnded(err) => {
                            isocast_diagnostics::diagnostic!("warn";
                                "Windows application audio: standby session ended before splice ({err}); will retry before OS ring wrap"
                            );
                        }
                    }
                }
                WorkerCommand::Retire { session, telemetry } => {
                    drop(session);
                    match telemetry {
                        SpliceTelemetry::Matched {
                            number,
                            silent,
                            lag_frames,
                            drift_before,
                            ppm,
                        } => {
                            isocast_diagnostics::diagnostic!("info";
                                "Windows application audio: spliced process-loopback session #{number} (mode={}, lag_frames={lag_frames}, drift_frames_before={drift_before:.1}, endpoint_ppm={ppm:.2})",
                                if silent { "silent" } else { "sample-match" }
                            );
                        }
                        SpliceTelemetry::Fallback {
                            number,
                            best_norm_sad,
                            drift_before,
                        } => {
                            isocast_diagnostics::diagnostic!("warn";
                                "Windows application audio: process-loopback splice #{number} fell back to reported discontinuity (best_norm_sad={best_norm_sad:.4}, drift_frames_before={drift_before:.1})"
                            );
                        }
                    }
                }
                WorkerCommand::WarnUnsplicedDrift { drift_frames, ppm } => {
                    let drift_ms = drift_frames * 1000.0 / f64::from(SAMPLE_RATE);
                    isocast_diagnostics::diagnostic!("warn";
                        "Windows application audio: process-loopback drift reached {drift_ms:.1} ms ({drift_frames:.1} frames at {ppm:.2} ppm) without a splice; reported gap before OS ring wrap"
                    );
                }
            }
        }
        if wait != WAIT_TIMEOUT {
            continue;
        }
        tick = tick.wrapping_add(1);
        if tick.is_multiple_of(ENDPOINT_SCAN_TICKS) {
            match query_desired_endpoint(&enumerator, &target) {
                Ok(scan) => {
                    let decision = tracker.observe(scan);
                    apply_endpoint_decision(
                        &enumerator,
                        &mut passive,
                        &shared,
                        &state,
                        &mut clock_error_warned,
                        decision,
                    );
                }
                Err(err) => {
                    state.clock_failures.fetch_add(1, Ordering::Relaxed);
                    if !clock_error_warned {
                        clock_error_warned = true;
                        isocast_diagnostics::diagnostic!("warn";
                            "Windows application audio: render endpoint query failed: {err:#}"
                        );
                    }
                }
            }
        }
        if let Some(clock) = &mut passive {
            match clock.poll() {
                Ok(units) => shared.frame_units.store(units, Ordering::Relaxed),
                Err(err) => {
                    passive = None;
                    shared
                        .frame_units
                        .store(NOMINAL_FRAME_UNITS, Ordering::Relaxed);
                    state.clock_failures.fetch_add(1, Ordering::Relaxed);
                    if !clock_error_warned {
                        clock_error_warned = true;
                        isocast_diagnostics::diagnostic!("warn";
                            "Windows application audio: passive render endpoint clock read failed: {err:#}"
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_chirp_stream(frames: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity(frames * 2);
        for n in 0..frames {
            // Deterministic broadband pseudo-random + chirp mixture so every sample
            // index has a distinct signature, like real music/speech/chirps.
            let t = n as f64 / 48_000.0;
            let chirp = (2.0 * std::f64::consts::PI * (350.0 * t + 900.0 * t * t)).sin();
            let mod_wave = (2.0 * std::f64::consts::PI * 73.0 * t).cos() * 0.4;
            let l = (0.05 * (chirp + mod_wave)) as f32;
            let r = (0.05 * (chirp - 0.5 * mod_wave)) as f32;
            out.push(l);
            out.push(r);
        }
        out
    }

    #[test]
    fn nominal_frame_units_are_exact_at_zero_ppm() {
        let mut timeline = ProcessTimeline::new();
        let base = Time::from_nanos(1_000_000_000);
        for pkt in 0..60_000_u64 {
            let raw = Time::from_nanos(base.as_nanos() + (pkt * 10_000_000) as i64);
            let mapped = timeline.packet_time(raw, 480, 0, NOMINAL_FRAME_UNITS);
            assert_eq!(mapped, raw);
        }
        assert_eq!(timeline.drift_frames(), 0.0);
    }

    #[test]
    fn estimator_converges_on_positive_and_negative_drift_and_survives_idle_pauses() {
        let freq = 384_000_u64;
        for target_ppm in [-45.5_f64, 0.0, 42.76] {
            let mut est = EndpointRateEstimator::new(freq);
            let base_qpc = 10_000_000_000_u64;
            let mut pos = 100_000_u64;
            let mut qpc = base_qpc;
            for step in 1..=60_i64 {
                pos += freq / 10;
                let jitter_100ns = ((step * 19) % 7 - 3) * 50;
                let ideal_qpc =
                    base_qpc as f64 + (step as f64 * 0.1) * 1e7 * (1.0 + target_ppm * 1e-6);
                qpc = (ideal_qpc.round() as i64 + jitter_100ns) as u64;
                est.observe(pos, qpc);
            }
            assert!((est.delta() * 1e6 - target_ppm).abs() < 0.25);
            for _ in 0..25 {
                qpc += 1_000_000;
                est.observe(pos, qpc);
            }
            assert!((est.delta() * 1e6 - target_ppm).abs() < 0.25);
            let (resume_qpc, resume_pos) = (qpc, pos);
            for step in 1..=20_i64 {
                let p = resume_pos + (step as u64) * (freq / 10);
                let q = (resume_qpc as f64 + (step as f64 * 0.1) * 1e7 * (1.0 + target_ppm * 1e-6))
                    .round() as u64;
                est.observe(p, q);
            }
            assert!((est.delta() * 1e6 - target_ppm).abs() < 0.25);
        }
    }

    #[test]
    fn estimator_clamps_extreme_drift_to_rate_follower_bound() {
        let freq = 384_000_u64;
        let mut est = EndpointRateEstimator::new(freq);
        for step in 0..20_u64 {
            let pos = step * (freq / 10);
            let qpc = 1_000_000_000 + (step as f64 * 1_000_350.0).round() as u64;
            est.observe(pos, qpc);
        }
        assert_eq!(est.delta(), MAX_RATE_PPM);
    }

    #[test]
    fn process_timeline_plus_rate_follower_matches_drifting_endpoint_over_ten_minutes() {
        for ppm in [-45_i64, 43] {
            let mut est = EndpointRateEstimator::new(384_000);
            for step in 0..50_u64 {
                est.observe(
                    step * 38_400,
                    1_000_000_000 + (step as f64 * 1_000_000.0 * (1.0 + ppm as f64 * 1e-6)) as u64,
                );
            }
            let units = est.frame_units();
            let mut timeline = ProcessTimeline::new();
            let mut follower =
                crate::audio::rate::RateFollower::new(2, 480, crate::audio::MAX_PHASE_ERROR);
            let base = Time::from_nanos(1_000_000_000);
            let mut emitted_frames = 0_u64;
            for pkt in 0..60_000_u64 {
                let synthetic_raw = Time::from_nanos(base.as_nanos() + (pkt * 10_000_000) as i64);
                let mapped = timeline.packet_time(synthetic_raw, 480, 0, units);
                follower.push(&[0.0; 960], mapped, false, &mut |samples, _| {
                    emitted_frames += (samples.len() / 2) as u64;
                });
            }
            follower.finish(&mut |samples, _| {
                emitted_frames += (samples.len() / 2) as u64;
            });
            assert_eq!(follower.resets, 0);
            let expected = 28_800_000 + 28_800_000 * ppm / 1_000_000;
            assert!((emitted_frames as i64 - expected).abs() <= 3);
        }
    }

    #[test]
    fn splice_buffer_reconstructs_exact_stream_for_positive_and_negative_lags() {
        let total = 28_800;
        let master = synthetic_chirp_stream(total);
        // Test both positive lag (standby behind active, slow endpoint) and
        // negative lag (standby ahead of active, fast endpoint), including
        // non-packet-aligned sample offsets.
        for true_lag in [-813_isize, -480, 0, 480, 725, 1103] {
            let mut buf = SpliceBuffer::new();
            let mut reconstructed = Vec::with_capacity(total * 2);
            let mut extra = Vec::new();
            let splice_start_pkt = 22_usize; // 10,560 frames into `old`
            let mut active_cursor: usize = 0;
            let mut standby_cursor: Option<usize> = None;
            let mut spliced = false;

            while active_cursor + 480 <= total {
                if !spliced && active_cursor == splice_start_pkt * 480 {
                    standby_cursor = Some((active_cursor as isize - true_lag) as usize);
                }
                let pkt = &master[active_cursor * 2..(active_cursor + 480) * 2];
                active_cursor += 480;
                let survived = buf.accept_active(pkt);
                reconstructed.extend_from_slice(survived);

                if let Some(sc) = &mut standby_cursor {
                    if *sc + 480 > total {
                        break;
                    }
                    let s_pkt = &master[*sc * 2..(*sc + 480) * 2];
                    *sc += 480;
                    assert!(buf.push_standby(s_pkt));
                    // Deliberately perturb expected_drift by 180 frames to prove
                    // exact sample matching locks onto the true lag anyway.
                    match buf.try_align(true_lag as i64 + 180) {
                        SpliceDecision::NeedMoreFrames => {}
                        SpliceDecision::Matched {
                            next_new_frame,
                            lag_frames,
                            silent,
                        } => {
                            assert!(!silent);
                            assert_eq!(lag_frames, true_lag as i64);
                            buf.commit_matched(next_new_frame, &mut extra);
                            reconstructed.extend_from_slice(&extra);
                            active_cursor = *sc;
                            standby_cursor = None;
                            spliced = true;
                        }
                        SpliceDecision::Failed { best_norm_sad } => {
                            panic!("unexpected splice failure for lag {true_lag}: {best_norm_sad}");
                        }
                    }
                }
            }
            assert!(spliced, "did not splice for lag {true_lag}");
            let out_frames = reconstructed.len() / 2;
            assert!(out_frames > 20_000);
            assert_eq!(
                &reconstructed[..],
                &master[..out_frames * 2],
                "reconstructed PCM diverged at lag {true_lag}"
            );
        }
    }

    #[test]
    fn splice_aligns_periodic_sine_silent_windows_and_boundary_pulses_and_rejects_mismatch() {
        // 1. Pure silence waits while !allow_silent, then aligns cleanly once allow_silent is true.
        let silent_old = vec![0.0_f32; 10_000 * 2];
        let silent_new = vec![0.0_f32; 4_320 * 2];
        assert_eq!(
            align_splice(&silent_old, &silent_new, 720, false, false),
            SpliceDecision::NeedMoreFrames
        );
        assert_eq!(
            align_splice(&silent_old, &silent_new, 720, true, false),
            SpliceDecision::Matched {
                next_new_frame: 4_320 + 720,
                lag_frames: 720,
                silent: true,
            }
        );

        // 2. Boundary pulse (silent in usable template, active in lead tail) waits for more frames.
        let mut edge_new = vec![0.0_f32; 3_840 * 2];
        for s in &mut edge_new[(3_840 - 400) * 2..] {
            *s = 0.05;
        }
        assert_eq!(
            align_splice(&silent_old, &edge_new, 720, false, false),
            SpliceDecision::NeedMoreFrames
        );

        // 3. Pure 1000 Hz sine wave (48-frame period) picks phase-exact minimum near expected drift.
        let total = 15_000;
        let mut sine = Vec::with_capacity(total * 2);
        for n in 0..total {
            let v = (2.0 * std::f64::consts::PI * 1000.0 * n as f64 / 48_000.0).sin() as f32 * 0.1;
            sine.extend_from_slice(&[v, v]);
        }
        let old_end = 11_000;
        let true_lag = 720_usize;
        let new_start = old_end - 4_320 - true_lag;
        let decision = align_splice(
            &sine[(old_end - 10_000) * 2..old_end * 2],
            &sine[new_start * 2..(new_start + 4_320) * 2],
            710, // within half a 48-sample period of 720
            false,
            false,
        );
        let SpliceDecision::Matched {
            next_new_frame,
            silent,
            ..
        } = decision
        else {
            panic!("expected sine match, got {decision:?}");
        };
        assert!(!silent);
        // Phase continuity at the splice point: next sample matches exact sine continuation.
        let expected_next = sine[old_end * 2];
        let actual_next = sine[(new_start + next_new_frame) * 2];
        assert!((expected_next - actual_next).abs() < 1e-6);

        // 4. Uncorrelated audio fails closed instead of repeating or guessing,
        //    including at the 4 s ceiling when new is silent while old had audio.
        let a = synthetic_chirp_stream(10_000);
        let b: Vec<f32> = (0..4_320 * 2)
            .map(|i| (i as f32 * 0.37).cos() * 0.08)
            .collect();
        assert!(matches!(
            align_splice(&a, &b, 720, false, true),
            SpliceDecision::Failed { .. }
        ));
        assert_eq!(
            align_splice(&a, &silent_new, 720, false, false),
            SpliceDecision::NeedMoreFrames
        );
        assert!(matches!(
            align_splice(&a, &silent_new, 720, true, true),
            SpliceDecision::Failed { .. }
        ));

        // 5. Periodic 1000 Hz tone (48-frame = 1 ms period < 20 ms template) with an
        //    energy onset locks to the exact true lag even when expected_drift_frames
        //    is offset by a full period (-48) or five periods (+240).
        let mut gated_sine = vec![0.0_f32; total * 2];
        for n in 7_500..total {
            let v = (2.0 * std::f64::consts::PI * 1000.0 * (n - 7_500) as f64 / 48_000.0).sin()
                as f32
                * 0.1;
            gated_sine[n * 2] = v;
            gated_sine[n * 2 + 1] = v;
        }
        for hint in [720 - 48, 720 + 48, 720 + 240] {
            let dec = align_splice(
                &gated_sine[(old_end - 10_000) * 2..old_end * 2],
                &gated_sine[new_start * 2..(new_start + 4_320) * 2],
                hint,
                false,
                false,
            );
            assert_eq!(
                dec,
                SpliceDecision::Matched {
                    next_new_frame: 4_320 + true_lag,
                    lag_frames: true_lag as i64,
                    silent: false,
                },
                "hint {hint} locked off-period: {dec:?}"
            );
        }
    }

    #[test]
    fn sparse_once_per_second_chirps_wait_past_silence_and_eliminate_startup_quantum_offset() {
        // 20 ms chirp every 48,000 frames (980 ms silence between chirps).
        // Standby has a +240-frame (+5 ms) OS startup-quantum offset relative to
        // expected_drift (true_lag = 960 vs expected = 720). Rolling standby
        // waits through the silent gap until the next chirp and locks to 960.
        let total = 144_000;
        let mut stream = vec![0.0_f32; total * 2];
        for sec in 0..3 {
            let base = sec * 48_000 + 24_000;
            let burst = synthetic_chirp_stream(960);
            stream[base * 2..(base + 960) * 2].copy_from_slice(&burst);
        }
        let true_lag = 960_isize;
        let mut buf = SpliceBuffer::new();
        let mut reconstructed = Vec::with_capacity(total * 2);
        let mut extra = Vec::new();
        let mut active_cursor = 0_usize;
        let mut standby_cursor: Option<usize> = None;
        let mut spliced = false;

        while active_cursor + 480 <= total {
            // Open standby at 26,400 frames (just after sec 0's chirp, entering 980 ms of silence!).
            if !spliced && active_cursor == 26_400 {
                standby_cursor = Some((active_cursor as isize - true_lag) as usize);
            }
            let pkt = &stream[active_cursor * 2..(active_cursor + 480) * 2];
            active_cursor += 480;
            reconstructed.extend_from_slice(buf.accept_active(pkt));

            if let Some(sc) = &mut standby_cursor {
                if *sc + 480 > total {
                    break;
                }
                let s_pkt = &stream[*sc * 2..(*sc + 480) * 2];
                *sc += 480;
                assert!(buf.push_standby(s_pkt));
                if let SpliceDecision::Matched {
                    next_new_frame,
                    lag_frames,
                    silent,
                } = buf.try_align(720)
                {
                    assert!(
                        !silent,
                        "must wait for chirp rather than splicing on +/-5ms guess"
                    );
                    assert_eq!(lag_frames, true_lag as i64);
                    buf.commit_matched(next_new_frame, &mut extra);
                    reconstructed.extend_from_slice(&extra);
                    active_cursor = *sc;
                    standby_cursor = None;
                    spliced = true;
                }
            }
        }
        assert!(spliced);
        let out_frames = reconstructed.len() / 2;
        assert_eq!(&reconstructed[..], &stream[..out_frames * 2]);
    }

    #[test]
    fn dual_output_refuses_only_initial_start_and_degrades_mid_take_with_one_warning() {
        let multi = || DesiredEndpoint::Multiple {
            message: "multiple outputs".into(),
            default_id: Some("ep-default".into()),
        };

        // 1. Initial source start (`strict_start = true`) refuses simultaneous multi-output.
        let mut strict = EndpointTracker::new();
        assert!(strict.startup(true, multi()).is_err());

        // 2. Mid-take app restart (`strict_start = false`) degrades to default output without failing.
        let mut restart = EndpointTracker::new();
        let restart_dec = restart.startup(false, multi()).unwrap();
        assert_eq!(restart_dec.switch_to, Some(Some("ep-default".into())));
        assert_eq!(
            restart_dec.degraded_multiple.as_deref(),
            Some("multiple outputs")
        );

        // 3. Running take: brief <1.5 s handover holds current clock; >2 s (5 scans = 2.5 s)
        //    degrades to default output, counts fallback, and warns once across the source.
        let state = State::new().unwrap();
        let warnings = std::cell::Cell::new(0_u32);
        let record_step = |dec: EndpointDecision| {
            if dec.degraded_multiple.is_some() {
                state.splice_fallbacks.fetch_add(1, Ordering::Relaxed);
                if !state.dual_output_warned.swap(true, Ordering::Relaxed) {
                    warnings.set(warnings.get() + 1);
                }
            }
        };

        let mut tracker = EndpointTracker::new();
        let start_dec = tracker
            .startup(true, DesiredEndpoint::Single(Some("ep-headset".into())))
            .unwrap();
        assert_eq!(start_dec.switch_to, Some(Some("ep-headset".into())));
        assert!(start_dec.degraded_multiple.is_none());

        // Two scans (1.0 s) of dual output then back to single: no warning, no fallback.
        for _ in 0..2 {
            let dec = tracker.observe(multi());
            assert_eq!(dec.switch_to, None);
            assert!(dec.degraded_multiple.is_none());
            record_step(dec);
        }
        let back = tracker.observe(DesiredEndpoint::Single(Some("ep-headset".into())));
        assert_eq!(back.switch_to, Some(Some("ep-headset".into())));
        assert_eq!(warnings.get(), 0);
        assert_eq!(state.splice_fallbacks.load(Ordering::Relaxed), 0);

        // Five scans (2.5 s > 2 s) of dual output: degrades on scan #3, switches to
        // default output, counts 1 fallback, and emits 1 warning.
        for scan_idx in 1..=5 {
            let dec = tracker.observe(multi());
            if scan_idx < MULTIPLE_ENDPOINT_STREAK {
                assert_eq!(dec.switch_to, None);
                assert!(dec.degraded_multiple.is_none());
            } else if scan_idx == MULTIPLE_ENDPOINT_STREAK {
                assert_eq!(dec.switch_to, Some(Some("ep-default".into())));
                assert!(dec.degraded_multiple.is_some());
            } else {
                assert_eq!(dec.switch_to, Some(Some("ep-default".into())));
                assert!(dec.degraded_multiple.is_none());
            }
            record_step(dec);
        }
        assert_eq!(warnings.get(), 1);
        assert_eq!(state.splice_fallbacks.load(Ordering::Relaxed), 1);

        // Recover to single output, then another >2 s dual-output spell: fallback counted,
        // still only one warning for the source.
        record_step(tracker.observe(DesiredEndpoint::Single(Some("ep-headset".into()))));
        for _ in 0..5 {
            record_step(tracker.observe(multi()));
        }
        assert_eq!(warnings.get(), 1);
        assert_eq!(state.splice_fallbacks.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn unspliced_drift_past_thirty_milliseconds_reports_once_until_splice_resets() {
        let mut timeline = ProcessTimeline::new();
        let fast_units = ((NOMINAL_FRAME_UNITS as f64) * (1.0 + 100e-6)).round() as u64;
        let base = Time::from_nanos(1_000_000_000);
        let mut triggered = 0_u32;
        // 35,000 packets = 350 s at +100 ppm -> 35 ms (1,680 frames > 1,440-frame threshold).
        for pkt in 0..35_000_u64 {
            let raw = Time::from_nanos(base.as_nanos() + (pkt * 10_000_000) as i64);
            timeline.packet_time(raw, 480, 0, fast_units);
            if timeline.check_unspliced_drift_limit() {
                triggered += 1;
            }
        }
        assert_eq!(triggered, 1);
        // After a splice resets drift, another 35 ms unspliced excursion reports again once.
        timeline.reset_drift();
        for pkt in 35_000..70_000_u64 {
            let raw = Time::from_nanos(base.as_nanos() + (pkt * 10_000_000) as i64);
            timeline.packet_time(raw, 480, 0, fast_units);
            if timeline.check_unspliced_drift_limit() {
                triggered += 1;
            }
        }
        assert_eq!(triggered, 2);
    }
}
