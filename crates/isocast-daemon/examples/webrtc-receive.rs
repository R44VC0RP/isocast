//! A minimal native receiver: normal Client pairing/selection, standard SDP
//! and str0m media. No daemon implementation code. Output is counts, decoded
//! Opus samples and (on the same host clock) packet age, not glass-to-glass.
//! Usage: ISOCAST_ENGINE=HOST:PORT webrtc-receive [seconds] [new-output.h264]
//! ISOCAST_WEBRTC_AV_SYNC_TEST=1 checks synthetic flash/beep capture timestamps
//! through RTCP sender reports; this is not a hardware playout measurement.

use std::io::{BufRead, ErrorKind, Read, Write};
use std::net::UdpSocket;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, ensure};
use isocast_protocol::api::{Method, WhepAnswer, WhepOfferParams};
use str0m::change::SdpAnswer;
use str0m::format::{Codec, CodecExtra};
use str0m::media::{Direction, KeyframeRequestKind, MediaKind};
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
    audio_gaps: u64,
    quiet: usize,
    pending_beep: Option<u32>,
    beeps: Vec<u32>,
}

impl AvSync {
    fn audio(&mut self, time: u32, samples: &[f32]) {
        self.audio_first.get_or_insert(time);
        if self.audio_next.is_some_and(|next| next != time) {
            // Missing samples are not observed silence. Invalidate any pulse
            // spanning the gap, but keep subsequent onsets on packet RTP time.
            self.audio_gaps += 1;
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
                "audio_packet_gaps": self.audio_gaps
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
        let decoded = Command::new("ffmpeg")
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
    let mut config = RtcConfig::new()
        .set_crypto_provider(Arc::new(str0m::crypto::from_feature_flags()))
        .clear_codecs()
        .enable_opus(true, false)
        .enable_h264(true);
    config
        .codec_config()
        .add_h264(102.into(), Some(103.into()), true, 0x640c34);
    let mut rtc = config.build(Instant::now());
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
    let mut video_age_ms = Vec::<f64>::new();
    let mut audio_age_ms = Vec::<f64>::new();
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
                        if let Some(sync) = &mut av_sync {
                            ensure!(
                                data.mid == video_mid && data.time.denom() == 90_000,
                                "unexpected program video track"
                            );
                            sync.video_clock.observe(data.last_sender_info, 90_000)?;
                            sync.video_times.push(data.time.numer() as u32);
                        }
                        video_frames += 1;
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
                let Ok(contents) = (&buffer[..length]).try_into() else {
                    continue;
                };
                rtc.handle_input(Input::Receive(
                    Instant::now(),
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
    println!(
        "{}",
        serde_json::json!({"video_frames":video_frames,"keyframes":keyframes,"feedback_sent":feedback_sent,"audio_packets":audio_packets,"decoded_audio_frames":audio_frames,"audio_peak":peak,"closed":closed,"packet_age_ms_p50":age(&video_age_ms,0.5),"packet_age_ms_p95":age(&video_age_ms,0.95),"audio_age_ms_p50":age(&audio_age_ms,0.5),"audio_age_ms_p95":age(&audio_age_ms,0.95)})
    );
    ensure!(
        video_frames > 0 && audio_frames > 0,
        "no playable H.264/Opus media received"
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
}
