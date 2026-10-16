//! A monitor's picture without copying it (`shared_frames`): a thread of
//! its own receives each frame, a GPU surface the daemon has finished
//! drawing, and hands it to the window's thread. Reconnects with back-off
//! when the daemon goes away, and at once when the target changes (the
//! edited monitor shows `preview` in Studio Mode, `program` otherwise).

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use isocast_client::shared::{SharedController, SharedFrame};
use isocast_protocol::api::SharedFramesParams;

/// What the window gets.
pub enum Incoming {
    Frame(SharedFrame),
    /// No picture: why (the daemon can't be reached, or refused).
    Lost(String),
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
            let _ = control.set_size(Some(width), Some(height));
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
            let stream = crate::daemon::connect().and_then(|client| {
                client.shared_frames(SharedFramesParams {
                    target: target.clone(),
                    max_width: Some(want.width.max(16)),
                    max_height: Some(want.height.max(16)),
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
                    control.set_size(Some(now.width), Some(now.height))?;
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
                        deliver(Incoming::Frame(frame));
                    }
                }
            });
            *self.control.lock().unwrap() = None;
            match result {
                // The target changed: the new one at once.
                Ok(()) => continue,
                Err(error) => deliver(Incoming::Lost(format!("{error:#}"))),
            }
            thread::sleep(delay);
            delay = (delay * 2).min(Duration::from_secs(5));
        }
    }
}
