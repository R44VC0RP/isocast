//! A minimal native receiver: normal Client pairing/selection, standard SDP
//! and str0m media. No daemon implementation code. Output is counts, decoded
//! Opus samples and (on the same host clock) packet age, not glass-to-glass.
//! Usage: ISOCAST_ENGINE=HOST:PORT webrtc-receive [seconds] [new-output.h264]

use std::io::{BufRead, ErrorKind, Read, Write};
use std::net::UdpSocket;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use isocast_protocol::api::{Method, WhepAnswer, WhepOfferParams};
use str0m::change::SdpAnswer;
use str0m::format::{Codec, CodecExtra};
use str0m::media::{Direction, KeyframeRequestKind, MediaKind};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, RtcConfig};

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
    let mut video = args
        .next()
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
    change.add_media(
        MediaKind::Video,
        Direction::RecvOnly,
        Some("program".into()),
        None,
        None,
    );
    change.add_media(
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
    Ok(())
}
