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
use crate::gpu::{FrameImporter, Gpu, GpuPixels, InFlight};
use crate::render::{Layer, Renderer};
use crate::{Platform, Time};

/// How long to wait for a slot to come back before looking again.
const WAIT: Duration = Duration::from_millis(50);
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
    /// In-memory readbacks the GPU finished mapping (Linux), by sequence
    /// number: the viewer's thread writes and announces them in order.
    mapped: Mutex<Vec<(u64, Result<(), String>)>>,
    /// Signalled when a slot comes back or the feed closes.
    changed: Condvar,
}

#[derive(Default)]
struct Pending {
    generation: u64,
    released: Vec<(u64, u32)>,
    size: Option<(Option<u32>, Option<u32>)>,
}

impl SharedControl {
    /// The viewer is done with `slot` of ring `generation`.
    pub fn release(&self, generation: u64, slot: u32) {
        let mut pending = self.0.pending.lock();
        if (generation != pending.generation
            && generation.checked_add(1) != Some(pending.generation))
            || slot >= MAX_SLOTS
            || pending.released.contains(&(generation, slot))
        {
            return;
        }
        pending.released.push((generation, slot));
        self.0.changed.notify_all();
    }

    fn generation(&self, generation: u64) {
        let mut pending = self.0.pending.lock();
        pending.generation = generation;
        pending.released.retain(|(released, _)| {
            *released == generation || released.checked_add(1) == Some(generation)
        });
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
        let mut pending = self.0.pending.lock();
        Pending {
            generation: pending.generation,
            released: std::mem::take(&mut pending.released),
            size: pending.size.take(),
        }
    }

    /// Wait up to `timeout` for a slot to come back.
    fn wait(&self, timeout: Duration) {
        let mut pending = self.0.pending.lock();
        let _ = self.0.changed.wait_while_for(
            &mut pending,
            |pending| {
                pending.released.is_empty()
                    && pending.size.is_none()
                    && self.0.mapped.lock().is_empty()
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

/// A source's filters as they are now (the live setup's), and when the
/// engine's clock started (mask animations on `clock` run from it).
pub(crate) type LiveFilters = Box<dyn Fn() -> (Vec<Filter>, Option<Time>) + Send>;

/// Start a viewer's stream of `request`. `source` finds where the target's
/// frames arrive (looked up again all the time, since a source can reopen,
/// and given the stream's rate then), or says why it can't; `filters`, if
/// given, are drawn over each frame.
pub(crate) fn watch<P: Platform>(
    platform: &P,
    request: Request,
    source: impl Fn(u32, (Option<u32>, Option<u32>)) -> Result<Source<P::Frame>, String>
    + Send
    + 'static,
    filters: Option<LiveFilters>,
) -> SharedFeed<P::SharedSurface> {
    let (tx, events) = channel();
    let control = SharedControl(Arc::new(Control {
        fps: AtomicU32::new(request.fps.clamp(1, MAX_FPS)),
        closed: AtomicBool::new(false),
        pending: Mutex::new(Pending::default()),
        mapped: Mutex::new(Vec::new()),
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
    /// Per slot, where a frame is read back to on platforms whose surfaces
    /// are in memory (Linux); empty elsewhere.
    readback: Vec<wgpu::Buffer>,
    /// Slots the viewer holds.
    held: Vec<bool>,
    /// No frame was drawn into it yet.
    fresh: bool,
    /// How its frames are made was logged (in memory: copied or drawn).
    said: bool,
}

/// A frame read back into a slot's buffer (in memory, Linux), waiting for
/// the GPU to map it; written into the slot and announced by the viewer's
/// thread, in order.
struct Readback<S, F> {
    seq: u64,
    buffer: wgpu::Buffer,
    surface: S,
    bgra: bool,
    width: u32,
    event: SharedEvent<S>,
    /// The source frame and its GPU picture, kept until the GPU is done.
    _held: F,
}

/// Write and announce the readbacks at the front of `queue` the GPU has
/// mapped, in order. A failed map ends the stream (the slot would stay
/// taken for good; its buffer was never mapped, so isn't unmapped).
fn publish<P: Platform, F>(
    platform: &P,
    control: &SharedControl,
    tx: &Sender<SharedEvent<P::SharedSurface>>,
    queue: &mut std::collections::VecDeque<Readback<P::SharedSurface, F>>,
) -> Result<(), String> {
    let mut mapped = std::mem::take(&mut *control.0.mapped.lock());
    if mapped.is_empty() {
        return Ok(());
    }
    while let Some(front) = queue.front() {
        let Some(at) = mapped.iter().position(|(seq, _)| *seq == front.seq) else {
            break;
        };
        let (_, result) = mapped.swap_remove(at);
        let read = queue.pop_front().expect("the front readback");
        result.map_err(|err| format!("could not read a shared frame back: {err}"))?;
        let written = read
            .buffer
            .slice(..)
            .get_mapped_range()
            .map_err(|err| anyhow::anyhow!("could not read a shared frame back: {err}"))
            .and_then(|view| {
                platform.write_shared_surface(
                    &read.surface,
                    &view,
                    padded_row(read.width) as usize,
                    read.bgra,
                )
            });
        read.buffer.unmap();
        written.map_err(|err| format!("{err:#}"))?;
        if tx.send(read.event).is_err() {
            return Ok(());
        }
    }
    // Mapped out of order: kept until those before them are.
    control.0.mapped.lock().extend(mapped);
    Ok(())
}

/// Draw frames for one viewer until it leaves (`Ok`) or the stream can't
/// go on (the reason).
fn run<P: Platform>(
    platform: &P,
    control: &SharedControl,
    tx: &Sender<SharedEvent<P::SharedSurface>>,
    request: &Request,
    source: impl Fn(u32, (Option<u32>, Option<u32>)) -> Result<Source<P::Frame>, String>,
    filters: Option<LiveFilters>,
) -> Result<(), String> {
    let failed = |err: anyhow::Error| format!("{err:#}");
    let gpu = Gpu::for_platform(platform).map_err(failed)?;
    let renderer = Renderer::new(&gpu, wgpu::TextureFormat::Rgba8Unorm);
    let mut importer = platform.frame_importer(&gpu).map_err(failed)?;
    let slots = request.slots.clamp(2, MAX_SLOTS) as usize;
    let mut max_size = request.max_size;
    let mut ring: Option<Ring<P::SharedSurface>> = None;
    // A viewer can retain an old frame during resize. Keep at most one old
    // ring, coalescing further resizes until its GPU reads are acknowledged.
    let mut retired: Option<Ring<P::SharedSurface>> = None;
    let mut generation = 0;
    let mut pace = Pace::default();
    // The filters drawn last, and what draws them (made on first use).
    let mut drawn_filters: Vec<Filter> = Vec::new();
    let mut filtering: Option<Filters> = None;
    let mut in_flight = InFlight::default();
    let mut readbacks = std::collections::VecDeque::new();
    let mut next_seq = 0u64;
    loop {
        if control.0.closed.load(Ordering::SeqCst) {
            return Ok(());
        }
        publish(platform, control, tx, &mut readbacks)?;
        if !in_flight.available().map_err(failed)? {
            control.wait(Duration::from_millis(1));
            continue;
        }
        let pending = control.take();
        for (released, slot) in pending.released {
            for ring in ring.iter_mut().chain(retired.iter_mut()) {
                if released == ring.generation
                    && let Some(held) = ring.held.get_mut(slot as usize)
                {
                    *held = false;
                }
            }
        }
        if retired
            .as_ref()
            .is_some_and(|ring| ring.held.iter().all(|held| !held))
        {
            retired = None;
            if let Some(ring) = &mut ring {
                ring.fresh = true;
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
        let mut epoch = None;
        if let Some(filters) = &filters {
            let (now, started) = filters();
            epoch = started;
            if now != drawn_filters {
                drawn_filters = now;
                if let Some(ring) = &mut ring {
                    ring.fresh = true;
                }
            }
        }
        let fps = crate::governor::monitor_fps(control.0.fps.load(Ordering::Relaxed));
        if fps == 0 {
            control.wait(WAIT);
            continue;
        }
        let size_limit = crate::governor::monitor_size(max_size);
        let Source { slot, grid, drawn } = source(fps, size_limit)?;
        let again = || ring.as_ref().is_none_or(|ring| ring.fresh);
        let Some((frame, at)) = pace.next(platform, &slot, grid, fps, again) else {
            continue;
        };
        // The completion below retains `frame` through the GPU read, so
        // pooled surfaces can reuse their texture wrappers between ticks.
        let mut picture = match importer.import_cached(&frame).map_err(failed)? {
            Some(picture) => picture,
            None => importer.import(&frame).map_err(failed)?,
        };
        if drawn_filters.iter().any(|filter| filter.enabled) {
            let filtering =
                filtering.get_or_insert_with(|| Filters::new(&gpu, renderer.format(), true));
            if let Some(epoch) = epoch {
                filtering.set_epoch(epoch);
            }
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
        let size = fitted(input, size_limit);
        if retired.is_none() && ring.as_ref().is_none_or(|ring| ring.size != size) {
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
            control.generation(generation);
            if tx.send(event).is_err() {
                return Ok(());
            }
            let readback = if platform.shared_surface_in_memory() {
                (0..slots)
                    .map(|_| {
                        gpu.device.create_buffer(&wgpu::BufferDescriptor {
                            label: Some("shared frame readback"),
                            size: u64::from(padded_row(size.0)) * u64::from(size.1),
                            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                            mapped_at_creation: false,
                        })
                    })
                    .collect()
            } else {
                Vec::new()
            };
            retired = ring.replace(Ring {
                generation,
                size,
                surfaces,
                readback,
                held: vec![false; slots],
                fresh: true,
                said: false,
            });
        }
        let ring = ring.as_mut().expect("made above");
        let size = ring.size;
        let index = ring
            .held
            .iter()
            .position(|held| !held)
            .expect("a slot is free");
        let mut layer = Layer::full(&picture, size.0, size.1);
        layer.scale_filter = if size.0 < input.0 {
            ScaleFilter::Area
        } else if size == input {
            // The same size (the target was drawn at the viewer's size):
            // pixel for pixel, the cheapest pass (a software GPU spent
            // three cores on a bicubic one).
            ScaleFilter::Point
        } else {
            ScaleFilter::Bicubic
        };
        let target = &ring.surfaces[index].1;
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        // In memory, a canvas already at the slot's size in 8-bit RGB is
        // copied as it is (red and blue swapped on the way if BGRA): no
        // render pass, which on a software GPU costs more than the H.264
        // encode it replaces.
        let direct = (!ring.readback.is_empty() && input == size)
            .then_some(&picture.pixels)
            .and_then(|pixels| match pixels {
                // A canvas (the program, the preview): opaque over black
                // as it is. Sources (`Rgb`, maybe with straight alpha) take
                // the render pass, which composites them over black.
                GpuPixels::Premultiplied(texture)
                    if texture.usage().contains(wgpu::TextureUsages::COPY_SRC) =>
                {
                    match texture.format() {
                        wgpu::TextureFormat::Rgba8Unorm => Some((texture.clone(), false)),
                        wgpu::TextureFormat::Bgra8Unorm => Some((texture.clone(), true)),
                        _ => None,
                    }
                }
                _ => None,
            });
        if !ring.readback.is_empty() && !std::mem::replace(&mut ring.said, true) {
            isocast_diagnostics::diagnostic!("info"; "shared frames of {}: {}×{} in memory, {}", request.target, size.0, size.1, if direct.is_some() { "copied as drawn" } else { "drawn to size" });
        }
        if direct.is_none() {
            renderer.encode(
                &gpu,
                &mut encoder,
                target,
                &[layer],
                size,
                wgpu::Color::BLACK,
            );
        }
        let stamp = drawn.and_then(|drawn| stamp_at(&drawn, at));
        let event = SharedEvent::Frame {
            generation: ring.generation,
            slot: index as u32,
            time: at,
            revision: stamp.map(|stamp| stamp.revision),
            layout_generation: stamp.map(|stamp| stamp.generation),
        };
        if let Some(buffer) = ring.readback.get(index).cloned() {
            // In memory (Linux): copy the picture into the slot's readback
            // buffer in the same submission. Once the GPU has mapped it,
            // this thread writes it into the slot's memory and announces it
            // (`publish`, in tick order). Pipelined like the GPU path: the
            // viewer's thread never waits for the GPU.
            let (source, bgra) = match &direct {
                Some((texture, bgra)) => (texture, *bgra),
                None => (target, false),
            };
            encoder.copy_texture_to_buffer(
                source.as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer: &buffer,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(padded_row(size.0)),
                        rows_per_image: Some(size.1),
                    },
                },
                wgpu::Extent3d {
                    width: size.0,
                    height: size.1,
                    depth_or_array_layers: 1,
                },
            );
            let _done = in_flight.submit(&gpu, encoder.finish());
            ring.held[index] = true;
            ring.fresh = false;
            // The callback runs on whichever thread polls or submits next
            // (wgpu fires mapping callbacks from any submit): it only says
            // so; this thread writes the memory and announces the frame.
            let seq = next_seq;
            next_seq += 1;
            let mapped = control.clone();
            buffer
                .slice(..)
                .map_async(wgpu::MapMode::Read, move |result| {
                    let _pending = mapped.0.pending.lock();
                    mapped
                        .0
                        .mapped
                        .lock()
                        .push((seq, result.map_err(|err| err.to_string())));
                    mapped.0.changed.notify_all();
                });
            gpu.nudge();
            readbacks.push_back(Readback {
                seq,
                buffer,
                surface: ring.surfaces[index].0.clone(),
                bgra,
                width: size.0,
                event,
                _held: (frame, picture),
            });
            continue;
        }
        let done = in_flight.submit(&gpu, encoder.finish());
        ring.held[index] = true;
        ring.fresh = false;
        let tx = tx.clone();
        let control = control.clone();
        let surface = ring.surfaces[index].0.clone();
        done.then(move |_| {
            // A resize/close can drop the ring while drawing. Keep its
            // native surface alive, and never announce unfinished pixels.
            let _held = (surface, frame, picture);
            if !control.0.closed.load(Ordering::SeqCst) {
                let _ = tx.send(event);
            }
        });
    }
}

/// A readback row: `width` RGBA pixels, padded to wgpu's copy alignment.
fn padded_row(width: u32) -> u32 {
    (width * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
}

/// `input` scaled down to fit in `max_size` (either side unbounded if
/// absent), keeping its shape; never larger than it is.
pub(super) fn fitted(
    (width, height): (u32, u32),
    max_size: (Option<u32>, Option<u32>),
) -> (u32, u32) {
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
