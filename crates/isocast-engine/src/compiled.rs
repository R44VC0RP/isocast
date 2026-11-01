//! Compiled scenes: the setup's scenes resolved, whenever they change, into
//! flat draw lists of ready-made draw records. The compositor uploads the
//! records once and then draws them frame after frame without walking the
//! item tree, parsing colours or allocating; only what changes per frame
//! (a source's picture size or format, a show or hide transition, an
//! animation) rewrites the records it touches.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use isocast_protocol::api::{ResolvedItem, ResolvedRect, ResolvedScene};
use isocast_protocol::{ItemTransition, Keyframes, Scene, SceneItem};

use crate::animate::{Animations, MOVES, Touched};
use crate::picture::Picture;
use crate::render::{
    self, Affine, ClipRecord, LayerClip, LayerShadow, MARGIN, Mode, Placed, Record,
};
use crate::scene::{clips, push_item_path, rect_of};

/// An item's stable id: its path ([`crate::scene::item_path`]) interned, so
/// it stays the
/// same across changes while the item keeps its key and its parents, and a
/// transition can follow it.
pub type NodeId = u32;

/// Item paths and their node ids, kept from one compile to the next.
#[derive(Default)]
pub struct NodeIds(HashMap<String, NodeId>);

impl NodeIds {
    pub fn id(&mut self, path: &str) -> NodeId {
        if let Some(id) = self.0.get(path) {
            return *id;
        }
        let id = self.0.len() as NodeId;
        self.0.insert(path.to_string(), id);
        id
    }
}

/// Every scene of a setup, compiled.
pub struct CompiledScenes {
    /// The canvas its records draw on.
    pub canvas: (u32, u32),
    /// What it was compiled from, laid out: every item has a rect (Move
    /// transitions match items by them).
    pub scenes: Arc<[Scene]>,
    /// One per scene, in the setup's order.
    pub list: Vec<CompiledScene>,
    /// The video sources items show, by source index.
    pub sources: Vec<Arc<str>>,
    /// Every item of every scene, parents before their children.
    pub nodes: Vec<Node>,
    /// Every draw, back to front within each scene.
    pub draws: Vec<Draw>,
    /// Each draw's record (one per draw), as drawn without a picture.
    pub records: Vec<Record>,
    /// The rounded rects of items that clip their children, which draws
    /// name in their records.
    pub clips: Vec<ClipRecord>,
    /// What layout took each source's picture size to be (by source
    /// index), so a picture of another size lays the scenes out again.
    pub sized: Vec<LaidOut>,
    /// The pictures content items show, by content index: each once,
    /// however many items show it.
    pub(crate) contents: Vec<CompiledContent>,
    /// Content items' paths ([`crate::scene::item_path`]) and their
    /// content index, for Move transitions' layers.
    pub content_paths: Vec<(String, u32)>,
    /// Items' keyframe animations.
    pub(crate) animations: Animations,
}

/// A picture content items show.
pub(crate) struct CompiledContent {
    /// What it is kept by: the same key, the same picture.
    pub key: u64,
    /// `None` if it couldn't be made (a missing file): items showing it
    /// draw their style only.
    pub picture: Option<Arc<Picture>>,
}

/// The picture an item's content shows, and its key (see
/// [`crate::content::Contents::picture`]).
pub(crate) type ContentOf<'a> =
    &'a mut dyn FnMut(&SceneItem) -> Option<(u64, Option<Arc<Picture>>)>;

/// What layout took a source's picture size to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaidOut {
    /// It didn't read it (no scene showing it has a layout).
    Unread,
    /// This size; `None` while it had none (a 16:9 shape).
    Read(Option<(u32, u32)>),
}

/// One scene, compiled.
pub struct CompiledScene {
    pub id: Arc<str>,
    /// The scenes its visible items show, and those they show, each once,
    /// the deepest first: each is drawn on a canvas of its own first.
    pub nested: Vec<u32>,
    /// Its draws, in `CompiledScenes::draws` (and `records`).
    pub draws: Range<usize>,
    /// Its items, in `CompiledScenes::nodes`.
    pub nodes: Range<usize>,
    /// The video sources its visible items show, and those of the scenes
    /// they show, each once (source indexes).
    pub sources: Vec<u32>,
    /// Its animated items, in `CompiledScenes::animations.items`, and the
    /// nodes whose records they change, in `animations.touched`.
    pub animated: Range<usize>,
    pub touched: Range<usize>,
}

/// An item of a scene.
pub struct Node {
    pub id: NodeId,
    /// Its parent, in `CompiledScenes::nodes`.
    pub parent: Option<usize>,
    pub visible: bool,
    /// Its own opacity (its children's multiply with it).
    pub opacity: f32,
    /// Its rect's width and height.
    pub size: [f32; 2],
    /// Maps its own pixels to the target's (a scene's canvas), with every
    /// transform of it and its parents.
    pub transform: Affine,
    pub show: Option<ItemTransition>,
    pub hide: Option<ItemTransition>,
    /// The video source it shows (a source index).
    pub source: Option<u32>,
    /// Where it is placed in its parent: its transform is its parent's
    /// after this.
    pub place: Affine,
    /// Its own draws (its shadow, its picture or fill), in
    /// `CompiledScenes::draws`.
    pub draws: Range<u32>,
    /// The clip it makes for its children (its index + 1 in
    /// `CompiledScenes::clips`), or 0.
    pub clip: u32,
    /// Its animations (their index + 1 in `animations.items`), or 0.
    pub animated: u32,
    /// An animation of its own or of an item it is inside changes its
    /// transform or opacity.
    pub moved: bool,
    /// Animations change its records (it is moved, or animates its style).
    pub touched: bool,
}

/// One draw: an item's shadow, or the item itself (its picture, or its
/// fill where it has none).
pub struct Draw {
    /// The item, in `CompiledScenes::nodes`.
    pub node: usize,
    pub shadow: bool,
    /// What the item's picture is (nothing for shadows).
    pub sample: Sample,
    /// Placed as compiled: its opacity is the product down the tree; no
    /// transition moves its picture.
    pub placed: Placed,
    pub margin: f32,
    /// The size of the target it is drawn on.
    pub target: (u32, u32),
    /// It is in (or is) an item that isn't visible, so it is drawn only
    /// while a show or hide transition shows it.
    pub hidden: bool,
}

/// What an item's picture is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sample {
    Nothing,
    /// A video source's frame (a source index).
    Source(u32),
    /// The canvas a scene is drawn on (a scene index).
    Scene(u32),
    /// A content item's picture (a content index).
    Content(u32),
}

impl CompiledScenes {
    /// The scene with `id`'s index, or the first scene's if `None`.
    pub fn scene_index(&self, id: Option<&str>) -> Option<usize> {
        match id {
            Some(id) => self.list.iter().position(|scene| &*scene.id == id),
            None => (!self.list.is_empty()).then_some(0),
        }
    }

    /// The index of the video source `id`, if an item shows it.
    pub fn source_index(&self, id: &str) -> Option<u32> {
        self.sources
            .iter()
            .position(|source| &**source == id)
            .map(|index| index as u32)
    }
}

/// Compile `scenes` (laid out: every item has a rect) for a program on a
/// `canvas`-sized canvas, content items showing the pictures `content_of`
/// gives and animated items playing the `keyframes` sets they name. A
/// snapshot gives `top`: draw the scene with that id through `root` onto a
/// target of its own size (scenes it shows stay on canvas-sized canvases).
pub(crate) fn compile(
    scenes: Arc<[Scene]>,
    canvas: (u32, u32),
    ids: &mut NodeIds,
    top: Option<(&str, Affine, (u32, u32))>,
    keyframes: &[Keyframes],
    content_of: ContentOf<'_>,
) -> CompiledScenes {
    let mut out = CompiledScenes {
        canvas,
        scenes: scenes.clone(),
        list: Vec::with_capacity(scenes.len()),
        sources: Vec::new(),
        nodes: Vec::new(),
        draws: Vec::new(),
        records: Vec::new(),
        clips: Vec::new(),
        sized: Vec::new(),
        contents: Vec::new(),
        content_paths: Vec::new(),
        animations: Animations::default(),
    };
    let sets: HashMap<&str, &Keyframes> =
        keyframes.iter().map(|set| (set.id.as_str(), set)).collect();
    let mut sources: HashMap<String, u32> = HashMap::new();
    let mut contents: HashMap<u64, u32> = HashMap::new();
    // Items' paths are built in one string as the walk goes.
    let mut path = String::new();
    for scene in scenes.iter() {
        let (root, target) = match top {
            Some((id, root, target)) if id == scene.id => (root, target),
            _ => (Affine::IDENTITY, canvas),
        };
        let nested = scene
            .shown_scenes(&scenes)
            .into_iter()
            .filter(|shown| shown.id != scene.id)
            .filter_map(|shown| scene_position(&scenes, &shown.id))
            .collect();
        let scene_sources = scene
            .video_sources(&scenes)
            .into_iter()
            .map(|id| intern(&mut sources, &mut out.sources, id))
            .collect();
        let (draws, nodes) = (out.draws.len(), out.nodes.len());
        let animated = out.animations.items.len();
        let mut walk = Walk {
            scenes: &scenes,
            ids,
            sources: &mut sources,
            contents: &mut contents,
            content_of: &mut *content_of,
            sets: &sets,
            out: &mut out,
            target,
        };
        path.clear();
        path.push_str(&scene.id);
        let top = Parent {
            node: None,
            transform: root,
            opacity: 1.0,
            hidden: false,
            clip: 0,
            moved: false,
        };
        walk.items(&scene.items, &mut path, top);
        let touched = out.animations.touched.len();
        for (index, node) in out.nodes.iter().enumerate().skip(nodes) {
            if !node.touched {
                continue;
            }
            let (parent, outer) = match node.parent {
                Some(parent) if out.nodes[parent].touched => (parent as u32 + 1, root),
                Some(parent) => (0, out.nodes[parent].transform),
                None => (0, root),
            };
            let styled = node
                .animated
                .checked_sub(1)
                .is_some_and(|item| out.animations.items[item as usize].props & !MOVES != 0);
            out.animations.touched.push(Touched {
                node: index as u32,
                parent,
                item: node.animated,
                draws: node.draws.clone(),
                clip: node.clip,
                moved: node.moved,
                styled,
                outer,
                place: node.place,
            });
        }
        out.list.push(CompiledScene {
            id: scene.id.as_str().into(),
            nested,
            draws: draws..out.draws.len(),
            nodes: nodes..out.nodes.len(),
            sources: scene_sources,
            animated: animated..out.animations.items.len(),
            touched: touched..out.animations.touched.len(),
        });
    }
    out.sized = vec![LaidOut::Unread; out.sources.len()];
    out
}

/// Where every item of `compiled`'s scenes is drawn, for `get` with
/// `resolved`.
pub fn resolved(compiled: &CompiledScenes) -> Vec<ResolvedScene> {
    fn walk(
        items: &[SceneItem],
        nodes: &mut std::slice::Iter<'_, Node>,
        shown: bool,
    ) -> Vec<ResolvedItem> {
        items
            .iter()
            .map(|item| {
                let node = nodes.next().expect("a node per item");
                let rect = rect_of(item);
                let [a, b, tx, c, d, ty] = node.transform.0;
                let corners = [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)]
                    .map(|(u, v)| (u * node.size[0], v * node.size[1]))
                    .map(|(x, y)| (a * x + b * y + tx, c * x + d * y + ty));
                let (mut left, mut top) = (f32::INFINITY, f32::INFINITY);
                let (mut right, mut bottom) = (f32::NEG_INFINITY, f32::NEG_INFINITY);
                for (x, y) in corners {
                    (left, top) = (left.min(x), top.min(y));
                    (right, bottom) = (right.max(x), bottom.max(y));
                }
                let shown = shown && item.visible;
                ResolvedItem {
                    id: item.key().map(str::to_string),
                    rect: ResolvedRect {
                        x: rect.x as f32,
                        y: rect.y as f32,
                        width: rect.width as f32,
                        height: rect.height as f32,
                    },
                    canvas: ResolvedRect {
                        x: left,
                        y: top,
                        width: right - left,
                        height: bottom - top,
                    },
                    transform: node.transform.0,
                    visible: shown,
                    children: walk(&item.children, nodes, shown),
                }
            })
            .collect()
    }
    compiled
        .list
        .iter()
        .zip(compiled.scenes.iter())
        .map(|(list, scene)| ResolvedScene {
            id: scene.id.clone(),
            items: walk(
                &scene.items,
                &mut compiled.nodes[list.nodes.clone()].iter(),
                true,
            ),
        })
        .collect()
}

/// Where scene `id` is in `scenes`.
fn scene_position(scenes: &[Scene], id: &str) -> Option<u32> {
    scenes
        .iter()
        .position(|scene| scene.id == id)
        .map(|index| index as u32)
}

/// The source index of `id`, given one if it has none yet.
fn intern(by_id: &mut HashMap<String, u32>, sources: &mut Vec<Arc<str>>, id: &str) -> u32 {
    if let Some(index) = by_id.get(id) {
        return *index;
    }
    let index = sources.len() as u32;
    sources.push(id.into());
    by_id.insert(id.to_string(), index);
    index
}

/// Compiling one scene's items.
struct Walk<'a, 'c> {
    scenes: &'a [Scene],
    ids: &'a mut NodeIds,
    sources: &'a mut HashMap<String, u32>,
    /// Content indexes by their pictures' keys.
    contents: &'a mut HashMap<u64, u32>,
    content_of: ContentOf<'c>,
    /// The setup's keyframe sets, by id.
    sets: &'a HashMap<&'a str, &'a Keyframes>,
    out: &'a mut CompiledScenes,
    target: (u32, u32),
}

/// What items are placed in: their parent (a node; none at the top), its
/// transform, opacity and clip, whether it (or an item it is inside) is
/// hidden, and whether an animation moves it.
#[derive(Clone, Copy)]
struct Parent {
    node: Option<usize>,
    transform: Affine,
    opacity: f32,
    hidden: bool,
    /// Index + 1, 0 for none.
    clip: u32,
    moved: bool,
}

impl Walk<'_, '_> {
    /// Every item of `items` (visible or not) and their children, placed
    /// within `parent` by its `transform`, faded by its `opacity`, drawn
    /// inside its `clip`: the same arithmetic, in the same order, as
    /// [`crate::scene::layers`], so the pictures are the same to the bit.
    fn items(&mut self, items: &[SceneItem], path: &mut String, parent: Parent) {
        let Parent {
            transform,
            opacity,
            hidden,
            clip,
            ..
        } = parent;
        for (index, item) in items.iter().enumerate() {
            let parent_path = path.len();
            push_item_path(path, item, index);
            let style = &item.style;
            let rect = rect_of(item);
            let (width, height) = (rect.width as f32, rect.height as f32);
            let t = style.transform;
            // Moved, rotated, scaled and flipped about its centre, inside its
            // parent.
            let place = Affine::translate(
                rect.x as f32 + width / 2.0 + t.translate[0],
                rect.y as f32 + height / 2.0 + t.translate[1],
            )
            .after(Affine::rotate(t.rotate))
            .after(Affine::scale(t.scale))
            .after(Affine::flip(t.flip_horizontal, t.flip_vertical))
            .after(Affine::translate(-width / 2.0, -height / 2.0));
            let transform = transform.after(place);
            let opacity = opacity * style.opacity;
            let hidden = hidden || !item.visible;
            let source = item
                .source
                .as_deref()
                .map(|id| intern(self.sources, &mut self.out.sources, id));
            let content = match (source, &item.scene, &item.content) {
                (None, None, Some(_)) => self.content(item, path),
                _ => None,
            };
            let sample = match (source, &item.scene, content) {
                (Some(source), ..) => Sample::Source(source),
                (None, Some(scene), _) => {
                    scene_position(self.scenes, scene).map_or(Sample::Nothing, Sample::Scene)
                }
                (None, None, Some(content)) => Sample::Content(content),
                (None, None, None) => Sample::Nothing,
            };
            let rgba = |color: &isocast_protocol::Color| color.rgba().unwrap_or([0.0; 4]);
            let crop = item.crop;
            let mut placed = Placed {
                crop: [crop.left, crop.top, crop.right, crop.bottom].map(|v| v as f32),
                fit: item.fit,
                align: item.align,
                scale_filter: item.scale_filter,
                size: [width, height],
                transform,
                opacity,
                blend: item.blend,
                background: style.background.as_ref().map(rgba),
                radius: style.radius.corners(),
                border: style
                    .border
                    .as_ref()
                    .filter(|border| border.width > 0.0)
                    .map(|border| (border.width, rgba(&border.color))),
                shadow: style.shadow.as_ref().map(|shadow| LayerShadow {
                    offset: [shadow.x, shadow.y],
                    blur: shadow.blur,
                    color: rgba(&shadow.color),
                }),
                shift: [0.0; 2],
                clip,
            };
            let node = self.out.nodes.len();
            let id = self.ids.id(path);
            let bounds = [rect.x as f32, rect.y as f32, width, height];
            let animated = self.out.animations.add(item, id, bounds, self.sets);
            let props = animated.map_or(0, |index| self.out.animations.items[index as usize].props);
            let moved = parent.moved || props & MOVES != 0;
            let mut shadow_margin = placed.shadow_margin();
            if let Some(index) = animated {
                // Animated backgrounds and borders need a fill draw, and
                // animated shadows a shadow draw as big as they get.
                if self.out.animations.items[index as usize].fills() && placed.background.is_none()
                {
                    placed.background = Some([0.0; 4]);
                }
                if let Some(margin) = self.out.animations.shadow_margin(index as usize) {
                    placed.shadow.get_or_insert(LayerShadow {
                        offset: [0.0; 2],
                        blur: 0.0,
                        color: [0.0; 4],
                    });
                    shadow_margin = Some(margin.max(shadow_margin.unwrap_or(0.0)));
                }
            }
            self.out.nodes.push(Node {
                id,
                parent: parent.node,
                visible: item.visible,
                opacity: style.opacity,
                size: [width, height],
                transform,
                show: item.show_transition,
                hide: item.hide_transition,
                source,
                place,
                draws: 0..0,
                clip: 0,
                animated: animated.map_or(0, |index| index + 1),
                moved,
                touched: moved || props != 0,
            });
            let first_draw = self.out.draws.len() as u32;
            // An item without a size never draws (it has none until the
            // setup changes, which compiles again).
            if width > 0.0 && height > 0.0 {
                if let Some(margin) = shadow_margin {
                    self.draw(
                        node,
                        true,
                        Sample::Nothing,
                        placed,
                        Mode::Shadow,
                        margin,
                        hidden,
                    );
                }
                if sample != Sample::Nothing || placed.fills() {
                    self.draw(node, false, sample, placed, Mode::Fill, MARGIN, hidden);
                }
            }
            self.out.nodes[node].draws = first_draw..self.out.draws.len() as u32;
            // Its children are drawn inside its rounded rect if it clips.
            let inner = match clips(item) {
                true => {
                    let own = LayerClip {
                        transform,
                        size: [width, height],
                        radius: style.radius.corners(),
                    };
                    self.out.clips.push(render::clip_record(&own, clip));
                    self.out.nodes[node].clip = self.out.clips.len() as u32;
                    self.out.clips.len() as u32
                }
                false => clip,
            };
            let within = Parent {
                node: Some(node),
                transform,
                opacity,
                hidden,
                clip: inner,
                moved,
            };
            self.items(&item.children, path, within);
            path.truncate(parent_path);
        }
    }

    /// The content index of `item`'s picture (at `path`), given one if
    /// no item showed it yet; `None` if it has none (no size).
    fn content(&mut self, item: &SceneItem, path: &str) -> Option<u32> {
        let (key, picture) = (self.content_of)(item)?;
        let index = match self.contents.get(&key) {
            Some(index) => *index,
            None => {
                let index = self.out.contents.len() as u32;
                self.out.contents.push(CompiledContent { key, picture });
                self.contents.insert(key, index);
                index
            }
        };
        self.out.content_paths.push((path.to_string(), index));
        Some(index)
    }

    #[allow(clippy::too_many_arguments)]
    fn draw(
        &mut self,
        node: usize,
        shadow: bool,
        sample: Sample,
        placed: Placed,
        mode: Mode,
        margin: f32,
        hidden: bool,
    ) {
        let record = render::record(&placed, None, mode, self.target, margin)
            .expect("only pictures can be cropped to nothing");
        self.out.records.push(record);
        self.out.draws.push(Draw {
            node,
            shadow,
            sample,
            placed,
            margin,
            target: self.target,
            hidden,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use isocast_protocol::Setup;

    fn setup() -> Setup {
        Setup::from_toml(
            r##"
version = 1
name = "t"
[canvas]
width = 1920
height = 1080
fps = 60
[[sources]]
id = "a"
kind = "color"
color = "#ff0000"
[[sources]]
id = "b"
kind = "color"
color = "#00ff00"
[[scenes]]
id = "inner"
items = [{ source = "b", rect = { x = 0, y = 0, width = 100, height = 100 } }]
[[scenes]]
id = "main"
items = [
  { source = "a", rect = { x = 0, y = 0, width = 1920, height = 1080 }, style = { shadow = { blur = 4, color = "#000" } } },
  { id = "group", rect = { x = 10, y = 10, width = 200, height = 200 }, visible = false, style = { opacity = 0.5 }, children = [
    { scene = "inner", rect = { x = 0, y = 0, width = 100, height = 100 } },
  ] },
  { id = "nothing", rect = { x = 0, y = 0, width = 10, height = 10 } },
]
[recording]
folder = "/tmp"
[recording.program]
encoder = { codec = "hevc", quality = 70 }
[recording.iso]
encoder = { codec = "hevc", quality = 70 }
"##,
        )
        .unwrap()
    }

    #[test]
    fn scenes_compile_to_flat_draw_lists() {
        let setup = setup();
        let mut ids = NodeIds::default();
        let none = &mut |_: &SceneItem| None;
        let compiled = compile(
            setup.scenes.clone().into(),
            (1920, 1080),
            &mut ids,
            None,
            &[],
            none,
        );
        assert_eq!(compiled.list.len(), 2);
        assert_eq!(compiled.draws.len(), compiled.records.len());
        let main = &compiled.list[1];
        // The hidden group's nested scene isn't drawn on a canvas, nor its
        // source counted on air.
        assert!(main.nested.is_empty());
        assert_eq!(
            main.sources
                .iter()
                .map(|s| &*compiled.sources[*s as usize])
                .collect::<Vec<_>>(),
            ["a"]
        );
        // a's shadow, a, the group's child (a box without a fill draws
        // nothing itself).
        let draws = &compiled.draws[main.draws.clone()];
        let kinds: Vec<(bool, Sample, bool)> = draws
            .iter()
            .map(|d| (d.shadow, d.sample, d.hidden))
            .collect();
        let a = compiled.source_index("a").unwrap();
        assert_eq!(
            kinds,
            [
                (true, Sample::Nothing, false),
                (false, Sample::Source(a), false),
                (false, Sample::Scene(0), true),
            ]
        );
        // Opacity multiplies down the tree.
        assert_eq!(draws[2].placed.opacity, 0.5);
        // Node ids stay with item paths from one compile to the next.
        let again = compile(
            setup.scenes.clone().into(),
            (1920, 1080),
            &mut ids,
            None,
            &[],
            none,
        );
        let id_of = |c: &CompiledScenes| c.nodes.iter().map(|n| n.id).collect::<Vec<_>>();
        assert_eq!(id_of(&compiled), id_of(&again));
    }
}
