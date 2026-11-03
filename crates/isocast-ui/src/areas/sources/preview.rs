//! A source's live picture in Properties and Filters (the web app's
//! SourcePreview): the daemon draws that source into a small ring of
//! shared GPU surfaces (`shared_frames` with the source as its target), a
//! thread of its own receives them, and the window shows the newest as a
//! Slint image on the same GPU device, without copying it. On Linux the
//! local monitor feed copies pixels from the memfd ring instead, or
//! decodes the engine's video if the ring is unavailable. It stops when
//! the dialog closes. A window on another computer's engine shows its
//! JPEG stills instead, a few a second (R2's interim previews).
//!
//! It also says how the picture is coming ([`PreviewStatus`]), so the
//! dialog can say why there is none ([`note`]) or that it lacks the
//! source's effects ([`caption`]) instead of showing black or a frozen
//! picture.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use isocast_client::shared::SharedFrame;
use isocast_protocol::api::SharedFramesParams;
use slint::wgpu_30::wgpu;

use crate::frames::{Feed, Incoming};

type Slot = Arc<Mutex<Option<SharedFrame>>>;
type StillSlot = Arc<Mutex<Option<slint::SharedPixelBuffer<slint::Rgba8Pixel>>>>;

/// How long a preview may go without its first picture before it counts
/// as having none ([`PreviewState::NoPicture`]).
const FIRST_PICTURE_WAIT: Duration = Duration::from_secs(3);

/// How the source's picture is coming.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PreviewState {
    /// Asked for; no picture yet.
    #[default]
    Starting,
    /// Still no picture, [`FIRST_PICTURE_WAIT`] after it was asked for.
    NoPicture,
    /// Pictures are coming.
    Showing,
    /// The connection to the engine dropped; it reconnects itself.
    Lost,
    /// This engine can't show its pictures in this window (asking again
    /// won't help).
    Unavailable,
}

/// What a source preview says about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PreviewStatus {
    pub state: PreviewState,
    /// The picture is the engine's video of the source, which has no
    /// filters (Linux, when its shared memory can't be had).
    pub without_effects: bool,
}

/// What the dialog says where the picture would be, if anything. `paused`
/// is why automatic quality paused previews (its `previews_paused_why`);
/// `silent`, that the engine says the running source delivers no frames.
/// `machine` names this kind of computer ("Mac").
pub fn note(
    status: PreviewStatus,
    paused: Option<&str>,
    silent: bool,
    machine: &str,
) -> Option<String> {
    if let Some(why) = paused {
        return Some(crate::areas::canvas::previews_paused_words(why, machine));
    }
    match status.state {
        PreviewState::Unavailable => Some(
            "Preview not available: this engine can’t show the source’s picture here. \
             Recording and streaming aren’t affected."
                .into(),
        ),
        PreviewState::Lost => Some("No picture: the engine isn’t answering. Trying again…".into()),
        PreviewState::NoPicture if silent => {
            Some("No picture: the source isn’t sending any.".into())
        }
        PreviewState::Starting | PreviewState::NoPicture | PreviewState::Showing => None,
    }
}

/// What the dialog says over the picture, if anything: that it lacks the
/// source's effects.
pub fn caption(status: PreviewStatus) -> Option<&'static str> {
    (status.without_effects && status.state == PreviewState::Showing)
        .then_some("Shown without effects: the fast preview isn’t available here.")
}

struct Watching {
    source: String,
    stop: Arc<AtomicBool>,
    feed: Option<Feed>,
    newest: Slot,
    still: StillSlot,
    images: HashMap<(u64, u32), slint::Image>,
    /// The frames shown now and just before (the GPU may still read it).
    showing: Option<SharedFrame>,
    previous: Option<SharedFrame>,
    /// Lost or Unavailable, from the receiving thread; Showing once a
    /// picture is shown.
    state: Arc<Mutex<PreviewState>>,
    started: Instant,
    /// What was last said through `changed`.
    said: Option<PreviewStatus>,
}

impl Watching {
    fn status(&self) -> PreviewStatus {
        PreviewStatus {
            state: *self.state.lock().unwrap(),
            without_effects: self.feed.as_ref().is_some_and(Feed::fell_back),
        }
    }
}

#[derive(Default)]
pub struct SourcePreview {
    device: RefCell<Option<wgpu::Device>>,
    watching: Rc<RefCell<Option<Watching>>>,
    timer: slint::Timer,
}

impl SourcePreview {
    /// The GPU device the window draws with (the pictures are its
    /// textures).
    pub fn set_device(&self, device: wgpu::Device) {
        *self.device.borrow_mut() = Some(device);
    }

    /// Show `source`'s picture through `show` (each new frame's image);
    /// `changed` runs when its [`PreviewStatus`] changes.
    pub fn start(
        &self,
        source: &str,
        show: impl Fn(slint::Image, f32) + 'static,
        changed: impl Fn(PreviewStatus) + 'static,
    ) {
        if self
            .watching
            .borrow()
            .as_ref()
            .is_some_and(|w| w.source == source)
        {
            return;
        }
        self.stop();
        let Some(device) = self.device.borrow().clone() else {
            return;
        };
        let stop = Arc::new(AtomicBool::new(false));
        let newest: Slot = Arc::default();
        let still: StillSlot = Arc::default();
        let state: Arc<Mutex<PreviewState>> = Arc::default();
        let feed = if cfg!(target_os = "linux") && crate::engine::is_local() {
            // Both sheets have about 600 px for the picture: the same
            // width ladder, slot release and H.264 fallback as a monitor.
            let (still, state) = (still.clone(), state.clone());
            Some(Feed::start_with_filters(
                Some(source),
                (640, 360),
                30,
                true,
                move |incoming| match incoming {
                    Incoming::Still(frame) => {
                        *still.lock().unwrap() = Some(frame.pixels);
                    }
                    Incoming::Lost(why) => {
                        isocast_diagnostics::diagnostic!("warn"; "source preview: {why}");
                        *state.lock().unwrap() = PreviewState::Lost;
                    }
                    Incoming::Unavailable(why) => {
                        isocast_diagnostics::diagnostic!("warn"; "source preview: {why}");
                        *state.lock().unwrap() = PreviewState::Unavailable;
                    }
                    _ => {}
                },
            ))
        } else {
            let (stop, newest, target) = (stop.clone(), newest.clone(), source.to_string());
            let (still, state) = (still.clone(), state.clone());
            let _ = thread::Builder::new()
                .name("source preview".into())
                .spawn(move || {
                    if crate::engine::is_local() {
                        receive(&target, &stop, &newest, &state)
                    } else {
                        receive_stills(&target, &stop, &still, &state)
                    }
                });
            None
        };
        *self.watching.borrow_mut() = Some(Watching {
            source: source.to_string(),
            stop,
            feed,
            newest,
            still,
            images: HashMap::new(),
            showing: None,
            previous: None,
            state,
            started: Instant::now(),
            said: None,
        });
        let watching = self.watching.clone();
        self.timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(16),
            move || tick(&watching, &device, &show, &changed),
        );
    }

    /// Stop showing a picture.
    pub fn stop(&self) {
        self.timer.stop();
        if let Some(w) = self.watching.borrow_mut().take() {
            if let Some(feed) = w.feed {
                feed.stop();
            }
            w.stop.store(true, Ordering::Relaxed);
            #[cfg(windows)]
            if let Some(frame) = w.showing.as_ref().or(w.previous.as_ref()) {
                frame.close_stream();
            }
        }
    }

    pub fn source(&self) -> Option<String> {
        self.watching.borrow().as_ref().map(|w| w.source.clone())
    }

    /// How the picture is coming, while one is asked for.
    pub fn status(&self) -> Option<PreviewStatus> {
        self.watching.borrow().as_ref().map(Watching::status)
    }
}

/// Show the newest frame, if a new one came.
fn tick(
    watching: &RefCell<Option<Watching>>,
    device: &wgpu::Device,
    show: &dyn Fn(slint::Image, f32),
    changed: &dyn Fn(PreviewStatus),
) {
    let shown = show_newest(watching, device, show);
    // Said after the borrow ends: `changed` reads the status again.
    let status = {
        let mut watching = watching.borrow_mut();
        let Some(w) = watching.as_mut() else { return };
        {
            let mut state = w.state.lock().unwrap();
            if shown {
                *state = PreviewState::Showing;
            } else if *state == PreviewState::Starting && w.started.elapsed() >= FIRST_PICTURE_WAIT
            {
                *state = PreviewState::NoPicture;
            }
        }
        let status = w.status();
        if w.said == Some(status) {
            return;
        }
        w.said = Some(status);
        status
    };
    changed(status);
}

/// Show the newest frame, if a new one came: whether one was shown.
fn show_newest(
    watching: &RefCell<Option<Watching>>,
    device: &wgpu::Device,
    show: &dyn Fn(slint::Image, f32),
) -> bool {
    {
        let mut watching = watching.borrow_mut();
        let Some(w) = watching.as_mut() else {
            return false;
        };
        if let Some(pixels) = w.still.lock().ok().and_then(|mut s| s.take()) {
            let aspect = pixels.width() as f32 / pixels.height().max(1) as f32;
            show(slint::Image::from_rgba8(pixels), aspect);
            return true;
        }
        let Some(frame) = w.newest.lock().ok().and_then(|mut f| f.take()) else {
            return false;
        };
        let key = frame.cache_key();
        if !w.images.contains_key(&key) {
            // A new ring: the old one's images go.
            w.images.retain(|(generation, _), _| *generation == key.0);
            match frame
                .texture(device)
                .and_then(|t| slint::Image::try_from(t).map_err(Into::into))
            {
                Ok(image) => {
                    w.images.insert(key, image);
                }
                Err(error) => {
                    isocast_diagnostics::diagnostic!("warn";
                        "source preview: can't show a shared frame: {error:#}"
                    );
                    return false;
                }
            }
        }
        let aspect = frame.width() as f32 / frame.height().max(1) as f32;
        show(w.images[&key].clone(), aspect);
        w.previous = w.showing.replace(frame);
    }
    true
}

/// The receiving thread: the newest frame into `newest`, until stopped.
fn receive(target: &str, stop: &AtomicBool, newest: &Slot, state: &Mutex<PreviewState>) {
    let mut delay = Duration::from_millis(250);
    while !stop.load(Ordering::Relaxed) {
        let result = crate::daemon::connect().and_then(|client| {
            let mut frames = client.shared_frames(SharedFramesParams {
                target: target.into(),
                max_width: Some(1280),
                max_height: Some(720),
                fps: Some(30),
                slots: None,
                // As the program shows it: Filters' preview shows each
                // change, Properties' the source as it will look.
                filtered: true,
            })?;
            while !stop.load(Ordering::Relaxed) {
                if let Some(frame) = frames.newest(Some(Duration::from_millis(500)))?
                    && let Ok(mut slot) = newest.lock()
                {
                    *slot = Some(frame);
                }
            }
            Ok(())
        });
        // Ended, or this engine can't share pictures here (the preview
        // says so rather than asking every few seconds).
        match result {
            Err(error) if crate::frames::unsupported(&error) => {
                *state.lock().unwrap() = PreviewState::Unavailable;
                break;
            }
            Err(_) => *state.lock().unwrap() = PreviewState::Lost,
            Ok(()) => break,
        }
        thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_secs(5));
    }
}

/// A remote engine's source: its stills into `newest`, until stopped.
fn receive_stills(
    target: &str,
    stop: &AtomicBool,
    newest: &StillSlot,
    state: &Mutex<PreviewState>,
) {
    let mut delay = Duration::from_millis(250);
    while !stop.load(Ordering::Relaxed) {
        let result = crate::daemon::connect().and_then(|mut client| {
            let mut after = None;
            while !stop.load(Ordering::Relaxed) {
                let started = std::time::Instant::now();
                let fps = crate::frames::STILL_FPS;
                if let Some((seq, still)) =
                    crate::frames::still(&mut client, target, 1280, after, 30)?
                {
                    after = Some(seq);
                    if let Ok(mut slot) = newest.lock() {
                        *slot = Some(still.pixels);
                    }
                }
                thread::sleep((Duration::from_secs(1) / fps).saturating_sub(started.elapsed()));
            }
            Ok(())
        });
        if result.is_ok() {
            break;
        }
        *state.lock().unwrap() = PreviewState::Lost;
        thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_secs(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(state: PreviewState, without_effects: bool) -> PreviewStatus {
        PreviewStatus {
            state,
            without_effects,
        }
    }

    #[test]
    fn a_paused_preview_says_why_before_anything_else() {
        for state in [
            PreviewState::Starting,
            PreviewState::Showing,
            PreviewState::Lost,
            PreviewState::Unavailable,
        ] {
            let said = note(
                status(state, true),
                Some("to protect recording"),
                true,
                "PC",
            );
            assert_eq!(
                said.as_deref(),
                Some(
                    "Automatic quality paused previews to protect recording on this PC’s \
                     software graphics."
                ),
                "{state:?}"
            );
            assert_eq!(
                caption(status(state, true)).is_some(),
                state == PreviewState::Showing
            );
        }
        let said = note(
            status(PreviewState::Showing, false),
            Some("until the program keeps up"),
            false,
            "Mac",
        );
        assert!(said.unwrap().contains("until the program keeps up"));
    }

    #[test]
    fn a_preview_without_a_picture_says_why() {
        let said = |state, silent| note(status(state, false), None, silent, "Mac");
        assert!(
            said(PreviewState::Unavailable, false)
                .unwrap()
                .starts_with("Preview not available")
        );
        assert!(
            said(PreviewState::Lost, false)
                .unwrap()
                .contains("Trying again")
        );
        assert_eq!(
            said(PreviewState::NoPicture, true).as_deref(),
            Some("No picture: the source isn’t sending any.")
        );
        // A still picture sends nothing new but was shown; a source that
        // sends frames has no note while its first picture is on its way.
        assert_eq!(said(PreviewState::Showing, true), None);
        assert_eq!(said(PreviewState::Starting, true), None);
        assert_eq!(said(PreviewState::NoPicture, false), None);
    }

    #[test]
    fn only_a_shown_picture_without_effects_is_captioned() {
        assert_eq!(caption(status(PreviewState::Showing, false)), None);
        assert!(
            caption(status(PreviewState::Showing, true))
                .unwrap()
                .contains("without effects")
        );
        for state in [
            PreviewState::Starting,
            PreviewState::NoPicture,
            PreviewState::Lost,
            PreviewState::Unavailable,
        ] {
            assert_eq!(caption(status(state, true)), None, "{state:?}");
        }
    }
}
