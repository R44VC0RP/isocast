//! A minimal native receiver: normal Client pairing/selection, standard SDP
//! and str0m media. No daemon implementation code. Output is counts, decoded
//! Opus samples and (on the same host clock) packet age, not glass-to-glass.
//! Usage: ISOCAST_ENGINE=HOST:PORT webrtc-receive [seconds] [new-output.h264]
//! ISOCAST_WEBRTC_AV_SYNC_TEST=1 checks synthetic flash/beep capture timestamps
//! through RTCP sender reports; this is not a hardware playout measurement.
//! ISOCAST_WEBRTC_CLOCK_OFFSET_MS is measured receiver-minus-sender wall time;
//! ISOCAST_WEBRTC_CLOCK_UNCERTAINTY_MS must bound that calibration over the run.
//! Together they enable SR-mapped capture-to-first-packet and complete-media
//! delivery ages, not network-only transit or playout latency. No cross-host Instant subtraction.
//! ISOCAST_WEBRTC_FFMPEG can select an ffmpeg executable (including on Windows).
//! ISOCAST_WEBRTC_H264_PROFILE selects the H.264 offer (all packetization-mode=1)
//! and checks the answer and received SPS; unset keeps the default offer.
//! constrained_baseline (42e01f) and constrained_high_and_baseline (640c1f,
//! 42e01f) require a Constrained Baseline answer and SPS (profile_idc 66,
//! constraint_set1). constrained_baseline_and_high (42e01f, 64001f) requires
//! High. plain_baseline (42001f) requires the answer to echo 42001f, and
//! baseline_no_profile (no profile-level-id) an answer without one; both
//! require a Constrained Baseline SPS.

use std::collections::BTreeMap;
use std::io::{BufRead, ErrorKind, Read, Write};
use std::net::UdpSocket;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, ensure};
use isocast_protocol::api::{Method, WhepAnswer, WhepOfferParams};
use str0m::change::SdpAnswer;
use str0m::format::{Codec, CodecExtra, FormatParams};
use str0m::media::{Direction, Frequency, KeyframeRequestKind, MediaData, MediaKind};
use str0m::net::{Protocol, Receive};
use str0m::rtp::rtcp::SenderInfo;
use str0m::{Candidate, Event, Input, Output, RtcConfig};

fn signed_rtp_delta(time: u32, anchor: u32) -> f64 {
    // Runs are bounded to 600 seconds, well below half a wrap at either rate.
    f64::from(time.wrapping_sub(anchor) as i32)
}

fn system_delta(time: SystemTime, anchor: SystemTime) -> f64 {
    match time.duration_since(anchor) {
        Ok(delta) => delta.as_secs_f64(),
        Err(error) => -error.duration().as_secs_f64(),
    }
}

#[derive(Clone, Copy)]
struct ClockCalibration {
    offset_ms: f64,
    uncertainty_ms: f64,
}

impl ClockCalibration {
    fn parse(offset: Option<&str>, uncertainty: Option<&str>) -> Result<Option<Self>> {
        if offset.is_none() && uncertainty.is_none() {
            return Ok(None);
        }
        let offset_ms: f64 = offset
            .context("ISOCAST_WEBRTC_CLOCK_OFFSET_MS required with clock uncertainty")?
            .parse()
            .context("ISOCAST_WEBRTC_CLOCK_OFFSET_MS: expected finite milliseconds")?;
        let uncertainty_ms: f64 = uncertainty
            .context("ISOCAST_WEBRTC_CLOCK_UNCERTAINTY_MS required with clock offset")?
            .parse()
            .context(
                "ISOCAST_WEBRTC_CLOCK_UNCERTAINTY_MS: expected nonnegative finite milliseconds",
            )?;
        ensure!(offset_ms.is_finite(), "clock offset must be finite");
        ensure!(
            uncertainty_ms.is_finite() && uncertainty_ms >= 0.0,
            "clock uncertainty must be finite and nonnegative"
        );
        Ok(Some(Self {
            offset_ms,
            uncertainty_ms,
        }))
    }

    fn age_ms(self, clock: &SenderClock, time: u32, arrival: SystemTime) -> Result<f64> {
        // map() is sender capture minus receiver arrival. Offset is R - S.
        Ok(-clock.map(time, arrival)? * 1000.0 - self.offset_ms)
    }
}

fn distribution(values: &mut [f64]) -> serde_json::Value {
    values.sort_by(f64::total_cmp);
    let percentile = |fraction: f64| {
        values
            .get((values.len().saturating_sub(1) as f64 * fraction) as usize)
            .copied()
    };
    serde_json::json!({"samples": values.len(), "min": values.first(),
        "p50": percentile(0.5), "p95": percentile(0.95), "max": values.last()})
}

// str0m SeqNo is already extended beyond 16 bits. Merge ranges so a late or
// overlapping delivery cannot invent a gap, including across repeated wraps.
fn sequence_gaps(ranges: &mut [(u64, u64)]) -> (u64, u64) {
    ranges.sort_unstable();
    let mut end = None;
    let mut gaps = 0;
    let mut missing = 0;
    for &(first, last) in ranges.iter() {
        if let Some(previous) = end {
            let gap = first.saturating_sub(previous).saturating_sub(1);
            gaps += u64::from(gap > 0);
            missing += gap;
        }
        end = Some(end.map_or(last, |previous: u64| previous.max(last)));
    }
    (gaps, missing)
}

#[derive(Default)]
struct TrackObservations {
    clock: SenderClock,
    ranges: Vec<(u64, u64)>,
    noncontiguous: u64,
    previous_time: Option<u32>,
    nonincreasing_times: u64,
    cadence_ms: Vec<f64>,
    age_ms: Vec<f64>,
    delivery_age_ms: Vec<f64>,
    no_sender_report: u64,
    no_arrival: u64,
}

impl TrackObservations {
    fn observe(
        &mut self,
        data: &MediaData,
        calibration: Option<ClockCalibration>,
        arrival: Option<SystemTime>,
    ) -> Result<()> {
        self.ranges
            .push((**data.seq_range.start(), **data.seq_range.end()));
        self.noncontiguous += u64::from(!data.contiguous);
        let time = data.time.numer() as u32;
        if let Some(previous) = self.previous_time.replace(time) {
            let delta = signed_rtp_delta(time, previous);
            if delta > 0.0 {
                self.cadence_ms
                    .push(delta * 1000.0 / f64::from(data.time.denom()));
            } else {
                self.nonincreasing_times += 1;
            }
        }
        if let Some(calibration) = calibration {
            self.clock
                .observe(data.last_sender_info, data.time.denom())?;
            if self.clock.latest.is_none() {
                self.no_sender_report += 1;
            } else {
                // The complete access unit is available to the application
                // now, after depacketization/reordering, before decoding.
                self.delivery_age_ms.push(calibration.age_ms(
                    &self.clock,
                    time,
                    SystemTime::now(),
                )?);
                if let Some(arrival) = arrival {
                    self.age_ms
                        .push(calibration.age_ms(&self.clock, time, arrival)?);
                } else {
                    self.no_arrival += 1;
                }
            }
        }
        Ok(())
    }

    fn summary(&mut self) -> serde_json::Value {
        let (gaps, missing) = sequence_gaps(&mut self.ranges);
        serde_json::json!({
            "delivered_units": self.ranges.len(),
            "first_extended_sequence": self.ranges.first().map(|range| range.0),
            "last_extended_sequence": self.ranges.iter().map(|range| range.1).max(),
            "sequence_gap_runs": gaps, "sequences_between_delivered_ranges": missing,
            "noncontiguous_units": self.noncontiguous,
            "capture_interval_ms": distribution(&mut self.cadence_ms),
            "nonincreasing_capture_times": self.nonincreasing_times,
            "calibrated_capture_to_first_arrival_ms": distribution(&mut self.age_ms),
            "calibrated_capture_to_media_delivery_ms": distribution(&mut self.delivery_age_ms),
            "units_before_sender_report": self.no_sender_report,
            "units_without_retained_arrival": self.no_arrival,
            "sender_reports": self.clock.reports,
            "sr_max_anchor_error_ms": self.clock.max_anchor_error * 1000.0
        })
    }
}

fn pair_pulses(flashes: &[f64], beeps: &[f64], start: f64, end: f64) -> Result<Vec<(f64, f64)>> {
    let frame = 1.0 / 30.0;
    let interior = |time: f64, margin: f64| time > start + margin && time < end - margin;
    let mut used = vec![false; beeps.len()];
    let mut pairs = Vec::new();
    // Match before excluding edges: a one-sample onset difference must not
    // put a flash and its beep on opposite sides of a window filter.
    for &flash in flashes.iter().filter(|&&time| interior(time, 0.15 + frame)) {
        let (index, &beep) = beeps
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| ((*a - flash).abs()).total_cmp(&((*b - flash).abs())))
            .context("flash has no beep")?;
        ensure!(
            !used[index] && (beep - flash).abs() <= frame,
            "flash/beep skew exceeds one frame or a pulse is missing"
        );
        used[index] = true;
        pairs.push((flash, beep));
    }
    for (index, &beep) in beeps.iter().enumerate() {
        ensure!(
            !interior(beep, 0.15 + 2.0 * frame) || used[index],
            "interior beep has no matching flash"
        );
    }
    ensure!(
        pairs.len() >= 2,
        "need at least two complete flash/beep pairs"
    );
    Ok(pairs)
}

#[derive(Default)]
struct SenderClock {
    latest: Option<SenderInfo>,
    reports: u64,
    max_anchor_error: f64,
}

impl SenderClock {
    fn observe(&mut self, info: Option<SenderInfo>, rate: u32) -> Result<()> {
        let Some(info) = info else { return Ok(()) };
        ensure!(info.rtp_time.denom() == rate, "unexpected SR clock rate");
        if let Some(previous) = self.latest {
            ensure!(info.ssrc == previous.ssrc, "SR SSRC changed during capture");
            if info.ntp_time == previous.ntp_time {
                ensure!(info.rtp_time == previous.rtp_time, "conflicting SR anchor");
                return Ok(());
            }
            let elapsed = system_delta(info.ntp_time, previous.ntp_time);
            ensure!((0.0..=600.0).contains(&elapsed), "SR time discontinuity");
            let error = signed_rtp_delta(
                info.rtp_time.numer() as u32,
                previous.rtp_time.numer() as u32,
            ) / f64::from(rate)
                - elapsed;
            self.max_anchor_error = self.max_anchor_error.max(error.abs());
        }
        self.latest = Some(info);
        self.reports += 1;
        Ok(())
    }

    fn map(&self, time: u32, origin: SystemTime) -> Result<f64> {
        let info = self.latest.context("track has no RTCP sender report")?;
        let delta =
            signed_rtp_delta(time, info.rtp_time.numer() as u32) / f64::from(info.rtp_time.denom());
        ensure!(delta.abs() <= 600.0, "RTP timestamp outside capture window");
        Ok(system_delta(info.ntp_time, origin) + delta)
    }
}

#[derive(Default)]
struct AvSync {
    video_clock: SenderClock,
    audio_clock: SenderClock,
    video_times: Vec<u32>,
    audio_first: Option<u32>,
    audio_next: Option<u32>,
    audio_gaps: Vec<(u32, u32)>,
    quiet: usize,
    pending_beep: Option<u32>,
    beeps: Vec<u32>,
}

impl AvSync {
    fn audio(&mut self, time: u32, samples: &[f32]) {
        self.audio_first.get_or_insert(time);
        if let Some(next) = self.audio_next
            && next != time
        {
            // Missing samples are not observed silence. Invalidate any pulse
            // spanning the gap, but keep subsequent onsets on packet RTP time.
            self.audio_gaps.push((next, time));
            self.quiet = 0;
            self.pending_beep = None;
        }
        for (index, frame) in samples.as_chunks::<2>().0.iter().enumerate() {
            if frame.iter().any(|sample| sample.abs() > 0.01) {
                if self.quiet >= 2400 {
                    self.pending_beep = Some(time.wrapping_add(index as u32));
                }
                self.quiet = 0;
            } else {
                self.quiet += 1;
                if self.quiet == 2400
                    && let Some(onset) = self.pending_beep.take()
                {
                    self.beeps.push(onset);
                }
            }
        }
        self.audio_next = Some(time.wrapping_add((samples.len() / 2) as u32));
    }

    fn verify(&self, path: &Path) -> Result<()> {
        println!(
            "{}",
            serde_json::json!({"av_sync_sender_reports": {
                "video_present": self.video_clock.latest.is_some(),
                "video_count": self.video_clock.reports,
                "video_max_anchor_error_ms": self.video_clock.max_anchor_error * 1000.0,
                "audio_present": self.audio_clock.latest.is_some(),
                "audio_count": self.audio_clock.reports,
                "audio_max_anchor_error_ms": self.audio_clock.max_anchor_error * 1000.0,
                "audio_packet_gaps": self.audio_gaps.len()
            }})
        );
        let video = self.video_clock.latest.context("video SR missing")?;
        let audio = self.audio_clock.latest.context("audio SR missing")?;
        // Both negotiated program tracks use the engine clock (not random RTP
        // offsets). Their 32-bit clocks wrap at different periods. Differences
        // of those periods are multiples of 2^32/lcm(90000,48000) seconds.
        let wrap = f64::from(u32::MAX) + 1.0;
        let period = wrap / 720_000.0;
        let anchor_error = f64::from(video.rtp_time.numer() as u32) / 90_000.0
            - f64::from(audio.rtp_time.numer() as u32) / 48_000.0
            - system_delta(video.ntp_time, audio.ntp_time);
        let anchor_error = (anchor_error + period / 2.0).rem_euclid(period) - period / 2.0;
        println!(
            "{}",
            serde_json::json!({"av_sync_engine_clock_anchor_error_ms": anchor_error * 1000.0})
        );
        ensure!(
            anchor_error.abs() <= 0.001
                && self.video_clock.max_anchor_error <= 0.001
                && self.audio_clock.max_anchor_error <= 0.001,
            "RTCP anchors disagree with the common engine clock by more than 1 ms"
        );
        let ffmpeg = std::env::var_os("ISOCAST_WEBRTC_FFMPEG").unwrap_or_else(|| "ffmpeg".into());
        let decoded = Command::new(ffmpeg)
            .args(["-v", "error", "-xerror", "-nostdin", "-f", "h264", "-i"])
            .arg(path)
            .args([
                "-vf",
                "crop=iw:ih/4:0:ih*3/4,scale=1:1",
                "-pix_fmt",
                "gray",
                "-fps_mode",
                "passthrough",
                "-f",
                "rawvideo",
                "pipe:1",
            ])
            .output()
            .context("run ffmpeg to decode the received flash band")?;
        ensure!(
            decoded.status.success(),
            "flash-band decode failed: {}",
            String::from_utf8_lossy(&decoded.stderr)
        );
        ensure!(
            decoded.stdout.len() == self.video_times.len(),
            "decoded {} frames for {} RTP access units (requires no B frames)",
            decoded.stdout.len(),
            self.video_times.len()
        );
        let origin = video.ntp_time;
        let times = self
            .video_times
            .iter()
            .map(|&time| self.video_clock.map(time, origin))
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            times.windows(2).all(|pair| pair[1] > pair[0]),
            "video RTP is not in presentation order"
        );
        let mut flashes = Vec::new();
        let mut pending = None;
        for (pair, &time) in decoded.stdout.windows(2).zip(times.iter().skip(1)) {
            if pair[1] > 128 && pair[0] <= 128 {
                pending = Some(time);
            } else if pair[1] <= 128
                && let Some(onset) = pending.take()
            {
                flashes.push(onset);
            }
        }
        let first_audio = self.audio_first.context("no audio samples")?;
        let start = times
            .first()
            .copied()
            .context("no video frames")?
            .max(self.audio_clock.map(first_audio, origin)?);
        let end = times.last().copied().context("no video frames")?.min(
            self.audio_clock
                .map(self.audio_next.context("no audio samples")?, origin)?,
        );
        // Exclude partial edge pulses in the common observation window. The
        // source pulses are 100 ms; completion also requires observed silence.
        let beeps = self
            .beeps
            .iter()
            .map(|&time| self.audio_clock.map(time, origin))
            .collect::<Result<Vec<_>>>()?;
        let audio_gaps = self
            .audio_gaps
            .iter()
            .map(|&(expected, actual)| {
                Ok((
                    self.audio_clock.map(expected, origin)?,
                    self.audio_clock.map(actual, origin)?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        // Emit the evidence BEFORE the verdict: a missing pulse otherwise
        // loses its timestamps, so a hosted failure cannot be attributed or
        // replayed. A timestamp gap alone does not prove a particular beep
        // was lost, and never relaxes the strict pairing check below.
        println!(
            "{}",
            serde_json::json!({"av_sync_observations": {
                "time_origin": "video RTCP SR NTP (seconds since Unix epoch)",
                "origin_unix_seconds": origin.duration_since(SystemTime::UNIX_EPOCH)?.as_secs_f64(),
                "start_seconds": start, "end_seconds": end,
                "flash_seconds": flashes, "beep_seconds": beeps,
                "video_seconds": times,
                "audio_gap_seconds": audio_gaps,
                "audio_gap_scope": "expected next decoded sample, actual packet start; not proof of network loss or late-input silence",
                "complete_beep_sample_indices": self.beeps.iter().map(|&time| signed_rtp_delta(time, first_audio) as i64).collect::<Vec<_>>()
            }})
        );
        let pairs = pair_pulses(&flashes, &beeps, start, end)?;
        let flashes: Vec<_> = pairs.iter().map(|(flash, _)| *flash).collect();
        let beeps: Vec<_> = pairs.iter().map(|(_, beep)| *beep).collect();
        let skew: Vec<_> = pairs.iter().map(|(flash, beep)| beep - flash).collect();
        let max_skew = skew.iter().map(|value| value.abs()).fold(0.0, f64::max);
        println!(
            "{}",
            serde_json::json!({"av_sync": {
                "measurement": "capture timeline at receiver, not hardware playout",
                "time_origin": "video RTCP SR NTP (seconds since Unix epoch)",
                "origin_unix_seconds": origin.duration_since(SystemTime::UNIX_EPOCH)?.as_secs_f64(),
                "flash_seconds": flashes, "beep_seconds": beeps,
                "complete_beep_sample_indices": self.beeps.iter().map(|&time| signed_rtp_delta(time, first_audio) as i64).collect::<Vec<_>>(),
                "beep_minus_flash_seconds": skew, "max_abs_skew_seconds": max_skew,
                "matched_pulses": skew.len(), "limit_seconds": 1.0 / 30.0
            }})
        );
        ensure!(
            flashes.len() == beeps.len() && flashes.len() >= 2,
            "need at least two complete, one-to-one flash/beep pulses ({} flashes, {} beeps)",
            flashes.len(),
            beeps.len()
        );
        ensure!(
            max_skew <= 1.0 / 30.0,
            "capture A/V skew exceeds one 30 fps frame"
        );
        Ok(())
    }
}

/// The H.264 offer this receiver makes.
#[derive(Clone, Copy, PartialEq, Debug)]
enum H264Offer {
    /// str0m's default set plus High 640c34.
    Default,
    /// Only 42e01f with packetization-mode=1.
    ConstrainedBaseline,
    /// Constrained High 640c1f then 42e01f, without plain High (6400xx).
    ConstrainedHighAndBaseline,
    /// 42e01f then High 64001f: plain High must still win.
    ConstrainedBaselineAndHigh,
    /// Only plain Baseline 42001f (no constraint flags).
    PlainBaseline,
    /// packetization-mode=1 without profile-level-id (str0m's Baseline 3.1 fallback).
    BaselineNoProfile,
}

impl H264Offer {
    const ALL: [(&'static str, Self); 5] = [
        ("constrained_baseline", Self::ConstrainedBaseline),
        (
            "constrained_high_and_baseline",
            Self::ConstrainedHighAndBaseline,
        ),
        (
            "constrained_baseline_and_high",
            Self::ConstrainedBaselineAndHigh,
        ),
        ("plain_baseline", Self::PlainBaseline),
        ("baseline_no_profile", Self::BaselineNoProfile),
    ];

    fn parse(value: Option<&str>) -> Result<Self> {
        match value {
            None | Some("") => Ok(Self::Default),
            Some(value) => Self::ALL
                .iter()
                .find(|(name, _)| *name == value)
                .map(|&(_, offer)| offer)
                .ok_or_else(|| {
                    let valid: Vec<_> = Self::ALL.iter().map(|(name, _)| *name).collect();
                    anyhow::anyhow!(
                        "ISOCAST_WEBRTC_H264_PROFILE: unknown profile {value:?}; valid options: {} (or unset for the default offer)",
                        valid.join(", ")
                    )
                }),
        }
    }

    fn name(self) -> &'static str {
        Self::ALL
            .iter()
            .find(|&&(_, offer)| offer == self)
            .map_or("default", |&(name, _)| name)
    }

    /// (payload type, RTX payload type, profile-level-id) offered in order,
    /// each with packetization-mode=1. Empty for the default offer.
    fn formats(self) -> &'static [(u8, u8, Option<u32>)] {
        match self {
            Self::Default => &[],
            Self::ConstrainedBaseline => &[(108, 109, Some(0x42e01f))],
            Self::ConstrainedHighAndBaseline => {
                &[(102, 103, Some(0x640c1f)), (108, 109, Some(0x42e01f))]
            }
            Self::ConstrainedBaselineAndHigh => {
                &[(108, 109, Some(0x42e01f)), (114, 115, Some(0x64001f))]
            }
            Self::PlainBaseline => &[(127, 121, Some(0x42001f))],
            Self::BaselineNoProfile => &[(106, 107, None)],
        }
    }

    /// Whether the answered H.264 formats fit this offer. The default offer
    /// keeps its historical behaviour and is not checked here.
    fn answer_ok(self, answered: &[(Option<u32>, Option<u8>)]) -> bool {
        let accepts = |id: Option<u32>| match self {
            Self::Default => true,
            Self::ConstrainedBaseline | Self::ConstrainedHighAndBaseline => {
                id.is_some_and(is_constrained_baseline)
            }
            Self::ConstrainedBaselineAndHigh => id.is_some_and(|id| id >> 16 == 0x64),
            Self::PlainBaseline => id == Some(0x42001f),
            Self::BaselineNoProfile => id.is_none(),
        };
        self == Self::Default
            || (!answered.is_empty()
                && answered
                    .iter()
                    .all(|&(id, mode)| accepts(id) && mode == Some(1)))
    }

    /// Whether every received SPS fits this offer: High for the High offer,
    /// otherwise Constrained Baseline (profile_idc 66 with constraint_set1),
    /// which every baseline-family receiver decodes.
    fn sps_ok(self, sps: &[(u8, u8, u8)]) -> bool {
        let accepts = |&(profile_idc, flags, _): &(u8, u8, u8)| match self {
            Self::Default => true,
            Self::ConstrainedBaselineAndHigh => profile_idc == 100,
            _ => profile_idc == 66 && flags & 0x40 != 0,
        };
        self == Self::Default || (!sps.is_empty() && sps.iter().all(accepts))
    }
}

/// The receiver's codecs: Opus plus the H.264 formats of `h264_offer`.
fn h264_rtc_config(h264_offer: H264Offer) -> RtcConfig {
    let mut config = RtcConfig::new()
        .set_crypto_provider(Arc::new(str0m::crypto::from_feature_flags()))
        .clear_codecs()
        .enable_opus(true, false);
    match h264_offer {
        H264Offer::Default => {
            config = config.enable_h264(true);
            config
                .codec_config()
                .add_h264(102.into(), Some(103.into()), true, 0x640c34);
        }
        offer => {
            for &(pt, rtx, profile_level_id) in offer.formats() {
                config.codec_config().add_config(
                    pt.into(),
                    Some(rtx.into()),
                    Codec::H264,
                    Frequency::NINETY_KHZ,
                    None,
                    FormatParams {
                        level_asymmetry_allowed: Some(true),
                        packetization_mode: Some(1),
                        profile_level_id,
                        ..Default::default()
                    },
                );
            }
        }
    }
    config
}

/// RFC 6184 constrained baseline: profile_idc 66 with constraint_set1.
fn is_constrained_baseline(profile_level_id: u32) -> bool {
    profile_level_id >> 16 == 0x42 && (profile_level_id >> 8) & 0x40 != 0
}

/// (profile-level-id, packetization-mode) of every H.264 payload type in an SDP.
fn sdp_h264_formats(sdp: &str) -> Vec<(Option<u32>, Option<u8>)> {
    let lines: Vec<&str> = sdp.lines().map(str::trim).collect();
    let mut formats = Vec::new();
    for line in &lines {
        let Some(rest) = line.strip_prefix("a=rtpmap:") else {
            continue;
        };
        let mut parts = rest.split_whitespace();
        let (Some(pt), Some(encoding)) = (parts.next(), parts.next()) else {
            continue;
        };
        if !encoding.to_ascii_uppercase().starts_with("H264/") {
            continue;
        }
        let prefix = format!("a=fmtp:{pt} ");
        let params = lines
            .iter()
            .find_map(|line| line.strip_prefix(prefix.as_str()))
            .unwrap_or("");
        let value = |key: &str| {
            params
                .split(';')
                .filter_map(|pair| pair.trim().split_once('='))
                .find(|(name, _)| name.eq_ignore_ascii_case(key))
                .map(|(_, value)| value.trim())
        };
        formats.push((
            value("profile-level-id").and_then(|id| u32::from_str_radix(id, 16).ok()),
            value("packetization-mode").and_then(|mode| mode.parse().ok()),
        ));
    }
    formats
}

/// (profile_idc, constraint flags byte, level_idc) of each SPS in an Annex B unit.
fn annexb_sps(data: &[u8]) -> Vec<(u8, u8, u8)> {
    let mut found = Vec::new();
    let mut index = 0;
    while index + 3 <= data.len() {
        if data[index..index + 3] == [0, 0, 1] {
            let nal = &data[index + 3..];
            // The three bytes after the SPS header precede any possible
            // emulation-prevention byte (profile_idc is never zero).
            if nal.len() >= 4 && nal[0] & 0x1f == 7 {
                found.push((nal[1], nal[2], nal[3]));
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    found
}

#[cfg(target_os = "macos")]
fn packet_age_ms(time: str0m::media::MediaTime) -> f64 {
    use isocast_engine::Platform;
    let now = isocast_platform_macos::MacPlatform.now().as_nanos().max(0) as u64;
    let local_ticks = ((u128::from(now) * u128::from(time.denom())) / 1_000_000_000) as u32;
    f64::from(local_ticks.wrapping_sub(time.numer() as u32) as i32) * 1000.0
        / f64::from(time.denom())
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let first = args.next().unwrap_or_else(|| "10".into());
    if first == "--pair" {
        let host = args
            .next()
            .context("--pair requires HOST; read the local pairing code from stdin")?;
        let mut code = String::new();
        std::io::stdin().lock().take(8).read_line(&mut code)?;
        let paired =
            isocast_client::remote::pair(&host, code.trim(), Some("WebRTC proof receiver"));
        code.clear();
        println!("{}", serde_json::json!({"paired": paired?.id}));
        return Ok(());
    }
    let seconds: u64 = first.parse()?;
    ensure!((1..=600).contains(&seconds), "seconds: expected 1..600");
    let av_sync_test = std::env::var("ISOCAST_WEBRTC_AV_SYNC_TEST").as_deref() == Ok("1");
    let clock_offset = std::env::var("ISOCAST_WEBRTC_CLOCK_OFFSET_MS").ok();
    let clock_uncertainty = std::env::var("ISOCAST_WEBRTC_CLOCK_UNCERTAINTY_MS").ok();
    let calibration =
        ClockCalibration::parse(clock_offset.as_deref(), clock_uncertainty.as_deref())?;
    let h264_offer =
        H264Offer::parse(std::env::var("ISOCAST_WEBRTC_H264_PROFILE").ok().as_deref())?;
    let video_path = args.next();
    ensure!(
        !av_sync_test || video_path.is_some(),
        "A/V sync test requires new-output.h264"
    );
    let mut av_sync = av_sync_test.then(AvSync::default);
    let mut video = video_path
        .as_ref()
        .map(|path| {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
        })
        .transpose()?;
    // Bind explicitly for remote clients: defaults to same-machine proof.
    let bind = std::env::var("ISOCAST_WEBRTC_BIND").unwrap_or_else(|_| "127.0.0.1:0".into());
    let socket = UdpSocket::bind(bind)?;
    let local = socket.local_addr()?;
    let mut rtc = h264_rtc_config(h264_offer).build(Instant::now());
    rtc.add_local_candidate(Candidate::host(local, "udp")?);
    let mut change = rtc.sdp_api();
    let video_mid = change.add_media(
        MediaKind::Video,
        Direction::RecvOnly,
        Some("program".into()),
        None,
        None,
    );
    let audio_mid = change.add_media(
        MediaKind::Audio,
        Direction::RecvOnly,
        Some("program".into()),
        None,
        None,
    );
    let (offer, pending) = change.apply().context("no SDP offer")?;
    let mut client = isocast_client::Client::connect()?.context("engine is not running")?;
    let answer: WhepAnswer = client.call(Method::WhepOffer(WhepOfferParams {
        sdp: offer.to_sdp_string(),
    }))?;
    let offered_h264 = sdp_h264_formats(&offer.to_sdp_string());
    let answered_h264 = sdp_h264_formats(&answer.sdp);
    if h264_offer != H264Offer::Default {
        let expected: Vec<_> = h264_offer
            .formats()
            .iter()
            .map(|&(_, _, id)| (id, Some(1)))
            .collect();
        ensure!(
            offered_h264 == expected,
            "{} offer must contain exactly {expected:?}: {offered_h264:?}",
            h264_offer.name()
        );
        ensure!(
            h264_offer.answer_ok(&answered_h264),
            "{} answer selected an unexpected H.264 format: {answered_h264:?}",
            h264_offer.name()
        );
    }
    rtc.sdp_api()
        .accept_answer(pending, SdpAnswer::from_sdp_string(&answer.sdp)?)?;
    let mut decoder = opus2::Decoder::new(48_000, opus2::Channels::Stereo)?;
    let mut audio = [0f32; 5760 * 2];
    let mut video_frames = 0u64;
    let mut audio_packets = 0u64;
    let mut audio_frames = 0u64;
    let mut peak = 0f32;
    let same_clock = std::env::var_os("ISOCAST_WEBRTC_SAME_CLOCK").is_some();
    let feedback = std::env::var_os("ISOCAST_WEBRTC_FEEDBACK_TEST").is_some();
    let mut feedback_sent = 0;
    let mut keyframes = 0;
    let mut sps_seen = Vec::<(u8, u8, u8)>::new();
    let mut video_age_ms = Vec::<f64>::new();
    let mut audio_age_ms = Vec::<f64>::new();
    let mut video_observations = TrackObservations::default();
    let mut audio_observations = TrackObservations::default();
    let mut arrivals = BTreeMap::<Instant, SystemTime>::new();
    let mut buffer = [0u8; 2048];
    let began = Instant::now();
    let deadline = began + Duration::from_secs(seconds);
    while Instant::now() < deadline && rtc.is_alive() {
        let timeout = match rtc.poll_output()? {
            Output::Timeout(timeout) => timeout,
            Output::Transmit(packet) => {
                socket.send_to(&packet.contents, packet.destination)?;
                continue;
            }
            Output::Event(Event::MediaData(data)) => {
                match data.params.spec().codec {
                    Codec::H264 => {
                        video_observations.observe(
                            &data,
                            calibration,
                            arrivals.get(&data.network_time).copied(),
                        )?;
                        if let Some(sync) = &mut av_sync {
                            ensure!(
                                data.mid == video_mid && data.time.denom() == 90_000,
                                "unexpected program video track"
                            );
                            sync.video_clock.observe(data.last_sender_info, 90_000)?;
                            sync.video_times.push(data.time.numer() as u32);
                        }
                        video_frames += 1;
                        for sps in annexb_sps(&data.data) {
                            if !sps_seen.contains(&sps) {
                                sps_seen.push(sps);
                            }
                        }
                        if matches!(data.codec_extra, CodecExtra::H264(extra) if extra.is_keyframe)
                        {
                            keyframes += 1;
                        }
                        if feedback
                            && feedback_sent < 2
                            && began.elapsed() >= Duration::from_secs(2 + feedback_sent * 2)
                        {
                            let kind = if feedback_sent == 0 {
                                KeyframeRequestKind::Pli
                            } else {
                                KeyframeRequestKind::Fir
                            };
                            rtc.writer(data.mid)
                                .context("missing video receiver")?
                                .request_keyframe(None, kind)?;
                            feedback_sent += 1;
                        }
                        #[cfg(target_os = "macos")]
                        if same_clock {
                            video_age_ms.push(packet_age_ms(data.time));
                        }
                        if let Some(video) = &mut video {
                            video.write_all(&data.data)?;
                        }
                    }
                    Codec::Opus => {
                        audio_observations.observe(
                            &data,
                            calibration,
                            arrivals.get(&data.network_time).copied(),
                        )?;
                        audio_packets += 1;
                        #[cfg(target_os = "macos")]
                        if same_clock {
                            audio_age_ms.push(packet_age_ms(data.time));
                        }
                        let frames = decoder.decode_float(&data.data, &mut audio, false)?;
                        if let Some(sync) = &mut av_sync {
                            ensure!(
                                data.mid == audio_mid && data.time.denom() == 48_000,
                                "unexpected program audio track"
                            );
                            sync.audio_clock.observe(data.last_sender_info, 48_000)?;
                            sync.audio(data.time.numer() as u32, &audio[..frames * 2]);
                        }
                        audio_frames += frames as u64;
                        for sample in &audio[..frames * 2] {
                            peak = peak.max(sample.abs());
                        }
                    }
                    _ => {}
                }
                continue;
            }
            _ => continue,
        };
        let wait = timeout
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(20));
        if wait.is_zero() {
            rtc.handle_input(Input::Timeout(Instant::now()))?;
            continue;
        }
        socket.set_read_timeout(Some(wait))?;
        match socket.recv_from(&mut buffer) {
            Ok((length, source)) => {
                let received = Instant::now();
                if calibration.is_some() {
                    // Preserve actual userspace UDP arrival wall time, keyed by
                    // the exact local Instant str0m returns as network_time.
                    arrivals.insert(received, SystemTime::now());
                    while arrivals.len() > 16_384
                        || arrivals.first_key_value().is_some_and(|(time, _)| {
                            received.duration_since(*time) > Duration::from_secs(10)
                        })
                    {
                        arrivals.pop_first();
                    }
                }
                let Ok(contents) = (&buffer[..length]).try_into() else {
                    continue;
                };
                rtc.handle_input(Input::Receive(
                    received,
                    Receive {
                        proto: Protocol::Udp,
                        source,
                        destination: local,
                        contents,
                    },
                ))?;
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                rtc.handle_input(Input::Timeout(Instant::now()))?
            }
            Err(error) => return Err(error.into()),
        }
    }
    let closed = client.call::<serde_json::Value>(Method::WhepClose).is_ok();
    #[cfg(not(target_os = "macos"))]
    let _ = same_clock;
    video_age_ms.sort_by(f64::total_cmp);
    audio_age_ms.sort_by(f64::total_cmp);
    let age = |values: &[f64], fraction: f64| {
        values
            .get(((values.len().saturating_sub(1)) as f64 * fraction) as usize)
            .copied()
    };
    let observations = serde_json::json!({
        "elapsed_seconds": began.elapsed().as_secs_f64(),
        "calibration": calibration.map(|value| serde_json::json!({
            "receiver_minus_sender_ms": value.offset_ms,
            "uncertainty_ms": value.uncertainty_ms,
            "measurement": "SR capture to first UDP arrival and complete application media delivery; excludes decode/playout, not network-only latency",
            "assumption": "external calibration bounds receiver wall minus sender SR clock throughout run, including drift and frozen str0m epoch"
        })),
        "loss_scope": "gaps between delivered extended RTP ranges, after reorder/recovery; includes received-but-discarded units, excludes run edges; not raw network packet loss",
        "network_packets_lost": null,
        "cadence_scope": "positive RTP capture intervals; source/governor cadence is not RTP loss",
        "video": video_observations.summary(), "audio": audio_observations.summary()
    });
    let format_json = |formats: &[(Option<u32>, Option<u8>)]| {
        formats
            .iter()
            .map(|(id, mode)| serde_json::json!({"profile_level_id": id.map(|id| format!("{id:06x}")), "packetization_mode": mode}))
            .collect::<Vec<_>>()
    };
    let h264 = serde_json::json!({
        "offer_profile": h264_offer.name(),
        "offered": format_json(&offered_h264),
        "answered": format_json(&answered_h264),
        "sps": sps_seen.iter().map(|&(profile_idc, flags, level_idc)| serde_json::json!({
            "profile_idc": profile_idc,
            "constraint_set0": (flags >> 7) & 1, "constraint_set1": (flags >> 6) & 1,
            "constraint_set2": (flags >> 5) & 1, "level_idc": level_idc
        })).collect::<Vec<_>>()
    });
    println!(
        "{}",
        serde_json::json!({"h264":h264,"video_frames":video_frames,"keyframes":keyframes,"feedback_sent":feedback_sent,"audio_packets":audio_packets,"decoded_audio_frames":audio_frames,"audio_peak":peak,"closed":closed,"packet_age_ms_p50":age(&video_age_ms,0.5),"packet_age_ms_p95":age(&video_age_ms,0.95),"audio_age_ms_p50":age(&audio_age_ms,0.5),"audio_age_ms_p95":age(&audio_age_ms,0.95),"receiver_observations":observations})
    );
    ensure!(
        video_frames > 0 && audio_frames > 0,
        "no playable H.264/Opus media received"
    );
    ensure!(
        calibration.is_none()
            || (!video_observations.age_ms.is_empty() && !audio_observations.age_ms.is_empty()),
        "calibrated latency requires RTCP SRs and retained UDP arrivals for both tracks"
    );
    ensure!(
        h264_offer.sps_ok(&sps_seen),
        "{} received SPS does not fit the offer: {sps_seen:?}",
        h264_offer.name()
    );
    ensure!(
        !feedback || (feedback_sent == 2 && keyframes >= 3),
        "PLI/FIR did not produce recovery keyframes"
    );
    drop(video);
    if let Some(sync) = av_sync {
        sync.verify(Path::new(
            video_path.as_deref().context("missing video output")?,
        ))?;
    }
    Ok(())
}

#[cfg(test)]
mod timing_tests {
    use super::*;
    use str0m::media::{Frequency, MediaTime};

    #[test]
    fn h264_profile_offer_answer_and_sps_parsing() {
        assert_eq!(H264Offer::parse(None).unwrap(), H264Offer::Default);
        assert_eq!(
            H264Offer::parse(Some("constrained_baseline")).unwrap(),
            H264Offer::ConstrainedBaseline
        );
        assert!(H264Offer::parse(Some("high")).is_err());
        assert!(is_constrained_baseline(0x42e01f) && is_constrained_baseline(0x42c01f));
        assert!(!is_constrained_baseline(0x42001f) && !is_constrained_baseline(0x640c34));
        let sdp = "v=0\r\na=rtpmap:108 H264/90000\r\na=rtpmap:109 rtx/90000\r\n\
            a=fmtp:109 apt=108\r\n\
            a=fmtp:108 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n\
            a=rtpmap:111 opus/48000/2\r\n";
        assert_eq!(sdp_h264_formats(sdp), [(Some(0x42e01f), Some(1))]);
        let unit = [
            0, 0, 0, 1, 0x67, 66, 0xc0, 31, 0xda, 0, 0, 1, 0x68, 0xce, 0, 0, 1, 0x65, 0x88,
        ];
        assert_eq!(annexb_sps(&unit), [(66, 0xc0, 31)]);
        assert!(annexb_sps(&[0, 0, 1, 0x65, 0x88]).is_empty());
    }

    #[test]
    fn h264_profile_modes_offer_exact_formats_and_check_answers_and_sps() {
        for (name, offer) in H264Offer::ALL {
            assert_eq!(H264Offer::parse(Some(name)).unwrap(), offer);
            assert_eq!(offer.name(), name);
            let mut rtc = h264_rtc_config(offer).build(Instant::now());
            let mut change = rtc.sdp_api();
            change.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
            let (sdp, _) = change.apply().unwrap();
            let sdp = sdp.to_sdp_string();
            let expected: Vec<_> = offer
                .formats()
                .iter()
                .map(|&(_, _, id)| (id, Some(1)))
                .collect();
            assert_eq!(sdp_h264_formats(&sdp), expected, "{name}: {sdp}");
            if offer == H264Offer::BaselineNoProfile {
                assert!(!sdp.contains("profile-level-id"), "{sdp}");
            }
        }
        assert_eq!(H264Offer::Default.name(), "default");
        assert!(
            H264Offer::parse(Some("high"))
                .unwrap_err()
                .to_string()
                .contains("plain_baseline")
        );
        // Neither offer contains plain High, so it must never be answered.
        let high = [(Some(0x64001f), Some(1))];
        let cb = [(Some(0x42e01f), Some(1))];
        for offer in [
            H264Offer::ConstrainedBaseline,
            H264Offer::ConstrainedHighAndBaseline,
        ] {
            assert!(offer.answer_ok(&cb) && offer.answer_ok(&[(Some(0x42c01f), Some(1))]));
            assert!(!offer.answer_ok(&high) && !offer.answer_ok(&[(Some(0x640c1f), Some(1))]));
            assert!(!offer.answer_ok(&[]) && !offer.answer_ok(&[(Some(0x42e01f), Some(0))]));
        }
        assert!(H264Offer::ConstrainedBaselineAndHigh.answer_ok(&high));
        assert!(!H264Offer::ConstrainedBaselineAndHigh.answer_ok(&cb));
        assert!(H264Offer::PlainBaseline.answer_ok(&[(Some(0x42001f), Some(1))]));
        assert!(!H264Offer::PlainBaseline.answer_ok(&cb));
        assert!(H264Offer::BaselineNoProfile.answer_ok(&[(None, Some(1))]));
        assert!(!H264Offer::BaselineNoProfile.answer_ok(&[(Some(0x42000a), Some(1))]));
        assert!(H264Offer::Default.answer_ok(&[]) && H264Offer::Default.sps_ok(&[]));
        let cb_sps = [(66, 0xc0, 31)];
        let baseline_sps = [(66, 0x80, 31)];
        let high_sps = [(100, 0x00, 31)];
        for (_, offer) in H264Offer::ALL {
            let high = offer == H264Offer::ConstrainedBaselineAndHigh;
            assert_eq!(offer.sps_ok(&cb_sps), !high);
            assert_eq!(offer.sps_ok(&high_sps), high);
            assert!(!offer.sps_ok(&baseline_sps) && !offer.sps_ok(&[]));
        }
    }

    #[test]
    fn calibration_requires_a_finite_offset_and_nonnegative_uncertainty() {
        assert!(ClockCalibration::parse(None, None).unwrap().is_none());
        assert!(ClockCalibration::parse(Some("12"), None).is_err());
        assert!(ClockCalibration::parse(None, Some("2")).is_err());
        for (offset, uncertainty) in [("NaN", "2"), ("1", "inf"), ("1", "-1")] {
            assert!(ClockCalibration::parse(Some(offset), Some(uncertainty)).is_err());
        }
        let calibration = ClockCalibration::parse(Some("-12.5"), Some("0.25"))
            .unwrap()
            .unwrap();
        assert_eq!(calibration.offset_ms, -12.5);
        assert_eq!(calibration.uncertainty_ms, 0.25);
    }

    #[test]
    fn remote_age_uses_signed_wall_offset_and_sr_across_rtp_wrap() {
        let origin = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut clock = SenderClock::default();
        clock
            .observe(
                Some(SenderInfo {
                    ssrc: 1.into(),
                    ntp_time: origin,
                    rtp_time: MediaTime::new(u64::from(u32::MAX - 479), Frequency::FORTY_EIGHT_KHZ),
                    sender_packet_count: 1,
                    sender_octet_count: 100,
                }),
                48_000,
            )
            .unwrap();
        // RTP zero is capture 10 ms after the anchor; actual arrival is 35 ms
        // after capture. Receiver clock may be ahead OR behind the sender.
        for offset_ms in [-250.0, 250.0] {
            let calibration = ClockCalibration {
                offset_ms,
                uncertainty_ms: 2.0,
            };
            let arrival = if offset_ms < 0.0 {
                origin - Duration::from_millis(205)
            } else {
                origin + Duration::from_millis(295)
            };
            assert!((calibration.age_ms(&clock, 0, arrival).unwrap() - 35.0).abs() < 1e-9);
        }
    }

    #[test]
    fn sequence_gaps_merge_late_overlaps_and_keep_multiple_wraps() {
        let mut ranges = [
            (65_534, 65_535),
            (65_538, 131_073),
            (65_536, 65_538),
            (131_079, 196_609),
            (65_534, 65_535),
        ];
        assert_eq!(sequence_gaps(&mut ranges), (1, 5));
        assert_eq!(sequence_gaps(&mut []), (0, 0));
        assert_eq!(sequence_gaps(&mut [(42, 42)]), (0, 0));
        // Continuous RTP sequence numbers do not imply a fixed video cadence.
        assert_eq!(signed_rtp_delta(5_999, u32::MAX - 3_000), 9_000.0);
    }

    #[test]
    fn sender_report_mapping_handles_rtp_wrap_and_delayed_reports() {
        let origin = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut clock = SenderClock::default();
        let mut info = SenderInfo {
            ssrc: 1.into(),
            ntp_time: origin,
            rtp_time: MediaTime::new(u64::from(u32::MAX - 99), Frequency::FORTY_EIGHT_KHZ),
            sender_packet_count: 1,
            sender_octet_count: 100,
        };
        clock.observe(Some(info), 48_000).unwrap();
        assert!((clock.map(380, origin).unwrap() - 0.01).abs() < 1e-12);
        info.ntp_time += Duration::from_millis(10);
        info.rtp_time = MediaTime::new(380, Frequency::FORTY_EIGHT_KHZ);
        clock.observe(Some(info), 48_000).unwrap();
        assert!(clock.map(u32::MAX - 99, origin).unwrap().abs() < 1e-12);
        assert!(clock.max_anchor_error < 1e-12);
    }

    #[test]
    fn pulse_edges_are_paired_without_hiding_interior_loss_or_drift() {
        let flashes = [0.15, 1.15, 2.15, 3.15];
        let beeps = [0.15002, 1.15002, 2.15002, 3.15002];
        assert_eq!(pair_pulses(&flashes, &beeps, 0.0, 3.4).unwrap().len(), 3);
        assert!(pair_pulses(&flashes, &[0.15002, 1.15002, 3.15002], 0.0, 3.4).is_err());
        assert!(pair_pulses(&flashes, &[0.35, 1.35, 2.35, 3.35], 0.0, 3.6).is_err());
    }

    #[test]
    fn audio_gap_evidence_preserves_positions_and_invalidates_partial_pulses() {
        let mut sync = AvSync::default();
        // Start just before RTP wraps; the first contiguous packet after
        // wrapping is not a gap. Observe silence, then a pulse onset.
        let start = u32::MAX - 479;
        sync.audio(start, &[0.0; 960]);
        sync.audio(0, &[0.0; 4800]);
        assert!(sync.audio_gaps.is_empty());
        sync.audio(2400, &[0.25; 960]);
        assert_eq!(sync.pending_beep, Some(2400));
        // The same 60 ms capture hole seen in run 39973821917. It erases
        // this incomplete pulse, not the location of the missing samples.
        sync.audio(5760, &[0.0; 4800]);
        assert_eq!(sync.audio_gaps, [(2880, 5760)]);
        assert!(sync.pending_beep.is_none());
        assert!(sync.beeps.is_empty());
        sync.audio(8160, &[0.25; 960]);
        sync.audio(8640, &[0.0; 4800]);
        assert_eq!(sync.beeps, [8160]);
    }

    #[test]
    fn aggregate_gap_counts_cannot_excuse_missing_or_mistimed_pulses() {
        let flashes = [1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(pair_pulses(&flashes, &flashes, 0.0, 6.0).unwrap().len(), 5);
        // Most pulses present is insufficient without loss attribution.
        assert!(pair_pulses(&flashes, &[1.0, 2.0, 4.0, 5.0], 0.0, 6.0).is_err());
        assert!(pair_pulses(&flashes, &[1.0, 2.0, 3.07, 4.0, 5.0], 0.0, 6.0).is_err());
        assert!(pair_pulses(&flashes, &[1.0, 2.0, 3.0, 3.5, 4.0, 5.0], 0.0, 6.0).is_err());
        assert!(pair_pulses(&flashes, &[1.0], 0.0, 6.0).is_err());
    }
}
