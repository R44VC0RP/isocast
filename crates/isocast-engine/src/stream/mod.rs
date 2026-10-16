//! Live streaming of the program over RTMP or RTMPS.
//!
//! A stream takes the live engine's program (one frame per canvas tick, and
//! the program mix as track 0) on two threads, apart from any recording:
//!
//! - **encode**: H.264 and AAC through the platform's encoders, interleaved
//!   by time into the send queue. It never waits on the network, so the
//!   program and any recording never wait on the stream. The encoder is
//!   shared with the replay buffer when both ask for the same format (see
//!   [`encoder`]).
//! - **send**: connects (Isocast's own RTMP client), sends the queue, and
//!   reconnects after a drop, resuming with sequence headers and a fresh
//!   keyframe. When the connection can't keep up, the queue drops video
//!   (see `queue`) rather than fall behind live.
//!
//! The stream key never leaves this module: errors and status have it
//! replaced (see [`redact`]).

mod amf;
pub mod encoder;
pub(crate) mod flv;
pub(crate) mod queue;
mod rtmp;
pub mod speed;

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{Local, SecondsFormat};
use isocast_protocol::stream::ServerUrl;
use parking_lot::{Condvar, Mutex};

pub use isocast_protocol::stream::{StreamSettings, StreamState, StreamStatus};

use crate::record::Engine;
use crate::{AudioEncoder, LiveAudioSpec, LiveVideoSpec, Platform, SAMPLE_RATE, Time};
use encoder::{EncoderSpec, Sink, Subscription};
use queue::{Kind, Media, SendQueue};

pub use rtmp::CA_FILE_ENV;

/// The Keychain service every build kept stream keys under up to 0.0.3
/// (the account is [`StreamSettings::key_account`]). Builds now keep them
/// under `<app bundle id>.stream` and move a key from here once, when they
/// may read it (#63).
pub const LEGACY_KEY_SERVICE: &str = "dev.isocast.stream";
/// For local testing: a stream key the daemon takes from its environment
/// instead of the Keychain.
pub const KEY_ENV: &str = "ISOCAST_STREAM_KEY";

/// How long connecting (TCP, TLS, handshake, publish) may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a connection must stay up before reconnecting starts afresh
/// (with all of `stream.reconnect.max_retries` again).
const STABLE: Duration = Duration::from_secs(10);
/// How long a stop waits for the connection to say goodbye before cutting
/// it: a server that stopped reading would otherwise hold a write for up
/// to the connection's write timeout.
const STOP_GRACE: Duration = Duration::from_millis(500);
/// How often the connection is checked for messages from the server.
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// How often bitrates and counters are worked out.
const RATE_INTERVAL: Duration = Duration::from_secs(1);
/// Audio's bounded stall reserve, not a playout delay. Video has a separate
/// eight-picture budget and evicts stale pictures without dropping audio
/// or pinning the compositor's entire frame pool.
pub(crate) const TAP_DEPTH: usize = 96;
/// How long one stream (audio or video) waits for the other before going
/// out alone.
const INTERLEAVE_WAIT: Duration = Duration::from_secs(1);
/// Audio gaps and overlaps smaller than this (2 ms) are ignored.
const AUDIO_JITTER: i64 = SAMPLE_RATE as i64 / 500;
/// Stereo out, whatever the mix.
const CHANNELS: u32 = 2;
/// Samples per channel in an AAC frame.
const AAC_FRAME: i64 = 1024;

/// What to stream: the settings, the size sent (the encoder scales the
/// program to it) and the canvas's frame rate.
#[derive(Debug, Clone)]
pub struct StreamSpec {
    pub settings: StreamSettings,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl StreamSpec {
    /// What the stream's encoder makes.
    pub fn encoder(&self) -> EncoderSpec {
        let settings = &self.settings;
        EncoderSpec {
            video: LiveVideoSpec {
                width: self.width,
                height: self.height,
                fps: self.fps,
                bitrate_kbps: settings.video_bitrate_kbps,
                constant_bitrate: settings.rate_control()
                    == isocast_protocol::stream::RateControl::Constant,
                // The engine asks for every keyframe on its grid (see
                // `encoder`); VideoToolbox's own limit is only a backstop,
                // at twice the interval. At the interval itself it added
                // keyframes of its own, a frame or two before the grid's,
                // whenever frames reached it late.
                keyframe_interval: 2 * self.fps * settings.keyframe_interval_seconds,
                low_latency: false,
            },
            audio: LiveAudioSpec {
                channels: CHANNELS,
                bitrate_kbps: settings.audio_bitrate_kbps,
            },
            keyframe_seconds: settings.keyframe_interval_seconds,
        }
    }

    /// What a stream of `settings` from `engine`'s canvas sends.
    pub fn of<P: Platform>(engine: &Engine<P>, settings: StreamSettings) -> Self {
        let canvas = engine.setup().canvas.clone();
        let (width, height) = settings.output_size((canvas.width, canvas.height));
        Self {
            settings,
            width,
            height,
            fps: canvas.fps,
        }
    }
}

/// Place the encoders of `setup`'s enabled stream destinations that don't
/// share the main destination's (their own bitrate or size), before a
/// recording's writers start: one starting mid-recording then joins a
/// placed hardware session instead of stalling the program (#68, #73).
/// The main destination's is placed by the recording itself. A failure
/// warns; the recording goes on.
pub(crate) fn warm_destinations<P: Platform>(
    engine: &Engine<P>,
    setup: &isocast_protocol::Setup,
    stop: &AtomicBool,
    warnings: &mut Vec<String>,
) -> Vec<Subscription> {
    let Some(stream) = &setup.stream else {
        return Vec::new();
    };
    let main = StreamSpec::of(engine, stream.clone()).encoder();
    let mut placed: Vec<EncoderSpec> = vec![main];
    let mut out = Vec::new();
    for destination in stream.resolve().into_iter().filter(|d| d.enabled) {
        let spec = StreamSpec::of(engine, destination.settings).encoder();
        if placed.contains(&spec) {
            continue;
        }
        placed.push(spec);
        match engine.encoders().warm(engine, &spec, stop) {
            Ok(subscription) => out.push(subscription),
            Err(error) => warnings.push(format!(
                "could not prepare stream destination {}'s encoder before recording: {error:#}",
                destination.id
            )),
        }
    }
    out
}

/// Called on the stream's threads whenever its state or bitrate warning changes.
pub type OnChange = Box<dyn Fn(&StreamStatus) + Send + Sync>;

/// A running stream. Dropping it stops it.
pub struct Stream {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

struct Shared {
    stop: AtomicBool,
    queue: Mutex<SendQueue>,
    queued: Condvar,
    /// Its place on the program's encoder, until sending ends.
    subscription: Mutex<Option<Subscription>>,
    /// Mix chunks the tap lost, once the stream has left the encoder.
    tap_audio_dropped: AtomicU64,
    /// Frames the video encoder dropped.
    encoder_dropped: AtomicU64,
    /// Video frames and bytes (protocol included) sent, over every
    /// connection.
    frames_sent: AtomicU64,
    bytes_sent: AtomicU64,
    /// Why encoding ended, once it has.
    ended: Mutex<Option<Ended>>,
    /// The live connection's socket, for a stop to cut it.
    socket: Mutex<Option<std::net::TcpStream>>,
    status: Mutex<StreamStatus>,
    started: Instant,
    on_change: OnChange,
}

impl Shared {
    fn fail(&self, message: String) {
        self.stop.store(true, Ordering::SeqCst);
        // Only stream-private state is discarded; recording routes are separate.
        *self.queue.lock() = SendQueue::default();
        *self.ended.lock() = Some(Ended::Failed(message.clone()));
        if let Some(socket) = self.socket.lock().as_ref() {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
        self.queued.notify_all();
        self.set_state(StreamState::Failed, Some(message));
    }
    fn stopping(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// Ask the encoder for a keyframe; the queue starts at it.
    fn request_keyframe(&self) {
        if let Some(subscription) = self.subscription.lock().as_ref() {
            subscription.request_keyframe();
        }
    }

    /// Leave the program's encoder (it stops if nothing else uses it).
    fn leave_encoder(&self) {
        let subscription = self.subscription.lock().take();
        if let Some(subscription) = subscription {
            self.tap_audio_dropped
                .store(subscription.tap_audio_dropped(), Ordering::Relaxed);
            // Dropped outside the lock: the last output waits for the
            // encoder's thread, which may be handing this stream media.
            drop(subscription);
        }
    }

    fn tap_audio_dropped(&self) -> u64 {
        match self.subscription.lock().as_ref() {
            Some(subscription) => subscription.tap_audio_dropped(),
            None => self.tap_audio_dropped.load(Ordering::Relaxed),
        }
    }

    /// Move to `state`, telling `on_change`.
    fn set_state(&self, state: StreamState, error: Option<String>) {
        let status = {
            let mut status = self.status.lock();
            if status.state == StreamState::Failed
                && state == StreamState::Offline
                && error.is_none()
            {
                return;
            }
            if status.state == state && error.is_none() {
                return;
            }
            // Counted up to the change (elapsed time stops once it ends).
            self.snapshot(&mut status);
            status.state = state;
            status.since = Some(now());
            if error.is_some() {
                status.last_error = error;
            }
            if state != StreamState::Live {
                status.bitrate_warning = None;
                status.video_kbps = 0.0;
                status.audio_kbps = 0.0;
                status.total_kbps = 0.0;
            }
            status.clone()
        };
        (self.on_change)(&status);
    }

    /// Bring the counters in `status` up to date.
    fn snapshot(&self, status: &mut StreamStatus) {
        let queue = self.queue.lock();
        let dropped = queue.dropped_frames;
        let queue_audio = queue.audio_dropouts;
        drop(queue);
        status.audio_dropouts = queue_audio + self.tap_audio_dropped();
        // Frames that never reached the encoder (its queue from the program
        // was full, or the program missed them) are counted from the gaps
        // between the frames it got, in `dropped`.
        status.dropped_frames = dropped + self.encoder_dropped.load(Ordering::Relaxed);
        status.frames_sent = self.frames_sent.load(Ordering::Relaxed);
        status.bytes_sent = self.bytes_sent.load(Ordering::Relaxed);
        if status.is_active() {
            status.elapsed_seconds = self.started.elapsed().as_secs_f64();
        }
    }
}

/// Why the encoder stopped.
#[derive(Debug, Clone)]
pub(crate) enum Ended {
    /// The stream was stopped.
    Stopped,
    /// The program stopped coming (the live engine stopped).
    Program,
    Failed(String),
}

fn now() -> String {
    Local::now().to_rfc3339_opts(SecondsFormat::Millis, false)
}

/// The stream key replaced by a placeholder, for anything shown or logged.
pub fn redact(text: &str, key: &str) -> String {
    if key.trim().is_empty() {
        text.to_string()
    } else {
        text.replace(key, "<stream key>")
    }
}

impl Stream {
    pub fn fail(&self, message: String) {
        self.shared.fail(message);
    }
    /// Its state and counters now.
    pub fn status(&self) -> StreamStatus {
        let mut status = self.shared.status.lock().clone();
        self.shared.snapshot(&mut status);
        status
    }

    /// Stop sending, close the connection and return the final status.
    pub fn stop(mut self) -> StreamStatus {
        self.halt();
        self.status()
    }

    fn halt(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.queued.notify_all();
        let threads = std::mem::take(&mut self.threads);
        if threads.is_empty() {
            self.shared.leave_encoder();
            return;
        }
        let (finished, joined) = std::sync::mpsc::channel();
        let failed = self.shared.clone();
        let _ = isocast_diagnostics::spawn(
            "stream join",
            move || {
                for thread in threads {
                    let _ = thread.join();
                }
                let _ = finished.send(());
            },
            move |why| failed.fail(why),
        );
        if joined.recv_timeout(STOP_GRACE).is_err() {
            if let Some(socket) = self.shared.socket.lock().as_ref() {
                let _ = socket.shutdown(std::net::Shutdown::Both);
            }
            let _ = joined.recv();
        }
        self.shared.leave_encoder();
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.halt();
    }
}

/// Stream `engine`'s program with `settings`, to the server under `key`:
/// video at the canvas rate, and the mix as audio track 0. The engine
/// keeps its canvas while the stream taps it. `id` names the destination
/// in its status (`main` for the `[stream]` section's own).
pub fn start<P: Platform>(
    engine: &Engine<P>,
    id: &str,
    settings: StreamSettings,
    key: String,
    on_change: OnChange,
) -> Result<Stream> {
    let spec = StreamSpec::of(engine, settings);
    let server = spec
        .settings
        .server()
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    let settings = &spec.settings;
    let mut status = StreamStatus::offline(settings, true);
    status.id = id.to_string();
    status.state = StreamState::Connecting;
    (status.width, status.height, status.fps) = (spec.width, spec.height, spec.fps);
    status.since = Some(now());
    let shared = Arc::new(Shared {
        stop: AtomicBool::new(false),
        queue: Mutex::new(SendQueue::default()),
        queued: Condvar::new(),
        subscription: Mutex::new(None),
        tap_audio_dropped: AtomicU64::new(0),
        encoder_dropped: AtomicU64::new(0),
        frames_sent: AtomicU64::new(0),
        bytes_sent: AtomicU64::new(0),
        ended: Mutex::new(None),
        socket: Mutex::new(None),
        status: Mutex::new(status.clone()),
        started: Instant::now(),
        on_change,
    });
    let subscription =
        engine
            .encoders()
            .subscribe(engine, &spec.encoder(), "the stream", shared.clone())?;
    *shared.subscription.lock() = Some(subscription);
    (shared.on_change)(&status);
    // The sender keeps pace with the program, ahead of background work.
    let sending = {
        let shared = shared.clone();
        let platform = engine.platform().clone();
        thread::Builder::new()
            .name("stream send".into())
            .spawn(move || {
                platform.real_time_thread();
                if let Err(message) = isocast_diagnostics::contain("stream sender", || {
                    send(&shared, &spec, &server, &key)
                }) {
                    shared.fail(message);
                }
                // The encoder stops with the stream, unless another output
                // uses it.
                shared.leave_encoder();
            })
    };
    let sending = match sending {
        Ok(sending) => sending,
        Err(err) => {
            shared.leave_encoder();
            return Err(err.into());
        }
    };
    Ok(Stream {
        shared,
        threads: vec![sending],
    })
}

impl Sink for Shared {
    fn missed(&self, frames: u64) {
        self.queue.lock().dropped(frames);
    }

    fn encoder_dropped(&self, frames: u64) {
        self.encoder_dropped.fetch_add(frames, Ordering::Relaxed);
    }

    fn keyframe_requested(&self, at: Time) {
        self.queue.lock().start_at(at);
    }

    fn push(&self, ready: &[Media]) {
        if self.stopping() {
            return;
        }
        let mut queue = self.queue.lock();
        for media in ready {
            queue.push(media.clone());
        }
        drop(queue);
        self.queued.notify_all();
    }

    fn ended(&self, ended: &Ended) {
        *self.ended.lock() = Some(ended.clone());
        self.queued.notify_all();
    }
}

/// The audio fed to the AAC encoder, kept continuous, and the times of the
/// frames it makes.
pub(crate) struct AudioTrack {
    /// The time of the first sample.
    start: Option<Time>,
    /// Samples per channel fed so far.
    fed: i64,
    /// AAC frames made so far.
    frames: i64,
    /// The encoder's delay, in samples.
    delay: i64,
}

impl AudioTrack {
    pub(crate) fn new(delay: u32) -> Self {
        Self {
            start: None,
            fed: 0,
            frames: 0,
            delay: i64::from(delay),
        }
    }

    /// Add stereo samples captured at `pts`: small jitter is ignored, gaps
    /// become silence and overlaps are trimmed, like file tracks.
    pub(crate) fn add<E: AudioEncoder>(
        &mut self,
        samples: &[f32],
        pts: Time,
        encoder: &mut E,
        order: &mut Interleaver,
    ) -> Result<()> {
        let start = *self.start.get_or_insert(pts);
        let at = pts.ticks_since(start, SAMPLE_RATE);
        let mut samples = samples;
        if at > self.fed + AUDIO_JITTER {
            // A long encoder stall must not allocate the whole silence gap.
            let silence = [0.0; AAC_FRAME as usize * CHANNELS as usize];
            while self.fed < at {
                let frames = (at - self.fed).min(AAC_FRAME) as usize;
                self.feed(&silence[..frames * CHANNELS as usize], encoder, order)?;
            }
        } else if at < self.fed - AUDIO_JITTER {
            let skip = ((self.fed - at) as usize * CHANNELS as usize).min(samples.len());
            samples = &samples[skip..];
        }
        if !samples.is_empty() {
            self.feed(samples, encoder, order)?;
        }
        Ok(())
    }

    fn feed<E: AudioEncoder>(
        &mut self,
        samples: &[f32],
        encoder: &mut E,
        order: &mut Interleaver,
    ) -> Result<()> {
        let start = self.start.expect("started");
        for frame in encoder.encode(samples)? {
            // Frame n holds the audio from sample n·1024 − delay.
            let first = self.frames * AAC_FRAME - self.delay;
            let ts =
                Time::from_nanos(start.as_nanos() + first * 1_000_000_000 / i64::from(SAMPLE_RATE));
            order.push(Media {
                ts,
                kind: Kind::Audio,
                data: frame,
                config: None,
            });
            self.frames += 1;
        }
        self.fed += (samples.len() / CHANNELS as usize) as i64;
        Ok(())
    }
}

/// Puts audio and video in time order. The mix runs a little behind the
/// picture, so video waits for audio of the same time (up to
/// [`INTERLEAVE_WAIT`]).
#[derive(Default)]
pub(crate) struct Interleaver {
    video: VecDeque<Media>,
    audio: VecDeque<Media>,
    /// The newest time seen.
    latest: Option<Time>,
}

impl Interleaver {
    pub(crate) fn push(&mut self, media: Media) {
        self.latest = Some(self.latest.map_or(media.ts, |t| t.max(media.ts)));
        match media.kind {
            Kind::Audio => self.audio.push_back(media),
            Kind::Video { .. } => self.video.push_back(media),
        }
    }

    /// Everything that can go out now, oldest first.
    pub(crate) fn release(&mut self) -> Vec<Media> {
        let mut out = Vec::new();
        let Some(latest) = self.latest else {
            return out;
        };
        let overdue = |media: &Media| media.ts + INTERLEAVE_WAIT <= latest;
        loop {
            let video = match (self.video.front(), self.audio.front()) {
                (Some(v), Some(a)) => v.ts <= a.ts,
                (Some(v), None) if overdue(v) => true,
                (None, Some(a)) if overdue(a) => false,
                _ => break,
            };
            let queue = if video {
                &mut self.video
            } else {
                &mut self.audio
            };
            out.extend(queue.pop_front());
        }
        out
    }
}

/// Connect, send, and reconnect, until the stream stops, fails, or the
/// program ends.
fn send(shared: &Shared, spec: &StreamSpec, server: &ServerUrl, key: &str) {
    let reconnect = &spec.settings.reconnect;
    let mut was_live = false;
    // Attempts since the server last accepted the stream: each outage gets
    // all of `max_retries`.
    let mut attempts = 0u32;
    // Connections in a row that dropped within `STABLE`: a server that
    // accepts the stream and drops it at once would otherwise be retried
    // forever, so these are limited to `max_retries` too.
    let mut short_lived = 0u32;
    // Until a connection has stayed up, a server that accepted every
    // connection (none failed) and dropped each of them (more than one) is
    // probably refusing the key; one that went away in between is not.
    let mut was_stable = false;
    let mut unreachable = false;
    let outcome = loop {
        if shared.stopping() {
            break Ok(());
        }
        let result = rtmp::Connection::publish(server, key, CONNECT_TIMEOUT, &shared.stop)
            .with_context(|| format!("could not stream to {server}"));
        let error = match result {
            Ok(mut connection) => {
                if was_live {
                    shared.status.lock().reconnects += 1;
                }
                was_live = true;
                shared.queue.lock().restart();
                shared.request_keyframe();
                shared.set_state(StreamState::Live, None);
                let connected = Instant::now();
                *shared.socket.lock() = connection.socket().ok();
                attempts = 0;
                let result = publish(&mut connection, shared, spec);
                if connected.elapsed() >= STABLE {
                    short_lived = 0;
                    was_stable = true;
                } else {
                    short_lived += 1;
                }
                if result.is_ok() {
                    connection.close(key);
                }
                *shared.socket.lock() = None;
                match result {
                    Ok(()) => break Ok(()),
                    Err(err) if !was_stable && !unreachable && short_lived >= 2 => format!(
                        "the connection to {server} dropped {:.0} s after the server accepted \
                         the stream: {err:#} (a server that drops each stream it accepts is \
                         usually refusing the stream key; check the key, `isocast stream key \
                         set`, and stream.server)",
                        connected.elapsed().as_secs_f64()
                    ),
                    Err(err) => format!("the connection to {server} dropped: {err:#}"),
                }
            }
            Err(err) => {
                unreachable = true;
                format!("{err:#}")
            }
        };
        let error = redact(&error, key);
        // Stopped while connecting: that's not a failure.
        if shared.stopping() {
            break Ok(());
        }
        // A first connection that fails is a setup or key problem: say so
        // at once rather than retry.
        if !was_live {
            break Err(error);
        }
        if !reconnect.enabled || reconnect.max_retries == 0 {
            break Err(format!(
                "{error}; not reconnecting (stream.reconnect is off)"
            ));
        }
        if attempts >= reconnect.max_retries {
            break Err(format!(
                "gave up after {attempts} attempts to reconnect \
                 (stream.reconnect.max_retries): {error}"
            ));
        }
        if short_lived >= reconnect.max_retries {
            break Err(format!(
                "gave up after the server dropped the stream {short_lived} times in a row \
                 within {} s of accepting it (stream.reconnect.max_retries): {error}",
                STABLE.as_secs()
            ));
        }
        attempts += 1;
        shared.set_state(StreamState::Reconnecting, Some(error));
        let resume = Instant::now() + Duration::from_secs(reconnect.delay_seconds.into());
        while Instant::now() < resume && !shared.stopping() && !program_ended(shared) {
            // Keep the queue from holding stale media meanwhile.
            shared.queue.lock().restart();
            thread::sleep(Duration::from_millis(50));
        }
        if program_ended(shared) {
            break Ok(());
        }
    };
    // The encoder stops with the stream.
    shared.stop.store(true, Ordering::SeqCst);
    let ended = shared.ended.lock().clone();
    match (outcome, ended) {
        (Err(error), _) => shared.set_state(StreamState::Failed, Some(error)),
        (Ok(()), Some(Ended::Failed(error))) => {
            shared.set_state(StreamState::Failed, Some(redact(&error, key)))
        }
        (Ok(()), Some(Ended::Program)) => shared.set_state(
            StreamState::Offline,
            Some("the live engine stopped, and the stream with it".into()),
        ),
        (Ok(()), _) => shared.set_state(StreamState::Offline, None),
    }
}

fn program_ended(shared: &Shared) -> bool {
    shared.ended.lock().is_some()
}

/// Send the queue over `connection` until the stream stops (`Ok`) or the
/// connection fails.
fn publish(connection: &mut rtmp::Connection, shared: &Shared, spec: &StreamSpec) -> Result<()> {
    let settings = &spec.settings;
    connection.send_metadata(&flv::metadata(&flv::Metadata {
        width: spec.width,
        height: spec.height,
        fps: spec.fps,
        video_kbps: settings.video_bitrate_kbps,
        audio_kbps: settings.audio_bitrate_kbps,
        sample_rate: SAMPLE_RATE,
        channels: CHANNELS,
    }))?;
    let audio_config = flv::audio_specific_config(SAMPLE_RATE, CHANNELS);
    let mut base: Option<Time> = None;
    let mut sent_config: Option<Arc<Vec<u8>>> = None;
    let mut sent_audio_config = false;
    let mut next_poll = Instant::now() + POLL_INTERVAL;
    let mut rate = Rate {
        total: Some(shared.bytes_sent.load(Ordering::Relaxed)),
        ..Rate::new()
    };
    let mut counted = connection.bytes_sent;
    loop {
        let media = {
            let mut queue = shared.queue.lock();
            loop {
                if shared.stopping() {
                    return Ok(());
                }
                if let Some(media) = queue.pop() {
                    break Some(media);
                }
                if queue.is_empty() && program_ended(shared) {
                    return Ok(());
                }
                let timeout = shared.queued.wait_for(&mut queue, POLL_INTERVAL);
                if timeout.timed_out() {
                    break None;
                }
            }
        };
        if let Some(media) = media {
            let base = *base.get_or_insert(media.ts);
            let ms = (media.ts.as_nanos() - base.as_nanos()).max(0) + 500_000;
            let timestamp = (ms / 1_000_000) as u32;
            match media.kind {
                Kind::Video { keyframe } => {
                    if keyframe
                        && let Some(config) = &media.config
                        && sent_config.as_ref() != Some(config)
                    {
                        connection.send_video(timestamp, &flv::avc_sequence_header(config))?;
                        sent_config = Some(config.clone());
                    }
                    // Nothing decodes before the configuration.
                    if sent_config.is_some() {
                        connection
                            .send_video(timestamp, &flv::avc_frame(keyframe, 0, &media.data))?;
                        rate.video += media.data.len() as u64;
                        shared.frames_sent.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Kind::Audio => {
                    if !sent_audio_config {
                        connection.send_audio(
                            timestamp,
                            &flv::aac_sequence_header(&audio_config, CHANNELS),
                        )?;
                        sent_audio_config = true;
                    }
                    connection.send_audio(timestamp, &flv::aac_frame(&media.data, CHANNELS))?;
                    rate.audio += media.data.len() as u64;
                }
            }
        }
        shared
            .bytes_sent
            .fetch_add(connection.bytes_sent - counted, Ordering::Relaxed);
        counted = connection.bytes_sent;
        if Instant::now() >= next_poll {
            next_poll = Instant::now() + POLL_INTERVAL;
            connection.poll()?;
        }
        if rate.since.elapsed() >= RATE_INTERVAL {
            rate.publish(shared);
        }
    }
}

/// Media sent since the last report of the bitrates.
struct Rate {
    since: Instant,
    video: u64,
    audio: u64,
    /// Everything sent (`Shared::bytes_sent`) at the last report.
    total: Option<u64>,
    /// Consecutive measured seconds above the target, reset on recovery.
    overshoot_seconds: f64,
}

impl Rate {
    fn new() -> Self {
        Self {
            since: Instant::now(),
            video: 0,
            audio: 0,
            total: None,
            overshoot_seconds: 0.0,
        }
    }

    /// Put the bitrates since the last report into the status.
    fn publish(&mut self, shared: &Shared) {
        let seconds = self.since.elapsed().as_secs_f64();
        let kbps = |bytes: u64| bytes as f64 * 8.0 / seconds / 1000.0;
        let total = shared.bytes_sent.load(Ordering::Relaxed);
        let congestion = shared.queue.lock().take_peak();
        let mut status = shared.status.lock();
        status.video_kbps = kbps(self.video);
        status.audio_kbps = kbps(self.audio);
        if let Some(before) = self.total {
            status.total_kbps = kbps(total - before);
        }
        status.congestion = congestion;
        if status.video_kbps > f64::from(status.target_video_kbps) * 1.3 {
            self.overshoot_seconds += seconds;
        } else {
            self.overshoot_seconds = 0.0;
        }
        let was_warning = status.bitrate_warning.is_some();
        if self.overshoot_seconds >= 5.0 {
            // Keep the message stable during this episode so clients warn once.
            status.bitrate_warning.get_or_insert_with(|| {
                String::from(
                    "the stream's sent video bitrate has exceeded 130% of its target for 5 s; \
                 reduce scene motion or stream resolution",
                )
            });
        } else {
            status.bitrate_warning = None;
        }
        let changed = was_warning != status.bitrate_warning.is_some();
        let update = changed.then(|| {
            shared.snapshot(&mut status);
            status.clone()
        });
        drop(status);
        if let Some(update) = update {
            (shared.on_change)(&update);
        }
        *self = Self {
            total: Some(total),
            overshoot_seconds: self.overshoot_seconds,
            ..Self::new()
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media(ms: i64, kind: Kind) -> Media {
        Media {
            ts: Time::from_nanos(ms * 1_000_000),
            kind,
            data: Vec::new(),
            config: None,
        }
    }

    #[test]
    fn video_waits_for_audio_of_the_same_time() {
        let mut order = Interleaver::default();
        for ms in [0, 33, 66, 100] {
            order.push(media(ms, Kind::Video { keyframe: ms == 0 }));
        }
        assert!(order.release().is_empty());
        order.push(media(-40, Kind::Audio));
        order.push(media(-19, Kind::Audio));
        order.push(media(2, Kind::Audio));
        order.push(media(23, Kind::Audio));
        order.push(media(45, Kind::Audio));
        let times: Vec<i64> = order
            .release()
            .iter()
            .map(|m| m.ts.as_nanos() / 1_000_000)
            .collect();
        assert_eq!(times, [-40, -19, 0, 2, 23, 33, 45]);
        // Without audio, video goes out a second late.
        order.push(media(1066, Kind::Video { keyframe: false }));
        let times: Vec<i64> = order
            .release()
            .iter()
            .map(|m| m.ts.as_nanos() / 1_000_000)
            .collect();
        assert_eq!(times, [66]);
    }

    #[test]
    fn the_key_never_shows() {
        assert_eq!(
            redact(
                "NetStream.Publish.BadName (live_123_abc in use)",
                "live_123_abc"
            ),
            "NetStream.Publish.BadName (<stream key> in use)"
        );
        assert_eq!(redact("no key here", ""), "no key here");
    }
}
