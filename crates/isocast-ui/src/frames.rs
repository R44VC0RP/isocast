//! A monitor's picture without copying it (`shared_frames`): a thread of
//! its own receives each frame, a GPU surface the daemon has finished
//! drawing, and hands it to the window's thread. Reconnects with back-off
//! when the daemon goes away, and at once when the target changes (the
//! edited monitor shows `preview` in Studio Mode, `program` otherwise).
//!
//! A window on another computer's engine can't share GPU surfaces (the
//! engine refuses `shared_frames` to network callers): it shows the
//! engine's live H.264 video instead, decoded here in hardware
//! (`crate::video`): each picture an [`Incoming::Frame`] of this
//! process's own surfaces where the decoder keeps it on the GPU, else an
//! [`Incoming::Still`]. If this
//! machine can't decode it, the engine's JPEG stills (`frame`), a few a
//! second. A Linux window on its own engine does the same (no shared GPU
//! surfaces there yet), without WebRTC: the engine plays its own sound.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use isocast_client::shared::{SharedController, SharedFrame};
use isocast_protocol::api::{FrameParams, FramesParams, Method, RpcError, SharedFramesParams};
use slint::{Rgba8Pixel, SharedPixelBuffer};

/// Whether this OS's engine shares its pictures with a local window
/// (`shared_frames`): GPU surfaces on macOS and Windows, shared memory on
/// Linux; elsewhere a local window shows its `frames` video.
const SHARED_SURFACES: bool = cfg!(any(target_os = "macos", windows, target_os = "linux"));

/// Linux's shared frames are pictures in shared memory: copied out as
/// pixels (the slot goes straight back) and shown as Slint images like the
/// decoded video they replace, falling back to that video when they can't
/// be had. Elsewhere they're GPU surfaces shown in place.
const SHARED_IN_MEMORY: bool = cfg!(target_os = "linux");

/// The size to ask shared frames for, for a monitor `width`×`height`
/// pixels. GPU surfaces: that size (the daemon scales into them as it
/// draws). Linux's frames in memory: a width on the video ladder
/// ([`crate::video::width_step`]) and no height, as the H.264 stream asks,
/// so the engine draws the picture at that size and copies it as it is
/// (a scaling pass to the monitor's exact size cost a software GPU three
/// times the CPU); the window scales it, and a resize changes the ring only
/// when it crosses a step.
fn shared_size(width: u32, height: u32) -> (Option<u32>, Option<u32>) {
    if SHARED_IN_MEMORY {
        (Some(crate::video::width_step(width)), None)
    } else {
        (Some(width.max(16)), Some(height.max(16)))
    }
}

/// The most stills a second a remote window asks for (R2's interim
/// previews: a few, at about 0.1 MB each on the network).
pub const STILL_FPS: u32 = 5;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missed_offer_retries_but_an_unavailable_service_does_not() {
        let fetch = |why| anyhow::anyhow!("{why}").context("could not fetch the shared surfaces");
        for why in [
            "the shared surface service had no ring for that token",
            "the shared surface service answered nothing",
        ] {
            assert_eq!(shared_failure(&fetch(why)), SharedFailure::MissedOffer);
        }
        assert_eq!(
            shared_failure(&fetch("could not reach the shared surface service @test")),
            SharedFailure::Unavailable
        );
    }

    #[test]
    fn only_an_engine_without_webrtc_gives_up_on_it() {
        let now = Instant::now();
        // An older engine: frames for good, even when the speaker is on.
        let mut plan = WebRtcPlan::after_failure(true, false, now);
        assert_eq!(plan, WebRtcPlan::Never);
        assert!(!plan.use_now(now + Duration::from_secs(3600), true));
        // A failure before the first picture (UDP, a canvas change,
        // shedding, a full cap): frames for a while, then offer again.
        let mut plan = WebRtcPlan::after_failure(false, false, now);
        assert!(!plan.use_now(now + Duration::from_secs(5), false));
        assert!(plan.use_now(now + WEBRTC_RETRY, false));
        assert_eq!(plan, WebRtcPlan::Use);
        // ...or at once when the speaker is turned on.
        let mut plan = WebRtcPlan::after_failure(false, false, now);
        assert!(plan.use_now(now + Duration::from_secs(1), true));
        // The engine ended a viewer that played: offer again at once.
        let mut plan = WebRtcPlan::after_failure(false, true, now);
        assert!(plan.use_now(now, false));
    }
}

/// The engine doesn't know a method (an older engine).
fn unknown_method(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<isocast_client::DaemonError>()
        .is_some_and(|e| {
            e.code() == RpcError::METHOD_NOT_FOUND || e.0.message.contains("unknown variant")
        })
}

/// Keyframes in a row that won't decode before a remote window gives up on
/// the video and shows the engine's stills.
const KEYFRAME_FAILURES: u32 = 3;

/// The engine's video keeps failing to decode here
/// ([`KEYFRAME_FAILURES`] keyframes in a row).
#[derive(Debug)]
struct Undecodable(String);

impl std::fmt::Display for Undecodable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{KEYFRAME_FAILURES} keyframes in a row didn't decode: {}",
            self.0
        )
    }
}

impl std::error::Error for Undecodable {}

/// The engine can't share pictures here at all (asking again won't help).
pub fn unsupported(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<isocast_client::DaemonError>()
        .is_some_and(|e| e.code() == isocast_protocol::api::RpcError::UNSUPPORTED)
}

#[derive(Debug, PartialEq, Eq)]
enum SharedFailure {
    MissedOffer,
    Unavailable,
    Other,
}

fn shared_failure(error: &anyhow::Error) -> SharedFailure {
    use isocast_protocol::api::RpcError;
    let refused = error
        .downcast_ref::<isocast_client::DaemonError>()
        .is_some_and(|e| matches!(e.code(), RpcError::METHOD_NOT_FOUND | RpcError::UNSUPPORTED));
    if error
        .chain()
        .any(|e| e.to_string() == "could not fetch the shared surfaces")
    {
        // Resize withdrew the offer, or a busy exporter closed without a reply.
        if error.chain().any(|e| {
            matches!(
                e.to_string().as_str(),
                "the shared surface service had no ring for that token"
                    | "the shared surface service answered nothing"
            ) || e.downcast_ref::<std::io::Error>().is_some_and(|e| {
                matches!(
                    e.kind(),
                    std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                )
            })
        }) {
            return SharedFailure::MissedOffer;
        }
        return SharedFailure::Unavailable;
    }
    if refused
        || error.chain().any(|e| {
            e.to_string()
                .contains("isn't a Linux shared surface service")
        })
    {
        SharedFailure::Unavailable
    } else {
        SharedFailure::Other
    }
}

/// What the window gets.
pub enum Incoming {
    Frame(SharedFrame),
    /// A picture from another computer's engine, decoded (its live video,
    /// or a JPEG still).
    Still(Still),
    /// No picture: why (the daemon can't be reached, or refused).
    Lost(String),
    /// No picture, and asking again won't help: this engine can't share
    /// its pictures here (`RpcError::UNSUPPORTED`). Not asked again until
    /// the target changes.
    Unavailable(String),
    /// Another computer's engine's program sound can't play now (the
    /// program came over `frames`, not WebRTC): why, and whether for good
    /// (an older engine); none once it can.
    SoundUnavailable(Option<(String, bool)>),
}

/// How long a feed stays on `frames` after WebRTC failed before it offers
/// again (a canvas change, the engine shedding viewers under pressure, a
/// full viewer cap: all pass).
const WEBRTC_RETRY: Duration = Duration::from_secs(30);

/// Whether a feed of the program uses WebRTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WebRtcPlan {
    Use,
    /// On `frames` until then (or until the speaker is turned on).
    FramesUntil(Instant),
    /// The engine has no WebRTC (an older version): `frames` for good.
    Never,
}

impl WebRtcPlan {
    /// After WebRTC failed: an engine without the method never will; a
    /// failure before the first picture waits a while; one after it
    /// offers again at once (the engine ended the viewer).
    fn after_failure(unknown_method: bool, pictured: bool, now: Instant) -> Self {
        match (unknown_method, pictured) {
            (true, _) => Self::Never,
            (false, false) => Self::FramesUntil(now + WEBRTC_RETRY),
            (false, true) => Self::Use,
        }
    }

    /// Whether to use WebRTC now; `sound_turned_on` (the speaker, just
    /// now) offers again before the wait is over.
    fn use_now(&mut self, now: Instant, sound_turned_on: bool) -> bool {
        match *self {
            Self::Use => true,
            Self::Never => false,
            Self::FramesUntil(until) if now >= until || sound_turned_on => {
                *self = Self::Use;
                true
            }
            Self::FramesUntil(_) => false,
        }
    }
}

/// A still picture, ready to show: its pixels and, for the program and the
/// preview, the setup revision it was drawn from.
pub struct Still {
    pub pixels: SharedPixelBuffer<Rgba8Pixel>,
    pub revision: Option<u64>,
}

impl Still {
    pub fn image(&self) -> slint::Image {
        slint::Image::from_rgba8(self.pixels.clone())
    }
}

/// Decode one of `frame`'s replies.
pub fn decode(frame: &isocast_protocol::api::Frame) -> anyhow::Result<Still> {
    use base64::Engine as _;
    let jpeg = base64::engine::general_purpose::STANDARD.decode(&frame.jpeg_base64)?;
    let picture = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg)?.to_rgba8();
    let pixels =
        SharedPixelBuffer::clone_from_slice(picture.as_raw(), picture.width(), picture.height());
    Ok(Still {
        pixels,
        revision: frame.revision,
    })
}

/// The newest still of `target` after `after` (one `frame` call), fit in
/// `max_width`: `None` when none came within its wait (ask again). `fps`
/// is how often the engine draws the picture meanwhile (as for a local
/// window's preview, so its program rate reads the same), not how many
/// stills are fetched.
pub fn still(
    client: &mut isocast_client::Client,
    target: &str,
    max_width: u32,
    after: Option<u64>,
    fps: u32,
) -> anyhow::Result<Option<(u64, Still)>> {
    let reply = client.call::<isocast_protocol::api::Frame>(Method::Frame(FrameParams {
        target: target.into(),
        max_width: Some(max_width.max(16)),
        after,
        timeout_ms: Some(1000),
        quality: Some(0.7),
        fps: Some(fps.clamp(1, 60)),
    }));
    match reply {
        Ok(frame) => Ok(Some((frame.seq, decode(&frame)?))),
        // Nothing newer within the wait (the picture didn't change, or the
        // engine is busy): not an error.
        Err(error)
            if error
                .downcast_ref::<isocast_client::DaemonError>()
                .is_some_and(|e| {
                    e.code() == RpcError::FAILED && e.0.message.contains("ask again")
                }) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// What the window wants: the target (none: nothing), the picture's size
/// in pixels and the rate.
#[derive(Debug, Clone, PartialEq)]
struct Want {
    target: Option<String>,
    width: u32,
    height: u32,
    fps: u32,
    /// The receiving thread ends (`Feed::stop`).
    stopped: bool,
}

/// The receiving thread's handle: change the target, the size or the rate.
#[derive(Clone)]
pub struct Feed {
    want: Arc<Mutex<Want>>,
    control: Arc<Mutex<Option<SharedController>>>,
    /// Another computer's engine: play the program's sound here too while
    /// this feed shows the program (one feed of the window does).
    sound: Arc<std::sync::atomic::AtomicBool>,
    /// Offer WebRTC again now, before the fallback's wait is over (the
    /// speaker was clicked).
    retry: Arc<std::sync::atomic::AtomicBool>,
    /// Another computer's engine: the monitor shows decoded pictures'
    /// surfaces in place (native layers), so they needn't come as pixels.
    surfaces: Arc<std::sync::atomic::AtomicBool>,
}

impl Feed {
    /// Start receiving `target`'s picture (none: wait for one), fit in
    /// `width`×`height` pixels at `fps`. `deliver` runs on the receiving
    /// thread.
    pub fn start(
        target: Option<&str>,
        (width, height): (u32, u32),
        fps: u32,
        deliver: impl Fn(Incoming) + Send + 'static,
    ) -> Self {
        let feed = Self {
            want: Arc::new(Mutex::new(Want {
                target: target.map(str::to_string),
                width,
                height,
                fps,
                stopped: false,
            })),
            control: Arc::default(),
            sound: Arc::default(),
            retry: Arc::default(),
            surfaces: Arc::default(),
        };
        let thread_feed = feed.clone();
        thread::Builder::new()
            .name("shared frames".into())
            .spawn(move || thread_feed.receive(deliver))
            .expect("spawn a thread");
        feed
    }

    /// Show `target` instead (none: nothing).
    pub fn set_target(&self, target: Option<&str>) {
        let mut want = self.want.lock().unwrap();
        if want.target.as_deref() == target {
            return;
        }
        // The open stream ends at its next frame (or within 250 ms).
        want.target = target.map(str::to_string);
    }

    /// Play the program's sound here while this feed shows another
    /// computer's engine's program (from its next connection).
    pub fn set_sound(&self, on: bool) {
        self.sound.store(on, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether the monitor takes decoded pictures as surfaces (see
    /// [`crate::video::Decoder::decode`]); else pixels. Off at first.
    pub fn set_surfaces(&self, on: bool) {
        self.surfaces.store(on, std::sync::atomic::Ordering::SeqCst);
    }

    /// Offer WebRTC again now if the feed fell back to `frames`.
    pub fn retry_webrtc(&self) {
        self.retry.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// End the receiving thread (and its stream) for good.
    pub fn stop(&self) {
        let mut want = self.want.lock().unwrap();
        want.stopped = true;
        want.target = None;
    }

    /// Fit the picture in `width`×`height` pixels (a new ring of surfaces
    /// follows when the size changes).
    pub fn set_size(&self, width: u32, height: u32) {
        let mut want = self.want.lock().unwrap();
        if (want.width, want.height) == (width, height) {
            return;
        }
        want.width = width;
        want.height = height;
        if let Some(control) = self.control.lock().unwrap().as_ref() {
            let (width, height) = shared_size(width, height);
            let _ = control.set_size(width, height);
        }
    }

    /// At most `fps` frames a second (1 while the window is hidden).
    pub fn set_fps(&self, fps: u32) {
        let mut want = self.want.lock().unwrap();
        if want.fps == fps {
            return;
        }
        want.fps = fps;
        if let Some(control) = self.control.lock().unwrap().as_ref() {
            let _ = control.set_fps(fps);
        }
    }

    fn receive(&self, deliver: impl Fn(Incoming)) {
        if !crate::engine::is_local() || !SHARED_SURFACES {
            return self.receive_video(deliver);
        }
        let mut delay = Duration::from_millis(250);
        const MAX_OFFER_RETRIES: u8 = 3;
        let mut missed_offers = 0;
        loop {
            let want = self.want.lock().unwrap().clone();
            if want.stopped {
                return;
            }
            let Some(target) = want.target.clone() else {
                thread::sleep(Duration::from_millis(50));
                continue;
            };
            let stream = crate::daemon::connect().and_then(|client| {
                client.shared_frames(SharedFramesParams {
                    target: target.clone(),
                    max_width: shared_size(want.width, want.height).0,
                    max_height: shared_size(want.width, want.height).1,
                    fps: Some(want.fps),
                    slots: None,
                    filtered: false,
                })
            });
            let result: anyhow::Result<()> = stream.and_then(|mut frames| {
                *self.control.lock().unwrap() = Some(frames.controller());
                // A change asked for while connecting.
                let now = self.want.lock().unwrap().clone();
                if (now.width, now.height, now.fps) != (want.width, want.height, want.fps) {
                    let control = frames.controller();
                    let (width, height) = shared_size(now.width, now.height);
                    control.set_size(width, height)?;
                    control.set_fps(now.fps)?;
                }
                delay = Duration::from_millis(250);
                loop {
                    if self.want.lock().unwrap().target.as_deref() != Some(target.as_str()) {
                        return Ok(());
                    }
                    // The newest frame: older ones that arrived meanwhile
                    // go straight back to the daemon.
                    if let Some(frame) = frames.newest(Some(Duration::from_millis(250)))? {
                        missed_offers = 0;
                        #[cfg(target_os = "linux")]
                        {
                            let pixels = SharedPixelBuffer::clone_from_slice(
                                frame.surface().pixels(),
                                frame.width(),
                                frame.height(),
                            );
                            let revision = frame.revision;
                            // Copied: the slot goes back now.
                            drop(frame);
                            deliver(Incoming::Still(Still { pixels, revision }));
                        }
                        #[cfg(not(target_os = "linux"))]
                        deliver(Incoming::Frame(frame));
                    }
                }
            });
            *self.control.lock().unwrap() = None;
            match result {
                // The target changed: the new one at once.
                Ok(()) => {
                    missed_offers = 0;
                    continue;
                }
                // Linux: an engine without shared frames (an older one), or
                // a hand-over that doesn't work here (a sandbox without the
                // engine's socket namespace): its H.264 video, as before.
                // Anything else (a daemon restarting, a lost connection) is
                // retried below, so the new daemon's shared frames are used.
                Err(error) if SHARED_IN_MEMORY => match shared_failure(&error) {
                    // Reconnect for a fresh offer; this ring may never be offered again.
                    SharedFailure::MissedOffer if missed_offers < MAX_OFFER_RETRIES => {
                        missed_offers += 1
                    }
                    SharedFailure::MissedOffer | SharedFailure::Unavailable => {
                        isocast_diagnostics::diagnostic!("info"; "preview: no shared frames ({error:#}); using the engine's video stream");
                        return self.receive_video(deliver);
                    }
                    SharedFailure::Other => deliver(Incoming::Lost(format!("{error:#}"))),
                },
                Err(error) if unsupported(&error) => {
                    deliver(Incoming::Unavailable(format!("{error:#}")));
                    // Wait for another target (or the end) instead of
                    // asking every few seconds.
                    loop {
                        let now = self.want.lock().unwrap().clone();
                        if now.stopped || now.target != want.target {
                            break;
                        }
                        thread::sleep(Duration::from_millis(100));
                    }
                    delay = Duration::from_millis(250);
                    continue;
                }
                Err(error) => deliver(Incoming::Lost(format!("{error:#}"))),
            }
            thread::sleep(delay);
            delay = (delay * 2).min(Duration::from_secs(5));
        }
    }

    /// A remote engine's live video (`frames`, H.264), decoded here,
    /// reconnecting with back-off; its stills if this machine can't decode
    /// it. The pictures fit in the monitor, at a width on
    /// [`crate::video::width_step`]'s ladder.
    fn receive_video(&self, deliver: impl Fn(Incoming)) {
        // Made once there's something to show (a hidden monitor costs no
        // decoder, and on Windows no D3D11 device).
        let mut decoder: Option<Box<dyn crate::video::Decoder>> = None;
        let mut delay = Duration::from_millis(250);
        // The engine can't scale its pictures for the stream (Windows'
        // encoders take only the canvas's size so far): ask for them as
        // they are, and the monitor scales them.
        let mut unscaled = false;
        // The local engine plays its own monitoring: no WebRTC sound here.
        let mut plan = match crate::engine::is_local() {
            true => WebRtcPlan::Never,
            false => WebRtcPlan::Use,
        };
        let mut sound_was = self.sound.load(std::sync::atomic::Ordering::SeqCst);
        loop {
            let want = self.want.lock().unwrap().clone();
            if want.stopped {
                return;
            }
            let Some(target) = want.target.clone() else {
                thread::sleep(Duration::from_millis(50));
                continue;
            };
            let decoder = match &mut decoder {
                Some(decoder) => decoder,
                None => match crate::video::decoder() {
                    Ok(made) => decoder.insert(made),
                    Err(error) => {
                        isocast_diagnostics::diagnostic!("warn"; "video: no H.264 decoder, stills instead: {error:#}");
                        return self.receive_stills(deliver);
                    }
                },
            };
            let width = (!unscaled).then(|| crate::video::width_step(want.width));
            // The program over WebRTC (picture and sound in sync) while the
            // engine has it and UDP gets through; else the `frames` stream.
            let sound = self.sound.load(std::sync::atomic::Ordering::SeqCst);
            let retry = self.retry.swap(false, std::sync::atomic::Ordering::SeqCst);
            let turned_on = (sound && !sound_was) || retry;
            sound_was = sound;
            let program = target == isocast_protocol::PROGRAM;
            let webrtc = program && plan.use_now(Instant::now(), turned_on);
            let fallback = match (program, plan) {
                (true, WebRtcPlan::FramesUntil(until)) => Some(until),
                _ => None,
            };
            if webrtc {
                deliver(Incoming::SoundUnavailable(None));
            }
            let mut pictured = false;
            let result = crate::daemon::connect().and_then(|client| {
                let mut source: Box<dyn crate::video::Source> = match webrtc {
                    true => Box::new(crate::video::WhepSource::open(client, self.sound.clone())?),
                    false => Box::new(crate::video::FramesSource::open(
                        client,
                        FramesParams {
                            target: target.clone(),
                            max_width: width,
                            fps: Some(want.fps.clamp(1, 60)),
                            profile: decoder.frames_profile(),
                        },
                    )?),
                };
                self.play(
                    &mut *source,
                    &mut **decoder,
                    &target,
                    width.filter(|_| !webrtc),
                    &deliver,
                    &mut delay,
                    &mut pictured,
                    fallback,
                )
            });
            if let Err(error) = &result
                && webrtc
                && (!pictured || unknown_method(error))
            {
                // An older engine for good; anything else (UDP blocked, a
                // canvas change, shedding, a full viewer cap) for a while.
                plan = WebRtcPlan::after_failure(unknown_method(error), pictured, Instant::now());
                isocast_diagnostics::diagnostic!("info"; "video: WebRTC unavailable ({plan:?}), using the frames stream: {error:#}");
                deliver(Incoming::SoundUnavailable(Some(match plan {
                    WebRtcPlan::Never => (
                        "No sound from this engine: it's an older version without WebRTC".into(),
                        true,
                    ),
                    _ => (
                        "No sound right now: the engine's WebRTC isn't available (click to try again)"
                            .into(),
                        false,
                    ),
                })));
                continue;
            }
            match result {
                // The target or size changed: the new one at once.
                Ok(()) => continue,
                Err(error) if !unscaled && format!("{error:#}").contains("video.size") => {
                    isocast_diagnostics::diagnostic!("info"; "video: the engine can't scale its pictures; asking for them full size ({error:#})");
                    unscaled = true;
                    continue;
                }
                // This machine can't decode the stream at all, or keeps
                // failing on it: the engine's stills instead.
                Err(error) if crate::video::no_decoder(&error) || error.is::<Undecodable>() => {
                    isocast_diagnostics::diagnostic!("warn"; "video: can't decode the engine's video, stills instead: {error:#}");
                    return self.receive_stills(deliver);
                }
                Err(error) => deliver(Incoming::Lost(format!("{error:#}"))),
            }
            thread::sleep(delay);
            delay = (delay * 2).min(Duration::from_secs(5));
        }
    }

    /// Show `source`'s pictures until the target or size step changes
    /// (`Ok`) or the source fails. Only the transport differs between
    /// sources. A picture that won't decode is skipped (the decoders start
    /// again at the next keyframe); [`KEYFRAME_FAILURES`] keyframes in a
    /// row that won't end it with [`Undecodable`].
    #[expect(
        clippy::too_many_arguments,
        reason = "the feed's state threaded through one loop"
    )]
    fn play(
        &self,
        source: &mut dyn crate::video::Source,
        decoder: &mut dyn crate::video::Decoder,
        target: &str,
        width: Option<u32>,
        deliver: &impl Fn(Incoming),
        delay: &mut Duration,
        pictured: &mut bool,
        fallback: Option<Instant>,
    ) -> anyhow::Result<()> {
        use crate::video::Intake;
        let mut sound_was = self.sound.load(std::sync::atomic::Ordering::SeqCst);
        let mut sets = None;
        let mut fps = self.want.lock().unwrap().fps;
        // Setup revisions by the picture time they start at.
        let mut revisions: std::collections::VecDeque<(u64, u64)> = Default::default();
        let mut stats = crate::video::Stats::default();
        let mut said = false;
        let mut failed_keyframes = 0;
        loop {
            let now = self.want.lock().unwrap().clone();
            if now.stopped
                || now.target.as_deref() != Some(target)
                || width.is_some_and(|width| crate::video::width_step(now.width) != width)
            {
                return Ok(());
            }
            // On `frames` after WebRTC failed: offer again when the wait
            // is over or the speaker is turned on.
            if let Some(until) = fallback {
                let sound = self.sound.load(std::sync::atomic::Ordering::SeqCst);
                let turned_on = sound && !sound_was;
                sound_was = sound;
                if Instant::now() >= until || turned_on {
                    return Ok(());
                }
                if self.retry.load(std::sync::atomic::Ordering::SeqCst) {
                    // Left set for the loop above to take.
                    return Ok(());
                }
            }
            if now.fps != fps {
                fps = now.fps;
                source.set_fps(fps)?;
            }
            match source.next(Duration::from_millis(250))? {
                None => {}
                Some(Intake::Config(new)) => sets = Some(new),
                Some(Intake::Revision { time, revision }) => {
                    revisions.push_back((time, revision));
                    while revisions.len() > 8 {
                        revisions.pop_front();
                    }
                }
                Some(Intake::Unit(unit)) => {
                    let Some(sets) = &sets else { continue };
                    let started = Instant::now();
                    let surfaces = self.surfaces.load(std::sync::atomic::Ordering::SeqCst);
                    let decoded = decoder.decode(sets, &unit, surfaces);
                    let skipped = decoder.take_skipped();
                    if skipped > 0 {
                        stats.skip(skipped);
                    }
                    let decoded = match decoded {
                        Ok(Some(decoded)) => decoded,
                        Ok(None) => continue,
                        Err(error) if crate::video::no_decoder(&error) => return Err(error),
                        Err(error) => {
                            isocast_diagnostics::diagnostic!("debug"; "video: skipped a picture of {target}: {error:#}");
                            if unit.keyframe {
                                failed_keyframes += 1;
                                if failed_keyframes >= KEYFRAME_FAILURES {
                                    return Err(Undecodable(format!("{error:#}")).into());
                                }
                            }
                            continue;
                        }
                    };
                    if unit.keyframe {
                        failed_keyframes = 0;
                    }
                    let late = source.clock_offset().map(|offset| {
                        let here = crate::video::now_nanos(Instant::now()) as i128 + offset;
                        (here - unit.time as i128) as f64 / 1e6
                    });
                    if !said {
                        said = true;
                        let (width, height) = decoded.size();
                        let on = match decoded {
                            crate::video::Decoded::Frame(_) => "shown from the GPU",
                            crate::video::Decoded::Pixels(_) => "shown as pixels",
                        };
                        isocast_diagnostics::diagnostic!("info"; "video: {target} {width}x{height} (sent as {}x{}) decoded by {}, {on}", unit.width, unit.height, decoder.name());
                    }
                    stats.add(late, started.elapsed().as_secs_f64() * 1e3);
                    let revision = revisions
                        .iter()
                        .rev()
                        .find(|(at, _)| *at <= unit.time)
                        .map(|(_, revision)| *revision);
                    *delay = Duration::from_millis(250);
                    *pictured = true;
                    deliver(match decoded {
                        crate::video::Decoded::Frame(mut frame) => {
                            frame.revision = revision;
                            Incoming::Frame(frame)
                        }
                        crate::video::Decoded::Pixels(pixels) => {
                            Incoming::Still(Still { pixels, revision })
                        }
                    });
                }
            }
        }
    }

    /// A remote engine's pictures: its JPEG stills, at most [`STILL_FPS`]
    /// a second, on one connection, reconnecting with back-off.
    fn receive_stills(&self, deliver: impl Fn(Incoming)) {
        let mut delay = Duration::from_millis(250);
        loop {
            let want = self.want.lock().unwrap().clone();
            if want.stopped {
                return;
            }
            let Some(target) = want.target.clone() else {
                thread::sleep(Duration::from_millis(50));
                continue;
            };
            let result = crate::daemon::connect().and_then(|mut client| {
                let mut after = None;
                loop {
                    let now = self.want.lock().unwrap().clone();
                    if now.stopped || now.target.as_deref() != Some(target.as_str()) {
                        return Ok(());
                    }
                    let fps = now.fps.clamp(1, STILL_FPS);
                    let started = Instant::now();
                    if let Some((seq, picture)) =
                        still(&mut client, &target, now.width, after, now.fps)?
                    {
                        after = Some(seq);
                        delay = Duration::from_millis(250);
                        deliver(Incoming::Still(picture));
                    }
                    let period = Duration::from_secs(1) / fps;
                    thread::sleep(period.saturating_sub(started.elapsed()));
                }
            });
            match result {
                Ok(()) => continue,
                Err(error) => deliver(Incoming::Lost(format!("{error:#}"))),
            }
            thread::sleep(delay);
            delay = (delay * 2).min(Duration::from_secs(5));
        }
    }
}
