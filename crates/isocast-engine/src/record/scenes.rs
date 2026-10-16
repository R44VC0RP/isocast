//! The scene thread: whenever the live setup (or the sources it runs)
//! changes, it lays out and resolves the setup's scenes into compiled draw
//! lists, off the program's thread, and hands them to the compositor with
//! a version number the compositor checks with one atomic load per frame.
//! A source whose picture turns out to be another size than layout took
//! it to be (the compositor tells) lays the scenes out again.
//!
//! Content items' pictures (text and images, 5.5a) are drawn here, as part
//! of resolving, so the scenes it publishes always come with the content
//! they show: one change never shows its new layout with old text. Text
//! showing a data key is resolved again when the key changes, at most once
//! a canvas frame however often it changes; text read from a file is
//! looked at twice a second.

use parking_lot::{Condvar, Mutex};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use isocast_protocol::{Filter, FilterKind, Setup};

use super::Slots;
use crate::compiled::{CompiledScenes, LaidOut, NodeId, NodeIds, compile};
use crate::content::{Contents, Sizer};
use crate::critical::Critical;
use crate::data::{DataStore, Listening};
use crate::filter::output_size;
use crate::layout::{Layouter, settings_size};
use crate::source::FrameSlot;

/// Counts layouts that differ from the one before, across every engine in
/// this process, so a generation never repeats while clients watch.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// How often files that text content reads are looked at.
const FILE_POLL: Duration = Duration::from_millis(500);

/// A frame's width and height (the platform's [`crate::Platform::frame_size`]).
pub(super) type FrameSize<F> = Arc<dyn Fn(&F) -> (u32, u32) + Send + Sync>;

/// Something to warn clients about (content that couldn't be drawn).
pub(super) type OnWarning = Arc<dyn Fn(String) + Send + Sync>;

/// What the program draws: the setup's scenes compiled, and the sources
/// they show.
pub(super) struct ProgramScenes<F> {
    /// Counts compiles; the compositor adopts a new version at its next
    /// frame.
    pub version: u64,
    /// The setup revision it comes from.
    pub revision: u64,
    /// Its layout's generation: the same as the version before's if every
    /// item is where it was.
    pub generation: u64,
    pub compiled: Arc<CompiledScenes>,
    /// Each compiled source's frames, by source index (`None` while it
    /// isn't running).
    pub slots: Vec<Option<Arc<FrameSlot<F>>>>,
    /// Each compiled source's filters, by source index.
    pub filters: Vec<Arc<[Filter]>>,
    /// Whether a source's filters change its picture over time even when
    /// its frame doesn't (scrolling, a render delay).
    pub moving: Vec<bool>,
    /// Every slot the program reads, stinger videos too, by id.
    pub all: Slots<F>,
}

/// A change for the scene thread to resolve.
pub(super) struct SceneInput<F> {
    pub setup: Arc<Setup>,
    pub revision: u64,
    /// The sources running, and stinger videos.
    pub slots: Slots<F>,
}

impl<F> Clone for SceneInput<F> {
    fn clone(&self) -> Self {
        Self {
            setup: self.setup.clone(),
            revision: self.revision,
            slots: self.slots.clone(),
        }
    }
}

pub(super) struct SceneThread<F> {
    shared: Arc<Shared<F>>,
    thread: Mutex<Option<JoinHandle<()>>>,
    /// Hears data keys change while the thread runs.
    _listening: Listening,
}

struct Shared<F> {
    failure: Mutex<Option<String>>,
    input: Critical<Waiting<F>>,
    wake: Condvar,
    stop: AtomicBool,
    version: AtomicU64,
    output: Critical<Option<Arc<ProgramScenes<F>>>>,
    /// How long the last resolve took, in nanoseconds.
    resolve_ns: AtomicU64,
    /// How long drawing content took the last time any was drawn.
    raster_ns: AtomicU64,
    /// Content pictures the scenes show, and their bytes.
    contents: AtomicU32,
    content_bytes: AtomicU64,
    /// The data keys the scenes' content shows.
    bound: Critical<HashSet<Arc<str>>>,
}

/// What waits for the scene thread.
struct Waiting<F> {
    /// A change (in place of any before it not yet resolved).
    next: Option<SceneInput<F>>,
    /// Pictures the compositor found to be another size than layout took
    /// them to be, by source id.
    sizes: Vec<(Arc<str>, (u32, u32))>,
    /// A data key that content shows changed.
    data: bool,
}

impl<F: Clone + Send + 'static> SceneThread<F> {
    /// Start the thread; `frame_size` tells the size of a source's frames,
    /// content shows `data`'s values, and `on_warning` hears of content
    /// that can't be drawn.
    pub(super) fn start(
        frame_size: FrameSize<F>,
        data: Arc<DataStore>,
        on_warning: OnWarning,
        on_failed: Arc<dyn Fn(String, bool) + Send + Sync>,
    ) -> Self {
        let shared = Arc::new(Shared {
            failure: Mutex::new(None),
            input: Critical::new(Waiting {
                next: None,
                sizes: Vec::new(),
                data: false,
            }),
            wake: Condvar::new(),
            stop: AtomicBool::new(false),
            version: AtomicU64::new(0),
            output: Critical::new(None),
            resolve_ns: AtomicU64::new(0),
            raster_ns: AtomicU64::new(0),
            contents: AtomicU32::new(0),
            content_bytes: AtomicU64::new(0),
            bound: Critical::new(HashSet::new()),
        });
        let listening = data.listen({
            let shared = Arc::downgrade(&shared);
            Arc::new(move |keys: &[Arc<str>]| {
                let Some(shared) = shared.upgrade() else {
                    return;
                };
                let shown = {
                    let bound = shared.bound.read();
                    !bound.is_empty() && keys.iter().any(|key| bound.contains(key))
                };
                if shown {
                    shared.input.lock().data = true;
                    shared.wake.notify_all();
                }
            })
        });
        let thread = thread::Builder::new()
            .name("scenes".into())
            .spawn({
                let shared = shared.clone();
                move || {
                    if let Err(message) = isocast_diagnostics::contain("scene worker", || {
                        run(&shared, &frame_size, &data, &on_warning)
                    }) {
                        *shared.failure.lock() = Some(message.clone());
                        shared.stop.store(true, Ordering::SeqCst);
                        on_failed(
                            message,
                            shared.input.is_broken()
                                || shared.output.is_broken()
                                || shared.bound.is_broken(),
                        );
                    }
                }
            })
            .expect("could not start the scene thread");
        Self {
            shared,
            thread: Mutex::new(Some(thread)),
            _listening: listening,
        }
    }

    /// Resolve `input` (in place of any change still waiting).
    pub(super) fn submit(&self, input: SceneInput<F>) {
        self.shared.input.lock().next = Some(input);
        self.shared.wake.notify_all();
    }

    /// The pictures of these sources (by id) are these sizes, which isn't
    /// what the current layout took them to be: lay out again.
    pub(super) fn resized(&self, sizes: impl Iterator<Item = (Arc<str>, (u32, u32))>) {
        let mut input = self.shared.input.lock();
        let before = input.sizes.len();
        input.sizes.extend(sizes);
        if input.sizes.len() > before {
            self.shared.wake.notify_all();
        }
    }

    /// The newest version resolved (0 before the first).
    pub(super) fn version(&self) -> u64 {
        self.shared.version.load(Ordering::Acquire)
    }

    /// The newest scenes resolved.
    pub(super) fn latest(&self) -> Option<Arc<ProgramScenes<F>>> {
        self.shared.output.read().clone()
    }

    /// How long the last resolve took, in microseconds.
    pub(super) fn resolve_us(&self) -> f64 {
        micros(self.shared.resolve_ns.load(Ordering::Relaxed))
    }

    /// How long drawing content took the last time any was drawn, in
    /// microseconds; the content pictures the scenes show and their size
    /// in megabytes.
    pub(super) fn content(&self) -> (f64, u32, f64) {
        let bytes = self.shared.content_bytes.load(Ordering::Relaxed);
        (
            micros(self.shared.raster_ns.load(Ordering::Relaxed)),
            self.shared.contents.load(Ordering::Relaxed),
            (bytes as f64 / 1e4).round() / 100.0,
        )
    }

    pub(super) fn stop(&self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        {
            // Under the lock, so the thread can't miss the wake-up.
            let _input = self.shared.input.read();
            self.shared.wake.notify_all();
        }
        if let Some(thread) = self.thread.lock().take() {
            let _ = thread.join();
        }
        // Let go of the sources' frames.
        self.shared.output.lock().take();
    }

    pub(super) fn broken(&self) -> bool {
        self.shared.input.is_broken()
            || self.shared.output.is_broken()
            || self.shared.bound.is_broken()
    }
    pub(super) fn failure(&self) -> Option<String> {
        self.shared.failure.lock().clone()
    }
}

impl<F> Drop for SceneThread<F> {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.wake.notify_all();
    }
}

fn micros(ns: u64) -> f64 {
    (ns as f64 / 100.0).round() / 10.0
}

/// What the scene thread keeps from one resolve to the next.
struct Resolver<F> {
    /// Item paths keep their node ids from one change to the next.
    ids: NodeIds,
    layouter: Layouter,
    /// Content items' pictures, and what goes into them.
    contents: Contents,
    /// The pictures' sizes the compositor reported since the last change,
    /// by source id: what layout uses before anything else.
    reported: HashMap<Arc<str>, (u32, u32)>,
    /// The last change resolved, to lay out again when a size changes.
    current: Option<SceneInput<F>>,
    /// Where every item was put last time, to tell whether the layout
    /// changed.
    geometry: Vec<(NodeId, [f32; 6], [f32; 2], bool)>,
    generation: u64,
}

fn run<F: Clone>(
    shared: &Shared<F>,
    frame_size: &FrameSize<F>,
    data: &DataStore,
    on_warning: &OnWarning,
) {
    let mut resolver = Resolver {
        ids: NodeIds::default(),
        layouter: Layouter::default(),
        contents: Contents::default(),
        reported: HashMap::new(),
        current: None,
        geometry: Vec::new(),
        generation: 0,
    };
    // Data changes are resolved at most once a canvas frame: not before
    // this.
    let mut data_due = Instant::now();
    loop {
        let (next, sizes) = {
            let mut input = shared.input.lock();
            loop {
                if shared.stop.load(Ordering::SeqCst) {
                    return;
                }
                let now = Instant::now();
                if input.next.is_some() || !input.sizes.is_empty() {
                    break;
                }
                if input.data && now >= data_due {
                    break;
                }
                let wait = match input.data {
                    true => Some(data_due - now),
                    false if resolver.contents.reads_files() => Some(FILE_POLL),
                    false => None,
                };
                match wait {
                    Some(wait) => {
                        let timeout = input.wait_for(&shared.wake, wait);
                        // A file text content shows changed: resolve.
                        if timeout.timed_out()
                            && !input.data
                            && resolver.contents.reads_files()
                            && resolver.contents.files_changed()
                        {
                            input.data = true;
                        }
                    }
                    None => input.wait(&shared.wake),
                }
            }
            input.data = false;
            (input.next.take(), std::mem::take(&mut input.sizes))
        };
        let changed = next.is_some();
        if let Some(next) = next {
            // Sources may have reopened, or their filters changed: their
            // sizes are worked out afresh (and reported again if wrong).
            resolver.reported.clear();
            resolver.current = Some(next);
        }
        for (id, size) in sizes {
            resolver.reported.insert(id, size);
        }
        let Some(input) = resolver.current.clone() else {
            continue;
        };
        let began = Instant::now();
        let fps = input.setup.canvas.fps.max(1);
        let version = shared.version.load(Ordering::Relaxed) + 1;
        let scenes = resolve(input, &mut resolver, frame_size, data, version);
        let drawn = resolver.contents.end();
        shared
            .resolve_ns
            .store(began.elapsed().as_nanos() as u64, Ordering::Relaxed);
        if drawn.raster_ns > 0 {
            shared.raster_ns.store(drawn.raster_ns, Ordering::Relaxed);
        }
        shared.contents.store(drawn.pictures, Ordering::Relaxed);
        shared.content_bytes.store(drawn.bytes, Ordering::Relaxed);
        {
            let keys = resolver.contents.keys();
            let mut bound = shared.bound.lock();
            if *bound != *keys {
                bound.clone_from(keys);
            }
        }
        *shared.output.lock() = Some(Arc::new(scenes));
        shared.version.store(version, Ordering::Release);
        data_due = began + Duration::from_secs(1) / fps;
        if changed {
            isocast_diagnostics::debug!(
                "compositor: scenes resolved for a setup change in {:?} ({} pictures)",
                began.elapsed(),
                drawn.pictures
            );
        }
        for warning in resolver.contents.take_warnings() {
            on_warning(warning);
        }
    }
}

/// `input`'s scenes laid out and compiled, as `version`, with the
/// pictures of their content drawn (text from `data`).
fn resolve<F: Clone>(
    input: SceneInput<F>,
    resolver: &mut Resolver<F>,
    frame_size: &FrameSize<F>,
    data: &DataStore,
    version: u64,
) -> ProgramScenes<F> {
    let setup = &input.setup;
    let canvas = (setup.canvas.width, setup.canvas.height);
    let slot = |id: &str| {
        input
            .slots
            .iter()
            .find(|(slot, _)| slot == id)
            .map(|(_, slot)| slot.clone())
    };
    // A picture's size: as the compositor last saw it, else its newest
    // frame's or what its settings say, through its filters (cropping and
    // scaling ones change it), as the compositor will draw it.
    let reported = &resolver.reported;
    let size_of = |id: &str| {
        let source = setup.source(id)?;
        reported.get(id).copied().or_else(|| {
            slot(id)
                .and_then(|slot| slot.newest())
                .map(|frame| frame_size(&frame))
                .or_else(|| settings_size(&source.kind, canvas))
                .map(|size| output_size(&source.filters, size))
        })
    };
    resolver.contents.begin();
    let mut scenes = setup.scenes.clone();
    let mut used = Vec::new();
    let mut sizer = Sizer::new(&mut resolver.contents, data);
    resolver.layouter.resolve(
        &mut scenes,
        canvas,
        &size_of,
        &mut sizer,
        &mut resolver.ids,
        &mut used,
    );
    let contents = &mut resolver.contents;
    let mut compiled = compile(
        scenes.into(),
        canvas,
        &mut resolver.ids,
        None,
        &setup.keyframes,
        &mut |item| contents.picture(item, data),
    );
    for (id, size) in used {
        if let Some(index) = compiled.source_index(&id) {
            compiled.sized[index as usize] = LaidOut::Read(size);
        }
    }
    // A new generation only if something moved.
    let geometry: Vec<_> = compiled
        .nodes
        .iter()
        .map(|node| (node.id, node.transform.0, node.size, node.visible))
        .collect();
    if geometry != resolver.geometry || resolver.generation == 0 {
        resolver.generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
        resolver.geometry = geometry;
    }
    let filters: Vec<Arc<[Filter]>> = compiled
        .sources
        .iter()
        .map(|id| {
            setup.source(id).map_or_else(
                || Arc::from(Vec::new()),
                |source| {
                    source
                        .filters
                        .iter()
                        .filter(|filter| filter.enabled)
                        .cloned()
                        .collect()
                },
            )
        })
        .collect();
    let moving = filters
        .iter()
        .map(|filters| {
            filters.iter().any(|filter| {
                matches!(
                    filter.kind,
                    FilterKind::Scroll(_) | FilterKind::RenderDelay(_)
                )
            })
        })
        .collect();
    ProgramScenes {
        version,
        revision: input.revision,
        generation: resolver.generation,
        slots: compiled.sources.iter().map(|id| slot(id)).collect(),
        filters,
        moving,
        all: input.slots,
        compiled: Arc::new(compiled),
    }
}
