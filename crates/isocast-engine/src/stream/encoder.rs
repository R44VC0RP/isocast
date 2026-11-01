//! The program's live H.264 and AAC encode, shared by the outputs that
//! take it compressed: the stream and the replay buffer.
//!
//! Hardware encoder sessions are what a busy Mac runs out of first (#41,
//! #60), so outputs asking for the same format share one encoder, as OBS's
//! outputs do: the first to start creates it, each later one with an equal
//! [`EncoderSpec`] subscribes to it, and it stops when the last
//! unsubscribes. Its thread taps the program, encodes, interleaves audio
//! and video by time and hands each batch to every [`Sink`].

use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, sync_channel};
use std::sync::{Arc, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use isocast_protocol::CanvasId;

use super::queue::{Kind, Media};
use super::{AudioTrack, Ended, Interleaver, TAP_DEPTH, flv};
use crate::record::{Engine, Packet, ProgramTap, ProgramTapSource};
use crate::{AudioEncoder, LiveAudioSpec, LiveVideoSpec, Platform, Time, VideoEncoder};

/// What a shared encoder makes. Outputs share an encoder only when their
/// specs are equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderSpec {
    pub video: LiveVideoSpec,
    pub audio: LiveAudioSpec,
    /// Keyframes are made on a grid of this many seconds of frame time.
    pub keyframe_seconds: u32,
}

/// An output of a shared encoder. Called on the encoder's thread; each
/// call must return quickly (no waiting on a network or a disk).
pub(crate) trait Sink: Send + Sync {
    /// A real output needs continuous media; a warm reservation does not.
    fn keeps_encoding(&self) -> bool {
        true
    }
    /// `frames` program frames never reached the encoder (the tap was
    /// full, or the program missed them).
    fn missed(&self, frames: u64);
    /// The encoder dropped `frames` instead of encoding them.
    fn encoder_dropped(&self, frames: u64);
    /// A keyframe someone asked for (see [`Subscription::request_keyframe`])
    /// was asked of the frame at `at`.
    fn keyframe_requested(&self, _at: Time) {}
    /// Encoded media, in time order. Video starts wherever the encoder is
    /// when the sink joins, so it may not start with a keyframe.
    fn push(&self, ready: &[Media]);
    /// The encoder ended (the program stopped, or it failed). Nothing more
    /// comes.
    fn ended(&self, ended: &Ended);
}

/// The live engine's shared encoders.
#[derive(Default)]
pub struct Encoders(Mutex<Vec<Weak<Hub>>>);

/// One running encoder and its sinks.
pub(crate) struct Hub {
    /// What it was started as; never changes, so outputs join (and the
    /// replay buffer's files match) by what they asked for, whatever the
    /// governor has the encoder doing now.
    spec: EncoderSpec,
    /// The video bitrate the encoder has acknowledged: `spec`'s until the
    /// governor retunes it (see [`crate::governor::StreamRate`]).
    applied_kbps: AtomicU32,
    stop: AtomicBool,
    /// Ended by itself (the program stopped or it failed): new outputs
    /// don't join it.
    ended: AtomicBool,
    /// At least one native picture has completed, so the session is placed.
    ready: AtomicBool,
    sinks: Mutex<Vec<(u64, Arc<dyn Sink>)>>,
    /// How many real outputs there are, read without `sinks` (callers may hold
    /// their own locks, which sinks take while `sinks` is held).
    sink_count: AtomicU64,
    /// Changed under `sinks`, so parking and the first real join cannot race.
    tapped: AtomicBool,
    /// Acquire fresh typed routes synchronously, then hand them to the
    /// worker; false if its worker has already ended on its own (before
    /// it set `ended`).
    retap: Box<dyn Fn() -> Result<bool> + Send + Sync>,
    next_sink: AtomicU64,
    want_keyframe: AtomicBool,
    /// Mix chunks the tap lost, counted even while the encoder stalls.
    tap_audio_dropped: Arc<AtomicU64>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

/// An output's place on a shared encoder. Dropping it leaves; the last to
/// leave stops the encoder.
pub(crate) struct Subscription {
    hub: Arc<Hub>,
    id: u64,
    /// The tap's lost mix chunks when it joined.
    audio_base: u64,
}

impl Encoders {
    /// Place the shared encoder before a recording's hardware writers start.
    /// Complete one picture, then park until a live output joins. The returned
    /// subscription keeps the session placed through the recording's splits.
    pub(crate) fn warm<P: Platform>(
        &self,
        engine: &Engine<P>,
        spec: &EncoderSpec,
        stop: &AtomicBool,
    ) -> Result<Subscription> {
        self.warm_canvas(engine, CanvasId::Main, spec, stop)
    }

    pub(crate) fn warm_canvas<P: Platform>(
        &self,
        engine: &Engine<P>,
        canvas: CanvasId,
        spec: &EncoderSpec,
        stop: &AtomicBool,
    ) -> Result<Subscription> {
        struct Warm;
        impl Sink for Warm {
            fn keeps_encoding(&self) -> bool {
                false
            }
            fn missed(&self, _: u64) {}
            fn encoder_dropped(&self, _: u64) {}
            fn push(&self, _: &[Media]) {}
            fn ended(&self, ended: &Ended) {
                if let Ended::Failed(message) = ended {
                    isocast_diagnostics::diagnostic!("warn";
                        "the recording's warm encoder stopped: {message}");
                }
            }
        }
        let subscription = self.subscribe_canvas(
            engine,
            canvas,
            spec,
            "the recording's warm encoder",
            Arc::new(Warm),
        )?;
        let deadline = Instant::now() + Duration::from_secs(3);
        while !subscription.hub.ready.load(Ordering::SeqCst) {
            if stop.load(Ordering::SeqCst) {
                bail!("the recording was stopped while preparing its live encoder");
            }
            if subscription.hub.ended.load(Ordering::SeqCst) || Instant::now() >= deadline {
                bail!("the program's H.264 encoder did not complete its first picture");
            }
            thread::sleep(Duration::from_millis(2));
        }
        Ok(subscription)
    }

    /// Join the running encoder of `spec`, or start one. `what` names the
    /// output in errors ("the stream").
    pub(crate) fn subscribe<P: Platform>(
        &self,
        engine: &Engine<P>,
        spec: &EncoderSpec,
        what: &str,
        sink: Arc<dyn Sink>,
    ) -> Result<Subscription> {
        self.subscribe_canvas(engine, CanvasId::Main, spec, what, sink)
    }

    pub(crate) fn subscribe_canvas<P: Platform>(
        &self,
        engine: &Engine<P>,
        canvas: CanvasId,
        spec: &EncoderSpec,
        what: &str,
        sink: Arc<dyn Sink>,
    ) -> Result<Subscription> {
        let mut hubs = self.0.lock();
        hubs.retain(|hub| hub.strong_count() > 0);
        for hub in hubs.iter().filter_map(Weak::upgrade) {
            if hub.spec != *spec || hub.ended.load(Ordering::SeqCst) {
                continue;
            }
            let mut sinks = hub.sinks();
            // A hub whose last output is leaving can't take another, nor
            // one that just ended (its thread sets `ended` before it takes
            // this lock to tell the sinks, so a sink added here is told).
            if hub.stop.load(Ordering::SeqCst) || hub.ended.load(Ordering::SeqCst) {
                continue;
            }
            if sink.keeps_encoding() && !hub.tapped.load(Ordering::SeqCst) {
                // Own the canvas before reporting a live output: an immediate
                // canvas edit must not slip in before the worker wakes.
                // Its worker ended on its own just now (the program
                // stopping, or its encoder failing): a new encoder starts
                // instead (#77).
                if !(hub.retap)()? {
                    continue;
                }
                hub.tapped.store(true, Ordering::SeqCst);
            }
            let id = hub.next_sink.fetch_add(1, Ordering::Relaxed);
            sinks.push((id, sink));
            hub.sink_count.store(
                sinks
                    .iter()
                    .filter(|(_, sink)| sink.keeps_encoding())
                    .count() as u64,
                Ordering::SeqCst,
            );
            drop(sinks);
            isocast_diagnostics::debug!(
                "encoder: {what} shares the program encoder ({}x{} at {} kb/s)",
                spec.video.width,
                spec.video.height,
                spec.video.bitrate_kbps
            );
            let audio_base = hub.tap_audio_dropped.load(Ordering::Relaxed);
            return Ok(Subscription {
                hub,
                id,
                audio_base,
            });
        }
        let platform = engine.platform();
        let mut video = platform
            .create_video_encoder(&spec.video)
            .with_context(|| format!("could not start {what}'s H.264 encoder"))?;
        video.prioritize_video(canvas);
        let audio = platform
            .create_audio_encoder(&spec.audio)
            .with_context(|| format!("could not start {what}'s AAC encoder"))?;
        let program = engine.canvas_tap_source(canvas)?;
        let tap = program.tap(TAP_DEPTH, Arc::new(AtomicU64::new(0)))?;
        let retap_program = program.clone();
        let retap_audio = tap.missed_audio_chunks.clone();
        let (tap_tx, retapped) = sync_channel(1);
        let sink_count = u64::from(sink.keeps_encoding());
        let hub = Arc::new(Hub {
            spec: *spec,
            applied_kbps: AtomicU32::new(spec.video.bitrate_kbps),
            stop: AtomicBool::new(false),
            ended: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            sinks: Mutex::new(vec![(0, sink)]),
            sink_count: AtomicU64::new(sink_count),
            tapped: AtomicBool::new(true),
            retap: Box::new(move || {
                Ok(tap_tx
                    .send(retap_program.tap(TAP_DEPTH, retap_audio.clone())?)
                    .is_ok())
            }),
            next_sink: AtomicU64::new(1),
            want_keyframe: AtomicBool::new(false),
            tap_audio_dropped: tap.missed_audio_chunks.clone(),
            thread: Mutex::new(None),
        });
        let thread = {
            let hub = hub.clone();
            let platform = platform.clone();
            thread::Builder::new()
                .name("program encode".into())
                .spawn(move || {
                    // Keeps pace with the program, ahead of background work.
                    platform.real_time_thread();
                    let encoded = isocast_diagnostics::contain("program encoder", || {
                        encode::<P>(video, audio, tap, retapped, program, canvas, &hub)
                    });
                    let ended = match encoded {
                        Ok(Ok(ended)) => ended,
                        Ok(Err(err)) => {
                            Ended::Failed(format!("the program's encoder failed: {err:#}"))
                        }
                        Err(message) => Ended::Failed(message),
                    };
                    hub.ended.store(true, Ordering::SeqCst);
                    if !matches!(ended, Ended::Stopped) {
                        // Called without `sinks` held: a sink's `ended` may
                        // tell clients, which may take their own locks.
                        let sinks: Vec<_> =
                            hub.sinks().iter().map(|(_, sink)| sink.clone()).collect();
                        for sink in sinks {
                            sink.ended(&ended);
                        }
                    }
                })?
        };
        *hub.thread.lock() = Some(thread);
        hubs.push(Arc::downgrade(&hub));
        Ok(Subscription {
            hub,
            id: 0,
            audio_base: 0,
        })
    }
}

impl Hub {
    /// The sinks, even after a sink panicked holding the lock (the encoder
    /// thread catches the panic; the others must still be told it ended).
    fn sinks(&self) -> parking_lot::MutexGuard<'_, Vec<(u64, Arc<dyn Sink>)>> {
        self.sinks.lock()
    }
}

impl Subscription {
    /// Ask for a keyframe as soon as possible (every output of the
    /// encoder gets it).
    pub(crate) fn request_keyframe(&self) {
        self.hub.want_keyframe.store(true, Ordering::SeqCst);
    }

    /// Other outputs use the same encoder now.
    pub(crate) fn shared(&self) -> bool {
        self.hub.sink_count.load(Ordering::SeqCst) > 1
    }

    /// The video bitrate the shared encoder is making now, as it
    /// acknowledged it (the same for every output of the encoder).
    pub(crate) fn video_kbps(&self) -> u32 {
        self.hub.applied_kbps.load(Ordering::Relaxed)
    }

    /// Mix chunks the tap lost since this output joined.
    pub(crate) fn tap_audio_dropped(&self) -> u64 {
        self.hub
            .tap_audio_dropped
            .load(Ordering::Relaxed)
            .saturating_sub(self.audio_base)
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let last = {
            let mut sinks = self.hub.sinks();
            sinks.retain(|(id, _)| *id != self.id);
            self.hub.sink_count.store(
                sinks
                    .iter()
                    .filter(|(_, sink)| sink.keeps_encoding())
                    .count() as u64,
                Ordering::SeqCst,
            );
            let last = sinks.is_empty();
            if last {
                self.hub.stop.store(true, Ordering::SeqCst);
            }
            last
        };
        if last {
            // The thread checks `stop` every few milliseconds.
            let thread = self.hub.thread.lock().take();
            if let Some(thread) = thread
                && thread.thread().id() != thread::current().id()
            {
                let _ = thread.join();
            }
        }
    }
}

/// Encode the program until the last output leaves or the program stops.
/// Returns why it ended.
fn encode<P: Platform>(
    mut video: P::VideoEncoder,
    mut audio: P::AudioEncoder,
    tap: ProgramTap<P::Frame>,
    retapped: Receiver<ProgramTap<P::Frame>>,
    program: ProgramTapSource<P>,
    canvas: CanvasId,
    hub: &Hub,
) -> Result<Ended> {
    let spec = &hub.spec;
    let mut retune = Retune::new(canvas, spec.video.bitrate_kbps);
    let each = |f: &dyn Fn(&dyn Sink)| {
        for (_, sink) in hub.sinks().iter() {
            f(sink.as_ref());
        }
    };
    let frame_ns = 1_000_000_000 / i64::from(spec.video.fps.max(1));
    // Keyframes are due on a grid of the frames' times, not every so many
    // frames, so frames that never reached the encoder don't stretch the
    // interval past what services allow (or leave VideoToolbox to put in
    // keyframes of its own). Half a frame early allows for rounding.
    let keyframe_ns = i64::from(spec.keyframe_seconds.max(1)) * 1_000_000_000;
    let mut last_frame: Option<Time> = None;
    let mut next_keyframe: Option<i64> = None;
    // A keyframe the encoder dropped (too many frames in flight) is asked
    // of the next frame instead.
    let mut keyframe_missed = false;
    let mut encoder_dropped = 0;
    let mut intentional_dropped = 0;
    let mut config: Option<Arc<Vec<u8>>> = None;
    let mut track = AudioTrack::new(audio.delay());
    let mut order = Interleaver::default();
    let mut warm_pts = None;
    let mut video_offset = 0;
    let mut live = false;
    let mut tap = Some(tap);
    loop {
        retune
            .rate
            .active(hub.sink_count.load(Ordering::SeqCst) > 0);
        if hub.stop.load(Ordering::SeqCst) {
            return Ok(Ended::Stopped);
        }
        if program.stopped() {
            return Ok(Ended::Program);
        }
        // One picture places the native session. Drop its tap while parked,
        // so a reservation alone does not keep the compositor drawing.
        // Once used, keep running until the recording ends rather than
        // restarting the audio timeline or stranding native pressure on Stop.
        if !live {
            live = hub.sink_count.load(Ordering::SeqCst) > 0;
        }
        if tap.is_none() {
            match retapped.recv_timeout(Duration::from_millis(5)) {
                Ok(joined) => {
                    tap = Some(joined);
                    live = true;
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return Ok(Ended::Stopped),
            }
        }
        match tap
            .as_ref()
            .unwrap()
            .packets
            .recv_timeout(Duration::from_millis(5))
        {
            Ok(Packet::Video { frame, pts }) if !live => {
                if warm_pts.is_none() {
                    video.encode(&frame, pts, true)?;
                    warm_pts = Some(pts);
                }
            }
            Ok(Packet::Audio { .. }) if !live => {}
            Ok(Packet::Video { frame, pts }) => {
                if let Some(warm) = warm_pts
                    && last_frame.is_none()
                {
                    // VT budgets bitrate in submitted PTS time. A parked gap
                    // looks like unspent budget and produces huge first packets.
                    // Keep its clock contiguous; the live timeline stays intact.
                    video_offset = pts.as_nanos() - warm.add_ticks(1, spec.video.fps).as_nanos();
                }
                // Frames come one per canvas tick; any between this and the
                // last never reached the encoder (the tap was full, or the
                // program missed them), so they count as dropped.
                if let Some(last) = last_frame.replace(pts) {
                    let ticks = (pts.as_nanos() - last.as_nanos() + frame_ns / 2) / frame_ns;
                    if ticks > 1 {
                        crate::governor::protected_loss(canvas, ticks as u64 - 1);
                        each(&|sink| sink.missed(ticks as u64 - 1));
                    }
                }
                // Between frames, never mid-submission: the session, frame
                // rate and keyframe grid stay as they are.
                retune.apply(&mut video, spec.video.fps, &hub.applied_kbps);
                let forced = hub.want_keyframe.swap(false, Ordering::SeqCst);
                if forced {
                    // A new connection starts with this frame, and so
                    // does the grid (below): its first interval is whole.
                    each(&|sink| sink.keyframe_requested(pts));
                }
                let t = pts.as_nanos();
                let due = next_keyframe.is_none_or(|at| t >= at - frame_ns / 2);
                if forced || due {
                    // The grid restarts at a forced keyframe (a new
                    // connection); a late one keeps it.
                    let mut at = match next_keyframe {
                        Some(at) if !forced => at,
                        _ => t,
                    } + keyframe_ns;
                    while at - frame_ns / 2 <= t {
                        at += keyframe_ns;
                    }
                    next_keyframe = Some(at);
                }
                let keyframe = forced || due || keyframe_missed;
                let dropped = video.dropped();
                video.encode(
                    &frame,
                    Time::from_nanos(pts.as_nanos() - video_offset),
                    keyframe,
                )?;
                keyframe_missed = keyframe && video.dropped() > dropped;
            }
            Ok(Packet::Audio { samples, pts, .. }) => {
                track.add(&samples, pts, &mut audio, &mut order)?;
            }
            // Only ISO files get skipped ticks; the program tap never does.
            Ok(Packet::Skipped { .. } | Packet::AudioGap { .. })
            | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(Ended::Program),
        }
        while let Some(mut frame) = video.receive()? {
            if !live {
                // Release the routes before warm() can return and recording
                // writers start, unless a real output joined in this loop.
                let sinks = hub.sinks();
                if !sinks.iter().any(|(_, sink)| sink.keeps_encoding()) {
                    tap = None;
                    hub.tapped.store(false, Ordering::SeqCst);
                }
            }
            hub.ready.store(true, Ordering::SeqCst);
            if !live || warm_pts == Some(frame.pts) {
                continue;
            }
            // Restore the host clock before interleaving with unshifted AAC.
            frame.pts = Time::from_nanos(frame.pts.as_nanos() + video_offset);
            frame.dts = Time::from_nanos(frame.dts.as_nanos() + video_offset);
            if let Some(record) = frame
                .parameter_sets
                .as_ref()
                .and_then(flv::avc_decoder_configuration)
                && config.as_deref() != Some(&record)
            {
                config = Some(Arc::new(record));
            }
            order.push(Media {
                ts: frame.dts,
                kind: Kind::Video {
                    keyframe: frame.keyframe,
                },
                data: frame.data,
                config: frame.keyframe.then(|| config.clone()).flatten(),
            });
        }
        let dropped = video.dropped();
        let intentional = video.intentional_video_drops();
        if dropped > encoder_dropped {
            if live {
                crate::governor::protected_loss(
                    canvas,
                    (dropped - encoder_dropped)
                        .saturating_sub(intentional.saturating_sub(intentional_dropped)),
                );
            }
            each(&|sink| sink.encoder_dropped(dropped - encoder_dropped));
            encoder_dropped = dropped;
        }
        intentional_dropped = intentional;
        let ready = order.release();
        if !ready.is_empty() {
            each(&|sink| sink.push(&ready));
        }
    }
}

/// The governor's live bitrate for one shared encoder. A refused change
/// keeps the rate the encoder had, is reported once for the value asked,
/// and is tried again only once the governor asks for something else.
struct Retune {
    canvas: CanvasId,
    rate: crate::governor::StreamRate,
    /// What the outputs asked for (the hub's spec).
    intended: u32,
    /// What the encoder acknowledged last.
    applied: u32,
    /// The value it last refused.
    refused: Option<u32>,
}

impl Retune {
    fn new(canvas: CanvasId, intended: u32) -> Self {
        Self {
            canvas,
            rate: crate::governor::StreamRate::new(canvas, intended),
            intended,
            applied: intended,
            refused: None,
        }
    }

    fn apply<E: VideoEncoder>(&mut self, video: &mut E, fps: u32, applied: &AtomicU32) {
        let wanted = self.rate.wanted();
        if wanted == self.applied && !video.rate_uncertain() {
            self.refused = None;
            return;
        }
        if self.refused == Some(wanted) {
            return;
        }
        let result = if wanted == 0 {
            Err(anyhow::anyhow!("0 kb/s is not a video bitrate"))
        } else {
            video.set_rate(fps, wanted)
        };
        match result {
            Ok(()) => {
                isocast_diagnostics::debug!(
                    "encoder: {:?} program video {} -> {wanted} kb/s (asked {} kb/s)",
                    self.canvas,
                    self.applied,
                    self.intended
                );
                self.applied = wanted;
                self.refused = None;
                applied.store(wanted, Ordering::Relaxed);
                self.rate.result(wanted, None);
            }
            Err(err) => {
                let message = format!(
                    "the program's encoder last acknowledged {} kb/s; could not set {wanted} kb/s: {err:#}",
                    self.applied
                );
                isocast_diagnostics::diagnostic!("warn"; "{message}");
                self.refused = Some(wanted);
                self.rate.result(self.applied, Some(message));
            }
        }
    }
}
