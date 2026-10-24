//! The engine's program as WebRTC (`whep_offer`, docs/server-protocol.md):
//! H.264 and Opus over UDP, negotiated on an authenticated R1 connection of
//! its own that owns the viewer (closing it ends the viewer; the engine
//! closing it, on an encoder reconfigure or a revocation, means offer
//! again). str0m, the engine's own WebRTC stack, runs sans-I/O on a thread
//! here: host candidates only on the interface that reaches the engine, no
//! STUN or TURN.
//!
//! Pictures come out as [`Intake`] at the time the viewer's
//! [`Playout`](super::sync::Playout) gives them, so they line up with the
//! sound (RTCP Sender Reports map both streams to the engine's wallclock).
//! The sound is decoded here and played by [`super::sound::Sound`].

use std::io::{BufRead, ErrorKind};
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use isocast_protocol::api::WhepOfferParams;
use str0m::change::SdpAnswer;
use str0m::format::Codec;
use str0m::media::{Direction, KeyframeRequestKind, MediaData, MediaKind};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc, RtcConfig};

use super::sound::{CHANNELS, RATE, Sound};
use super::sync::{Kind, Playout, Report};
use super::{Intake, ParameterSets, Source, from_annex_b};

/// How long a picture takes to reach the screen after the window gets it
/// (frames the window's layer keeps queued: about two refreshes; measured
/// 37 ms for the editor's sound on macOS, src/sound.rs).
const PICTURE_LATENCY: f64 = 0.037;
/// With no picture this long after the offer, UDP isn't getting through
/// (a firewall): the caller uses the `frames` stream instead.
const FIRST_PICTURE: Duration = Duration::from_secs(5);
/// Pictures waiting for their time (a few hundred ms of sound delay at
/// 60 fps, and some).
const QUEUED: usize = 64;

/// A picture (or its parameter sets) and when to show it.
type Timed = (Instant, Intake);

pub struct WhepSource {
    items: Receiver<Result<Timed>>,
    pending: Option<Timed>,
    signaling: isocast_ipc::Stream,
    stop: Arc<AtomicBool>,
    ended: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    opened: Instant,
    pictured: bool,
    link: Option<u64>,
}

impl WhepSource {
    /// Offer to watch the program on `client`'s connection (another
    /// computer's engine), playing its sound here too while `sound` is set
    /// (it can change while watching: the speaker on the monitor).
    pub fn open(client: isocast_client::Client, sound: Arc<AtomicBool>) -> Result<Self> {
        let engine = client
            .peer_addr()
            .context("WebRTC video is for another computer's engine")?;
        let socket = UdpSocket::bind(SocketAddr::new(route_to(engine.ip())?, 0))
            .context("could not open a UDP port for the engine's video")?;
        receive_buffer(&socket);
        let local = socket.local_addr()?;
        let mut config = RtcConfig::new()
            .set_crypto_provider(Arc::new(str0m::crypto::from_feature_flags()))
            .clear_codecs()
            .enable_opus(true, false)
            .enable_h264(true)
            // Don't hold sound back waiting for a lost packet's
            // retransmission (str0m's default waits up to 15 packets, 150 ms:
            // the output then runs short). Opus conceals the gap instead
            // (`Viewer::sound`).
            .set_reordering_size_audio(SOUND_HOLD_BACK);
        // Constrained High level 5.2 as well as str0m's defaults: the
        // engine's Macs send it (the SPS agrees with the answer's profile).
        config
            .codec_config()
            .add_h264(102.into(), Some(103.into()), true, 0x640c34);
        let mut rtc = config.build(Instant::now());
        rtc.add_local_candidate(Candidate::host(local, "udp")?);
        let mut change = rtc.sdp_api();
        for kind in [MediaKind::Video, MediaKind::Audio] {
            change.add_media(
                kind,
                Direction::RecvOnly,
                Some("program".into()),
                None,
                None,
            );
        }
        let (offer, pending) = change.apply().context("no SDP offer")?;
        let (answer, mut reader) = client.whep(WhepOfferParams {
            sdp: offer.to_sdp_string(),
        })?;
        rtc.sdp_api()
            .accept_answer(pending, SdpAnswer::from_sdp_string(&answer.sdp)?)
            .context("the engine's SDP answer")?;
        let signaling = reader.get_ref().try_clone()?;
        // The call's own read timeout (30 s) would end a quiet viewer: the
        // signaling connection carries nothing until the engine closes it.
        reader.get_ref().set_read_timeout(None)?;
        // The engine closing the signaling connection ends the viewer.
        let ended = Arc::new(AtomicBool::new(false));
        thread::Builder::new()
            .name("remote webrtc signaling".into())
            .spawn({
                let ended = ended.clone();
                move || {
                    let mut line = String::new();
                    loop {
                        match reader.read_line(&mut line) {
                            Ok(0) => break,
                            Ok(_) => line.clear(),
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    ErrorKind::TimedOut
                                        | ErrorKind::WouldBlock
                                        | ErrorKind::Interrupted
                                ) => {}
                            Err(_) => break,
                        }
                    }
                    ended.store(true, Ordering::SeqCst);
                }
            })?;
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, items) = sync_channel(QUEUED);
        let playout = Playout::default();
        let thread = thread::Builder::new().name("remote webrtc".into()).spawn({
            let (stop, ended) = (stop.clone(), ended.clone());
            move || {
                let mut receiver = Viewer {
                    rtc,
                    socket,
                    local,
                    tx,
                    playout,
                    sound: None,
                    wants_sound: sound,
                    sound_failed: false,
                    restarted: None,
                    sets: None,
                    video_report: None,
                    audio_report: None,
                    opus: None,
                    pcm: vec![0f32; 5760 * CHANNELS],
                    pictures: 0,
                    arrivals: Vec::new(),
                    gaps: 0,
                    sound_gaps: 0,
                    last_sound_rtp: None,
                    sound_spacing_ms: 0.0,
                    sound_packet: 480,
                    concealed: 0,
                };
                let result = receiver.run(&stop);
                ended.store(true, Ordering::SeqCst);
                if let Err(error) = result {
                    let _ = receiver.tx.try_send(Err(error));
                }
            }
        })?;
        let (up, generation) = crate::daemon::link();
        Ok(Self {
            items,
            pending: None,
            signaling,
            stop,
            ended,
            thread: Some(thread),
            opened: Instant::now(),
            pictured: false,
            link: up.then_some(generation),
        })
    }

    /// Nothing new: fine unless the viewer or the window's link ended.
    fn idle(&mut self) -> Result<()> {
        if self.ended.load(Ordering::SeqCst) {
            bail!("the engine ended the WebRTC viewer");
        }
        if !self.pictured && self.opened.elapsed() > FIRST_PICTURE {
            bail!(
                "no WebRTC picture {} s after the offer (UDP blocked?)",
                FIRST_PICTURE.as_secs()
            );
        }
        let (up, generation) = crate::daemon::link();
        match self.link {
            None if up => self.link = Some(generation),
            Some(began) if !up || generation != began => bail!("lost the link to the engine"),
            _ => {}
        }
        Ok(())
    }
}

impl Drop for WhepSource {
    fn drop(&mut self) {
        // Hanging up is WHEP's DELETE: the engine ends the viewer.
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.signaling.shutdown(std::net::Shutdown::Both);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Source for WhepSource {
    fn next(&mut self, wait: Duration) -> Result<Option<Intake>> {
        let deadline = Instant::now() + wait;
        if self.pending.is_none() {
            match self.items.recv_timeout(wait) {
                Ok(item) => self.pending = Some(item?),
                Err(RecvTimeoutError::Timeout) => {
                    self.idle()?;
                    return Ok(None);
                }
                Err(RecvTimeoutError::Disconnected) => {
                    bail!("the engine ended the WebRTC viewer")
                }
            }
        }
        let due = self.pending.as_ref().map(|(due, _)| *due).unwrap();
        let now = Instant::now();
        if due > now {
            thread::sleep(due.min(deadline).saturating_duration_since(now));
            if due > deadline {
                return Ok(None);
            }
        }
        let (_, intake) = self.pending.take().unwrap();
        if matches!(intake, Intake::Unit(_)) {
            self.pictured = true;
        }
        Ok(Some(intake))
    }

    /// The engine paces WebRTC itself (the program's rate, at most 30 fps
    /// for this rung).
    fn set_fps(&mut self, _fps: u32) -> Result<()> {
        Ok(())
    }
}

/// Sound packets str0m may hold back for reordering: 30 ms at the engine's
/// 10 ms packets. Sound is scheduled from its packets' arrival
/// (`network_time`), so a held-back packet reaches the queue up to 30 ms
/// later than its schedule assumes; the playout margin (`sync::MARGIN`,
/// 40 ms) absorbs that.
const SOUND_HOLD_BACK: usize = 3;
/// The most lost sound packets concealed at once (longer gaps start the
/// queue again).
const MAX_CONCEALED: u32 = 5;

/// How many sound packets of `packet` RTP ticks were lost between RTP
/// times `last` and `now` (wrapping): none for the next one in line, a
/// late or repeated one, or a jump too big to be a gap.
pub(crate) fn lost_between(last: u32, now: u32, packet: u32) -> u32 {
    if packet == 0 {
        return 0;
    }
    let ticks = now.wrapping_sub(last) as i32;
    if ticks <= packet as i32 {
        return 0;
    }
    // To the nearest packet: RTP times can be a tick or two off.
    let lost = (ticks as u32 + packet / 2) / packet - 1;
    if lost > MAX_CONCEALED { 0 } else { lost }
}

/// A 4 MB receive buffer on Windows: room for a burst of the engine's
/// packets while this thread is busy (Windows gives a UDP socket 64 KB).
/// It didn't change the ~1 in 1,000 sound packets lost on the way to
/// exon-desk (concealed instead, see [`SOUND_HOLD_BACK`]); kept as cheap
/// headroom.
const RECEIVE_BUFFER: i32 = 4 << 20;

#[cfg(windows)]
fn receive_buffer(socket: &UdpSocket) {
    use std::os::windows::io::AsRawSocket;
    use windows::Win32::Networking::WinSock::{SO_RCVBUF, SOCKET, SOL_SOCKET, setsockopt};
    let size = RECEIVE_BUFFER.to_ne_bytes();
    let result = unsafe {
        setsockopt(
            SOCKET(socket.as_raw_socket() as usize),
            SOL_SOCKET,
            SO_RCVBUF,
            Some(&size),
        )
    };
    if result != 0 {
        isocast_diagnostics::diagnostic!("warn"; "video: could not enlarge the UDP receive buffer");
    }
}

#[cfg(not(windows))]
fn receive_buffer(_socket: &UdpSocket) {
    // macOS's default (786 KB) already holds the bursts.
    let _ = RECEIVE_BUFFER;
}

/// The local address that reaches `engine`: the R1 viewer's UDP address
/// must be the one its TCP connection comes from.
fn route_to(engine: IpAddr) -> Result<IpAddr> {
    let any: IpAddr = match engine {
        IpAddr::V4(_) => [0, 0, 0, 0].into(),
        IpAddr::V6(_) => [0u16; 8].into(),
    };
    let probe = UdpSocket::bind(SocketAddr::new(any, 0))?;
    // Connecting a UDP socket sends nothing; it only picks the route.
    probe.connect(SocketAddr::new(engine, 9))?;
    Ok(probe.local_addr()?.ip())
}

/// The thread's state: str0m, the socket, the playout clock and sound.
struct Viewer {
    rtc: Rtc,
    socket: UdpSocket,
    local: SocketAddr,
    tx: SyncSender<Result<Timed>>,
    playout: Playout,
    sound: Option<Sound>,
    /// Whether sound should play now (the monitor's speaker).
    wants_sound: Arc<AtomicBool>,
    /// The output couldn't start: picture only until the speaker is
    /// turned off and on.
    sound_failed: bool,
    /// When a stopped output was last started again.
    restarted: Option<Instant>,
    sets: Option<ParameterSets>,
    video_report: Option<Report>,
    audio_report: Option<Report>,
    opus: Option<opus2::Decoder>,
    pcm: Vec<f32>,
    pictures: u64,
    /// Pictures' arrival after they were drawn, in ms by the system clocks,
    /// since the last log line.
    arrivals: Vec<f64>,
    /// Pictures after lost packets (RTP sequence gaps), in all.
    gaps: u64,
    /// Sound packets after lost ones, the last sound packet's RTP time, and
    /// the widest spacing between two since the last log line, in ms.
    sound_gaps: u64,
    last_sound_rtp: Option<u32>,
    sound_spacing_ms: f64,
    /// Frames in a sound packet (the last one decoded), and lost packets
    /// concealed.
    sound_packet: usize,
    concealed: u64,
}

impl Viewer {
    fn run(&mut self, stop: &AtomicBool) -> Result<()> {
        let mut buffer = vec![0u8; 2048];
        let mut timeout_set = None;
        while !stop.load(Ordering::SeqCst) && self.rtc.is_alive() {
            let timeout = match self.rtc.poll_output()? {
                Output::Timeout(timeout) => timeout,
                Output::Transmit(packet) => {
                    self.socket.send_to(&packet.contents, packet.destination)?;
                    continue;
                }
                Output::Event(Event::MediaData(data)) => {
                    self.media(data)?;
                    continue;
                }
                Output::Event(_) => continue,
            };
            let wait = timeout
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(20));
            if wait.is_zero() {
                self.rtc.handle_input(Input::Timeout(Instant::now()))?;
                continue;
            }
            // Whole milliseconds, set only when they change.
            let wait = Duration::from_millis(wait.as_millis().max(1) as u64);
            if timeout_set != Some(wait) {
                self.socket.set_read_timeout(Some(wait))?;
                timeout_set = Some(wait);
            }
            match self.socket.recv_from(&mut buffer) {
                Ok((length, source)) => {
                    let Ok(contents) = (&buffer[..length]).try_into() else {
                        continue;
                    };
                    self.rtc.handle_input(Input::Receive(
                        Instant::now(),
                        Receive {
                            proto: Protocol::Udp,
                            source,
                            destination: self.local,
                            contents,
                        },
                    ))?;
                }
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    self.rtc.handle_input(Input::Timeout(Instant::now()))?
                }
                // Windows reports an ICMP "port unreachable" for an earlier
                // datagram this way: a lost packet, not a broken socket.
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::ConnectionReset | ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    /// When `data`, drawn at its sender time, plays here: none until its
    /// stream's first Sender Report (then picture and sound share a clock).
    fn schedule(&mut self, data: &MediaData, kind: Kind) -> Option<f64> {
        let (report, rate) = match kind {
            Kind::Video => (&mut self.video_report, 90_000),
            Kind::Audio => (&mut self.audio_report, RATE),
        };
        if let Some(info) = &data.last_sender_info {
            let new = Report::new(info.rtp_time.numer(), info.ntp_time);
            // An SR with no real wallclock (NTP 0, or before 2001) maps
            // nothing; keep the last good one.
            if new.wallclock > 1.0e9 {
                *report = Some(new);
            }
        }
        let sender = report.as_ref()?.wallclock_of(data.time.numer(), rate);
        let arrival = self.playout.seconds(data.network_time);
        if kind == Kind::Video {
            // How long after the engine drew it a picture arrived, by the
            // two machines' system clocks (as good as their sync): the log's
            // latency figure, nothing schedules by it.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64();
            let since_receipt = data.network_time.elapsed().as_secs_f64();
            self.arrivals.push((now - since_receipt - sender) * 1e3);
        }
        self.playout.schedule(kind, sender, arrival)
    }

    fn media(&mut self, data: MediaData) -> Result<()> {
        match data.params.spec().codec {
            Codec::H264 => self.picture(data),
            Codec::Opus => {
                self.sound(data);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn picture(&mut self, data: MediaData) -> Result<()> {
        self.follow_speaker();
        // With sound playing, at its time (with the sound); without, as
        // soon as it's decoded. The schedule still learns the transits, so
        // turning sound on lines up at once. Too late (a stall): now.
        let at = self.schedule(&data, Kind::Video);
        let due = match (&self.sound, at) {
            (Some(_), Some(at)) => self.playout.instant(at - PICTURE_LATENCY),
            _ => Instant::now(),
        };
        self.pictures += 1;
        if self.pictures % 150 == 1 {
            let mut arrivals = std::mem::take(&mut self.arrivals);
            arrivals.sort_by(f64::total_cmp);
            let pick = |q: f64| arrivals.get(((arrivals.len().max(1) - 1) as f64 * q) as usize);
            if let (Some(median), Some(p95)) = (pick(0.5), pick(0.95)) {
                isocast_diagnostics::diagnostic!("debug"; "remote video: pictures arrive {median:.1} ms after drawn (median; 95% within {p95:.1} ms; by the system clocks); {} pictures, {} after lost packets", self.pictures, self.gaps);
            }
            if let Some((wait, skew)) = self.playout.waits() {
                isocast_diagnostics::diagnostic!("debug"; "remote video: sound arrives {:.0} ms after the picture (jitter {:.0} ms; {} sound packets after lost ones, {} concealed, widest spacing {:.0} ms); pictures wait {:.0} ms to play with it{}", skew * 1e3, self.playout.sound_jitter().unwrap_or_default() * 1e3, self.sound_gaps, self.concealed, std::mem::take(&mut self.sound_spacing_ms), wait * 1e3, self.sound.as_ref().map(|s| format!("; {}", s.report())).unwrap_or_default());
            }
        }
        let (sets, unit) = from_annex_b(&data.data, data.time.as_micros() * 1000);
        if !data.contiguous {
            self.gaps += 1;
        }
        if !data.contiguous && !unit.keyframe {
            // Packets were lost: the pictures until the next keyframe
            // won't decode; ask for one now (PLI).
            if let Some(mut writer) = self.rtc.writer(data.mid) {
                let _ = writer.request_keyframe(None, KeyframeRequestKind::Pli);
            }
        }
        if let Some(sets) = sets
            && self.sets.as_ref() != Some(&sets)
            // Remembered only once sent: a dropped one goes again with the
            // next keyframe.
            && self.send((due, Intake::Config(sets.clone())))?
        {
            self.sets = Some(sets);
        }
        if !unit.data.is_empty() {
            self.send((due, Intake::Unit(unit)))?;
        }
        Ok(())
    }

    /// Hand a picture to the window: false when its queue is full (the
    /// window is behind; pictures are dropped until the next keyframe gets
    /// through, and the decoders skip broken ones).
    fn send(&self, item: Timed) -> Result<bool> {
        match self.tx.try_send(Ok(item)) {
            Ok(()) => Ok(true),
            Err(TrySendError::Full(_)) => Ok(false),
            Err(TrySendError::Disconnected(_)) => bail!("the viewer was closed"),
        }
    }

    /// Start or stop the sound as the speaker says. Stopped, sound no
    /// longer holds the picture back.
    fn follow_speaker(&mut self) {
        let speaker = self.wants_sound.load(Ordering::SeqCst);
        if !speaker {
            // Turning the speaker off and on tries a failed output again.
            self.sound_failed = false;
        }
        // An output that stopped (its device went away): start again on
        // the default one, at most once a second.
        if self.sound.as_ref().is_some_and(|sound| !sound.alive())
            && self
                .restarted
                .is_none_or(|at| at.elapsed() > Duration::from_secs(1))
        {
            isocast_diagnostics::diagnostic!("info"; "video: the sound output stopped; starting it again");
            self.sound = None;
            self.restarted = Some(Instant::now());
        }
        let wanted = speaker && !self.sound_failed;
        match (wanted, self.sound.is_some()) {
            (true, false) => match Sound::start(self.playout.instant(0.0)) {
                Ok(sound) => self.sound = Some(sound),
                Err(error) => {
                    isocast_diagnostics::diagnostic!("warn"; "video: no sound output, picture only: {error:#}");
                    self.sound_failed = true;
                }
            },
            (false, true) => {
                self.sound = None;
                self.opus = None;
                self.playout.forget_sound();
            }
            _ => {}
        }
    }

    fn sound(&mut self, data: MediaData) {
        self.follow_speaker();
        // Sound packets lost on the way (RTP sequence gaps), and the
        // longest time between two in sender time (for the log).
        if !data.contiguous {
            self.sound_gaps += 1;
        }
        let rtp = data.time.numer() as u32;
        let mut lost = 0;
        if let Some(last) = self.last_sound_rtp {
            let ms = f64::from(rtp.wrapping_sub(last) as i32) / 48.0;
            self.sound_spacing_ms = self.sound_spacing_ms.max(ms);
            lost = lost_between(last, rtp, self.sound_packet as u32);
        }
        self.last_sound_rtp = Some(rtp);
        let Some(lead) = self.sound.as_ref().map(Sound::lead) else {
            return;
        };
        self.playout.set_extra(lead);
        // Too late (a stall): still decoded, to keep the decoder's state,
        // but not played.
        let at = self.schedule(&data, Kind::Audio);
        let decoder = match &mut self.opus {
            Some(decoder) => decoder,
            None => match opus2::Decoder::new(RATE, opus2::Channels::Stereo) {
                Ok(decoder) => self.opus.insert(decoder),
                Err(error) => {
                    isocast_diagnostics::diagnostic!("warn"; "video: no Opus decoder: {error}");
                    self.sound = None;
                    return;
                }
            },
        };
        // Lost packets (not waited for): Opus's concealment fills their
        // time, just before this one.
        let packet = self.sound_packet;
        for k in (1..=lost).rev() {
            let samples = &mut self.pcm[..packet * CHANNELS];
            if let Ok(frames) = decoder.decode_float(&[], samples, false)
                && let (Some(sound), Some(at)) = (&self.sound, at)
            {
                let back = f64::from(k) * packet as f64 / f64::from(RATE);
                sound.push(at - back, &self.pcm[..frames * CHANNELS]);
                self.concealed += 1;
            }
        }
        match decoder.decode_float(&data.data, &mut self.pcm, false) {
            Ok(frames) => {
                self.sound_packet = frames;
                if let (Some(sound), Some(at)) = (&self.sound, at) {
                    sound.push(at, &self.pcm[..frames * CHANNELS]);
                }
            }
            Err(error) => {
                isocast_diagnostics::diagnostic!("debug"; "video: skipped a sound packet: {error}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lost_sound_packets_are_counted_from_rtp_times() {
        // 10 ms packets: 480 ticks at 48 kHz.
        assert_eq!(lost_between(1000, 1480, 480), 0);
        assert_eq!(lost_between(1000, 1960, 480), 1);
        assert_eq!(lost_between(1000, 1000 + 480 * 4, 480), 3);
        // Across the 32-bit wrap.
        assert_eq!(lost_between(u32::MAX - 100, 860, 480), 1);
        // Late, repeated, or a jump too long to conceal.
        assert_eq!(lost_between(1960, 1480, 480), 0);
        assert_eq!(lost_between(1000, 1000, 480), 0);
        assert_eq!(lost_between(1000, 1000 + 480 * 20, 480), 0);
    }
}
