//! WHEP offer/answer semantics over an already authenticated R1 connection.
//! The connection owns the media, including revocation and disconnect.

use super::*;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::Instant;

use isocast_engine::governor::RemoteViewer;
use isocast_engine::record::OpusFeed;
use isocast_protocol::api::{WhepAnswer, WhepOfferParams, frames};
use isocast_protocol::caller::Origin;
use str0m::change::{SdpAnswer, SdpOffer};
use str0m::format::Codec;
use str0m::media::{Direction, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event as RtcEvent, Input, Output, Rtc, RtcConfig};

const MAX_SDP: usize = 64 * 1024;
const HANDSHAKE: Duration = Duration::from_secs(10);
const TICK: Duration = Duration::from_millis(5);
type Track = (Mid, Pt);

fn failed(error: impl std::fmt::Display) -> RpcError {
    RpcError {
        code: RpcError::FAILED,
        message: format!("WebRTC: {error}"),
    }
}

fn invalid(message: &str) -> RpcError {
    RpcError {
        code: RpcError::INVALID_PARAMS,
        message: message.into(),
    }
}

fn unavailable(message: &str) -> RpcError {
    RpcError {
        code: RpcError::WRONG_STATE,
        message: message.into(),
    }
}

fn viewer_reply(request: &Request) -> Response {
    if request.method == Method::WhepClose {
        Response::ok(request.id, serde_json::Value::Null)
    } else {
        Response::err(
            Some(request.id),
            unavailable(
                "only whep_close is accepted on a viewer connection; open another connection for other methods",
            ),
        )
    }
}

fn transient_send(error: &io::Error) -> bool {
    if matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    ) {
        return true;
    }
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::ENOBUFS) {
        return true;
    }
    // WSAENOBUFS, from Winsock (not POSIX's ENOBUFS value).
    #[cfg(windows)]
    if error.raw_os_error() == Some(10055) {
        return true;
    }
    false
}

pub(super) struct Viewer {
    answer: WhepAnswer,
    rtc: Rtc,
    socket: UdpSocket,
    peer: IpAddr,
    local: SocketAddr,
    video: FrameFeed,
    audio: OpusFeed,
    video_track: (Mid, Pt),
    audio_track: (Mid, Pt),
    config: Vec<u8>,
    parameter_sets: Vec<u8>,
    admission: RemoteViewer,
}

pub(super) fn prepare(
    daemon: &Arc<Daemon>,
    params: &WhepOfferParams,
    caller: &Caller,
) -> Result<Viewer, RpcError> {
    let offer = validate_offer(&params.sdp)?;
    prepare_offer(daemon, offer, caller)
}

fn validate_offer(sdp: &str) -> Result<SdpOffer, RpcError> {
    if sdp.len() > MAX_SDP {
        return Err(invalid("params.sdp: at most 65536 bytes"));
    }
    // SDP parsing is authenticated; diagnostics never echo offers or ICE credentials.
    let offer =
        SdpOffer::from_sdp_string(sdp).map_err(|_| invalid("params.sdp: invalid SDP offer"))?;
    let lines: Vec<_> = sdp.lines().collect();
    if lines.iter().filter(|line| line.starts_with("m=")).count() != 2
        || lines
            .iter()
            .filter(|line| line.starts_with("m=video "))
            .count()
            != 1
        || lines
            .iter()
            .filter(|line| line.starts_with("m=audio "))
            .count()
            != 1
        || lines
            .iter()
            .any(|line| matches!(*line, "a=sendonly" | "a=sendrecv" | "a=inactive"))
        || !lines.contains(&"a=recvonly")
        || lines
            .iter()
            .filter(|line| line.starts_with("a=candidate:"))
            .count()
            > 32
        || lines
            .iter()
            .filter(|line| line.starts_with("a=candidate:"))
            .any(|line| !line.contains(" typ host"))
    {
        return Err(invalid(
            "params.sdp: expected one recvonly H.264 video and one recvonly Opus audio track, at most 32 host candidates",
        ));
    }
    Ok(offer)
}

fn prepare_offer(
    daemon: &Arc<Daemon>,
    offer: SdpOffer,
    caller: &Caller,
) -> Result<Viewer, RpcError> {
    let peer = match caller.origin {
        Origin::Local => IpAddr::V4(Ipv4Addr::LOCALHOST),
        Origin::Remote { addr, .. } => addr.ip(),
        Origin::ObsWebsocket { .. } => {
            return Err(invalid(
                "whep_offer requires a dedicated R1 or local connection",
            ));
        }
    };
    if !peer.is_loopback()
        && !isocast_remote::local_address(peer)
        && !isocast_protocol::remote::on_link(peer, &remote::prefixes())
    {
        return Err(invalid(
            "whep_offer: media is limited to loopback or LAN peers",
        ));
    }
    let admission =
        RemoteViewer::acquire().map_err(|reason| unavailable(&format!("whep_offer: {reason}")))?;
    // UDP connect chooses the route without sending a packet. Bind the actual
    // media socket only to that interface, never advertise wildcard addresses.
    let probe = UdpSocket::bind(if peer.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    })
    .map_err(failed)?;
    probe.connect(SocketAddr::new(peer, 9)).map_err(failed)?;
    let socket = UdpSocket::bind(SocketAddr::new(probe.local_addr().map_err(failed)?.ip(), 0))
        .map_err(failed)?;
    let local = socket.local_addr().map_err(failed)?;
    socket.set_read_timeout(Some(TICK)).map_err(failed)?;
    socket.set_write_timeout(Some(TICK)).map_err(failed)?;
    let (_, video) = daemon.frames(&FramesParams {
        target: PROGRAM.into(),
        max_width: Some(960),
        fps: Some(30),
    })?;
    let engine = daemon.ensure_engine()?;
    let audio = engine.opus().map_err(|message| RpcError {
        code: RpcError::FAILED,
        message,
    })?;
    // Negotiate the actual encoder profile/level, never label a High stream Baseline.
    let deadline = Instant::now() + Duration::from_secs(3);
    let config = loop {
        let bytes = video.records.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| invalid("whep_offer: program encoder did not provide H.264 configuration within 3 seconds"))?;
        let record = frames::Record::read(&mut bytes.as_ref())
            .map_err(failed)?
            .ok_or_else(|| invalid("whep_offer: program encoder ended"))?;
        if record.kind == frames::END {
            return Err(invalid("whep_offer: program encoder ended"));
        }
        if record.kind == frames::CONFIG {
            break record.payload;
        }
        if Instant::now() >= deadline {
            return Err(invalid(
                "whep_offer: program encoder configuration timed out",
            ));
        }
    };
    let parameter_sets = avcc_config(&config).map_err(failed)?;
    let profile = u32::from_be_bytes([0, config[1], config[2], config[3]]);
    let mut settings = RtcConfig::new()
        .set_crypto_provider(Arc::new(str0m::crypto::from_feature_flags()))
        .set_send_buffer_audio(20)
        .set_send_buffer_video(256)
        .clear_codecs()
        .enable_opus(true, false);
    settings
        .codec_config()
        .add_h264(96.into(), Some(97.into()), true, profile);
    let mut rtc = settings.build(Instant::now());
    rtc.add_local_candidate(Candidate::host(local, "udp").map_err(failed)?);
    let answer = rtc.sdp_api().accept_offer(offer).map_err(|_| {
        invalid("params.sdp: offer is not compatible with this program's H.264 profile and Opus")
    })?;
    // MediaAdded events are gated on DTLS/SRTP readiness in str0m. Read
    // the negotiated answer now; waiting for those events before replying
    // would make completing the handshake impossible.
    let (video_track, audio_track) = tracks(&mut rtc, &answer, profile)?;
    Ok(Viewer {
        answer: WhepAnswer {
            sdp: answer.to_sdp_string(),
            session: format!("viewer-{}", next_watcher_key()),
        },
        rtc,
        socket,
        peer,
        local,
        video,
        audio,
        video_track,
        audio_track,
        config,
        parameter_sets,
        admission,
    })
}

fn tracks(rtc: &mut Rtc, answer: &SdpAnswer, profile: u32) -> Result<(Track, Track), RpcError> {
    let mut video = None;
    let mut audio = None;
    for line in &answer.media_lines {
        if line.disabled || line.direction() != Direction::SendOnly {
            continue;
        }
        let mid = line.mid();
        let Some(media) = rtc.media(mid) else {
            continue;
        };
        let kind = media.kind();
        let codec = match kind {
            MediaKind::Video => Codec::H264,
            MediaKind::Audio => Codec::Opus,
        };
        let Some(writer) = rtc.writer(mid) else {
            continue;
        };
        if let Some(pt) = writer
            .payload_params()
            .find(|p| p.spec().codec == codec)
            .map(|p| p.pt())
        {
            match kind {
                MediaKind::Video => video = Some((mid, pt)),
                MediaKind::Audio => audio = Some((mid, pt)),
            }
        }
    }
    match (video, audio) {
        (Some(video), Some(audio)) => Ok((video, audio)),
        _ => Err(invalid(&format!(
            "params.sdp: offer and engine H.264 profile_level_id={profile:06x} must negotiate video and Opus audio"
        ))),
    }
}

impl Viewer {
    pub(super) fn run(
        mut self,
        mut reader: BufReader<UnixStream>,
        mut writer: UnixStream,
        daemon: &Arc<Daemon>,
        id: u64,
    ) -> io::Result<()> {
        writer.set_write_timeout(Some(WATCH_WRITE_TIMEOUT))?;
        reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(100)))?;
        writeln!(
            writer,
            "{}",
            serde_json::to_string(&Response::ok(id, serde_json::to_value(&self.answer)?))?
        )?;
        let result = self.media(&mut reader, &mut writer, daemon);
        if result.is_err() {
            // Detailed SDP/DTLS parser errors may contain credential material.
            isocast_diagnostics::diagnostic!("warn"; "WebRTC program viewer ended after a media transport error");
        }
        result
    }

    fn media(
        &mut self,
        reader: &mut BufReader<UnixStream>,
        writer: &mut UnixStream,
        daemon: &Arc<Daemon>,
    ) -> io::Result<()> {
        let began = Instant::now();
        let clock = now().as_nanos();
        let mut connected = false;
        let mut synced = false;
        let mut wanted_at = Instant::now();
        let mut buffer = [0u8; 2048];
        while self.rtc.is_alive()
            && !writer.aborted()
            && !hung_up(writer)
            && !daemon.shutting_down.load(Ordering::Acquire)
            && !self.admission.should_shed()
        {
            if wanted_at.elapsed() >= Duration::from_secs(1) {
                wanted_at = Instant::now();
                daemon.state.lock().wanted_at = Some(wanted_at);
            }
            if !connected && began.elapsed() > HANDSHAKE {
                isocast_diagnostics::diagnostic!("warn"; "WebRTC program viewer handshake timed out after 10 seconds");
                return Ok(());
            }
            if !reader.buffer().is_empty() || isocast_ipc::readable(reader.get_ref())? {
                let mut line = String::new();
                if (&mut *reader).take(1025).read_line(&mut line)? == 0 {
                    return Ok(());
                }
                if line.len() > 1024 {
                    let reply = Response::err(
                        None,
                        RpcError {
                            code: RpcError::INVALID_REQUEST,
                            message: "viewer request exceeds 1024 bytes".into(),
                        },
                    );
                    writeln!(writer, "{}", serde_json::to_string(&reply)?)?;
                    return Ok(());
                }
                let (reply, close) = match serde_json::from_str::<Request>(&line) {
                    Ok(request) if request.jsonrpc == "2.0" => (
                        viewer_reply(&request), request.method == Method::WhepClose,
                    ),
                    Ok(request) => (Response::err(Some(request.id), RpcError {
                        code: RpcError::INVALID_REQUEST,
                        message: "jsonrpc: expected 2.0".into(),
                    }), false),
                    Err(_) => (Response::err(id_of(&line), RpcError {
                        code: request_error_code(&line),
                        message: "invalid viewer request; only whep_close is accepted on this connection".into(),
                    }), false),
                };
                writeln!(writer, "{}", serde_json::to_string(&reply)?)?;
                if close {
                    return Ok(());
                }
            }
            // Drain after each mutation, as required by str0m's sans-I/O API.
            let timeout = loop {
                match self.rtc.poll_output().map_err(io::Error::other)? {
                    Output::Timeout(timeout) => break timeout,
                    Output::Transmit(packet) => {
                        // A large IDR can produce a burst of datagrams. Revocation
                        // or recording pressure must not wait for that burst.
                        if writer.aborted() || self.admission.should_shed() {
                            return Ok(());
                        }
                        // Even authenticated SDP cannot make this an arbitrary UDP proxy.
                        if packet.destination.ip() == self.peer
                            && let Err(error) =
                                self.socket.send_to(&packet.contents, packet.destination)
                            && !transient_send(&error)
                        {
                            return Err(error);
                        }
                    }
                    Output::Event(RtcEvent::KeyframeRequest(_)) => self.video.request_keyframe(),
                    _ => {}
                }
            };
            if self.rtc.is_connected() && !connected {
                connected = true;
                self.video.connected();
            }
            if connected {
                let video = match self.video.records.try_recv() {
                    Ok(bytes) => Some(bytes),
                    Err(mpsc::TryRecvError::Empty) => None,
                    Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
                };
                if let Some(bytes) = video {
                    let record = frames::Record::read(&mut bytes.as_ref())?
                        .ok_or_else(|| io::Error::other("empty frame"))?;
                    match record.kind {
                        frames::END => return Ok(()),
                        frames::CONFIG if record.payload != self.config => return Ok(()),
                        frames::KEY_FRAME | frames::FRAME => {
                            if record.kind == frames::KEY_FRAME {
                                synced = true;
                            }
                            if synced {
                                let mut annex = if record.kind == frames::KEY_FRAME {
                                    self.parameter_sets.clone()
                                } else {
                                    Vec::new()
                                };
                                avcc_frame(&record.payload, &mut annex)?;
                                self.write(self.video_track, record.seq, &annex, began, clock)?;
                                continue;
                            }
                        }
                        _ => {}
                    }
                }
                let audio = match self.audio.packets.try_recv() {
                    Ok(packet) => Some(packet),
                    Err(mpsc::TryRecvError::Empty) => None,
                    Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
                };
                if let Some(packet) = audio {
                    self.write(
                        self.audio_track,
                        packet.pts.as_nanos().max(0) as u64,
                        &packet.bytes,
                        began,
                        clock,
                    )?;
                    continue;
                }
            } else {
                // Waiting for DTLS must never retain a seconds-old media backlog.
                loop {
                    match self.video.records.try_recv() {
                        Ok(bytes) => {
                            let record = frames::Record::read(&mut bytes.as_ref())?
                                .ok_or_else(|| io::Error::other("empty frame"))?;
                            if record.kind == frames::END
                                || (record.kind == frames::CONFIG && record.payload != self.config)
                            {
                                return Ok(());
                            }
                        }
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
                    }
                }
                loop {
                    match self.audio.packets.try_recv() {
                        Ok(_) => {}
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
                    }
                }
            }
            let wait = timeout.saturating_duration_since(Instant::now()).min(TICK);
            if wait.is_zero() {
                self.rtc
                    .handle_input(Input::Timeout(Instant::now()))
                    .map_err(io::Error::other)?;
                continue;
            }
            self.socket.set_read_timeout(Some(wait))?;
            match self.socket.recv_from(&mut buffer) {
                Ok((length, source)) if source.ip() == self.peer => {
                    let Ok(contents) = (&buffer[..length]).try_into() else {
                        continue;
                    };
                    self.rtc
                        .handle_input(Input::Receive(
                            Instant::now(),
                            Receive {
                                proto: Protocol::Udp,
                                source,
                                destination: self.local,
                                contents,
                            },
                        ))
                        .map_err(io::Error::other)?;
                }
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) =>
                {
                    self.rtc
                        .handle_input(Input::Timeout(Instant::now()))
                        .map_err(io::Error::other)?;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn write(
        &mut self,
        track: (Mid, Pt),
        pts: u64,
        bytes: &[u8],
        began: Instant,
        clock: i64,
    ) -> io::Result<()> {
        let wall = if pts >= clock.max(0) as u64 {
            began + Duration::from_nanos(pts - clock.max(0) as u64)
        } else {
            began
                .checked_sub(Duration::from_nanos(clock.max(0) as u64 - pts))
                .unwrap_or(began)
        };
        if let Some(writer) = self.rtc.writer(track.0) {
            writer
                .write(track.1, wall, MediaTime::from_micros(pts / 1000), bytes)
                .map_err(io::Error::other)?;
        }
        Ok(())
    }
}

fn avcc_config(bytes: &[u8]) -> io::Result<Vec<u8>> {
    if bytes.len() < 7 || bytes[0] != 1 || bytes[4] & 3 != 3 {
        return Err(io::Error::other("invalid AVC configuration"));
    }
    let mut output = Vec::new();
    let mut rest = &bytes[6..];
    let mut count = bytes[5] & 31;
    for group in 0..2 {
        if count == 0 {
            return Err(io::Error::other("missing AVC parameter sets"));
        }
        for _ in 0..count {
            if rest.len() < 2 {
                return Err(io::Error::other("truncated AVC parameter set"));
            }
            let length = u16::from_be_bytes([rest[0], rest[1]]) as usize;
            rest = &rest[2..];
            if length == 0 || rest.len() < length {
                return Err(io::Error::other("truncated AVC parameter set"));
            }
            output.extend_from_slice(&[0, 0, 0, 1]);
            output.extend_from_slice(&rest[..length]);
            rest = &rest[length..];
        }
        if group == 0 {
            count = *rest
                .first()
                .ok_or_else(|| io::Error::other("missing PPS count"))?;
            rest = &rest[1..];
        }
    }
    Ok(output)
}

fn avcc_frame(mut bytes: &[u8], output: &mut Vec<u8>) -> io::Result<()> {
    while !bytes.is_empty() {
        if bytes.len() < 4 {
            return Err(io::Error::other("truncated AVC length"));
        }
        let length = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
        bytes = &bytes[4..];
        if length == 0 || bytes.len() < length {
            return Err(io::Error::other("truncated AVC NAL"));
        }
        output.extend_from_slice(&[0, 0, 0, 1]);
        output.extend_from_slice(&bytes[..length]);
        bytes = &bytes[length..];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn other_viewer_methods_receive_a_json_rpc_error() {
        let reply = serde_json::to_value(viewer_reply(&Request::new(42, Method::Status))).unwrap();
        assert_eq!(reply["jsonrpc"], "2.0");
        assert_eq!(reply["id"], 42);
        assert_eq!(reply["error"]["code"], RpcError::WRONG_STATE);
        assert_eq!(
            reply["error"]["message"],
            "only whep_close is accepted on a viewer connection; open another connection for other methods"
        );
        let close =
            serde_json::to_value(viewer_reply(&Request::new(43, Method::WhepClose))).unwrap();
        assert_eq!(close["id"], 43);
        assert!(close["result"].is_null());
        assert!(close.get("error").is_none());
    }

    #[test]
    fn accepts_standard_recvonly_offer_and_rejects_wrong_shapes() {
        let mut config = RtcConfig::new()
            .set_crypto_provider(Arc::new(str0m::crypto::from_feature_flags()))
            .clear_codecs()
            .enable_opus(true, false)
            .enable_h264(true);
        config
            .codec_config()
            .add_h264(102.into(), Some(103.into()), true, 0x640c34);
        let mut rtc = config.build(Instant::now());
        rtc.add_local_candidate(
            Candidate::host("127.0.0.1:50000".parse().unwrap(), "udp").unwrap(),
        );
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
        let (offer, _) = change.apply().unwrap();
        let offer = offer.to_sdp_string();
        assert!(validate_offer(&offer).is_ok());
        // The receiver offers High via enable_h264 and Constrained High
        // explicitly. Both the preferred encoder profile and fallback work.
        for profile in [0x640c1f, 0x64001f] {
            let mut config = RtcConfig::new()
                .set_crypto_provider(Arc::new(str0m::crypto::from_feature_flags()))
                .clear_codecs()
                .enable_opus(true, false);
            config
                .codec_config()
                .add_h264(96.into(), Some(97.into()), true, profile);
            let mut sender = config.build(Instant::now());
            let answer = sender
                .sdp_api()
                .accept_offer(validate_offer(&offer).unwrap())
                .unwrap();
            assert!(!sender.is_connected());
            assert!(
                tracks(&mut sender, &answer, profile).is_ok(),
                "both encoder profiles must negotiate before DTLS"
            );
        }
        assert!(validate_offer(&offer.replace("a=recvonly", "a=sendrecv")).is_err());
        assert!(validate_offer(&offer.replace("typ host", "typ relay")).is_err());
        assert!(
            validate_offer(
                &(offer.clone() + "m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n")
            )
            .is_err()
        );
        assert!(validate_offer(&" ".repeat(MAX_SDP + 1)).is_err());
        assert!(validate_offer("not an SDP offer").is_err());
    }

    #[test]
    fn avcc_converts_lengths_and_rejects_truncation() {
        let mut bytes = Vec::new();
        avcc_frame(&[0, 0, 0, 2, 0x65, 7, 0, 0, 0, 1, 9], &mut bytes).unwrap();
        assert_eq!(bytes, [0, 0, 0, 1, 0x65, 7, 0, 0, 0, 1, 9]);
        for bad in [&[0, 0][..], &[0, 0, 0, 0], &[0, 0, 0, 2, 5]] {
            assert!(avcc_frame(bad, &mut Vec::new()).is_err());
        }
    }

    #[test]
    fn configuration_requires_complete_parameter_sets() {
        let bytes = [1, 100, 12, 31, 255, 225, 0, 2, 0x67, 1, 1, 0, 1, 0x68];
        assert_eq!(
            avcc_config(&bytes).unwrap(),
            [0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68]
        );
        for length in 0..bytes.len() {
            assert!(avcc_config(&bytes[..length]).is_err());
        }
    }
}
