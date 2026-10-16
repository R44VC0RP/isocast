//! Monitors' video for native clients on the same machine
//! (`shared_frames`): a target's picture (the program, studio mode's
//! preview or a source) drawn by the GPU, scaled to the size the viewer
//! asks for, into a small ring of surfaces the platform shares with other
//! processes (8-bit RGBA, on macOS IOSurfaces). The viewer's GPU shows a
//! slot in place and hands it back when done; nothing is copied or
//! encoded.
//!
//! Each viewer has its own ring and thread, so a viewer that stops handing
//! slots back only stalls itself. Frames are picked at the viewer's rate
//! like the H.264 monitors' ([`Pace`]), and asking for them keeps the
//! program (or preview) drawn at that rate. A source can be shown through
//! its filters (Filters' preview in the app), read live so each change
//! shows at once.

use parking_lot::{Condvar, Mutex};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use isocast_protocol::Filter;
use isocast_protocol::ScaleFilter;

use super::frame_stream::{MAX_FPS, Pace, Source, stamp_at};
use crate::filter::Filters;
use crate::gpu::{FrameImporter, Gpu};
use crate::render::{Layer, Renderer};
use crate::{Platform, Time};

/// How long to wait for a slot to come back before looking again.
const WAIT: Duration = Duration::from_millis(50);
/// How long the GPU may take to draw one frame.
const DRAW_TIMEOUT: Duration = Duration::from_secs(1);
/// Surfaces in a ring.
pub const DEFAULT_SLOTS: u32 = 4;
pub const MAX_SLOTS: u32 = 8;

/// What a viewer's stream sends.
pub enum SharedEvent<S> {
    /// A new ring of surfaces, replacing any before: at the start and when
    /// the picture's size changes.
    Ring {
        generation: u64,
        width: u32,
        height: u32,
        surfaces: Vec<S>,
    },
    /// A frame the GPU has finished drawing into `slot` of ring
    /// `generation`, which stays the viewer's until it releases it.
    Frame {
        generation: u64,
        slot: u32,
        time: Time,
        /// For the program and the preview, the setup revision it shows.
        revision: Option<u64>,
        /// And the layout generation it was drawn at.
        layout_generation: Option<u64>,
    },
    /// The stream stopped, and why.
    End(String),
}

/// One viewer's stream: its events until dropped, which stops it.
pub struct SharedFeed<S> {
    pub events: Receiver<SharedEvent<S>>,
    control: SharedControl,
}

impl<S> SharedFeed<S> {
    /// A handle to release slots and change the rate or size, from any
    /// thread.
    pub fn control(&self) -> SharedControl {
        self.control.clone()
    }
}

impl<S> Drop for SharedFeed<S> {
    fn drop(&mut self) {
        self.control.0.closed.store(true, Ordering::SeqCst);
        self.control.0.changed.notify_all();
    }
}

/// Releases a viewer's slots and changes its rate or size (see
/// [`SharedFeed::control`]).
#[derive(Clone)]
pub struct SharedControl(Arc<Control>);

struct Control {
    fps: AtomicU32,
    closed: AtomicBool,
    pending: Mutex<Pending>,
    /// Signalled when a slot comes back or the feed closes.
    changed: Condvar,
}

#[derive(Default)]
struct Pending {
    released: Vec<(u64, u32)>,
    size: Option<(Option<u32>, Option<u32>)>,
}

impl SharedControl {
    /// The viewer is done with `slot` of ring `generation`.
    pub fn release(&self, generation: u64, slot: u32) {
        self.0.pending.lock().released.push((generation, slot));
        self.0.changed.notify_all();
    }

    /// At most `fps` frames a second from the next frame.
    pub fn set_fps(&self, fps: u32) {
        self.0.fps.store(fps.clamp(1, MAX_FPS), Ordering::Relaxed);
    }

    /// Fit the picture in a new size from the next frame (a new ring if
    /// its size changes).
    pub fn set_size(&self, max_width: Option<u32>, max_height: Option<u32>) {
        self.0.pending.lock().size = Some((max_width, max_height));
        self.0.changed.notify_all();
    }

    /// Slots released and a new size, since last asked.
    fn take(&self) -> Pending {
        std::mem::take(&mut self.0.pending.lock())
    }

    /// Wait up to `timeout` for a slot to come back.
    fn wait(&self, timeout: Duration) {
        let mut pending = self.0.pending.lock();
        let _ = self.0.changed.wait_while_for(
            &mut pending,
            |pending| {
                pending.released.is_empty()
                    && pending.size.is_none()
                    && !self.0.closed.load(Ordering::SeqCst)
            },
            timeout,
        );
    }
}

/// What a stream is asked to show.
pub(crate) struct Request {
    pub(crate) target: String,
    pub(crate) max_size: (Option<u32>, Option<u32>),
    pub(crate) fps: u32,
    pub(crate) slots: u32,
}

/// A source's filters as they are now (the live setup's).
pub(crate) type LiveFilters = Box<dyn Fn() -> Vec<Filter> + Send>;

/// Start a viewer's stream of `request`. `source` finds where the target's
/// frames arrive (looked up again all the time, since a source can reopen,
/// and given the stream's rate then), or says why it can't; `filters`, if
/// given, are drawn over each frame.
pub(crate) fn watch<P: Platform>(
    platform: &P,
    request: Request,
    source: impl Fn(u32) -> Result<Source<P::Frame>, String> + Send + 'static,
    filters: Option<LiveFilters>,
) -> SharedFeed<P::SharedSurface> {
    let (tx, events) = channel();
    let control = SharedControl(Arc::new(Control {
        fps: AtomicU32::new(request.fps.clamp(1, MAX_FPS)),
        closed: AtomicBool::new(false),
        pending: Mutex::new(Pending::default()),
        changed: Condvar::new(),
    }));
    let platform = platform.clone();
    let running = control.clone();
    let failed_tx = tx.clone();
    let failed_control = control.clone();
    isocast_diagnostics::spawn(
        format!("shared frames {}", request.target),
        move || {
            if let Err(why) = run(&platform, &running, &tx, &request, source, filters) {
                let _ = tx.send(SharedEvent::End(why));
            }
        },
        move |why| {
            failed_control.0.closed.store(true, Ordering::SeqCst);
            failed_control.0.changed.notify_all();
            let _ = failed_tx.send(SharedEvent::End(why));
        },
    )
    .expect("could not start a shared frame thread");
    SharedFeed { events, control }
}

/// The ring a viewer's frames are drawn into.
struct Ring<S> {
    generation: u64,
    size: (u32, u32),
    surfaces: Vec<(S, wgpu::Texture)>,
    /// Slots the viewer holds.
    held: Vec<bool>,
    /// No frame was drawn into it yet.
    fresh: bool,
}

/// Draw frames for one viewer until it leaves (`Ok`) or the stream can't
/// go on (the reason).
fn run<P: Platform>(
    platform: &P,
    control: &SharedControl,
    tx: &Sender<SharedEvent<P::SharedSurface>>,
    request: &Request,
    source: impl Fn(u32) -> Result<Source<P::Frame>, String>,
    filters: Option<LiveFilters>,
) -> Result<(), String> {
    let failed = |err: anyhow::Error| format!("{err:#}");
    let gpu = Gpu::new().map_err(failed)?;
    let renderer = Renderer::new(&gpu, wgpu::TextureFormat::Rgba8Unorm);
    let mut importer = platform.frame_importer(&gpu).map_err(failed)?;
    let slots = request.slots.clamp(2, MAX_SLOTS) as usize;
    let mut max_size = request.max_size;
    let mut ring: Option<Ring<P::SharedSurface>> = None;
    let mut generation = 0;
    let mut pace = Pace::default();
    // The filters drawn last, and what draws them (made on first use).
    let mut drawn_filters: Vec<Filter> = Vec::new();
    let mut filtering: Option<Filters> = None;
    loop {
        if control.0.closed.load(Ordering::SeqCst) {
            return Ok(());
        }
        let pending = control.take();
        if let Some(ring) = &mut ring {
            for (released, slot) in pending.released {
                if released == ring.generation
                    && let Some(held) = ring.held.get_mut(slot as usize)
                {
                    *held = false;
                }
            }
        }
        if let Some(size) = pending.size
            && size != max_size
        {
            max_size = size;
            // Drawn again at the new size, even if nothing changes.
            if let Some(ring) = &mut ring {
                ring.fresh = true;
            }
        }
        // The viewer holds every slot: nothing to draw into. The target
        // isn't asked for meanwhile, so an idle program stops drawing.
        if ring
            .as_ref()
            .is_some_and(|ring| ring.held.iter().all(|held| *held))
        {
            control.wait(WAIT);
            continue;
        }
        // Changed filters show at once, even on a still picture.
        if let Some(filters) = &filters {
            let now = filters();
            if now != drawn_filters {
                drawn_filters = now;
                if let Some(ring) = &mut ring {
                    ring.fresh = true;
                }
            }
        }
        let fps = control.0.fps.load(Ordering::Relaxed);
        let Source { slot, grid, drawn } = source(fps)?;
        let again = || ring.as_ref().is_none_or(|ring| ring.fresh);
        let Some((frame, at)) = pace.next(platform, &slot, grid, fps, again) else {
            continue;
        };
        let mut picture = importer.import(&frame).map_err(failed)?;
        if drawn_filters.iter().any(|filter| filter.enabled) {
            let filtering =
                filtering.get_or_insert_with(|| Filters::new(&gpu, renderer.format(), true));
            picture = filtering.apply(
                &gpu,
                &renderer,
                &request.target,
                picture,
                &drawn_filters,
                at,
            );
        }
        let input = (picture.width, picture.height);
        let size = fitted(input, max_size);
        if ring.as_ref().is_none_or(|ring| ring.size != size) {
            generation += 1;
            let mut surfaces = Vec::with_capacity(slots);
            for _ in 0..slots {
                surfaces.push(
                    platform
                        .shared_surface(&gpu, size.0, size.1)
                        .map_err(failed)?,
                );
            }
            let event = SharedEvent::Ring {
                generation,
                width: size.0,
                height: size.1,
                surfaces: surfaces
                    .iter()
                    .map(|(surface, _)| surface.clone())
                    .collect(),
            };
            if tx.send(event).is_err() {
                return Ok(());
            }
            ring = Some(Ring {
                generation,
                size,
                surfaces,
                held: vec![false; slots],
                fresh: true,
            });
        }
        let ring = ring.as_mut().expect("made above");
        let index = ring
            .held
            .iter()
            .position(|held| !held)
            .expect("a slot is free");
        let mut layer = Layer::full(&picture, size.0, size.1);
        layer.scale_filter = if size.0 < input.0 {
            ScaleFilter::Area
        } else {
            ScaleFilter::Bicubic
        };
        let target = &ring.surfaces[index].1;
        let submission = renderer.render(&gpu, target, &[layer], size);
        // The viewer's GPU must read a finished picture.
        gpu.device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: Some(DRAW_TIMEOUT),
            })
            .map_err(|err| format!("the GPU did not finish drawing a frame: {err}"))?;
        drop(picture);
        ring.held[index] = true;
        ring.fresh = false;
        let stamp = drawn.and_then(|drawn| stamp_at(&drawn, at));
        let event = SharedEvent::Frame {
            generation: ring.generation,
            slot: index as u32,
            time: at,
            revision: stamp.map(|stamp| stamp.revision),
            layout_generation: stamp.map(|stamp| stamp.generation),
        };
        if tx.send(event).is_err() {
            return Ok(());
        }
    }
}

/// `input` scaled down to fit in `max_size` (either side unbounded if
/// absent), keeping its shape; never larger than it is.
fn fitted((width, height): (u32, u32), max_size: (Option<u32>, Option<u32>)) -> (u32, u32) {
    let (width, height) = (width.max(1), height.max(1));
    let across = max_size
        .0
        .filter(|&max| max > 0)
        .map_or(1.0, |max| f64::from(max) / f64::from(width));
    let down = max_size
        .1
        .filter(|&max| max > 0)
        .map_or(1.0, |max| f64::from(max) / f64::from(height));
    let scale = across.min(down).min(1.0);
    let scaled = |side: u32| ((f64::from(side) * scale).round() as u32).max(1);
    (scaled(width), scaled(height))
}

#[cfg(test)]
mod tests {
    use super::fitted;

    #[test]
    fn pictures_fit_their_box_keeping_their_shape() {
        assert_eq!(fitted((3840, 2160), (Some(1600), None)), (1600, 900));
        assert_eq!(fitted((3840, 2160), (Some(1600), Some(600))), (1067, 600));
        assert_eq!(fitted((1920, 1080), (Some(4000), Some(4000))), (1920, 1080));
        assert_eq!(fitted((1920, 1080), (None, None)), (1920, 1080));
        assert_eq!(fitted((1206, 2622), (None, Some(500))), (230, 500));
        assert_eq!(fitted((100, 1), (Some(10), None)), (10, 1));
    }
}
