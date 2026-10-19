//! A source's live picture in Properties and Filters (the web app's
//! SourcePreview): the daemon draws that source into a small ring of
//! shared GPU surfaces (`shared_frames` with the source as its target), a
//! thread of its own receives them, and the window shows the newest as a
//! Slint image on the same GPU device, without copying it. It stops when
//! the dialog closes.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use isocast_client::shared::SharedFrame;
use isocast_protocol::api::SharedFramesParams;
use slint::wgpu_30::wgpu;

type Slot = Arc<Mutex<Option<SharedFrame>>>;

struct Watching {
    source: String,
    stop: Arc<AtomicBool>,
    newest: Slot,
    images: HashMap<(u64, u32), slint::Image>,
    /// The frames shown now and just before (the GPU may still read it).
    showing: Option<SharedFrame>,
    previous: Option<SharedFrame>,
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

    /// Show `source`'s picture through `show` (each new frame's image).
    pub fn start(&self, source: &str, show: impl Fn(slint::Image, f32) + 'static) {
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
        {
            let (stop, newest, target) = (stop.clone(), newest.clone(), source.to_string());
            let _ = thread::Builder::new()
                .name("source preview".into())
                .spawn(move || receive(&target, &stop, &newest));
        }
        *self.watching.borrow_mut() = Some(Watching {
            source: source.to_string(),
            stop,
            newest,
            images: HashMap::new(),
            showing: None,
            previous: None,
        });
        let watching = self.watching.clone();
        self.timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(16),
            move || tick(&watching, &device, &show),
        );
    }

    /// Stop showing a picture.
    pub fn stop(&self) {
        self.timer.stop();
        if let Some(w) = self.watching.borrow_mut().take() {
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
}

/// Show the newest frame, if a new one came.
fn tick(
    watching: &RefCell<Option<Watching>>,
    device: &wgpu::Device,
    show: &dyn Fn(slint::Image, f32),
) {
    {
        let mut watching = watching.borrow_mut();
        let Some(w) = watching.as_mut() else { return };
        let Some(frame) = w.newest.lock().ok().and_then(|mut f| f.take()) else {
            return;
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
                    return;
                }
            }
        }
        let aspect = frame.width() as f32 / frame.height().max(1) as f32;
        show(w.images[&key].clone(), aspect);
        w.previous = w.showing.replace(frame);
    }
}

/// The receiving thread: the newest frame into `newest`, until stopped.
fn receive(target: &str, stop: &AtomicBool, newest: &Slot) {
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
        // stays empty rather than asking every few seconds).
        match result {
            Err(error) if !crate::frames::unsupported(&error) => {}
            _ => break,
        }
        thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_secs(5));
    }
}
