//! The program's live H.264 and AAC encode, shared by the outputs that
//! take it compressed: the stream and the replay buffer.
//!
//! Hardware encoder sessions are what a busy Mac runs out of first (#41,
//! #60), so outputs asking for the same format share one encoder, as OBS's
//! outputs do: the first to start creates it, each later one with an equal
//! [`EncoderSpec`] subscribes to it, and it stops when the last
//! unsubscribes. Its thread taps the program, encodes, interleaves audio
//! and video by time and hands each batch to every [`Sink`].

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result};

use super::queue::{Kind, Media};
use super::{AudioTrack, Ended, Interleaver, TAP_DEPTH, flv};
use crate::record::{Engine, Packet, ProgramTap};
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
    spec: EncoderSpec,
    stop: AtomicBool,
    /// Ended by itself (the program stopped or it failed): new outputs
    /// don't join it.
    ended: AtomicBool,
    sinks: Mutex<Vec<(u64, Arc<dyn Sink>)>>,
    /// How many sinks there are, read without `sinks` (callers may hold
    /// their own locks, which sinks take while `sinks` is held).
    sink_count: AtomicU64,
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
    /// Join the running encoder of `spec`, or start one. `what` names the
    /// output in errors ("the stream").
    pub(crate) fn subscribe<P: Platform>(
        &self,
        engine: &Engine<P>,
        spec: &EncoderSpec,
        what: &str,
        sink: Arc<dyn Sink>,
    ) -> Result<Subscription> {
        let mut hubs = self.0.lock().unwrap();
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
            let id = hub.next_sink.fetch_add(1, Ordering::Relaxed);
            sinks.push((id, sink));
            hub.sink_count.store(sinks.len() as u64, Ordering::SeqCst);
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
        let video = platform
            .create_video_encoder(&spec.video)
            .with_context(|| format!("could not start {what}'s H.264 encoder"))?;
        let audio = platform
            .create_audio_encoder(&spec.audio)
            .with_context(|| format!("could not start {what}'s AAC encoder"))?;
        let tap = engine.tap_program(TAP_DEPTH);
        let hub = Arc::new(Hub {
            spec: *spec,
            stop: AtomicBool::new(false),
            ended: AtomicBool::new(false),
            sinks: Mutex::new(vec![(0, sink)]),
            sink_count: AtomicU64::new(1),
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
                        encode::<P>(video, audio, tap, &hub)
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
        *hub.thread.lock().unwrap_or_else(|p| p.into_inner()) = Some(thread);
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
    fn sinks(&self) -> std::sync::MutexGuard<'_, Vec<(u64, Arc<dyn Sink>)>> {
        self.sinks.lock().unwrap_or_else(|p| p.into_inner())
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
            self.hub
                .sink_count
                .store(sinks.len() as u64, Ordering::SeqCst);
            let last = sinks.is_empty();
            if last {
                self.hub.stop.store(true, Ordering::SeqCst);
            }
            last
        };
        if last {
            // The thread checks `stop` every few milliseconds.
            let thread = self
                .hub
                .thread
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take();
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
    hub: &Hub,
) -> Result<Ended> {
    let spec = &hub.spec;
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
    let mut config: Option<Arc<Vec<u8>>> = None;
    let mut track = AudioTrack::new(audio.delay());
    let mut order = Interleaver::default();
    loop {
        if hub.stop.load(Ordering::SeqCst) {
            return Ok(Ended::Stopped);
        }
        match tap.packets.recv_timeout(Duration::from_millis(5)) {
            Ok(Packet::Video { frame, pts }) => {
                // Frames come one per canvas tick; any between this and the
                // last never reached the encoder (the tap was full, or the
                // program missed them), so they count as dropped.
                if let Some(last) = last_frame.replace(pts) {
                    let ticks = (pts.as_nanos() - last.as_nanos() + frame_ns / 2) / frame_ns;
                    if ticks > 1 {
                        each(&|sink| sink.missed(ticks as u64 - 1));
                    }
                }
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
                video.encode(&frame, pts, keyframe)?;
                keyframe_missed = keyframe && video.dropped() > dropped;
            }
            Ok(Packet::Audio { samples, pts, .. }) => {
                track.add(&samples, pts, &mut audio, &mut order)?;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(Ended::Program),
        }
        while let Some(frame) = video.receive()? {
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
        if dropped > encoder_dropped {
            each(&|sink| sink.encoder_dropped(dropped - encoder_dropped));
            encoder_dropped = dropped;
        }
        let ready = order.release();
        if !ready.is_empty() {
            each(&|sink| sink.push(&ready));
        }
    }
}
