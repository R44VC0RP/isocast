//! The program: the scene on air, composited on the GPU at the canvas
//! rate, with transitions between scenes.
//!
//! It draws compiled scenes (from the scene thread, adopted when their
//! version changes). Each frame it takes each source's newest frame; if
//! none is new, nothing animates and nothing changed, it sends the last
//! frame again instead of drawing it. Otherwise it rewrites only the draw
//! records that changed (by a source's picture, a show or hide transition,
//! or an animation at the tick's time) and encodes the draw list with
//! cached bind groups, so a frame costs the same CPU however many items
//! the scene has.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use isocast_protocol::{Crop, Filter, ItemTransition, MatteLayout, Stinger, TransitionKind};

use super::scenes::ProgramScenes;
use super::{Changing, OnAir};
use crate::compiled::{CompiledScenes, LaidOut, NodeId};
use crate::draw_list::DrawList;
use crate::filter::Filters;
use crate::gpu::{FrameImporter, Gpu, GpuFrame, InFlight, PendingFrame};
use crate::render::{Affine, Layer, Rect, Renderer};
use crate::scene::ItemLook;
use crate::transition::{self, Overlay, Transitions, stinger_id};
use crate::ycbcr::ToYCbCr;
use crate::{FramePool, Platform, Time};

/// What a frame is drawn for: the program, or studio mode's preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Feed {
    Program = 0,
    Preview = 1,
}

/// The encoder frame and BGRA monitor picture, and whether they were reused.
type Drawn<F> = (PendingFrame<(F, Option<F>)>, bool);

pub(super) struct Compositor<P: Platform> {
    gpu: Gpu,
    renderer: Renderer,
    importer: P::Importer,
    /// The canvas the scene is drawn on, then converted into a frame.
    picture: wgpu::Texture,
    /// With no monitor, keep drawing into one private texture instead of
    /// cycling native BGRA buffers that nobody will read.
    private_picture: wgpu::Texture,
    picture_view: wgpu::TextureView,
    to_ycbcr: ToYCbCr,
    /// What the conversion reads: `picture`.
    picture_bound: wgpu::BindGroup,
    /// Y'CbCr frames the canvas is converted into, handed to the program's
    /// outputs (encoders take them as they are).
    pool: P::FramePool,
    /// The actual BGRA canvas, handed to monitors without a YUV round trip.
    rgb_pool: P::FramePool,
    pictures: Vec<(wgpu::Texture, wgpu::TextureView, wgpu::BindGroup, u64)>,
    canvas: (u32, u32),
    /// Raster size; scene monitors keep the layout's coordinates but draw
    /// directly into the viewer's smaller picture.
    size: (u32, u32),
    /// The scenes it draws, as last adopted.
    inputs: Option<Arc<ProgramScenes<P::Frame>>>,
    list: Option<DrawList>,
    /// Each compiled source's newest frame (by source index), repeated
    /// until a newer one arrives.
    frames: Vec<Option<SourceFrame<P::Frame>>>,
    /// The sources the frame being drawn shows (source indexes).
    needed: Vec<u32>,
    /// Their frames' times, to compare with those a feed last drew.
    seen: Vec<(u32, Time)>,
    /// What each feed drew last, to send again while nothing changes.
    shown: [Shown<P::Frame>; 2],
    /// Views of the frames the canvas is converted into, by their luma
    /// texture (frame pools recycle a few), and when each was last used.
    targets: Vec<(wgpu::Texture, wgpu::TextureView, wgpu::TextureView, u64)>,
    /// Canvases for the two scenes of a transition, made on first use.
    scratch: Option<[(wgpu::Texture, wgpu::TextureView); 2]>,
    filters: Filters,
    transitions: Transitions,
    /// The stinger video playing, if one is.
    stinger: Option<Playing>,
    /// Canvases for a stinger's picture and matte, made on first use.
    overlays: Option<[wgpu::Texture; 2]>,
    /// Items being shown or hidden with a transition, by node.
    items: HashMap<NodeId, ItemChange>,
    /// Their looks at the frame being drawn.
    looks: Vec<(NodeId, ItemLook)>,
    /// What is on air, as the program last published it.
    on_air: OnAirKey,
    /// Draws in the last frame drawn.
    draws: u32,
    /// Frames drawn, for letting go of unused target views.
    count: u64,
    /// Each compiled source's picture size, if told to the scene thread
    /// because layout took it to be another (by source index).
    told: Vec<Option<(u32, u32)>>,
    /// Pictures of another size than layout took them to be, not yet told.
    resized: Vec<(Arc<str>, (u32, u32))>,
    /// When the engine's clock started (animations on the `clock` clock
    /// run from it).
    epoch: Time,
    /// The scenes each feed shows (the one on air and the one a transition
    /// changes from), by id, and when it started showing each: animations
    /// on the `on_air` clock run from it.
    starts: [Vec<(Arc<str>, Time)>; 2],
    /// Animated properties evaluated for the last frame drawn.
    animated: u32,
    in_flight: InFlight,
}

/// A source's newest frame, as drawn.
struct SourceFrame<F> {
    /// The frame, kept so its surface isn't reused while it is drawn.
    _frame: F,
    at: Time,
    /// As imported, and after the source's filters.
    imported: GpuFrame,
    picture: GpuFrame,
    /// What samples `picture`.
    group: wgpu::BindGroup,
    /// Its textures stay the same for its surface, so its bind group can
    /// be kept.
    cached: bool,
    /// Carried over from scenes adopted before: its filters may have
    /// changed, so `picture` is made again.
    stale: bool,
}

/// What a feed drew last.
struct Shown<F> {
    frame: Option<PendingFrame<(F, Option<F>)>>,
    rgb: bool,
    version: u64,
    scene: Option<usize>,
    generation: u64,
    seen: Vec<(u32, Time)>,
}

impl<F> Default for Shown<F> {
    fn default() -> Self {
        Self {
            frame: None,
            rgb: false,
            version: 0,
            scene: None,
            generation: 0,
            seen: Vec::new(),
        }
    }
}

/// What the program shows, as far as which sources are on air goes.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct OnAirKey {
    version: u64,
    scene: Option<usize>,
    from: Option<usize>,
    stinger: Option<Time>,
    known: bool,
}

/// An item's show or hide transition under way.
struct ItemChange {
    /// The source it shows (an index), whose frames it still needs while
    /// hiding.
    source: Option<u32>,
    showing: bool,
    began: Time,
    transition: ItemTransition,
}

impl ItemChange {
    fn progress(&self, at: Time) -> f32 {
        let duration = f64::from(self.transition.duration_ms.max(1)) / 1000.0;
        (at.saturating_since(self.began).as_secs_f64() / duration) as f32
    }
}

/// A stinger's video playing for a transition.
struct Playing {
    /// Its source id.
    id: String,
    /// The switch it plays for.
    began: Time,
    /// When it came on air, which its video's start is placed at.
    since: Time,
}

impl<P: Platform> Compositor<P> {
    pub(super) fn new(platform: &P, canvas: (u32, u32)) -> Result<Self> {
        Self::with_gpu(platform, canvas, Gpu::for_platform(platform)?)
    }

    /// A compositor drawing on `gpu`, shared with others (the scene feeds'
    /// compositors, one per watched scene, share one device).
    pub(super) fn with_gpu(platform: &P, canvas: (u32, u32), gpu: Gpu) -> Result<Self> {
        Self::with_size(platform, canvas, canvas, gpu)
    }

    pub(super) fn with_size(
        platform: &P,
        canvas: (u32, u32),
        size: (u32, u32),
        gpu: Gpu,
    ) -> Result<Self> {
        let renderer = Renderer::new(&gpu, wgpu::TextureFormat::Bgra8Unorm);
        let filters = Filters::new(&gpu, renderer.format(), true);
        let transitions = Transitions::new(&gpu, renderer.format());
        let importer = platform.frame_importer(&gpu)?;
        let pool = platform.ycbcr_frame_pool(size.0, size.1)?;
        let rgb_pool = platform.frame_pool(size.0, size.1)?;
        let picture = crate::scene::canvas_texture(&gpu, renderer.format(), size);
        let picture_view = picture.create_view(&Default::default());
        let to_ycbcr = ToYCbCr::new(&gpu);
        let picture_bound = to_ycbcr.bind(&gpu, &picture);
        Ok(Self {
            gpu,
            renderer,
            importer,
            private_picture: picture.clone(),
            picture,
            picture_view,
            to_ycbcr,
            picture_bound,
            pool,
            rgb_pool,
            pictures: Vec::new(),
            canvas,
            size,
            inputs: None,
            list: None,
            frames: Vec::new(),
            needed: Vec::new(),
            seen: Vec::new(),
            shown: Default::default(),
            targets: Vec::new(),
            scratch: None,
            filters,
            transitions,
            stinger: None,
            overlays: None,
            items: HashMap::new(),
            looks: Vec::new(),
            on_air: OnAirKey::default(),
            draws: 0,
            count: 0,
            told: Vec::new(),
            resized: Vec::new(),
            epoch: Time::from_nanos(0),
            starts: Default::default(),
            animated: 0,
            in_flight: InFlight::default(),
        })
    }

    /// The canvas size it draws.
    pub(super) fn canvas(&self) -> (u32, u32) {
        self.canvas
    }

    pub(super) fn size(&self) -> (u32, u32) {
        self.size
    }

    pub(super) fn device_lost(&self) -> bool {
        self.gpu.is_lost()
    }

    /// The engine's clock started at `epoch`.
    pub(super) fn set_epoch(&mut self, epoch: Time) {
        self.epoch = epoch;
    }

    /// Animated properties evaluated for the last frame [`Compositor::draw`]
    /// drew.
    pub(super) fn animated(&self) -> u32 {
        self.animated
    }

    /// The pictures found since the last call to be another size than the
    /// layout took them to be (by source id), for the scene thread.
    pub(super) fn resized(&mut self) -> std::vec::Drain<'_, (Arc<str>, (u32, u32))> {
        self.resized.drain(..)
    }

    /// Draws in the last frame [`Compositor::draw`] drew.
    pub(super) fn draws(&self) -> u32 {
        self.draws
    }

    /// Draw `scenes` from the next frame on; items they show or hide with
    /// a transition (compared with the scenes before) start it at `at`.
    /// Returns false (keeping the scenes before) if they were compiled for
    /// another canvas size: the scenes for this one are on their way.
    pub(super) fn adopt(&mut self, scenes: Arc<ProgramScenes<P::Frame>>, at: Time) -> bool {
        let compiled = scenes.compiled.clone();
        if compiled.canvas != self.canvas {
            return false;
        }
        if let Some(old) = &self.inputs {
            let old = old.compiled.clone();
            self.start_item_changes(&old, &compiled, at);
        }
        match &mut self.list {
            Some(list) => list.adopt(&self.gpu, &self.renderer, compiled.clone(), at),
            None => {
                self.list = Some(DrawList::new(
                    &self.gpu,
                    &self.renderer,
                    compiled.clone(),
                    at,
                ));
            }
        }
        if self.size != self.canvas
            && let Some(list) = &mut self.list
        {
            list.set_raster_size(self.size);
        }
        // Sources that keep running keep their last frame (and its
        // filters' picture is made again from it).
        let mut frames: Vec<Option<SourceFrame<P::Frame>>> =
            compiled.sources.iter().map(|_| None).collect();
        if let Some(old) = &self.inputs {
            for (index, id) in old.compiled.sources.iter().enumerate() {
                let Some(new) = compiled.source_index(id).map(|new| new as usize) else {
                    continue;
                };
                let same = match (&old.slots[index], &scenes.slots[new]) {
                    (Some(old), Some(new)) => Arc::ptr_eq(old, new),
                    _ => false,
                };
                if same && let Some(mut frame) = self.frames[index].take() {
                    frame.stale = true;
                    frames[new] = Some(frame);
                }
            }
        }
        self.frames = frames;
        self.told = vec![None; compiled.sources.len()];
        self.inputs = Some(scenes);
        true
    }

    /// Start (or stop) the show and hide transitions of items whose
    /// visibility differs between `old` and `new`, at `at`; those under
    /// way follow their items into `new`.
    fn start_item_changes(&mut self, old: &CompiledScenes, new: &CompiledScenes, at: Time) {
        let before: HashMap<NodeId, bool> = old
            .nodes
            .iter()
            .map(|node| (node.id, node.visible))
            .collect();
        let after: HashMap<NodeId, usize> = new
            .nodes
            .iter()
            .enumerate()
            .map(|(index, node)| (node.id, index))
            .collect();
        self.items.retain(|id, change| match after.get(id) {
            Some(&index) => {
                change.source = new.nodes[index].source;
                true
            }
            None => false,
        });
        for node in &new.nodes {
            let Some(&was) = before.get(&node.id) else {
                continue;
            };
            if was == node.visible {
                continue;
            }
            let transition = match node.visible {
                true => node.show,
                false => node.hide,
            };
            match transition {
                Some(transition) => {
                    self.items.insert(
                        node.id,
                        ItemChange {
                            source: node.source,
                            showing: node.visible,
                            began: at,
                            transition,
                        },
                    );
                }
                None => {
                    self.items.remove(&node.id);
                }
            }
        }
    }

    /// The scene `id` (the first scene if `None`), as an index.
    fn scene(&self, id: Option<&str>) -> Option<usize> {
        self.list.as_ref()?.compiled().scene_index(id)
    }

    /// Forget what `feed` drew last (it isn't wanted any more), so its
    /// frame goes back to the pool. The preview's scene, drawn again, comes
    /// on air again.
    pub(super) fn forget(&mut self, feed: Feed) {
        self.shown[feed as usize].frame = None;
        if feed == Feed::Preview {
            self.starts[Feed::Preview as usize].clear();
        }
    }

    /// Note that `feed` shows `scenes` (by id) at `tick`: those it didn't
    /// show before start showing now.
    fn note_starts(&mut self, feed: Feed, scenes: [Option<&Arc<str>>; 2], tick: Time) {
        let starts = &mut self.starts[feed as usize];
        let showing = |id: &Arc<str>| scenes.iter().flatten().any(|scene| scene[..] == id[..]);
        if starts.len() == scenes.iter().flatten().count()
            && starts.iter().all(|(id, _)| showing(id))
        {
            return;
        }
        starts.retain(|(id, _)| showing(id));
        for scene in scenes.into_iter().flatten() {
            if !starts.iter().any(|(id, _)| **id == **scene) {
                starts.push((scene.clone(), tick));
            }
        }
    }

    /// Evaluate the animations of scene `scene` (an index) and of the
    /// scenes it shows at `at`, for `feed`. Returns whether a record
    /// changed.
    fn animate(&mut self, feed: Feed, scene: usize, at: Time) -> bool {
        let on_air = self.start(feed, scene, at);
        let epoch = self.epoch;
        self.list
            .as_mut()
            .is_some_and(|list| list.animate(scene, at, on_air, epoch))
    }

    /// When `feed` started showing scene `scene` (an index); `at` if it
    /// didn't note it.
    fn start(&self, feed: Feed, scene: usize, at: Time) -> Time {
        let Some(list) = &self.list else {
            return at;
        };
        let id = &list.compiled().list[scene].id;
        self.starts[feed as usize]
            .iter()
            .find(|(scene, _)| scene == id)
            .map_or(at, |(_, start)| *start)
    }

    /// The canvas showing what is on air (the first scene if nothing is)
    /// at `tick`, drawn from each source's newest frame captured no later
    /// than `limit`, for `feed`, and whether it is the frame drawn last
    /// time sent again because nothing changed. `None` if every canvas
    /// frame is still in use downstream (the frame is then dropped).
    pub(super) fn draw(
        &mut self,
        feed: Feed,
        on_air: Option<&OnAir>,
        tick: Time,
        limit: Time,
        rgb: bool,
    ) -> Result<Option<Drawn<P::Frame>>> {
        if !self.in_flight.available()? {
            return Ok(None);
        }
        let changing = on_air.and_then(|on_air| on_air.from.as_ref());
        let scene = self.scene(on_air.map(|on_air| &*on_air.scene));
        let from = changing.and_then(|changing| self.scene(Some(&changing.scene)));
        if feed == Feed::Preview {
            self.note_starts(feed, [on_air.map(|on_air| &on_air.scene), None], tick);
        }
        self.gather(scene, from);
        let moving = self.update_frames(limit)?;
        let animating = self.update_looks(limit);
        if changing.is_none()
            && let Some(scene) = scene
        {
            self.animate(feed, scene, tick);
        }
        let (version, generation) = match (&self.inputs, &self.list) {
            (Some(inputs), Some(list)) => (inputs.version, list.generation()),
            _ => (0, 0),
        };
        let shown = &self.shown[feed as usize];
        if let Some(frame) = &shown.frame
            && changing.is_none()
            && self.stinger.is_none()
            && !moving
            && !animating
            && shown.version == version
            && shown.generation == generation
            && shown.scene == scene
            && shown.seen == self.seen
            && shown.rgb == rgb
        {
            return Ok(Some((frame.clone(), true)));
        }
        let monitor = rgb;
        let rgb = if monitor { self.rgb_pool.take()? } else { None };
        let canvas = match feed {
            Feed::Program => self.pool.take()?,
            Feed::Preview => rgb.clone(),
        };
        // Cached imports do not retain their CVPixelBuffers. Hold every
        // sampled source through completion, including filters submitted
        // above even if no output buffer is available.
        let inputs: Vec<_> = self
            .frames
            .iter()
            .flatten()
            .map(|frame| frame._frame.clone())
            .collect();
        let canvas = canvas.filter(|_| !monitor || rgb.is_some());
        let Some(canvas) = canvas else {
            let encoder = self.gpu.device.create_command_encoder(&Default::default());
            let done = self.in_flight.submit(&self.gpu, encoder.finish());
            done.then(move |_| drop(inputs));
            return Ok(None);
        };
        if let Some(list) = &mut self.list {
            list.next_tick();
            list.take_draws();
        }
        match &rgb {
            Some(rgb) => self.picture_target(rgb)?,
            None if self.picture != self.private_picture => {
                self.picture = self.private_picture.clone();
                self.picture_view = self.picture.create_view(&Default::default());
                self.picture_bound = self.to_ycbcr.bind(&self.gpu, &self.picture);
            }
            None => {}
        }
        let ycbcr = if feed == Feed::Program {
            Some(self.target_views(&canvas)?)
        } else {
            None
        };
        let mut encoder = self.gpu.device.create_command_encoder(&Default::default());
        match changing {
            None => {
                let view = self.picture_view.clone();
                self.encode_scene(&mut encoder, scene, &view);
            }
            Some(changing) => {
                self.draw_transition(feed, changing, scene, from, tick, limit)?;
            }
        }
        if let Some((luma, chroma)) = ycbcr {
            self.to_ycbcr
                .encode(&mut encoder, &self.picture_bound, &luma, &chroma);
        }
        let done = self.in_flight.submit(&self.gpu, encoder.finish());
        done.then(move |_| drop(inputs));
        self.draws = self.list.as_mut().map_or(0, DrawList::take_draws);
        self.animated = self.list.as_mut().map_or(0, DrawList::take_animated);
        let canvas = PendingFrame::new((canvas, rgb), done);
        let shown = &mut self.shown[feed as usize];
        shown.frame = Some(canvas.clone());
        shown.rgb = monitor;
        shown.version = version;
        shown.generation = generation;
        shown.scene = scene;
        std::mem::swap(&mut shown.seen, &mut self.seen);
        Ok(Some((canvas, false)))
    }

    /// Finish handing off submitted frames before replacing this compositor.
    pub(super) fn drain(&mut self) -> Result<()> {
        self.in_flight.drain()
    }

    fn picture_target(&mut self, frame: &P::Frame) -> Result<()> {
        let texture = match self.importer.target_cached(frame)? {
            Some(texture) => texture,
            None => {
                self.picture = self.importer.target(frame)?;
                self.picture_view = self.picture.create_view(&Default::default());
                self.picture_bound = self.to_ycbcr.bind(&self.gpu, &self.picture);
                return Ok(());
            }
        };
        self.count += 1;
        let count = self.count;
        self.pictures.retain(|picture| count - picture.3 < 600);
        let index = match self
            .pictures
            .iter()
            .position(|picture| picture.0 == texture)
        {
            Some(index) => index,
            None => {
                let view = texture.create_view(&Default::default());
                let bound = self.to_ycbcr.bind(&self.gpu, &texture);
                self.pictures.push((texture, view, bound, count));
                self.pictures.len() - 1
            }
        };
        let picture = &mut self.pictures[index];
        picture.3 = count;
        self.picture = picture.0.clone();
        self.picture_view = picture.1.clone();
        self.picture_bound = picture.2.clone();
        Ok(())
    }

    /// The sources a frame of `scene` (changing from `from`) shows, into
    /// `needed`: the scenes', and those of items still hiding.
    fn gather(&mut self, scene: Option<usize>, from: Option<usize>) {
        self.needed.clear();
        let Some(list) = &self.list else {
            return;
        };
        let compiled = list.compiled();
        for scene in [scene, from].into_iter().flatten() {
            for &source in &compiled.list[scene].sources {
                if !self.needed.contains(&source) {
                    self.needed.push(source);
                }
            }
        }
        // Items hiding with a transition still show their sources.
        for change in self.items.values().filter(|change| !change.showing) {
            if let Some(source) = change.source
                && !self.needed.contains(&source)
            {
                self.needed.push(source);
            }
        }
    }

    /// Take the newest frame of each source in `needed` captured no later
    /// than `limit` (a source's last frame if none is new), through its
    /// filters, and show it in the draw list; note the frames' times in
    /// `seen`. Returns whether a source's filters move by themselves.
    fn update_frames(&mut self, limit: Time) -> Result<bool> {
        self.seen.clear();
        let (Some(inputs), Some(list)) = (self.inputs.clone(), self.list.as_mut()) else {
            return Ok(false);
        };
        let compiled = list.compiled().clone();
        let mut moving = false;
        for &source in &self.needed {
            let index = source as usize;
            let newest = inputs.slots[index]
                .as_ref()
                .and_then(|slot| slot.peek_until_timed(limit));
            let filters = &inputs.filters[index];
            let id = &*compiled.sources[index];
            match newest {
                Some((frame, at)) if self.frames[index].as_ref().is_none_or(|f| f.at != at) => {
                    let (imported, cached) = match self.importer.import_cached(&frame)? {
                        Some(imported) => (imported, true),
                        None => (self.importer.import(&frame)?, false),
                    };
                    let picture = filtered(
                        &mut self.filters,
                        &self.gpu,
                        &self.renderer,
                        id,
                        &imported,
                        filters,
                        limit,
                    );
                    let group = group(list, &self.gpu, &self.renderer, &imported, &picture, cached);
                    self.frames[index] = Some(SourceFrame {
                        _frame: frame,
                        at,
                        imported,
                        picture,
                        group,
                        cached,
                        stale: false,
                    });
                }
                // The same frame: only moving filters (or new ones) change
                // its picture.
                _ if inputs.moving[index]
                    || self.frames[index].as_ref().is_some_and(|frame| frame.stale) =>
                {
                    if let Some(current) = &mut self.frames[index] {
                        current.stale = false;
                        current.picture = filtered(
                            &mut self.filters,
                            &self.gpu,
                            &self.renderer,
                            id,
                            &current.imported,
                            filters,
                            limit,
                        );
                        current.group = group(
                            list,
                            &self.gpu,
                            &self.renderer,
                            &current.imported,
                            &current.picture,
                            current.cached,
                        );
                    }
                }
                _ => {}
            }
            moving |= inputs.moving[index];
            let current = self.frames[index].as_ref();
            list.set_picture(
                source,
                current.map(|frame| (&frame.picture, frame.group.clone())),
            );
            if let Some(frame) = current {
                self.seen.push((source, frame.at));
                // Layout took the picture to be another size: tell the
                // scene thread (once per size).
                let size = (frame.picture.width, frame.picture.height);
                if let LaidOut::Read(laid_out) = compiled.sized[index]
                    && laid_out != Some(size)
                    && self.told[index] != Some(size)
                {
                    self.told[index] = Some(size);
                    self.resized.push((compiled.sources[index].clone(), size));
                }
            }
        }
        Ok(moving)
    }

    /// Work out how items part of the way through a show or hide
    /// transition look at `at` (finished ones are forgotten) and apply it
    /// to the draw list. Returns whether any is under way or just ended.
    fn update_looks(&mut self, at: Time) -> bool {
        self.items.retain(|_, change| change.progress(at) < 1.0);
        self.looks.clear();
        for (id, change) in &self.items {
            let (opacity, shift) = change
                .transition
                .look(change.showing, change.progress(at).max(0.0));
            self.looks.push((*id, ItemLook { opacity, shift }));
        }
        let changed = self
            .list
            .as_mut()
            .is_some_and(|list| list.apply_looks(&self.looks));
        changed || !self.looks.is_empty()
    }

    /// Encode drawing `scene` (on black; only black if `None`) into
    /// `target`.
    fn encode_scene(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        scene: Option<usize>,
        target: &wgpu::TextureView,
    ) {
        match (&mut self.list, scene) {
            (Some(list), Some(scene)) => {
                list.upload(encoder);
                list.encode(
                    &self.gpu,
                    &self.renderer,
                    encoder,
                    scene,
                    target,
                    wgpu::Color::BLACK,
                );
            }
            _ => {
                encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("nothing on air"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: target,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                });
            }
        }
    }

    /// Views of the luma and chroma planes of `canvas`, a frame from the
    /// pool, kept for the frames the pool recycles.
    fn target_views(
        &mut self,
        canvas: &P::Frame,
    ) -> Result<(wgpu::TextureView, wgpu::TextureView)> {
        self.count += 1;
        let Some((luma, chroma)) = self.importer.ycbcr_target_cached(canvas)? else {
            let (luma, chroma) = self.importer.ycbcr_target(canvas)?;
            let view = |texture: &wgpu::Texture| texture.create_view(&Default::default());
            return Ok((view(&luma), view(&chroma)));
        };
        let count = self.count;
        if let Some(target) = self.targets.iter_mut().find(|target| target.0 == luma) {
            target.3 = count;
            return Ok((target.1.clone(), target.2.clone()));
        }
        // Views of frames the pool no longer hands out go.
        self.targets.retain(|target| count - target.3 < 600);
        let views = (
            luma.create_view(&Default::default()),
            chroma.create_view(&Default::default()),
        );
        self.targets
            .push((luma, views.0.clone(), views.1.clone(), count));
        Ok(views)
    }

    /// Which sources the program shows at `tick`: of the scene on air and
    /// of the one it changes from, and a stinger's video while one runs
    /// (from a frame `ahead`, so its video has a frame's time to start).
    /// Fills `out` and returns true only when they may have changed since
    /// the last call.
    pub(super) fn visible(
        &mut self,
        on_air: Option<&OnAir>,
        ahead: Option<&OnAir>,
        tick: Time,
        out: &mut Vec<Arc<str>>,
    ) -> bool {
        let stinger = |on_air: Option<&OnAir>| {
            let changing = on_air?.from.as_ref()?;
            let TransitionKind::Stinger(stinger) = &changing.transition.kind else {
                return None;
            };
            Some((stinger_id(&stinger.file), changing.began))
        };
        self.note_starts(
            Feed::Program,
            [
                on_air.map(|on_air| &on_air.scene),
                on_air
                    .and_then(|on_air| on_air.from.as_ref())
                    .map(|changing| &changing.scene),
            ],
            tick,
        );
        let wanted = stinger(on_air).or_else(|| stinger(ahead));
        self.stinger = match (self.stinger.take(), wanted) {
            (_, None) => None,
            (Some(playing), Some((id, began))) if playing.id == id && playing.began == began => {
                Some(playing)
            }
            // The same video for another switch: off air for a tick, so it
            // starts again.
            (Some(playing), Some((id, _))) if playing.id == id => None,
            (_, Some((id, began))) => {
                // Frames of an earlier play must not show in this one.
                if let Some(inputs) = &self.inputs
                    && let Some((_, slot)) = inputs.all.iter().find(|(slot, _)| *slot == id)
                {
                    slot.clear();
                }
                Some(Playing {
                    id,
                    began,
                    since: tick,
                })
            }
        };
        let scene = self.scene(on_air.map(|on_air| &*on_air.scene));
        let from = on_air
            .and_then(|on_air| on_air.from.as_ref())
            .and_then(|changing| self.scene(Some(&changing.scene)));
        let key = OnAirKey {
            version: self.inputs.as_ref().map_or(0, |inputs| inputs.version),
            scene,
            from,
            stinger: self.stinger.as_ref().map(|playing| playing.began),
            known: true,
        };
        if key == self.on_air {
            return false;
        }
        self.on_air = key;
        out.clear();
        if let Some(list) = &self.list {
            let compiled = list.compiled();
            for scene in [scene, from].into_iter().flatten() {
                for &source in &compiled.list[scene].sources {
                    let id = &compiled.sources[source as usize];
                    if !out.contains(id) {
                        out.push(id.clone());
                    }
                }
            }
        }
        if let Some(playing) = &self.stinger {
            out.push(playing.id.as_str().into());
        }
        true
    }

    /// Draw the transition `changing` to `scene` (from `from`; indexes)
    /// at `tick` for `feed` into the program's canvas: Move from both
    /// scenes' items, the others from both scenes drawn on canvases of
    /// their own.
    fn draw_transition(
        &mut self,
        feed: Feed,
        changing: &Changing,
        scene: Option<usize>,
        from: Option<usize>,
        tick: Time,
        limit: Time,
    ) -> Result<wgpu::SubmissionIndex> {
        let progress = changing.progress.clamp(0.0, 1.0);
        let kind = &changing.transition.kind;
        if let TransitionKind::Move(settings) = kind
            && let Some(submission) =
                self.draw_move(feed, from, scene, settings.easing.apply(progress), tick)
        {
            return Ok(submission);
        }
        let [(before, before_view), (after, after_view)] = self.scratch().clone();
        for (scene, view) in [(from, &before_view), (scene, &after_view)] {
            // Each scene animates on its own clock; their records are
            // written before each is drawn.
            if let Some(scene) = scene {
                self.animate(feed, scene, tick);
            }
            let mut encoder = self.gpu.device.create_command_encoder(&Default::default());
            self.encode_scene(&mut encoder, scene, view);
            self.gpu.queue.submit([encoder.finish()]);
        }
        let overlays;
        let overlay = match kind {
            TransitionKind::Stinger(stinger) => {
                overlays = self.overlays().clone();
                Some(self.stinger_overlay(stinger, changing, limit, &overlays)?)
            }
            _ => None,
        };
        Ok(self.transitions.draw(
            &self.gpu,
            &self.picture,
            &before,
            &after,
            kind,
            progress,
            overlay,
            self.canvas,
        ))
    }

    /// A stinger's video at `limit` for the transition `changing`, its
    /// picture (and matte) drawn on the `overlays`. The scenes switch
    /// behind it once the frame shown is past the cut point, so the cut
    /// keeps to the video even if it started late.
    fn stinger_overlay<'a>(
        &mut self,
        stinger: &Stinger,
        changing: &Changing,
        limit: Time,
        overlays: &'a [wgpu::Texture; 2],
    ) -> Result<Overlay<'a>> {
        let length = std::time::Duration::from_millis(changing.transition.duration_ms.into());
        let cut_point = stinger
            .cut_point_ms
            .map_or(length / 2, |ms| std::time::Duration::from_millis(ms.into()));
        let mut overlay = Overlay {
            picture: None,
            matte: None,
            cut: changing.progress * length.as_secs_f32() >= cut_point.as_secs_f32(),
            invert: stinger.matte.is_some_and(|matte| matte.invert),
        };
        let id = stinger_id(&stinger.file);
        let Some(playing) = self
            .stinger
            .as_ref()
            .filter(|playing| playing.id == id && playing.began == changing.began)
        else {
            return Ok(overlay);
        };
        // Its video plays from `since`; the frame for this moment of the
        // transition, or the newest yet if it started late.
        let wanted = playing.since + limit.saturating_since(changing.began);
        let since = playing.since;
        let Some((frame, at)) = self
            .inputs
            .as_ref()
            .and_then(|inputs| inputs.all.iter().find(|(slot, _)| *slot == id))
            .and_then(|(_, slot)| slot.peek_between(since, wanted))
        else {
            return Ok(overlay);
        };
        overlay.cut = at.saturating_since(since) >= cut_point;
        let frame = self.importer.import(&frame)?;
        let (width, height) = (frame.width, frame.height);
        let halves = stinger.matte.map(|matte| match matte.layout {
            MatteLayout::SideBySide => (
                Crop {
                    right: width - width / 2,
                    ..Crop::default()
                },
                Crop {
                    left: width / 2,
                    ..Crop::default()
                },
            ),
            MatteLayout::Stacked => (
                Crop {
                    bottom: height - height / 2,
                    ..Crop::default()
                },
                Crop {
                    top: height / 2,
                    ..Crop::default()
                },
            ),
        });
        let whole = Rect {
            x: 0.0,
            y: 0.0,
            width: self.canvas.0 as f32,
            height: self.canvas.1 as f32,
        };
        let (picture, matte) = match halves {
            Some((picture, matte)) => (picture, Some(matte)),
            None => (Crop::default(), None),
        };
        let layer = Layer::rect(&frame, picture, whole, 1.0);
        self.renderer.render_on(
            &self.gpu,
            &overlays[0],
            &[layer],
            self.canvas,
            wgpu::Color::TRANSPARENT,
        );
        overlay.picture = Some(&overlays[0]);
        if let Some(matte) = matte {
            let layer = Layer::rect(&frame, matte, whole, 1.0);
            self.renderer.render_on(
                &self.gpu,
                &overlays[1],
                &[layer],
                self.canvas,
                wgpu::Color::TRANSPARENT,
            );
            overlay.matte = Some(&overlays[1]);
        }
        Ok(overlay)
    }

    /// Draw a Move `t` (eased) of the way from scene `from` to `to`
    /// (indexes), each animated at `tick` for `feed`, into the program's
    /// canvas; `None` if the scenes' layers can't be told apart (it then
    /// fades).
    fn draw_move(
        &mut self,
        feed: Feed,
        from: Option<usize>,
        to: Option<usize>,
        t: f32,
        tick: Time,
    ) -> Option<wgpu::SubmissionIndex> {
        let (from, to) = (from?, to?);
        let starts = [from, to].map(|scene| self.start(feed, scene, tick));
        let epoch = self.epoch;
        let list = self.list.as_mut()?;
        let compiled = list.compiled().clone();
        // Each source's picture, where layers look for them.
        let frames: Vec<(&str, GpuFrame)> = self
            .frames
            .iter()
            .enumerate()
            .filter_map(|(index, frame)| {
                let frame = frame.as_ref()?;
                Some((&*compiled.sources[index], frame.picture.clone()))
            })
            .collect();
        let mut nested = Vec::new();
        for (scene, start) in [from, to].into_iter().zip(starts) {
            list.animate(scene, tick, start, epoch);
            let mut encoder = self.gpu.device.create_command_encoder(&Default::default());
            list.upload(&mut encoder);
            list.encode_nested(&self.gpu, &self.renderer, &mut encoder, scene);
            self.gpu.queue.submit([encoder.finish()]);
            nested.push(
                compiled.list[scene]
                    .nested
                    .iter()
                    .filter_map(|&index| {
                        let canvas = list.canvas_frame(index as usize)?;
                        Some((compiled.list[index as usize].id.to_string(), canvas))
                    })
                    .collect::<Vec<_>>(),
            );
        }
        // Content items' pictures, by their paths.
        let contents: Vec<(String, GpuFrame)> = compiled
            .content_paths
            .iter()
            .filter_map(|(path, index)| Some((path.clone(), list.content_frame(*index as usize)?)))
            .collect();
        let identified = |scene: usize, nested| {
            let scene = &compiled.scenes[scene];
            let layers = crate::scene::layers(scene, &frames, nested, &contents, Affine::IDENTITY);
            let identities = transition::identities(scene);
            (identities.len() == layers.len())
                .then(|| identities.into_iter().zip(layers).collect::<Vec<_>>())
        };
        let mut before = identified(from, &nested[0])?;
        let mut after = identified(to, &nested[1])?;
        // Animated items move as their animations have them.
        list.animate_layers(from, before.iter_mut().map(|(_, layer)| layer));
        list.animate_layers(to, after.iter_mut().map(|(_, layer)| layer));
        let layers = transition::moved(before, after, t);
        Some(
            self.renderer
                .render(&self.gpu, &self.picture, &layers, self.canvas),
        )
    }

    fn overlays(&mut self) -> &[wgpu::Texture; 2] {
        let (gpu, canvas) = (&self.gpu, self.canvas);
        let format = self.renderer.format();
        self.overlays.get_or_insert_with(|| {
            [0, 1].map(|_| crate::scene::canvas_texture(gpu, format, canvas))
        })
    }

    fn scratch(&mut self) -> &[(wgpu::Texture, wgpu::TextureView); 2] {
        let (gpu, canvas) = (&self.gpu, self.canvas);
        let format = self.renderer.format();
        self.scratch.get_or_insert_with(|| {
            [0, 1].map(|_| {
                let texture = crate::scene::canvas_texture(gpu, format, canvas);
                let view = texture.create_view(&Default::default());
                (texture, view)
            })
        })
    }
}

impl<P: Platform> Drop for Compositor<P> {
    fn drop(&mut self) {
        // Also covers a scene disappearing or a contained worker failure.
        // On timeout InFlight cancels delivery, but GPU callbacks retain
        // the native buffers until it is actually safe to release them.
        let _ = self.in_flight.drain();
    }
}

/// `imported` through `filters` (none: as it is).
fn filtered(
    filters: &mut Filters,
    gpu: &Gpu,
    renderer: &Renderer,
    id: &str,
    imported: &GpuFrame,
    list: &[Filter],
    at: Time,
) -> GpuFrame {
    if list.is_empty() {
        return imported.clone();
    }
    filters.apply(gpu, renderer, id, imported.clone(), list, at)
}

/// What samples `picture`, made from `imported` (`cached`: its textures
/// stay the same for its surface): kept while its textures stay the same
/// (a cached import, or a filter's canvas), else made for this frame.
fn group(
    list: &mut DrawList,
    gpu: &Gpu,
    renderer: &Renderer,
    imported: &GpuFrame,
    picture: &GpuFrame,
    cached: bool,
) -> wgpu::BindGroup {
    let filtered = picture.planes().0 != imported.planes().0;
    match cached || filtered {
        true => list.group(gpu, renderer, picture),
        false => renderer.frame_group(gpu, picture),
    }
}
