//! Monitors' video (`frames`): a target's picture (the program, studio
//! mode's preview or a source) as low-latency H.264 from the platform
//! encoder. Everyone watching the same target at the same size and profile shares one
//! encoder thread, which runs only while someone watches, at the highest
//! rate any of them asks for (a viewer can change its rate at any time, as
//! the app does while the user drags on the canvas). Each record is encoded
//! once (see `isocast_protocol::api::frames`) and the same bytes go to
//! every viewer.
//!
//! A monitor-only GPU stage resizes mismatched frames before encoding;
//! native encoders need not support implicit scaling. Both the stage and
//! encoder are shared by `frames` clients and WHEP viewers at this width.

use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use isocast_protocol::ScaleFilter;
use isocast_protocol::api::{H264Profile, frames};

use crate::gpu::Gpu;
use crate::source::FrameSlot;
use crate::stream::flv::avc_decoder_configuration;
use crate::{EncodedVideo, EncoderFrameFormat, LiveVideoSpec, Platform, Time, VideoEncoder};

use super::iso_frames::IsoFrames;

/// Bits per pixel per frame: a sharp monitor picture of a screen.
const BITS_PER_PIXEL: f64 = 0.1;
/// Keyframes come at least this often. A viewer joining or falling behind
/// gets one at once, and nothing is lost on the way, so more would only
/// add bitrate spikes (and a slower frame, mid-drag).
const KEYFRAME_SECONDS: u32 = 60;
/// Records a viewer may fall behind before it skips ahead to the next
/// keyframe (a frame can come with a revision record).
const VIEWER_QUEUE: usize = 16;
/// How long to wait for a frame before checking on viewers, the target and
/// keyframe requests.
const WAIT: Duration = Duration::from_millis(50);
/// The longest an encoded frame may take to come out.
const ENCODE_TIMEOUT: Duration = Duration::from_millis(100);

/// One record's bytes, shared by every viewer.
pub type Record = Arc<[u8]>;

/// The most frames a second a stream sends.
pub const MAX_FPS: u32 = 60;

/// What a stream shows.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Key {
    pub(crate) target: String,
    pub(crate) max_width: Option<u32>,
    pub(crate) profile: H264Profile,
}

/// The running streams.
#[derive(Default)]
pub(crate) struct Streams {
    running: Mutex<HashMap<Key, Arc<Shared>>>,
    workers: Mutex<Vec<thread::JoinHandle<()>>>,
    stopping: AtomicBool,
}

/// Native encoders must finish destruction before the daemon exits and runs
/// driver-library finalizers. Removing a stream from the map is not completion:
/// its worker still owns the encoder and GPU conversion stage at that point.
pub(crate) fn stop(streams: &Streams) {
    // Serialize shutdown with admission and other stop callers. Workers never
    // take this lock, including while removing themselves from the map.
    let mut workers = streams.workers.lock();
    streams.stopping.store(true, Ordering::SeqCst);
    for worker in workers.drain(..) {
        let _ = worker.join();
    }
}

/// Where a target's frames arrive and, for pictures drawn on the canvas's
/// frame grid (the program and the preview), that grid (when it starts and
/// its rate) and the setup revision each frame was drawn from.
pub(crate) struct Source<F> {
    pub(crate) slot: Arc<FrameSlot<F>>,
    pub(crate) grid: Option<(Time, u32)>,
    pub(crate) drawn: Option<Arc<Drawn>>,
}

/// What a program (or preview) frame was drawn from: the setup revision,
/// and the layout generation (see [`crate::record::Engine::resolved`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stamp {
    pub revision: u64,
    pub generation: u64,
}

/// What recent program frames were drawn from, by tick.
pub(crate) type Drawn = Mutex<VecDeque<(Time, Stamp)>>;

/// Ticks of [`Drawn`] kept: more than any frame waits to be streamed.
const DRAWN_KEPT: usize = 32;

/// Note that the frame at `tick` was drawn from `stamp`.
pub(crate) fn drawn(drawn: &Drawn, tick: Time, stamp: Stamp) {
    let mut drawn = drawn.lock();
    if drawn.len() == DRAWN_KEPT {
        drawn.pop_front();
    }
    drawn.push_back((tick, stamp));
}

/// What the frame at `tick` was drawn from, if known.
pub(crate) fn stamp_at(drawn: &Drawn, tick: Time) -> Option<Stamp> {
    let drawn = drawn.lock();
    drawn
        .iter()
        .rev()
        .find(|(at, _)| *at == tick)
        .map(|(_, stamp)| *stamp)
}

pub(crate) struct Shared {
    viewers: Mutex<Vec<Viewer>>,
    want_keyframe: AtomicBool,
    feedback_at: Mutex<Option<std::time::Instant>>,
    next_viewer: AtomicU64,
}

struct Viewer {
    id: u64,
    tx: SyncSender<Record>,
    /// The most frames a second it wants.
    fps: Arc<AtomicU32>,
    /// Has the current config and a keyframe, so frames decode.
    synced: bool,
    /// The setup revision and layout generation it was last told frames
    /// show.
    revision: Option<Stamp>,
}

/// One client watching a target: the stream's records, in order, until
/// dropped. The channel closes when the stream ends (after an end record
/// saying why, when there is room for it).
pub struct FrameFeed {
    pub records: Receiver<Record>,
    fps: Arc<AtomicU32>,
    shared: Arc<Shared>,
    id: u64,
}

impl FrameFeed {
    /// The viewer connected after its handshake and needs a fresh IDR.
    /// Unlike RTCP feedback, this happens only once per admitted viewer.
    pub fn connected(&self) {
        self.shared.want_keyframe.store(true, Ordering::SeqCst);
    }

    /// PLI/FIR recovery is coalesced and rate-limited by the shared encoder.
    pub fn request_keyframe(&self) {
        let mut last = self.shared.feedback_at.lock();
        if last.is_none_or(|last| last.elapsed() >= Duration::from_secs(1)) {
            *last = Some(std::time::Instant::now());
            self.shared.want_keyframe.store(true, Ordering::SeqCst);
        }
    }

    /// A handle that changes how many frames a second this viewer wants,
    /// from any thread; the stream follows from its next frame, without a
    /// new keyframe.
    pub fn rate(&self) -> FeedRate {
        FeedRate(self.fps.clone())
    }
}

/// Changes a viewer's rate (see [`FrameFeed::rate`]).
#[derive(Clone)]
pub struct FeedRate(Arc<AtomicU32>);

impl FeedRate {
    pub fn set(&self, fps: u32) {
        self.0.store(fps.clamp(1, MAX_FPS), Ordering::Relaxed);
    }
}

/// The highest rate any running stream's viewer asks for.
pub(crate) fn highest_rate(streams: &Streams) -> Option<u32> {
    let running = streams.running.lock();
    running
        .values()
        .flat_map(|shared| {
            shared
                .viewers
                .lock()
                .iter()
                .map(|viewer| viewer.fps.load(Ordering::Relaxed))
                .collect::<Vec<_>>()
        })
        .max()
}

impl Drop for FrameFeed {
    fn drop(&mut self) {
        self.shared
            .viewers
            .lock()
            .retain(|viewer| viewer.id != self.id);
    }
}

/// Watch `key`'s stream at up to `fps` frames a second, starting it if
/// nobody watches it yet. `source` finds where the target's frames arrive
/// (looked up again all the time, since a source can reopen, and given the
/// stream's rate then), or says why it can't.
pub(crate) fn watch<P: Platform>(
    streams: &Arc<Streams>,
    platform: &P,
    key: Key,
    fps: u32,
    source: impl Fn(u32) -> Result<Source<P::Frame>, String> + Send + 'static,
) -> Result<FrameFeed, String> {
    let mut workers = streams.workers.lock();
    if streams.stopping.load(Ordering::SeqCst) {
        return Err("engine stopped".into());
    }
    // Finished threads have already dropped their encoders; do not retain one
    // handle for every monitor ever opened during a long-lived engine session.
    workers.retain(|worker| !worker.is_finished());
    let (tx, records) = sync_channel(VIEWER_QUEUE);
    // Held while the viewer is added, so a stream whose last viewer just
    // left can't remove itself in between.
    let mut running = streams.running.lock();
    let shared = match running.get(&key) {
        Some(shared) => shared.clone(),
        None => {
            let shared = Arc::new(Shared {
                viewers: Mutex::new(Vec::new()),
                want_keyframe: AtomicBool::new(true),
                feedback_at: Mutex::new(None),
                next_viewer: AtomicU64::new(0),
            });
            running.insert(key.clone(), shared.clone());
            let (streams, stream, platform) = (streams.clone(), shared.clone(), platform.clone());
            let (failed_streams, failed_stream, failed_key) =
                (streams.clone(), stream.clone(), key.clone());
            let worker = isocast_diagnostics::spawn(
                format!("frames {}", key.target),
                move || run(&streams, &stream, &key, &platform, source),
                move |why| end(&failed_streams, &failed_key, &failed_stream, &why),
            )
            .expect("could not start a frame stream thread");
            workers.push(worker);
            shared
        }
    };
    let id = shared.next_viewer.fetch_add(1, Ordering::Relaxed);
    let fps = Arc::new(AtomicU32::new(fps.clamp(1, MAX_FPS)));
    shared.viewers.lock().push(Viewer {
        id,
        tx,
        fps: fps.clone(),
        synced: false,
        revision: None,
    });
    shared.want_keyframe.store(true, Ordering::SeqCst);
    Ok(FrameFeed {
        records,
        fps,
        shared,
        id,
    })
}

/// The encoder for one input size.
struct Encoding<E> {
    encoder: E,
    input: (u32, u32),
    output: (u32, u32),
    /// The rate it was told to expect: the last one it accepted.
    fps: u32,
    /// Don't retry an unchanged refused request on every picture.
    refused: Option<u32>,
    /// This encoder's rate status, for the governor.
    notice: crate::governor::PreviewRate,
    /// The current config record.
    config: Option<Record>,
    last_pts: Option<Time>,
    /// The setup revision of frames given to the encoder and not out yet,
    /// by time.
    revisions: VecDeque<(Time, Option<Stamp>)>,
}

/// Encode the target's frames, at most as many a second as any viewer
/// wants, until nobody watches or the target goes away.
fn run<P: Platform>(
    streams: &Streams,
    shared: &Arc<Shared>,
    key: &Key,
    platform: &P,
    source: impl Fn(u32) -> Result<Source<P::Frame>, String>,
) {
    let mut encoding: Option<Encoding<P::VideoEncoder>> = None;
    // Reuse the existing bounded GPU resize/conversion machinery, but with
    // an independent pool and no recording filters or routes. Matching-size
    // and matching-format monitors bypass it. No pixel readback.
    let mut scaling: Option<IsoFrames<P>> = None;
    let mut gpu_retries = 0;
    let (mut scaled_tx, mut scaled_rx) = sync_channel(1);
    let mut pace = Pace::default();
    let mut effective_key = key.clone();
    loop {
        if streams.stopping.load(Ordering::SeqCst) {
            return end(streams, key, shared, "engine stopped");
        }
        let fps = shared
            .viewers
            .lock()
            .iter()
            .map(|viewer| viewer.fps.load(Ordering::Relaxed))
            .max();
        let Some(fps) = fps else {
            let mut running = streams.running.lock();
            if shared.viewers.lock().is_empty() {
                remove(&mut running, key, shared);
                return;
            }
            continue;
        };
        let fps = crate::governor::monitor_fps(fps);
        if fps == 0 {
            thread::sleep(Duration::from_millis(50));
            continue;
        }
        let Source { slot, grid, drawn } = match source(fps) {
            Ok(source) => source,
            Err(why) => return end(streams, key, shared, &why),
        };
        // Someone who just joined gets the newest frame again.
        let again = || shared.want_keyframe.load(Ordering::SeqCst);
        let Some((frame, at)) = pace.next(platform, &slot, grid, fps, again) else {
            continue;
        };
        let size = platform.frame_size(&frame);
        effective_key.max_width = Some(
            super::shared_frames::fitted(
                size,
                crate::governor::monitor_size((key.max_width, None)),
            )
            .0,
        );
        let needs_stage = needs_conversion(
            size,
            scaled(size, effective_key.max_width),
            platform.frame_is_ycbcr(&frame),
            platform.encoder_frame_format(),
        );
        if encoding
            .as_ref()
            .is_none_or(|e| e.input != size || e.output != scaled(size, effective_key.max_width))
            || scaling.is_some() != needs_stage
        {
            let started = start_encoder(platform, size, &effective_key, fps).and_then(|started| {
                let scaling = if needs_stage {
                    Some(
                        IsoFrames::new(
                            platform,
                            Gpu::for_platform(platform)?,
                            &key.target,
                            started.output,
                            None,
                            Time::from_nanos(0),
                            ScaleFilter::Area,
                        )?
                        .with_cached_imports(),
                    )
                } else {
                    None
                };
                Ok((started, scaling))
            });
            match started {
                Ok((started, resized)) => {
                    encoding = Some(started);
                    scaling = resized;
                    shared.want_keyframe.store(true, Ordering::SeqCst);
                }
                Err(err) => return end(streams, key, shared, &format!("{err:#}")),
            }
        }
        let frame = match &mut scaling {
            Some(stage) => {
                let resized = stage.draw(&frame, at).and_then(|pending| {
                    let Some(pending) = pending else {
                        return Ok(None);
                    };
                    let tx = scaled_tx.clone();
                    pending.then(move |frame, _| {
                        let _ = tx.try_send(frame);
                    });
                    // Only this optional monitor worker waits. The GPU polling
                    // thread never blocks, and unfinished pooled pixels cannot
                    // reach the encoder (or return to their pool) early.
                    scaled_rx
                        .recv_timeout(Duration::from_secs(1))
                        .map(Some)
                        .context("monitor GPU resize did not complete")
                });
                match resized {
                    Ok(Some(resized)) => {
                        // GPU reads are complete; don't pin the source pool
                        // while waiting for the native encoder's output.
                        drop(frame);
                        gpu_retries = 0;
                        resized
                    }
                    Ok(None) => continue,
                    Err(_) if stage.device_lost() && gpu_retries < 3 => {
                        // Like the ISO stage, recover on a fresh shared device.
                        // Retain subscribers, but never retry a persistently
                        // failing device forever or spin while it reopens.
                        gpu_retries += 1;
                        scaling = None;
                        encoding = None;
                        // An old completion racing device loss must never be
                        // mistaken for the new stage's frame/timestamp.
                        (scaled_tx, scaled_rx) = sync_channel(1);
                        thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                    Err(err) if stage.device_lost() => {
                        return end(
                            streams,
                            key,
                            shared,
                            &format!(
                                "monitor GPU device lost after three recovery attempts: {err:#}"
                            ),
                        );
                    }
                    Err(err) => return end(streams, key, shared, &format!("{err:#}")),
                }
            }
            None => frame,
        };
        let encoding = encoding.as_mut().expect("started above");
        // A new rate goes to the running encoder: no new session, no
        // keyframe. One it refuses isn't fatal: it keeps its last rate
        // while frames are still picked at the new one, and that rate
        // isn't asked again until the request changes.
        let uncertain = encoding.encoder.rate_uncertain();
        if encoding.fps == fps && !uncertain && encoding.refused.take().is_some() {
            encoding.notice.result(None);
        } else if (encoding.fps != fps || uncertain) && encoding.refused != Some(fps) {
            let bitrate = bitrate_kbps(encoding.output, fps);
            match encoding.encoder.set_rate(fps, bitrate) {
                Ok(()) => {
                    encoding.fps = fps;
                    encoding.refused = None;
                    encoding.notice.result(None);
                }
                Err(err) => {
                    let message = format!(
                        "the {} monitor's last acknowledged encoder rate is {} fps; could not set {fps} fps: {err:#}",
                        key.target, encoding.fps
                    );
                    isocast_diagnostics::diagnostic!("warn"; "{message}");
                    encoding.refused = Some(fps);
                    encoding.notice.result(Some(message));
                }
            }
        }
        // Frame times must increase, even for a frame sent again.
        let pts = match encoding.last_pts {
            Some(last) if at <= last => last + Duration::from_millis(1),
            _ => at,
        };
        let keyframe = shared.want_keyframe.swap(false, Ordering::SeqCst);
        let revision = drawn.and_then(|drawn| stamp_at(&drawn, at));
        let result = encoding
            .encoder
            .encode(&frame, pts, keyframe)
            .and_then(|()| {
                drop(frame);
                encoding.last_pts = Some(pts);
                if encoding.revisions.len() == DRAWN_KEPT {
                    encoding.revisions.pop_front();
                }
                encoding.revisions.push_back((pts, revision));
                let mut out = encoding.encoder.receive_timeout(ENCODE_TIMEOUT)?;
                while let Some(encoded) = out {
                    deliver(shared, encoding, encoded);
                    out = encoding.encoder.receive()?;
                }
                Ok(())
            });
        if let Err(err) = result {
            return end(streams, key, shared, &format!("{err:#}"));
        }
    }
}

/// Picks the frames a monitor shows at a rate from a target's slot.
#[derive(Default)]
pub(crate) struct Pace {
    last: Option<Time>,
    /// A frame passed over for being off the grid.
    skipped: Option<Time>,
}

impl Pace {
    /// The next frame to show at `fps`, waiting up to [`WAIT`] for it; or,
    /// when none comes and `again` says so, the newest one again (a still
    /// screen only sends frames when it changes). `None` if none is due:
    /// ask again.
    pub(crate) fn next<P: Platform>(
        &mut self,
        platform: &P,
        slot: &FrameSlot<P::Frame>,
        grid: Option<(Time, u32)>,
        fps: u32,
        again: impl Fn() -> bool,
    ) -> Option<(P::Frame, Time)> {
        self.pick(platform, slot, grid, fps, again, WAIT)
    }

    /// With a zero wait, try the same frame grid without sleeping on the
    /// source: a shared-memory viewer has a readback to wake for instead.
    pub(super) fn pick<P: Platform>(
        &mut self,
        platform: &P,
        slot: &FrameSlot<P::Frame>,
        grid: Option<(Time, u32)>,
        fps: u32,
        again: impl Fn() -> bool,
        wait_limit: Duration,
    ) -> Option<(P::Frame, Time)> {
        let period = Duration::from_secs(1) / fps;
        // On the canvas's grid at a multiple of the stream's rate, only every
        // n-th tick from its start: then every program and preview stream at
        // a rate carries the same ticks, and a window showing several
        // monitors redraws them together (one composite a frame, not one
        // per monitor).
        let grid = grid.filter(|&(_, rate)| rate % fps == 0 && rate >= 2 * fps);
        // A frame at least about a period after the last one, so a 60 fps
        // program is shown at 30 without waking for the frames between.
        let from = self
            .last
            .map_or(Time::from_nanos(i64::MIN), |last| last + period * 3 / 4);
        let from = self
            .skipped
            .map_or(from, |at| from.max(Time::from_nanos(at.as_nanos() + 1)));
        if let Some(wait) = from.as_nanos().checked_sub(platform.now().as_nanos())
            && wait > 0
        {
            if wait_limit.is_zero() {
                return None;
            }
            thread::sleep(Duration::from_nanos(wait as u64).min(wait_limit));
        }
        let (picked, again) = match slot.wait_from(from, wait_limit) {
            Some(frame) => (Some(frame), false),
            None if again() => (slot.newest_timed(), true),
            None => (None, false),
        };
        let (frame, at) = picked?;
        if let Some((start, rate)) = grid
            && !again
            && at
                .ticks_since(start, rate)
                .rem_euclid(i64::from(rate / fps))
                != 0
        {
            self.skipped = Some(at);
            return None;
        }
        self.last = Some(at);
        self.skipped = None;
        P::retain_frame(&frame, crate::FrameUse::Auxiliary).map(|frame| (frame, at))
    }
}

/// An encoder for frames of `input` size at `fps`, scaled to the stream's
/// width.
fn start_encoder<P: Platform>(
    platform: &P,
    input: (u32, u32),
    key: &Key,
    fps: u32,
) -> Result<Encoding<P::VideoEncoder>> {
    let output = scaled(input, key.max_width);
    let encoder = platform.create_video_encoder(&LiveVideoSpec {
        profile: key.profile,
        width: output.0,
        height: output.1,
        fps,
        bitrate_kbps: bitrate_kbps(output, fps),
        keyframe_interval: fps * KEYFRAME_SECONDS,
        constant_bitrate: false,
        low_latency: true,
    })?;
    Ok(Encoding {
        encoder,
        input,
        output,
        fps,
        refused: None,
        notice: crate::governor::PreviewRate::new(),
        config: None,
        last_pts: None,
        revisions: VecDeque::new(),
    })
}

/// Native encoders receive their own size and Y'CbCr/BGRA frame family.
fn needs_conversion(
    input: (u32, u32),
    output: (u32, u32),
    ycbcr: bool,
    format: EncoderFrameFormat,
) -> bool {
    input != output || ycbcr != (format == EncoderFrameFormat::Ycbcr)
}

/// The bitrate for `size` at `fps`.
fn bitrate_kbps((width, height): (u32, u32), fps: u32) -> u32 {
    let bits = f64::from(width) * f64::from(height) * f64::from(fps) * BITS_PER_PIXEL;
    ((bits / 1000.0) as u32).max(300)
}

/// `size` scaled down to at most `max_width` wide, keeping its shape, in
/// even numbers (4:2:0 H.264 needs them).
fn scaled((width, height): (u32, u32), max_width: Option<u32>) -> (u32, u32) {
    let (w, h) = match max_width {
        Some(max) if max > 0 && max < width => (
            max,
            ((u64::from(height) * u64::from(max) + u64::from(width) / 2) / u64::from(width)) as u32,
        ),
        _ => (width, height),
    };
    ((w & !1).max(2), (h & !1).max(2))
}

/// Send an encoded frame to every viewer: a viewer that isn't in sync yet
/// (just joined, or fell behind) gets the config and waits for a keyframe;
/// one that doesn't know the setup revision the frame shows is told first.
fn deliver<E>(shared: &Shared, encoding: &mut Encoding<E>, encoded: EncodedVideo) {
    let (width, height) = encoding.output;
    let revision = match encoding
        .revisions
        .iter()
        .position(|(pts, _)| *pts == encoded.pts)
    {
        Some(index) => {
            let revision = encoding.revisions[index].1;
            encoding.revisions.drain(..=index);
            revision
        }
        None => None,
    };
    let mut config_changed = false;
    if let Some(record) = encoded
        .parameter_sets
        .as_ref()
        .filter(|_| encoded.keyframe)
        .and_then(avc_decoder_configuration)
    {
        let current = encoding.config.as_deref().map(|c| &c[frames::HEADER..]);
        if current != Some(&record[..]) {
            encoding.config =
                Some(frames::encode(frames::CONFIG, 0, width, height, &record).into());
            config_changed = true;
        }
    }
    let kind = match encoded.keyframe {
        true => frames::KEY_FRAME,
        false => frames::FRAME,
    };
    let seq = encoded.pts.as_nanos().max(0) as u64;
    let record: Record = frames::encode(kind, seq, width, height, &encoded.data).into();
    let revision_record: Option<Record> = revision.map(|stamp| {
        let mut payload = [0u8; 16];
        payload[..8].copy_from_slice(&stamp.revision.to_be_bytes());
        payload[8..].copy_from_slice(&stamp.generation.to_be_bytes());
        frames::encode(frames::REVISION, seq, 0, 0, &payload).into()
    });
    let mut behind = false;
    shared.viewers.lock().retain_mut(|viewer| {
        if encoded.keyframe && (!viewer.synced || config_changed) {
            viewer.synced = false;
            if let Some(config) = &encoding.config {
                match viewer.tx.try_send(config.clone()) {
                    Ok(()) => viewer.synced = true,
                    Err(TrySendError::Full(_)) => {}
                    Err(TrySendError::Disconnected(_)) => return false,
                }
            }
        }
        if viewer.synced
            && let Some(update) = &revision_record
            && viewer.revision != revision
        {
            match viewer.tx.try_send(update.clone()) {
                Ok(()) => viewer.revision = revision,
                Err(TrySendError::Full(_)) => viewer.synced = false,
                Err(TrySendError::Disconnected(_)) => return false,
            }
        }
        if viewer.synced {
            match viewer.tx.try_send(record.clone()) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => viewer.synced = false,
                Err(TrySendError::Disconnected(_)) => return false,
            }
        }
        if !viewer.synced {
            // Told again once back in sync.
            viewer.revision = None;
            behind = true;
        }
        true
    });
    if behind {
        shared.want_keyframe.store(true, Ordering::SeqCst);
    }
}

/// Stop the stream: tell every viewer why, then close their channels.
fn end(streams: &Streams, key: &Key, shared: &Arc<Shared>, why: &str) {
    remove(&mut streams.running.lock(), key, shared);
    let record: Record = frames::encode(frames::END, 0, 0, 0, why.as_bytes()).into();
    for viewer in shared.viewers.lock().drain(..) {
        let _ = viewer.tx.try_send(record.clone());
    }
}

/// Forget `shared` as `key`'s stream (unless another has taken its place).
fn remove(running: &mut HashMap<Key, Arc<Shared>>, key: &Key, shared: &Arc<Shared>) {
    if running
        .get(key)
        .is_some_and(|current| Arc::ptr_eq(current, shared))
    {
        running.remove(key);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn stop_waits_for_worker_resource_destruction() {
        use super::*;

        struct Resource(SyncSender<&'static str>);
        impl Drop for Resource {
            fn drop(&mut self) {
                self.0.send("destroyed").unwrap();
            }
        }

        let streams = Arc::new(Streams::default());
        let worker_streams = streams.clone();
        let (events, received) = sync_channel(2);
        let finished = events.clone();
        streams.workers.lock().push(thread::spawn(move || {
            let _resource = Resource(events);
            while !worker_streams.stopping.load(Ordering::SeqCst) {
                thread::yield_now();
            }
        }));
        stop(&streams);
        finished.send("stopped").unwrap();
        assert_eq!(received.recv().unwrap(), "destroyed");
        assert_eq!(received.recv().unwrap(), "stopped");
        stop(&streams);
    }

    #[test]
    fn monitor_conversion_matches_encoder_size_and_format() {
        use super::{
            EncoderFrameFormat::{Bgra, Ycbcr},
            needs_conversion,
        };
        let size = (640, 480);
        assert!(!needs_conversion(size, size, false, Bgra));
        assert!(!needs_conversion(size, size, true, Ycbcr));
        assert!(
            needs_conversion(size, size, true, Bgra),
            "native-size NV12 must become BGRA"
        );
        assert!(needs_conversion(size, size, false, Ycbcr));
        assert!(needs_conversion(size, (480, 360), false, Bgra));
        assert!(needs_conversion(size, (480, 360), true, Ycbcr));
    }

    #[test]
    fn connection_keyframe_is_not_lost_to_feedback_rate_limit() {
        use super::*;
        let shared = Arc::new(Shared {
            viewers: Mutex::new(Vec::new()),
            want_keyframe: AtomicBool::new(false),
            feedback_at: Mutex::new(Some(std::time::Instant::now())),
            next_viewer: AtomicU64::new(1),
        });
        let (_, records) = sync_channel(1);
        let feed = FrameFeed {
            records,
            fps: Arc::new(AtomicU32::new(30)),
            shared: shared.clone(),
            id: 0,
        };
        feed.request_keyframe();
        assert!(!shared.want_keyframe.load(Ordering::SeqCst));
        feed.connected();
        assert!(shared.want_keyframe.load(Ordering::SeqCst));
    }

    use super::scaled;

    #[test]
    fn sizes_keep_their_shape_in_even_numbers() {
        assert_eq!(scaled((1920, 1080), Some(1664)), (1664, 936));
        assert_eq!(scaled((1920, 1080), None), (1920, 1080));
        assert_eq!(scaled((1280, 720), Some(1920)), (1280, 720));
        assert_eq!(scaled((1206, 2622), Some(500)), (500, 1086));
        assert_eq!(scaled((3, 3), None), (2, 2));
    }
}
