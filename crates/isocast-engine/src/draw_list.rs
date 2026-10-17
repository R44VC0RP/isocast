//! Compiled scenes adopted on a GPU: their draw records in one storage
//! buffer (and their clips in another), uploaded when adopted and
//! afterwards only where a record changes (a source's picture changes
//! size or format, a show or hide transition or an animation runs), and
//! drawn into render passes without creating anything: canvases for nested
//! scenes are kept, and bind groups are cached by texture.

use std::collections::HashMap;
use std::sync::Arc;

use crate::Time;
use crate::animate::{Clocks, Values};
use crate::compiled::{CompiledScenes, NodeId, Sample};
use crate::gpu::{Gpu, GpuFrame};
use crate::picture::Picture;
use crate::render::{
    self, Affine, ClipRecord, Layer, LayerClip, Mode, Record, RecordBuffer, Renderer, TexelKind,
    Texels, record_bytes,
};
use crate::scene::{ItemLook, canvas_texture};

/// How many ticks a cached bind group lives on unused.
const GROUP_TTL: u64 = 120;

/// The size of the staging buffers record uploads go through (a write
/// bigger than one gets one of its own, kept for later writes too).
const STAGING_CHUNK: u64 = 256 * 1024;

pub(crate) struct DrawList {
    compiled: Arc<CompiledScenes>,
    /// Optional smaller raster for monitor-only nested scenes. Records,
    /// crop and layout still use the compiled canvas's coordinates.
    raster_size: Option<(u32, u32)>,
    /// The records as drawn: the compiled ones, rewritten for the pictures
    /// drawn and the transitions running.
    records: Vec<Record>,
    bytes: Vec<u8>,
    /// Records changed since they were last uploaded.
    dirty: Option<(usize, usize)>,
    /// Counts record changes, so a picture drawn earlier can tell it is
    /// out of date.
    generation: u64,
    buffer: RecordBuffer,
    state: Vec<DrawState>,
    /// Each source's picture this frame (by source index): its texels and
    /// the bind group that samples it.
    pictures: Vec<Option<(Texels, wgpu::BindGroup)>>,
    /// The draws that show each source, by source index.
    by_source: Vec<Vec<usize>>,
    /// Canvases of scenes shown inside others, by scene index, made on
    /// first use.
    canvases: Vec<Option<Canvas>>,
    /// The pass each canvas was last drawn in.
    drawn: Vec<u64>,
    pass: u64,
    /// Each node's opacity and whether it is shown, while transitions run.
    nodes: Vec<(f32, bool)>,
    /// Transitions changed the records last time they were applied.
    looking: bool,
    /// Draws made since [`DrawList::take_draws`].
    draws: u32,
    /// Bind groups of pictures whose textures stay the same (kept per
    /// surface by the importer, or a filter's canvas), by their first
    /// plane.
    groups: HashMap<wgpu::Texture, Group>,
    tick: u64,
    /// Content items' pictures as textures, by content index.
    contents: Vec<Option<Arc<ContentTexture>>>,
    /// The textures of the content adopted, by picture key: made when a
    /// compiled scene first shows a picture, kept while the scenes adopted
    /// show it.
    content_textures: HashMap<u64, Arc<ContentTexture>>,
    /// Each node's transform and the opacity animations give it (which
    /// multiplies its own), as last evaluated; kept up to date for the
    /// nodes animations touch.
    moved: Vec<(Affine, f32)>,
    /// Each animated item's values, as last evaluated.
    values: Vec<Values>,
    /// The clip records as drawn (animations move them).
    clips: Vec<ClipRecord>,
    /// Clip records changed since they were last uploaded.
    clips_dirty: Option<(usize, usize)>,
    /// Whether each item is shown and since when (for animations on the
    /// `shown` clock), by node id, kept from one adoption to the next.
    shown: Vec<Shown>,
    /// Counts adoptions, so a node id missing from the scenes before reads
    /// as not shown.
    adoptions: u64,
    /// Animated properties evaluated since [`DrawList::take_animated`].
    animated: u32,
    /// Staging buffers that changed records go to the GPU through, mapped
    /// again once the GPU has copied them: no buffer is made per frame
    /// however many records animations rewrite.
    staging: wgpu::util::StagingBelt,
}

/// Whether an item (by node id) is shown, and since when.
#[derive(Clone, Copy, Default)]
struct Shown {
    /// The adoption that last saw it, and whether it was shown (it and the
    /// items it is inside are visible) then.
    seen: u64,
    shown: bool,
    since: Option<Time>,
}

/// A content item's picture on the GPU.
struct ContentTexture {
    texture: wgpu::Texture,
    texels: Texels,
    group: wgpu::BindGroup,
}

/// A cached bind group.
struct Group {
    plane1: wgpu::Texture,
    group: wgpu::BindGroup,
    used: u64,
}

/// A canvas a scene shown inside others is drawn on.
struct Canvas {
    view: wgpu::TextureView,
    /// What draws of it sample.
    group: wgpu::BindGroup,
}

/// What a draw's record was last written for.
#[derive(Clone, Copy)]
struct DrawState {
    texels: Option<Texels>,
    opacity: f32,
    shift: [f32; 2],
    /// It is drawn: its items are shown, it has some opacity and a size.
    shown: bool,
    /// Its crop leaves nothing of its picture.
    empty: bool,
}

impl DrawList {
    /// `compiled`, adopted at `at`: its records uploaded.
    pub(crate) fn new(
        gpu: &Gpu,
        renderer: &Renderer,
        compiled: Arc<CompiledScenes>,
        at: Time,
    ) -> Self {
        let buffer = renderer.record_buffer(gpu, compiled.records.len(), compiled.clips.len());
        let mut list = Self {
            compiled: compiled.clone(),
            raster_size: None,
            records: Vec::new(),
            bytes: Vec::new(),
            dirty: None,
            generation: 0,
            buffer,
            state: Vec::new(),
            pictures: Vec::new(),
            by_source: Vec::new(),
            canvases: Vec::new(),
            drawn: Vec::new(),
            pass: 0,
            nodes: Vec::new(),
            looking: false,
            draws: 0,
            groups: HashMap::new(),
            tick: 0,
            contents: Vec::new(),
            content_textures: HashMap::new(),
            moved: Vec::new(),
            values: Vec::new(),
            clips: Vec::new(),
            clips_dirty: None,
            shown: Vec::new(),
            adoptions: 0,
            animated: 0,
            staging: wgpu::util::StagingBelt::new(gpu.device.clone(), STAGING_CHUNK),
        };
        list.adopt(gpu, renderer, compiled, at);
        list
    }

    /// Make textures of `compiled`'s content pictures, keeping those of
    /// pictures already adopted. A new picture the size of one no longer
    /// shown is drawn into its texture (text changing in a fixed box, a
    /// counter: no texture made per change); the rest are let go. Returns
    /// whether it made, wrote or let go of any.
    fn adopt_contents(
        &mut self,
        gpu: &Gpu,
        renderer: &Renderer,
        compiled: &CompiledScenes,
    ) -> bool {
        self.contents.clear();
        let mut old = std::mem::take(&mut self.content_textures);
        let mut textures = HashMap::with_capacity(compiled.contents.len());
        for content in &compiled.contents {
            if let Some(texture) = old.remove(&content.key) {
                textures.insert(content.key, texture);
            }
        }
        let touched = compiled
            .contents
            .iter()
            .any(|content| content.picture.is_some() && !textures.contains_key(&content.key))
            || !old.is_empty();
        let mut spare: Vec<Arc<ContentTexture>> = old.into_values().collect();
        self.contents = compiled
            .contents
            .iter()
            .map(|content| {
                let picture = content.picture.as_ref()?;
                if let Some(texture) = textures.get(&content.key) {
                    return Some(texture.clone());
                }
                let size = (picture.width, picture.height);
                let texture = match spare
                    .iter()
                    .position(|spare| (spare.texels.width, spare.texels.height) == size)
                {
                    Some(at) => {
                        let texture = spare.swap_remove(at);
                        write_picture(gpu, &texture.texture, picture);
                        texture
                    }
                    None => Arc::new(content_texture(gpu, renderer, picture)),
                };
                textures.insert(content.key, texture.clone());
                Some(texture)
            })
            .collect();
        self.content_textures = textures;
        touched
    }

    /// Content `content`'s picture (a content index), as a picture for
    /// [`crate::scene::layers`].
    pub(crate) fn content_frame(&self, content: usize) -> Option<GpuFrame> {
        let texture = self.contents.get(content)?.as_ref()?;
        Some(GpuFrame {
            width: texture.texels.width,
            height: texture.texels.height,
            pixels: crate::gpu::GpuPixels::Rgb(texture.texture.clone()),
        })
    }

    /// Draw `compiled` from now on (from `at`), keeping canvases (by scene
    /// id), cached bind groups and the buffer if it is big enough.
    pub(crate) fn adopt(
        &mut self,
        gpu: &Gpu,
        renderer: &Renderer,
        compiled: Arc<CompiledScenes>,
        at: Time,
    ) {
        let old = std::mem::replace(&mut self.compiled, compiled.clone());
        let mut canvases: Vec<Option<Canvas>> = compiled.list.iter().map(|_| None).collect();
        for (index, scene) in old.list.iter().enumerate() {
            if let Some(canvas) = self.canvases.get_mut(index).and_then(Option::take)
                && compiled.canvas == old.canvas
                && let Some(new) = compiled.list.iter().position(|s| s.id == scene.id)
            {
                canvases[new] = Some(canvas);
            }
        }
        self.canvases = canvases;
        self.drawn = vec![0; compiled.list.len()];
        self.records = compiled.records.clone();
        if self.buffer.capacity < self.records.len()
            || self.buffer.clip_capacity < compiled.clips.len()
        {
            self.buffer = renderer.record_buffer(gpu, self.records.len(), compiled.clips.len());
        }
        // Clips change with the scenes, and as animations move them.
        self.clips.clone_from(&compiled.clips);
        self.clips_dirty = None;
        if !compiled.clips.is_empty() {
            record_bytes(&compiled.clips, &mut self.bytes);
            gpu.queue.write_buffer(&self.buffer.clips, 0, &self.bytes);
        }
        self.moved = compiled
            .nodes
            .iter()
            .map(|node| (node.transform, 1.0))
            .collect();
        self.values = compiled
            .animations
            .items
            .iter()
            .map(|item| item.base)
            .collect();
        self.note_shown(&compiled, at);
        self.state = compiled
            .draws
            .iter()
            .map(|draw| DrawState {
                texels: None,
                opacity: draw.placed.opacity,
                shift: [0.0; 2],
                shown: !draw.hidden && !draw.placed.is_empty(),
                empty: false,
            })
            .collect();
        self.pictures = vec![None; compiled.sources.len()];
        self.by_source = vec![Vec::new(); compiled.sources.len()];
        self.nodes = vec![(1.0, true); compiled.nodes.len()];
        self.looking = false;
        // Writes to the queue wait for a submission, and textures let go of
        // wait for the device to be polled: a program that isn't drawn
        // (nothing records, streams or watches it) adopts scenes all the
        // same, so it does both here rather than keep them until it draws.
        if self.adopt_contents(gpu, renderer, &compiled) || !compiled.clips.is_empty() {
            gpu.queue.submit(std::iter::empty::<wgpu::CommandBuffer>());
            let _ = gpu.device.poll(wgpu::PollType::Poll);
        }
        let canvas = Texels {
            width: compiled.canvas.0,
            height: compiled.canvas.1,
            kind: TexelKind::Premultiplied,
        };
        for (index, draw) in compiled.draws.iter().enumerate() {
            match draw.sample {
                Sample::Source(source) if !draw.shadow => {
                    self.by_source[source as usize].push(index);
                }
                // Canvases are canvas-sized, so their records are known now.
                Sample::Scene(_) if !draw.shadow => {
                    self.state[index].texels = Some(canvas);
                    self.rewrite(index);
                }
                // So are content's pictures, which only change with the
                // scenes.
                Sample::Content(content) if !draw.shadow => {
                    if let Some(texture) = &self.contents[content as usize] {
                        self.state[index].texels = Some(texture.texels);
                        self.rewrite(index);
                    }
                }
                _ => {}
            }
        }
        self.dirty = (!self.records.is_empty()).then_some((0, self.records.len()));
        self.generation += 1;
    }

    pub(crate) fn compiled(&self) -> &Arc<CompiledScenes> {
        &self.compiled
    }

    /// Note which of `compiled`'s items are shown (they and the items they
    /// are inside are visible): one that wasn't shown in the scenes before
    /// (or wasn't in them) is shown from `at`.
    fn note_shown(&mut self, compiled: &CompiledScenes, at: Time) {
        let before = self.adoptions;
        self.adoptions += 1;
        let now = self.adoptions;
        let most = compiled.nodes.iter().map(|node| node.id).max();
        if let Some(most) = most
            && self.shown.len() <= most as usize
        {
            self.shown.resize(most as usize + 1, Shown::default());
        }
        // Parents come before their children, so their `nodes` entry is
        // already this adoption's.
        for node in &compiled.nodes {
            let parent = node
                .parent
                .is_none_or(|parent| self.shown[compiled.nodes[parent].id as usize].shown);
            let shown = parent && node.visible;
            let entry = &mut self.shown[node.id as usize];
            let was = entry.seen == before && entry.shown;
            if shown && (!was || entry.since.is_none()) {
                entry.since = Some(at);
            }
            entry.seen = now;
            entry.shown = shown;
        }
    }

    /// Evaluate the animations of scene `scene` (an index) and of the
    /// scenes it shows at `at`, the scene having come on air at `on_air`
    /// and the engine's clock having started at `epoch`, and rewrite the
    /// records (and clips) they change. Returns whether any changed. With
    /// nothing animated this is an empty loop.
    pub(crate) fn animate(&mut self, scene: usize, at: Time, on_air: Time, epoch: Time) -> bool {
        let generation = self.generation;
        let compiled = self.compiled.clone();
        let nested = &compiled.list[scene].nested;
        for index in std::iter::once(scene).chain(nested.iter().map(|&n| n as usize)) {
            let list = &compiled.list[index];
            if list.touched.is_empty() {
                continue;
            }
            let animations = &compiled.animations;
            for item in list.animated.clone() {
                let animated = &animations.items[item];
                let shown = animated
                    .shown
                    .and_then(|id| self.shown[id as usize].since)
                    .unwrap_or(at);
                let clocks = Clocks {
                    on_air,
                    shown,
                    epoch,
                };
                animations.evaluate(item, at, &clocks, &mut self.values[item]);
                self.animated += animated.tracks;
            }
            for touched in &animations.touched[list.touched.clone()] {
                let node = touched.node as usize;
                let moved = match touched.moved {
                    true => {
                        let (outer, fade) = match touched.parent {
                            0 => (touched.outer, 1.0),
                            parent => self.moved[parent as usize - 1],
                        };
                        let (place, own) = match touched.item.checked_sub(1) {
                            Some(item) => {
                                let animated = &animations.items[item as usize];
                                let values = &self.values[item as usize];
                                let place = match animated.moves() {
                                    true => animated.place(values),
                                    false => touched.place,
                                };
                                (place, animated.opacity(values))
                            }
                            None => (touched.place, 1.0),
                        };
                        let moved = (outer.after(place), fade * own);
                        self.moved[node] = moved;
                        moved
                    }
                    // Only its style animates: where it is stays as compiled.
                    false => self.moved[node],
                };
                for draw in touched.draws.clone() {
                    match touched.styled {
                        true => self.rewrite(draw as usize),
                        false => self.reposition(draw as usize, moved),
                    }
                }
                if touched.clip > 0 {
                    self.rewrite_clip(node);
                }
            }
        }
        self.generation != generation
    }

    /// Write the clip node `index` makes as animations have moved it.
    fn rewrite_clip(&mut self, index: usize) {
        let node = &self.compiled.nodes[index];
        let clip = node.clip as usize - 1;
        let old = self.clips[clip];
        let radius = node.animated.checked_sub(1).and_then(|item| {
            let item = item as usize;
            self.compiled.animations.items[item].radius(&self.values[item])
        });
        let own = LayerClip {
            transform: self.moved[index].0,
            size: node.size,
            radius: radius.unwrap_or([0.0; 4]),
        };
        let mut record = render::clip_record(&own, old[2][2] as u32);
        if radius.is_none() {
            record[3] = old[3];
        }
        if record != old {
            self.clips[clip] = record;
            self.clips_dirty = Some(match self.clips_dirty {
                Some((from, to)) => (from.min(clip), to.max(clip + 1)),
                None => (clip, clip + 1),
            });
            self.generation += 1;
        }
    }

    /// Give `layers`, the layers [`crate::scene::layers`] makes of scene
    /// `scene` (an index; one per shown item, in order), the looks its
    /// animations last gave its items (for Move transitions).
    pub(crate) fn animate_layers<'a, 'b>(
        &self,
        scene: usize,
        layers: impl Iterator<Item = &'b mut Layer<'a>>,
    ) where
        'a: 'b,
    {
        let compiled = &self.compiled;
        let list = &compiled.list[scene];
        if list.touched.is_empty() {
            return;
        }
        let first = list.nodes.start;
        let nodes = &compiled.nodes[list.nodes.clone()];
        let mut shown = vec![false; nodes.len()];
        let mut layers = layers;
        for (offset, node) in nodes.iter().enumerate() {
            let parent = node.parent.is_none_or(|parent| shown[parent - first]);
            shown[offset] = parent && node.visible;
            if !shown[offset] {
                continue;
            }
            let Some(layer) = layers.next() else {
                return;
            };
            if !node.touched {
                continue;
            }
            let (transform, opacity) = self.moved[first + offset];
            layer.transform = transform;
            layer.opacity *= opacity;
            if let Some(item) = node.animated.checked_sub(1) {
                let item = item as usize;
                let mut placed = layer.placed();
                compiled.animations.items[item].style(&self.values[item], &mut placed);
                layer.background = placed.background;
                layer.border = placed.border;
                layer.radius = placed.radius;
                layer.shadow = placed.shadow;
                let [left, top, right, bottom] = placed.crop.map(|v| v.round() as u32);
                layer.crop = isocast_protocol::Crop {
                    left,
                    top,
                    right,
                    bottom,
                };
            }
        }
    }

    /// The animated properties evaluated since this was last called.
    pub(crate) fn take_animated(&mut self) -> u32 {
        std::mem::take(&mut self.animated)
    }

    /// Counts changes to the records: a picture drawn at another
    /// generation would be drawn differently now.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// Show `picture` (with `group`, the bind group that samples it) for
    /// source `source` (an index) from now on, or nothing.
    pub(crate) fn set_picture(
        &mut self,
        source: u32,
        picture: Option<(&GpuFrame, wgpu::BindGroup)>,
    ) {
        let source = source as usize;
        let Some((frame, group)) = picture else {
            self.pictures[source] = None;
            return;
        };
        let texels = frame.texels();
        if self.pictures[source]
            .as_ref()
            .is_none_or(|(was, _)| *was != texels)
        {
            for i in 0..self.by_source[source].len() {
                let index = self.by_source[source][i];
                if self.state[index].texels != Some(texels) {
                    self.state[index].texels = Some(texels);
                    self.rewrite(index);
                }
            }
        }
        self.pictures[source] = Some((texels, group));
    }

    /// The bind group that samples `frame`, whose textures stay the same
    /// from frame to frame (see [`crate::gpu::FrameImporter::import_cached`]):
    /// made once, and kept while it is used.
    pub(crate) fn group(
        &mut self,
        gpu: &Gpu,
        renderer: &Renderer,
        frame: &GpuFrame,
    ) -> wgpu::BindGroup {
        let (plane0, plane1) = frame.planes();
        if let Some(cached) = self.groups.get_mut(plane0)
            && cached.plane1 == *plane1
        {
            cached.used = self.tick;
            return cached.group.clone();
        }
        let group = renderer.frame_group(gpu, frame);
        self.groups.insert(
            plane0.clone(),
            Group {
                plane1: plane1.clone(),
                group: group.clone(),
                used: self.tick,
            },
        );
        group
    }

    /// A new frame begins: bind groups unused for a while are let go.
    pub(crate) fn next_tick(&mut self) {
        self.tick += 1;
        if self.tick.is_multiple_of(GROUP_TTL) {
            let tick = self.tick;
            self.groups
                .retain(|_, group| tick - group.used <= GROUP_TTL);
        }
    }

    /// Apply the looks of items part of the way through a show or hide
    /// transition (by node id); with none, put back the looks of items
    /// that just finished one. Returns whether any record changed.
    pub(crate) fn apply_looks(&mut self, looks: &[(NodeId, ItemLook)]) -> bool {
        if looks.is_empty() && !self.looking {
            return false;
        }
        let generation = self.generation;
        let compiled = self.compiled.clone();
        let look = |id: NodeId| looks.iter().find(|(node, _)| *node == id).map(|(_, l)| *l);
        for (index, node) in compiled.nodes.iter().enumerate() {
            let look = look(node.id);
            let (opacity, shown) = node.parent.map_or((1.0, true), |parent| self.nodes[parent]);
            // As `crate::scene::layers_with` multiplies them.
            let opacity = opacity * node.opacity * look.map_or(1.0, |look| look.opacity);
            let shown = shown && (node.visible || look.is_some());
            self.nodes[index] = (opacity, shown);
        }
        for (index, draw) in compiled.draws.iter().enumerate() {
            let node = &compiled.nodes[draw.node];
            let (opacity, shown) = self.nodes[draw.node];
            let shift = look(node.id).map_or([0.0; 2], |look| {
                [look.shift[0] * node.size[0], look.shift[1] * node.size[1]]
            });
            let state = &mut self.state[index];
            state.shown = shown && !faded(opacity);
            if state.opacity != opacity || state.shift != shift {
                state.opacity = opacity;
                state.shift = shift;
                self.rewrite(index);
            }
        }
        self.looking = !looks.is_empty();
        self.generation != generation
    }

    /// Write the transform and opacity animations give draw `index`'s item
    /// (`moved`) into its record: all an item that only moves or fades
    /// changes, and what
    /// [`DrawList::rewrite`] would write there (the rest of the record
    /// doesn't depend on them).
    fn reposition(&mut self, index: usize, (transform, opacity): (Affine, f32)) {
        let opacity = (self.state[index].opacity * opacity).clamp(0.0, 1.0);
        let [a, b, tx, c, d, ty] = transform.0;
        let rows = [[a, b, tx, 0.0], [c, d, ty, 0.0]];
        let record = &mut self.records[index];
        if record[0] == rows[0] && record[1] == rows[1] && record[8][0] == opacity {
            return;
        }
        record[0] = rows[0];
        record[1] = rows[1];
        record[8][0] = opacity;
        self.dirty = Some(match self.dirty {
            Some((from, to)) => (from.min(index), to.max(index + 1)),
            None => (index, index + 1),
        });
        self.generation += 1;
    }

    /// Write draw `index`'s record for its picture, look and animations.
    fn rewrite(&mut self, index: usize) {
        let draw = &self.compiled.draws[index];
        let state = self.state[index];
        let mut placed = draw.placed;
        placed.opacity = state.opacity;
        placed.shift = state.shift;
        let node = &self.compiled.nodes[draw.node];
        if node.touched {
            let (transform, opacity) = self.moved[draw.node];
            placed.transform = transform;
            placed.opacity *= opacity;
            if let Some(item) = node.animated.checked_sub(1) {
                let item = item as usize;
                self.compiled.animations.items[item].style(&self.values[item], &mut placed);
            }
        }
        let mode = match (draw.shadow, state.texels) {
            (true, _) => Mode::Shadow,
            (false, Some(_)) => Mode::Picture,
            (false, None) => Mode::Fill,
        };
        let record = render::record(&placed, state.texels, mode, draw.target, draw.margin);
        self.state[index].empty = record.is_none();
        // With nothing left of its picture it can still be filled.
        let record = record.unwrap_or_else(|| {
            render::record(&placed, None, Mode::Fill, draw.target, draw.margin)
                .expect("fills always draw")
        });
        if self.records[index] == record {
            return;
        }
        self.records[index] = record;
        self.dirty = Some(match self.dirty {
            Some((from, to)) => (from.min(index), to.max(index + 1)),
            None => (index, index + 1),
        });
        self.generation += 1;
    }

    /// Copy the records (and clips) changed since the last upload to the
    /// GPU, at the start of `encoder` (whose work draws them; it must be
    /// submitted).
    pub(crate) fn upload(&mut self, encoder: &mut wgpu::CommandEncoder) {
        let clips = self.clips_dirty.take();
        let records = self.dirty.take();
        if clips.is_none() && records.is_none() {
            return;
        }
        if let Some((from, to)) = clips {
            record_bytes(&self.clips[from..to], &mut self.bytes);
            let offset = from as u64 * render::CLIP_BYTES;
            stage(
                &mut self.staging,
                encoder,
                &self.buffer.clips,
                offset,
                &self.bytes,
            );
        }
        if let Some((from, to)) = records {
            record_bytes(&self.records[from..to], &mut self.bytes);
            let offset = from as u64 * render::RECORD_BYTES;
            stage(
                &mut self.staging,
                encoder,
                &self.buffer.buffer,
                offset,
                &self.bytes,
            );
        }
        self.staging.finish_and_recall_on_submit(encoder);
    }

    /// Encode drawing scene `scene` (an index) into `target` on `clear`:
    /// the scenes it shows first, each on its canvas.
    pub(crate) fn encode(
        &mut self,
        gpu: &Gpu,
        renderer: &Renderer,
        encoder: &mut wgpu::CommandEncoder,
        scene: usize,
        target: &wgpu::TextureView,
        clear: wgpu::Color,
    ) {
        self.encode_nested(gpu, renderer, encoder, scene);
        self.draws += self.encode_one(renderer, encoder, scene, target, clear);
    }

    pub(crate) fn set_raster_size(&mut self, size: (u32, u32)) {
        if self.raster_size != Some(size) {
            self.canvases.iter_mut().for_each(|canvas| *canvas = None);
            self.raster_size = Some(size);
        }
    }

    /// Encode drawing the scenes scene `scene` shows on their canvases,
    /// starting a new pass: only canvases drawn since show in it.
    pub(crate) fn encode_nested(
        &mut self,
        gpu: &Gpu,
        renderer: &Renderer,
        encoder: &mut wgpu::CommandEncoder,
        scene: usize,
    ) {
        self.pass += 1;
        for i in 0..self.compiled.list[scene].nested.len() {
            let nested = self.compiled.list[scene].nested[i] as usize;
            self.make_canvas(gpu, renderer, nested);
            let view = &self.canvases[nested].as_ref().expect("made above").view;
            let draws = self.encode_one(renderer, encoder, nested, view, wgpu::Color::TRANSPARENT);
            self.draws += draws;
            self.drawn[nested] = self.pass;
        }
    }

    /// The canvas scene `scene` (an index) was drawn on in this pass, as a
    /// picture for [`crate::scene::layers`].
    pub(crate) fn canvas_frame(&self, scene: usize) -> Option<GpuFrame> {
        (self.drawn[scene] == self.pass).then_some(())?;
        let texture = self.canvases[scene].as_ref()?.view.texture().clone();
        let (width, height) = self.raster_size.unwrap_or(self.compiled.canvas);
        Some(GpuFrame {
            width,
            height,
            pixels: crate::gpu::GpuPixels::Premultiplied(texture),
        })
    }

    fn make_canvas(&mut self, gpu: &Gpu, renderer: &Renderer, scene: usize) {
        if self.canvases[scene].is_some() {
            return;
        }
        let texture = canvas_texture(
            gpu,
            renderer.format(),
            self.raster_size.unwrap_or(self.compiled.canvas),
        );
        let view = texture.create_view(&Default::default());
        let group = renderer.planes_group(gpu, &texture, &texture);
        self.canvases[scene] = Some(Canvas { view, group });
    }

    /// Encode one render pass drawing scene `scene` into `target`. Returns
    /// the draws made.
    fn encode_one(
        &self,
        renderer: &Renderer,
        encoder: &mut wgpu::CommandEncoder,
        scene: usize,
        target: &wgpu::TextureView,
        clear: wgpu::Color,
    ) -> u32 {
        let compiled = &self.compiled;
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("scene"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(clear),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        let range = compiled.list[scene].draws.clone();
        if range.is_empty() {
            return 0;
        }
        pass.set_bind_group(0, &self.buffer.group, &[]);
        let blank = renderer.blank_group();
        let mut pipeline = None;
        let mut bound: Option<&wgpu::BindGroup> = None;
        let mut count = 0;
        for index in range {
            let state = &self.state[index];
            if !state.shown {
                continue;
            }
            let draw = &compiled.draws[index];
            let (mode, group) = if draw.shadow {
                (Mode::Shadow, blank)
            } else {
                let picture = match draw.sample {
                    Sample::Source(source) => self.pictures[source as usize]
                        .as_ref()
                        .map(|(_, group)| group),
                    Sample::Scene(nested) => self.canvases[nested as usize]
                        .as_ref()
                        .filter(|_| self.drawn[nested as usize] == self.pass)
                        .map(|canvas| &canvas.group),
                    Sample::Content(content) => self.contents[content as usize]
                        .as_ref()
                        .map(|texture| &texture.group),
                    Sample::Nothing => None,
                };
                match picture {
                    // Its crop leaves nothing to draw.
                    Some(_) if state.empty => continue,
                    Some(group) => (Mode::Picture, group),
                    None if draw.placed.fills() => (Mode::Fill, blank),
                    None => continue,
                }
            };
            let blend = draw.placed.blend;
            if pipeline != Some((mode, blend)) {
                pass.set_pipeline(renderer.pipeline(mode, blend));
                pipeline = Some((mode, blend));
            }
            if bound.is_none_or(|bound| bound != group) {
                pass.set_bind_group(1, group, &[]);
                bound = Some(group);
            }
            let record = index as u32;
            pass.draw(0..4, record..record + 1);
            count += 1;
        }
        count
    }

    /// The draws encoded since this was last called.
    pub(crate) fn take_draws(&mut self) -> u32 {
        std::mem::take(&mut self.draws)
    }
}

/// `picture` (straight-alpha RGBA) as a texture, sampled as a captured
/// BGRA frame is (the same texels, so content draws as the same picture
/// shown by a source would).
fn content_texture(gpu: &Gpu, renderer: &Renderer, picture: &Picture) -> ContentTexture {
    let size = wgpu::Extent3d {
        width: picture.width,
        height: picture.height,
        depth_or_array_layers: 1,
    };
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("content"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    write_picture(gpu, &texture, picture);
    let group = renderer.planes_group(gpu, &texture, &texture);
    ContentTexture {
        texels: Texels {
            width: picture.width,
            height: picture.height,
            kind: TexelKind::Rgb,
        },
        texture,
        group,
    }
}

/// Write `picture` into `texture`, a content texture its size (queued
/// before the next frame's work, which is the first to draw it).
fn write_picture(gpu: &Gpu, texture: &wgpu::Texture, picture: &Picture) {
    gpu.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &picture.rgba,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(picture.width * 4),
            rows_per_image: Some(picture.height),
        },
        wgpu::Extent3d {
            width: picture.width,
            height: picture.height,
            depth_or_array_layers: 1,
        },
    );
}

/// Copy `bytes` into `target` at `offset` through `staging`, in `encoder`.
fn stage(
    staging: &mut wgpu::util::StagingBelt,
    encoder: &mut wgpu::CommandEncoder,
    target: &wgpu::Buffer,
    offset: u64,
    bytes: &[u8],
) {
    let Some(size) = wgpu::BufferSize::new(bytes.len() as u64) else {
        return;
    };
    staging
        .write_buffer(encoder, target, offset, size)
        .copy_from_slice(bytes);
}

/// Whether an `opacity` draws nothing (as [`render::Placed::is_empty`] has
/// it).
fn faded(opacity: f32) -> bool {
    opacity <= 0.0
}
