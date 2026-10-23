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
        let local = socket.local_addr()?;
        let mut config = RtcConfig::new()
            .set_crypto_provider(Arc::new(str0m::crypto::from_feature_flags()))
            .clear_codecs()
            .enable_opus(true, false)
            .enable_h264(true);
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
        if self.pictures % 150 == 1
            && let Some((wait, skew)) = self.playout.waits()
        {
            isocast_diagnostics::diagnostic!("debug"; "remote video: sound arrives {:.0} ms after the picture; pictures wait {:.0} ms to play with it{}", skew * 1e3, wait * 1e3, self.sound.as_ref().map(|s| format!("; {}", s.report())).unwrap_or_default());
        }
        let (sets, unit) = from_annex_b(&data.data, data.time.as_micros() * 1000);
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
        match decoder.decode_float(&data.data, &mut self.pcm, false) {
            Ok(frames) => {
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
