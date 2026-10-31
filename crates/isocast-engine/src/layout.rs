//! Layout (scenes as code, 5.1): items with a `layout` placed by CSS
//! flexbox, grid, block and absolute positioning, using taffy, on the scene
//! thread whenever the scenes or the sizes of the pictures they show
//! change; never per frame. Every item comes out with a `rect` in its
//! parent's pixels, so compiling, drawing and Move transitions work on
//! rects as before.
//!
//! A scene without any `layout` isn't laid out at all, and items with a
//! `rect` keep it exactly (it is `position = "absolute"` with those
//! numbers), so setups without layout compile to the same draw lists as
//! before. Pictures are replaced elements, as CSS's `<img>`: their natural
//! size and shape are a source's picture size (or the canvas, for a
//! scene), and stretching one in a flex line keeps its shape; but they
//! never insist on their size (their least size is 0, CSS's `min-width: 0`
//! and `min-height: 0`, unless their layout gives one), since `fit`
//! decides how a picture fills whatever box it gets.

use isocast_protocol::layout::{
    AlignItems, FlexDirection, FlexWrap, GridAutoFlow, GridPlace, GridRepeat, GridTrack,
    JustifyContent, LayoutDisplay, Length, LengthValue, Overflow, Position, TrackMax, TrackMin,
    TrackSize,
};
use isocast_protocol::{Content, Fit, Layout, Rect, Scene, SceneItem, Setup, SourceKind};
use taffy::prelude::{
    AvailableSpace, Dimension, LengthPercentage, LengthPercentageAuto, NodeId as TaffyNode, Size,
    Style, TaffyTree,
};
use taffy::{
    GridPlacement, GridTemplateComponent, GridTemplateRepetition, MaxTrackSizingFunction,
    MinTrackSizingFunction, RepetitionCount, TrackSizingFunction,
};

use isocast_protocol::api::ResolvedScene;

use crate::compiled::{NodeIds, compile, resolved};
use crate::content::{Contents, Sizer};
use crate::data::DataStore;
use crate::filter::output_size;
use crate::scene::push_item_path;

/// What a leaf of the layout tree shows, for its natural size.
#[derive(Debug, Clone, Copy)]
struct Picture {
    /// Its natural size, if known (else it takes a 16:9 shape).
    natural: Option<(f32, f32)>,
}

/// A leaf of the layout tree: a picture, or text measured at the width
/// it is offered (an index for [`ContentSizes`]).
#[derive(Debug, Clone, Copy)]
enum Leaf {
    Picture(Picture),
    Text(u32),
}

/// What a content item is to layout.
pub(crate) enum ContentLeaf {
    /// A picture of this natural size (an image), if known.
    Picture(Option<(u32, u32)>),
    /// Text, to be measured by [`ContentSizes::text_size`] with this index.
    Text(u32),
}

/// The sizes of content items' text and images, for layout.
pub(crate) trait ContentSizes {
    /// What `content` is to layout.
    fn leaf(&mut self, content: &Content) -> ContentLeaf;
    /// The size text `text` (from [`ContentSizes::leaf`]) takes: its own
    /// (`None`), or wrapped to fit a box `width` wide.
    fn text_size(&mut self, text: u32, width: Option<f32>) -> (f32, f32);
    /// The narrowest box text `text` fits: its widest word.
    fn text_least_width(&mut self, text: u32) -> f32;
}

/// Content of unknown size: pictures of a 16:9 shape.
#[cfg(test)]
pub(crate) struct NoContent;

#[cfg(test)]
impl ContentSizes for NoContent {
    fn leaf(&mut self, _content: &Content) -> ContentLeaf {
        ContentLeaf::Picture(None)
    }

    fn text_size(&mut self, _text: u32, _width: Option<f32>) -> (f32, f32) {
        (0.0, 0.0)
    }

    fn text_least_width(&mut self, _text: u32) -> f32 {
        0.0
    }
}

/// Lays out scenes, keeping its tree's memory and each item's last place
/// from one layout to the next.
pub(crate) struct Layouter {
    tree: TaffyTree<Leaf>,
    /// The tree's nodes for the scene being laid out, its items in order
    /// (parents before their children).
    nodes: Vec<TaffyNode>,
    /// Where each item that takes part in layout was last put (by node
    /// id, which are small numbers): an item hidden with a hide transition
    /// leaves the flow at once but finishes its transition there.
    last: Vec<Option<Rect>>,
}

impl Default for Layouter {
    fn default() -> Self {
        Self {
            tree: TaffyTree::new(),
            nodes: Vec::new(),
            last: Vec::new(),
        }
    }
}

/// A source's picture size by its id, if known.
pub(crate) type SizeOf<'a> = &'a dyn Fn(&str) -> Option<(u32, u32)>;

impl Layouter {
    /// Give every item of `scenes` a rect: those with a `layout` where
    /// layout puts them on a `canvas`-sized canvas, with sources' natural
    /// sizes from `size_of` and content's from `content`. Adds the sources
    /// whose sizes it read, and the sizes, to `used` (each once).
    pub(crate) fn resolve(
        &mut self,
        scenes: &mut [Scene],
        canvas: (u32, u32),
        size_of: SizeOf<'_>,
        content: &mut dyn ContentSizes,
        ids: &mut NodeIds,
        used: &mut Vec<(String, Option<(u32, u32)>)>,
    ) {
        for scene in scenes.iter_mut() {
            native_rects(&mut scene.items, canvas, size_of, used);
            if !uses_layout(scene) {
                continue;
            }
            self.scene(scene, canvas, size_of, content, used);
            self.place(scene, ids);
        }
    }

    /// Lay out `scene`: the tree built and computed, its nodes in
    /// `self.nodes`.
    fn scene(
        &mut self,
        scene: &Scene,
        canvas: (u32, u32),
        size_of: SizeOf<'_>,
        content: &mut dyn ContentSizes,
        used: &mut Vec<(String, Option<(u32, u32)>)>,
    ) {
        self.tree.clear();
        self.nodes.clear();
        let mut root_style = Style::DEFAULT;
        container(scene.layout.as_ref(), &mut root_style);
        root_style.size = Size {
            width: Dimension::length(canvas.0 as f32),
            height: Dimension::length(canvas.1 as f32),
        };
        let mut sizes = Sizes {
            canvas,
            size_of,
            content,
            used,
        };
        let children = self.items(&scene.items, &mut sizes);
        let root = self
            .tree
            .new_with_children(root_style, &children)
            .expect("taffy keeps every node");
        let available = Size {
            width: AvailableSpace::Definite(canvas.0 as f32),
            height: AvailableSpace::Definite(canvas.1 as f32),
        };
        let content = sizes.content;
        self.tree
            .compute_layout_with_measure(root, available, |known, available, _, leaf, _| match leaf
                .copied()
            {
                Some(Leaf::Text(text)) => measure_text(known, available, text, content),
                Some(Leaf::Picture(picture)) => measure(known, available, Some(picture)),
                None => measure(known, available, None),
            })
            .expect("taffy lays out every tree");
    }

    /// Add `items` (and their children) to the tree, in order; returns
    /// their nodes.
    fn items(&mut self, items: &[SceneItem], sizes: &mut Sizes<'_, '_>) -> Vec<TaffyNode> {
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let index = self.nodes.len();
            // Its place, filled in once its children are made.
            self.nodes.push(TaffyNode::new(0));
            let leaf = match (&item.source, &item.scene, &item.content) {
                (Some(source), ..) => Some(Leaf::Picture(Picture {
                    natural: sizes.source(source).map(|(w, h)| (w as f32, h as f32)),
                })),
                (None, Some(_), _) => Some(Leaf::Picture(Picture {
                    natural: Some((sizes.canvas.0 as f32, sizes.canvas.1 as f32)),
                })),
                (None, None, Some(content)) => Some(match sizes.content.leaf(content) {
                    ContentLeaf::Picture(natural) => Leaf::Picture(Picture {
                        natural: natural.map(|(w, h)| (w as f32, h as f32)),
                    }),
                    ContentLeaf::Text(text) => Leaf::Text(text),
                }),
                (None, None, None) => None,
            };
            let children = self.items(&item.children, sizes);
            // Text with children is a container to taffy, which then
            // doesn't measure it: it takes its own size, as a picture.
            let leaf = match leaf {
                Some(Leaf::Text(text)) if !children.is_empty() => {
                    let (width, height) = sizes.content.text_size(text, None);
                    Some(Leaf::Picture(Picture {
                        natural: (width > 0.0 && height > 0.0).then_some((width, height)),
                    }))
                }
                leaf => leaf,
            };
            let picture = match leaf {
                Some(Leaf::Picture(picture)) => Some(picture),
                _ => None,
            };
            let style = item_style(item, picture, !children.is_empty());
            let node = match (leaf, children.is_empty()) {
                (Some(leaf), true) => self.tree.new_leaf_with_context(style, leaf),
                _ => self.tree.new_with_children(style, &children),
            }
            .expect("taffy keeps every node");
            self.nodes[index] = node;
            out.push(node);
        }
        out
    }

    /// Write the rects layout gave into `scene`'s items with a `layout`.
    fn place(&mut self, scene: &mut Scene, ids: &mut NodeIds) {
        let mut next = 0;
        let mut path = scene.id.clone();
        self.place_items(&mut scene.items, &mut path, true, &mut next, ids);
    }

    fn place_items(
        &mut self,
        items: &mut [SceneItem],
        path: &mut String,
        shown: bool,
        next: &mut usize,
        ids: &mut NodeIds,
    ) {
        for (index, item) in items.iter_mut().enumerate() {
            let node = self.nodes[*next];
            *next += 1;
            let parent_path = path.len();
            push_item_path(path, item, index);
            let shown = shown && item.visible;
            if item.layout.is_some() {
                let id = ids.id(path);
                if shown {
                    let layout = self.tree.layout(node).expect("laid out above");
                    let rect = Rect {
                        x: layout.location.x.round() as i32,
                        y: layout.location.y.round() as i32,
                        width: layout.size.width.round().max(0.0) as u32,
                        height: layout.size.height.round().max(0.0) as u32,
                    };
                    let at = id as usize;
                    if self.last.len() <= at {
                        self.last.resize(at + 1, None);
                    }
                    self.last[at] = Some(rect);
                    item.rect = Some(rect);
                } else {
                    // Out of the flow: where it last was, if it ever was.
                    item.rect = Some(self.last.get(id as usize).copied().flatten().unwrap_or(
                        Rect {
                            x: 0,
                            y: 0,
                            width: 0,
                            height: 0,
                        },
                    ));
                }
            }
            self.place_items(&mut item.children, path, shown, next, ids);
            path.truncate(parent_path);
        }
    }
}

/// No-bounds source items have a native (cropped) box. CSS layout retains
/// its explicitly authored sizing rules; transforms supply the item scale.
fn native_rects(
    items: &mut [SceneItem],
    canvas: (u32, u32),
    size_of: SizeOf<'_>,
    used: &mut Vec<(String, Option<(u32, u32)>)>,
) {
    for item in items {
        if item.fit == Fit::None && item.layout.is_none() {
            let size = if let Some(id) = &item.source {
                let size = size_of(id);
                if !used.iter().any(|(key, _)| key == id) {
                    used.push((id.clone(), size));
                }
                size
            } else {
                item.scene.as_ref().map(|_| canvas)
            };
            if let (Some(rect), Some((w, h))) = (&mut item.rect, size) {
                rect.width = w
                    .saturating_sub(item.crop.left)
                    .saturating_sub(item.crop.right)
                    .max(1);
                rect.height = h
                    .saturating_sub(item.crop.top)
                    .saturating_sub(item.crop.bottom)
                    .max(1);
            }
        }
        native_rects(&mut item.children, canvas, size_of, used);
    }
}

/// Normalize new API items and source-size edits without changing authored
/// bounds. An omitted legacy fit follows only if its unchanged box was the
/// previous native size. Explicit stretch never migrates.
pub fn follow_source_sizes(
    previous: &Setup,
    setup: &mut Setup,
    live: &[(String, (u32, u32))],
    new_items: bool,
) {
    fn walk(
        old: &[SceneItem],
        items: &mut [SceneItem],
        previous: &Setup,
        setup: &Setup,
        live: &[(String, (u32, u32))],
        new_items: bool,
    ) {
        for item in items {
            let before = old.iter().find(|old| old.key() == item.key());
            if new_items
                && before.is_none()
                && item.fit.is_legacy()
                && item.layout.is_none()
                && (item.source.is_some() || item.scene.is_some())
            {
                item.fit = Fit::None;
            }
            if let Some(id) = &item.source {
                let a = previous.source(id);
                let b = setup.source(id);
                if let (Some(a), Some(b), Some(before), Some(rect)) = (a, b, before, item.rect)
                    && a.kind != b.kind
                    && item.fit.is_legacy()
                    && before.fit.is_legacy()
                    && before.rect == item.rect
                    && item.layout.is_none()
                {
                    let native =
                        settings_size(&a.kind, (previous.canvas.width, previous.canvas.height))
                            .or_else(|| {
                                live.iter()
                                    .find(|(key, _)| key == id)
                                    .map(|(_, size)| *size)
                            });
                    if native == Some((rect.width, rect.height)) && item.crop == Default::default()
                    {
                        item.fit = Fit::None;
                    }
                }
            }
            walk(
                before.map_or(&[], |i| i.children.as_slice()),
                &mut item.children,
                previous,
                setup,
                live,
                new_items,
            );
        }
    }
    let sizes = setup.clone();
    for scene in &mut setup.scenes {
        let old = previous.scenes.iter().find(|s| s.id == scene.id);
        walk(
            old.map_or(&[], |s| s.items.as_slice()),
            &mut scene.items,
            previous,
            &sizes,
            live,
            new_items,
        );
        native_rects(
            &mut scene.items,
            (sizes.canvas.width, sizes.canvas.height),
            &|id| {
                let source = sizes.source(id)?;
                settings_size(&source.kind, (sizes.canvas.width, sizes.canvas.height)).or_else(
                    || {
                        live.iter()
                            .find(|(key, _)| key == id)
                            .map(|(_, size)| *size)
                    },
                )
            },
            &mut Vec::new(),
        );
    }
}

/// Whether `scene` or any of its items has a layout.
pub(crate) fn uses_layout(scene: &Scene) -> bool {
    fn any(items: &[SceneItem]) -> bool {
        items
            .iter()
            .any(|item| item.layout.is_some() || any(&item.children))
    }
    scene.layout.is_some() || any(&scene.items)
}

/// The natural sizes of the pictures items show.
struct Sizes<'s, 'u> {
    canvas: (u32, u32),
    size_of: SizeOf<'s>,
    content: &'u mut dyn ContentSizes,
    used: &'u mut Vec<(String, Option<(u32, u32)>)>,
}

impl Sizes<'_, '_> {
    fn source(&mut self, id: &str) -> Option<(u32, u32)> {
        if let Some((_, size)) = self.used.iter().find(|(used, _)| used == id) {
            return *size;
        }
        let size = (self.size_of)(id).filter(|(w, h)| *w > 0 && *h > 0);
        self.used.push((id.to_string(), size));
        size
    }
}

/// The size a source's settings give its pictures, before it has any:
/// `None` for captures, whose size comes with their first frame.
pub(crate) fn settings_size(kind: &SourceKind, canvas: (u32, u32)) -> Option<(u32, u32)> {
    let pair = |w: Option<u32>, h: Option<u32>| w.zip(h);
    match kind {
        SourceKind::TestPattern(pattern) => Some((pattern.width, pattern.height)),
        SourceKind::Camera(camera) => pair(camera.width, camera.height),
        SourceKind::Color(color) => Some(pair(color.width, color.height).unwrap_or(canvas)),
        SourceKind::Image(image) => pair(image.width, image.height),
        SourceKind::Slideshow(slideshow) => pair(slideshow.width, slideshow.height),
        SourceKind::Text(text) => pair(text.width, text.height).or_else(|| {
            (text.file.is_none() && text.data.is_none())
                .then(|| crate::text::size(text, text.text.as_deref().unwrap_or_default()))
        }),
        SourceKind::Browser(browser) => Some((browser.width, browser.height)),
        _ => None,
    }
}

/// The size of a leaf that shows text: what is known of it; else its own
/// size if it fits the room it has, or wrapped to fit it, as CSS lays out
/// text (its least width is its widest word).
fn measure_text(
    known: Size<Option<f32>>,
    available: Size<AvailableSpace>,
    text: u32,
    content: &mut dyn ContentSizes,
) -> Size<f32> {
    let size = |(width, height): (f32, f32)| Size { width, height };
    match (known.width, known.height) {
        (Some(width), Some(height)) => Size { width, height },
        (Some(width), None) => Size {
            width,
            height: content.text_size(text, Some(width)).1,
        },
        (None, Some(height)) => Size {
            width: content.text_size(text, None).0,
            height,
        },
        (None, None) => match available.width {
            AvailableSpace::MinContent => {
                let least = content.text_least_width(text);
                size(content.text_size(text, Some(least)))
            }
            AvailableSpace::MaxContent => size(content.text_size(text, None)),
            AvailableSpace::Definite(room) => {
                let own = content.text_size(text, None);
                match own.0 <= room {
                    true => size(own),
                    false => size(content.text_size(text, Some(room))),
                }
            }
        },
    }
}

/// The size of a leaf that shows a picture: what is known of it, else its
/// natural size or shape. Its least (min-content) size is 0.
fn measure(
    known: Size<Option<f32>>,
    available: Size<AvailableSpace>,
    picture: Option<Picture>,
) -> Size<f32> {
    let Some(picture) = picture else {
        return Size {
            width: known.width.unwrap_or(0.0),
            height: known.height.unwrap_or(0.0),
        };
    };
    let ratio = picture
        .natural
        .map_or(16.0 / 9.0, |(width, height)| width / height);
    match (known.width, known.height) {
        (Some(width), Some(height)) => Size { width, height },
        (Some(width), None) => Size {
            width,
            height: width / ratio,
        },
        (None, Some(height)) => Size {
            width: height * ratio,
            height,
        },
        (None, None) => {
            let least = matches!(available.width, AvailableSpace::MinContent)
                || matches!(available.height, AvailableSpace::MinContent);
            match (least, picture.natural, available.width) {
                (true, ..) => Size::ZERO,
                (false, Some((width, height)), _) => Size { width, height },
                // Not known yet: 16:9 across the room it has.
                (false, None, AvailableSpace::Definite(width)) => Size {
                    width,
                    height: width / ratio,
                },
                (false, None, _) => Size::ZERO,
            }
        }
    }
}

/// The taffy style of `item`: from its `layout`, or its `rect` (absolute,
/// at exactly those pixels), out of layout altogether while hidden.
fn item_style(item: &SceneItem, picture: Option<Picture>, children: bool) -> Style {
    let mut style = Style::DEFAULT;
    container(item.layout.as_ref(), &mut style);
    match (&item.layout, &item.rect) {
        (Some(layout), _) => place(layout, &mut style),
        (None, Some(rect)) => {
            style.position = taffy::Position::Absolute;
            style.inset = taffy::Rect {
                left: LengthPercentageAuto::length(rect.x as f32),
                top: LengthPercentageAuto::length(rect.y as f32),
                right: LengthPercentageAuto::auto(),
                bottom: LengthPercentageAuto::auto(),
            };
            style.size = Size {
                width: Dimension::length(rect.width as f32),
                height: Dimension::length(rect.height as f32),
            };
        }
        (None, None) => {}
    }
    if let Some(picture) = picture {
        style.item_is_replaced = true;
        // Pictures shrink to fit (CSS's `min-width: 0` and `min-height:
        // 0`) unless their layout says otherwise: `fit` decides how a
        // picture fills whatever box it gets.
        let least = item.layout.as_ref();
        if least.is_none_or(|layout| layout.min_width.is_none()) {
            style.min_size.width = Dimension::length(0.0);
        }
        if least.is_none_or(|layout| layout.min_height.is_none()) {
            style.min_size.height = Dimension::length(0.0);
        }
        // A picture with children is a container to taffy, which then
        // doesn't ask its natural size: give its shape (and, with nothing
        // else to size it, its size) up front.
        if children && let (None, Some((width, height))) = (style.aspect_ratio, picture.natural) {
            style.aspect_ratio = Some(width / height);
            if style.size.width.is_auto() && style.size.height.is_auto() {
                style.size = Size {
                    width: Dimension::length(width),
                    height: Dimension::length(height),
                };
            }
        }
    }
    if !item.visible {
        style.display = taffy::Display::None;
    }
    style
}

/// Set the fields of `style` that lay out children from `layout` (block
/// layout if it has none).
fn container(layout: Option<&Layout>, style: &mut Style) {
    style.display = taffy::Display::Block;
    let Some(layout) = layout else {
        return;
    };
    if let Some(display) = layout.display {
        style.display = match display {
            LayoutDisplay::Block => taffy::Display::Block,
            LayoutDisplay::Flex => taffy::Display::Flex,
            LayoutDisplay::Grid => taffy::Display::Grid,
        };
    }
    if let Some(direction) = layout.direction {
        style.flex_direction = match direction {
            FlexDirection::Row => taffy::FlexDirection::Row,
            FlexDirection::Column => taffy::FlexDirection::Column,
            FlexDirection::RowReverse => taffy::FlexDirection::RowReverse,
            FlexDirection::ColumnReverse => taffy::FlexDirection::ColumnReverse,
        };
    }
    if let Some(wrap) = layout.wrap {
        style.flex_wrap = match wrap {
            FlexWrap::Nowrap => taffy::FlexWrap::NoWrap,
            FlexWrap::Wrap => taffy::FlexWrap::Wrap,
            FlexWrap::WrapReverse => taffy::FlexWrap::WrapReverse,
        };
    }
    if let Some(gap) = &layout.gap {
        let [rows, columns] = gap.gaps();
        style.gap = Size {
            width: length_percentage(columns),
            height: length_percentage(rows),
        };
    }
    if let Some(padding) = &layout.padding {
        let [top, right, bottom, left] = padding.sides().map(length_percentage);
        style.padding = taffy::Rect {
            left,
            right,
            top,
            bottom,
        };
    }
    style.justify_content = layout.justify_content.map(content);
    style.align_content = layout.align_content.map(content);
    style.align_items = layout.align_items.map(items);
    if let Some(tracks) = &layout.grid_template_columns {
        style.grid_template_columns = template(tracks.tracks().unwrap_or_default());
    }
    if let Some(tracks) = &layout.grid_template_rows {
        style.grid_template_rows = template(tracks.tracks().unwrap_or_default());
    }
    if let Some(flow) = layout.grid_auto_flow {
        style.grid_auto_flow = match flow {
            GridAutoFlow::Row => taffy::GridAutoFlow::Row,
            GridAutoFlow::Column => taffy::GridAutoFlow::Column,
            GridAutoFlow::RowDense => taffy::GridAutoFlow::RowDense,
            GridAutoFlow::ColumnDense => taffy::GridAutoFlow::ColumnDense,
        };
    }
    if layout.overflow == Some(Overflow::Hidden) {
        style.overflow = taffy::Point {
            x: taffy::Overflow::Hidden,
            y: taffy::Overflow::Hidden,
        };
    }
}

/// Set the fields of `style` that place an item in its parent from
/// `layout`.
fn place(layout: &Layout, style: &mut Style) {
    if layout.position == Some(Position::Absolute) {
        style.position = taffy::Position::Absolute;
    }
    let mut inset = layout.inset.as_ref().map_or([None; 4], |inset| {
        inset.sides().map(|side| Some(length_auto(side)))
    });
    for (i, side) in [&layout.top, &layout.right, &layout.bottom, &layout.left]
        .into_iter()
        .enumerate()
    {
        if let Some(side) = side {
            inset[i] = Some(length_auto(side));
        }
    }
    let [top, right, bottom, left] = inset.map(|side| side.unwrap_or(LengthPercentageAuto::auto()));
    style.inset = taffy::Rect {
        left,
        right,
        top,
        bottom,
    };
    let size = |length: &Option<Length>| length.as_ref().map_or(Dimension::auto(), dimension);
    style.size = Size {
        width: size(&layout.width),
        height: size(&layout.height),
    };
    style.min_size = Size {
        width: size(&layout.min_width),
        height: size(&layout.min_height),
    };
    style.max_size = Size {
        width: size(&layout.max_width),
        height: size(&layout.max_height),
    };
    style.aspect_ratio = layout.aspect_ratio;
    if let Some(margin) = &layout.margin {
        let [top, right, bottom, left] = margin.sides().map(length_auto);
        style.margin = taffy::Rect {
            left,
            right,
            top,
            bottom,
        };
    }
    if let Some(grow) = layout.flex_grow {
        style.flex_grow = grow;
    }
    if let Some(shrink) = layout.flex_shrink {
        style.flex_shrink = shrink;
    }
    if let Some(basis) = &layout.flex_basis {
        style.flex_basis = dimension(basis);
    }
    style.align_self = layout.align_self.map(items);
    style.justify_self = layout.justify_self.map(items);
    if let Some(row) = &layout.grid_row
        && let Ok(places) = row.places()
    {
        style.grid_row = line(places);
    }
    if let Some(column) = &layout.grid_column
        && let Ok(places) = column.places()
    {
        style.grid_column = line(places);
    }
}

/// A length's value; one that doesn't read (validation refuses those)
/// as `auto`.
fn value(length: &Length) -> LengthValue {
    length.value().unwrap_or(LengthValue::Auto)
}

fn dimension(length: &Length) -> Dimension {
    match value(length) {
        LengthValue::Px(px) => Dimension::length(px),
        LengthValue::Percent(percent) => Dimension::percent(percent / 100.0),
        LengthValue::Auto => Dimension::auto(),
    }
}

fn length_auto(length: &Length) -> LengthPercentageAuto {
    match value(length) {
        LengthValue::Px(px) => LengthPercentageAuto::length(px),
        LengthValue::Percent(percent) => LengthPercentageAuto::percent(percent / 100.0),
        LengthValue::Auto => LengthPercentageAuto::auto(),
    }
}

fn length_percentage(length: &Length) -> LengthPercentage {
    match value(length) {
        LengthValue::Px(px) => LengthPercentage::length(px),
        LengthValue::Percent(percent) => LengthPercentage::percent(percent / 100.0),
        LengthValue::Auto => LengthPercentage::length(0.0),
    }
}

fn content(value: JustifyContent) -> taffy::AlignContent {
    match value {
        JustifyContent::Start => taffy::AlignContent::Start,
        JustifyContent::End => taffy::AlignContent::End,
        JustifyContent::FlexStart => taffy::AlignContent::FlexStart,
        JustifyContent::FlexEnd => taffy::AlignContent::FlexEnd,
        JustifyContent::Center => taffy::AlignContent::Center,
        JustifyContent::Stretch => taffy::AlignContent::Stretch,
        JustifyContent::SpaceBetween => taffy::AlignContent::SpaceBetween,
        JustifyContent::SpaceAround => taffy::AlignContent::SpaceAround,
        JustifyContent::SpaceEvenly => taffy::AlignContent::SpaceEvenly,
    }
}

fn items(value: AlignItems) -> taffy::AlignItems {
    match value {
        AlignItems::Start => taffy::AlignItems::Start,
        AlignItems::End => taffy::AlignItems::End,
        AlignItems::FlexStart => taffy::AlignItems::FlexStart,
        AlignItems::FlexEnd => taffy::AlignItems::FlexEnd,
        AlignItems::Center => taffy::AlignItems::Center,
        AlignItems::Baseline => taffy::AlignItems::Baseline,
        AlignItems::Stretch => taffy::AlignItems::Stretch,
    }
}

fn template(tracks: Vec<GridTrack>) -> Vec<GridTemplateComponent<String>> {
    tracks
        .into_iter()
        .map(|track| match track {
            GridTrack::Size(size) => GridTemplateComponent::Single(track_size(size)),
            GridTrack::Repeat(count, sizes) => {
                GridTemplateComponent::Repeat(GridTemplateRepetition {
                    count: match count {
                        GridRepeat::Count(n) => RepetitionCount::Count(n),
                        GridRepeat::AutoFill => RepetitionCount::AutoFill,
                        GridRepeat::AutoFit => RepetitionCount::AutoFit,
                    },
                    tracks: sizes.into_iter().map(track_size).collect(),
                    line_names: Vec::new(),
                })
            }
        })
        .collect()
}

/// A track size as CSS reads it: `1fr` is `minmax(auto, 1fr)`, a length
/// is both its least and its most.
fn track_size(size: TrackSize) -> TrackSizingFunction {
    let (min, max) = match size {
        TrackSize::Px(px) => (
            MinTrackSizingFunction::length(px),
            MaxTrackSizingFunction::length(px),
        ),
        TrackSize::Percent(percent) => (
            MinTrackSizingFunction::percent(percent / 100.0),
            MaxTrackSizingFunction::percent(percent / 100.0),
        ),
        TrackSize::Fr(fr) => (
            MinTrackSizingFunction::auto(),
            MaxTrackSizingFunction::fr(fr),
        ),
        TrackSize::Auto => (
            MinTrackSizingFunction::auto(),
            MaxTrackSizingFunction::auto(),
        ),
        TrackSize::MinContent => (
            MinTrackSizingFunction::min_content(),
            MaxTrackSizingFunction::min_content(),
        ),
        TrackSize::MaxContent => (
            MinTrackSizingFunction::max_content(),
            MaxTrackSizingFunction::max_content(),
        ),
        TrackSize::MinMax(min, max) => (
            match min {
                TrackMin::Px(px) => MinTrackSizingFunction::length(px),
                TrackMin::Percent(percent) => MinTrackSizingFunction::percent(percent / 100.0),
                TrackMin::Auto => MinTrackSizingFunction::auto(),
                TrackMin::MinContent => MinTrackSizingFunction::min_content(),
                TrackMin::MaxContent => MinTrackSizingFunction::max_content(),
            },
            match max {
                TrackMax::Px(px) => MaxTrackSizingFunction::length(px),
                TrackMax::Percent(percent) => MaxTrackSizingFunction::percent(percent / 100.0),
                TrackMax::Fr(fr) => MaxTrackSizingFunction::fr(fr),
                TrackMax::Auto => MaxTrackSizingFunction::auto(),
                TrackMax::MinContent => MaxTrackSizingFunction::min_content(),
                TrackMax::MaxContent => MaxTrackSizingFunction::max_content(),
            },
        ),
    };
    TrackSizingFunction { min, max }
}

fn line((start, end): (GridPlace, GridPlace)) -> taffy::Line<GridPlacement<String>> {
    let place = |place: GridPlace| match place {
        GridPlace::Auto => GridPlacement::Auto,
        GridPlace::Line(line) => GridPlacement::Line(line.into()),
        GridPlace::Span(span) => GridPlacement::Span(span),
    };
    taffy::Line {
        start: place(start),
        end: place(end),
    }
}

/// Where every item of `setup`'s scenes is drawn, as a live engine would
/// place them before any source has a picture (sources' sizes from their
/// settings; content measured, its text from `data`): for `get` with
/// `resolved` while no engine runs.
pub fn resolve_setup(setup: &Setup, data: &DataStore) -> Vec<ResolvedScene> {
    let canvas = (setup.canvas.width, setup.canvas.height);
    resolve_canvas(setup, canvas, data)
}

/// [`resolve_setup`] for a canvas's view of a setup
/// ([`Setup::for_canvas`]) whose sources make their pictures at
/// `sources_canvas`, the main canvas's size: a Color source without a
/// size fills the main canvas, as it does in a live engine (#112).
pub fn resolve_canvas(
    setup: &Setup,
    sources_canvas: (u32, u32),
    data: &DataStore,
) -> Vec<ResolvedScene> {
    let canvas = (setup.canvas.width, setup.canvas.height);
    let mut ids = NodeIds::default();
    let mut contents = Contents::default();
    let scenes = laid_out(setup, sources_canvas, &mut ids, &mut contents, data);
    resolved(&compile(
        scenes.into(),
        canvas,
        &mut ids,
        None,
        &[],
        &mut |_| None,
    ))
}

/// `setup`'s scenes with every item given a rect, sources' sizes from
/// their settings (`sources_canvas` for what fills the canvas).
fn laid_out(
    setup: &Setup,
    sources_canvas: (u32, u32),
    ids: &mut NodeIds,
    contents: &mut Contents,
    data: &DataStore,
) -> Vec<Scene> {
    let canvas = (setup.canvas.width, setup.canvas.height);
    let mut scenes = setup.scenes.clone();
    let size_of = |id: &str| {
        let source = setup.source(id)?;
        settings_size(&source.kind, sources_canvas).map(|size| output_size(&source.filters, size))
    };
    let mut sizer = Sizer::new(contents, data);
    Layouter::default().resolve(
        &mut scenes,
        canvas,
        &size_of,
        &mut sizer,
        ids,
        &mut Vec::new(),
    );
    scenes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(scenes: &str) -> Setup {
        let setup = Setup::from_toml(&format!(
            r##"
version = 1
name = "t"
[canvas]
width = 1920
height = 1080
fps = 60
[[sources]]
id = "p"
kind = "test_pattern"
width = 320
height = 180
[[sources]]
id = "cam"
kind = "camera"
device = "x"
{scenes}
[recording]
folder = "/tmp"
[recording.program]
encoder = {{ codec = "hevc", quality = 70 }}
"##
        ))
        .unwrap();
        let issues = setup.validate();
        assert!(issues.is_empty(), "{issues:?}");
        setup
    }

    /// Every item's rect, in order, scene by scene.
    fn rects(setup: &Setup) -> Vec<Vec<[i64; 4]>> {
        fn walk(items: &[SceneItem], out: &mut Vec<[i64; 4]>) {
            for item in items {
                let r = item.rect.unwrap();
                out.push([r.x.into(), r.y.into(), r.width.into(), r.height.into()]);
                walk(&item.children, out);
            }
        }
        laid_out(
            setup,
            (setup.canvas.width, setup.canvas.height),
            &mut NodeIds::default(),
            &mut Contents::default(),
            &DataStore::default(),
        )
        .iter()
        .map(|scene| {
            let mut out = Vec::new();
            walk(&scene.items, &mut out);
            out
        })
        .collect()
    }

    #[test]
    fn a_flex_row_places_pictures_at_their_size() {
        let setup = setup(
            r##"
[[scenes]]
id = "row"
layout = { display = "flex", padding = 40, gap = 20, align_items = "start" }
items = [
  { id = "a", source = "p", layout = {} },
  { id = "b", source = "p", layout = {} },
  { id = "c", source = "p", layout = {} },
]
[[scenes]]
id = "centred"
layout = { display = "flex", justify_content = "center", align_items = "center" }
items = [{ source = "p", layout = { width = 640 } }]
"##,
        );
        let rects = rects(&setup);
        assert_eq!(
            rects[0],
            [[40, 40, 320, 180], [380, 40, 320, 180], [720, 40, 320, 180]]
        );
        // 640 wide keeps the picture's 16:9 shape; centred both ways.
        assert_eq!(rects[1], [[640, 360, 640, 360]]);
    }

    #[test]
    fn grow_and_grid_tracks_split_the_room() {
        let setup = setup(
            r##"
[[scenes]]
id = "column"
layout = { display = "flex", direction = "column" }
items = [
  { id = "one", layout = { flex_grow = 1, flex_basis = 0 }, style = { background = "#f00" } },
  { id = "two", source = "p", layout = { flex_grow = 2, flex_basis = 0 } },
]
[[scenes]]
id = "grid"
layout = { display = "grid", grid_template_columns = "1fr 2fr" }
items = [
  { id = "left", layout = { height = "100%" }, style = { background = "#f00" } },
  { id = "right", layout = { height = "100%" }, style = { background = "#0f0" } },
]
"##,
        );
        let rects = rects(&setup);
        assert_eq!(rects[0], [[0, 0, 1920, 360], [0, 360, 1920, 720]]);
        assert_eq!(rects[1], [[0, 0, 640, 1080], [640, 0, 1280, 1080]]);
    }

    #[test]
    fn rects_and_insets_place_the_same() {
        // Ryan's facecam corner: by rect, and by right/bottom.
        let setup = setup(
            r##"
[[scenes]]
id = "rect"
items = [{ source = "cam", rect = { x = 1489, y = 838, width = 431, height = 242 } }]
[[scenes]]
id = "inset"
layout = { display = "flex" }
items = [
  { source = "cam", layout = { position = "absolute", right = 0, bottom = 0, width = 431, height = 242 } },
  { id = "box", rect = { x = 10, y = 20, width = 30, height = 40 }, style = { background = "#fff" } },
  { id = "pct", layout = { position = "absolute", inset = ["10%", "auto", "auto", "25%"], width = "50%", aspect_ratio = 2 } },
]
"##,
        );
        let rects = rects(&setup);
        assert_eq!(rects[0], [[1489, 838, 431, 242]]);
        assert_eq!(
            rects[1],
            [
                [1489, 838, 431, 242],
                [10, 20, 30, 40],
                [480, 108, 960, 480]
            ]
        );
    }

    #[test]
    fn unknown_pictures_are_16_by_9_and_hidden_items_keep_their_place() {
        let mut setup = setup(
            r##"
[[scenes]]
id = "s"
layout = { display = "flex", direction = "column" }
items = [
  { id = "first", layout = { height = 100 }, style = { background = "#fff" } },
  { source = "cam", layout = { width = 960 } },
  { id = "last", layout = { height = 100 }, style = { background = "#fff" } },
]
"##,
        );
        let mut layouter = Layouter::default();
        let mut ids = NodeIds::default();
        let lay = |layouter: &mut Layouter, ids: &mut NodeIds, setup: &Setup| {
            let mut scenes = setup.scenes.clone();
            let mut used = Vec::new();
            layouter.resolve(
                &mut scenes,
                (1920, 1080),
                &|_| None,
                &mut NoContent,
                ids,
                &mut used,
            );
            let rects: Vec<Rect> = scenes[0].items.iter().map(|i| i.rect.unwrap()).collect();
            (rects, used)
        };
        let (before, used) = lay(&mut layouter, &mut ids, &setup);
        assert_eq!(used, [("cam".to_string(), None)]);
        assert_eq!(
            (before[1].y, before[1].width, before[1].height, before[2].y),
            (100, 960, 540, 640)
        );
        // Hidden, the camera leaves the flow (the last box moves up) but
        // keeps its place for a hide transition.
        setup.scenes[0].items[1].visible = false;
        let (after, _) = lay(&mut layouter, &mut ids, &setup);
        assert_eq!(after[1], before[1]);
        assert_eq!(after[2].y, 100);
    }

    #[test]
    fn the_designs_panel_shares_the_row() {
        // docs/scenes-as-code.md 5.1: a facecam taking the room a 30%
        // sidebar leaves.
        let setup = setup(
            r##"
[[scenes]]
id = "panel"
layout = { display = "flex", direction = "row", gap = 24, padding = 48, align_items = "center" }
items = [
  { id = "facecam", source = "cam", fit = "cover", layout = { flex_grow = 1, flex_basis = 0, aspect_ratio = 1.7778 } },
  { id = "sidebar", layout = { width = "30%", height = "100%", display = "flex", direction = "column", gap = 12 }, style = { background = "#000000aa", radius = 16 } },
]
"##,
        );
        // 1824 wide inside the padding: 30% is 547; the facecam gets the
        // rest after the gap (1253), 16:9 tall (704.7), centred, edges on
        // whole pixels.
        assert_eq!(
            rects(&setup)[0],
            [[48, 188, 1253, 704], [1325, 48, 547, 984]]
        );
    }

    #[test]
    fn clips_and_resolved_boxes() {
        let setup = setup(
            r##"
[[scenes]]
id = "s"
items = [
  { id = "card", layout = { position = "absolute", left = 100, top = 50, width = 200, height = 100, overflow = "hidden" }, style = { radius = 10, background = "#fff" }, children = [
    { id = "inner", rect = { x = -20, y = 0, width = 50, height = 50 }, style = { background = "#f00", transform = { rotate = 90 } } },
  ] },
  { id = "after", rect = { x = 0, y = 0, width = 10, height = 10 }, style = { background = "#0f0" } },
]
"##,
        );
        let mut ids = NodeIds::default();
        let data = DataStore::default();
        let scenes = laid_out(
            &setup,
            (1920, 1080),
            &mut ids,
            &mut Contents::default(),
            &data,
        );
        let compiled = compile(
            scenes.into(),
            (1920, 1080),
            &mut ids,
            None,
            &[],
            &mut |_| None,
        );
        assert_eq!(compiled.clips.len(), 1);
        // The card's own fill isn't clipped; its child is; the next item
        // isn't.
        let clips: Vec<u32> = compiled.draws.iter().map(|d| d.placed.clip).collect();
        assert_eq!(clips, [0, 1, 0]);
        let resolved = resolved(&compiled);
        let card = &resolved[0].items[0];
        assert_eq!(card.id.as_deref(), Some("card"));
        assert_eq!(
            (card.rect.x, card.rect.y, card.rect.width, card.rect.height),
            (100.0, 50.0, 200.0, 100.0)
        );
        assert_eq!(card.canvas, card.rect);
        assert_eq!(card.transform, [1.0, 0.0, 100.0, 0.0, 1.0, 50.0]);
        // Turned a quarter about its centre: the same box on the canvas.
        let inner = &card.children[0].canvas;
        let near = |a: f32, b: f32| (a - b).abs() < 1e-3;
        assert!(
            near(inner.x, 80.0) && near(inner.y, 50.0) && near(inner.width, 50.0),
            "{inner:?}"
        );
        assert!(resolved[0].items[1].visible);
    }

    #[test]
    fn flipped_items_are_mirrored_in_their_own_box() {
        let setup = setup(
            r##"
[[scenes]]
id = "s"
items = [
  { id = "across", source = "p", rect = { x = 100, y = 50, width = 200, height = 100 }, style = { transform = { flip_horizontal = true } } },
  { id = "down", source = "p", rect = { x = 100, y = 50, width = 200, height = 100 }, style = { transform = { flip_vertical = true } } },
  { id = "turned", source = "p", rect = { x = 100, y = 50, width = 200, height = 100 }, style = { transform = { flip_horizontal = true, rotate = 90 } } },
]
"##,
        );
        let mut ids = NodeIds::default();
        let scenes = laid_out(
            &setup,
            (1920, 1080),
            &mut ids,
            &mut Contents::default(),
            &DataStore::default(),
        );
        let compiled = compile(
            scenes.into(),
            (1920, 1080),
            &mut ids,
            None,
            &[],
            &mut |_| None,
        );
        let resolved = resolved(&compiled);
        let items = &resolved[0].items;
        // Its own left edge lands on the box's right, and the reverse.
        assert_eq!(items[0].transform, [-1.0, 0.0, 300.0, 0.0, 1.0, 50.0]);
        assert_eq!(items[1].transform, [1.0, 0.0, 100.0, 0.0, -1.0, 150.0]);
        assert_eq!(items[0].canvas, items[0].rect);
        assert_eq!(items[1].canvas, items[1].rect);
        // Mirrored, then turned a quarter clockwise about its centre (200,
        // 100): its own top left (0, 0) ends at the turned box's bottom
        // right, (250, 200), where unflipped it would be top right.
        let [a, b, tx, c, d, ty] = items[2].transform;
        let near = |x: f32, y: f32| (x - y).abs() < 1e-3;
        assert!(near(a, 0.0) && near(b, -1.0) && near(c, -1.0) && near(d, 0.0));
        assert!(
            near(tx, 250.0) && near(ty, 200.0),
            "{:?}",
            items[2].transform
        );
    }

    #[test]
    fn scenes_without_layout_are_left_alone() {
        let setup = setup(
            r##"
[[scenes]]
id = "plain"
items = [{ source = "p", rect = { x = 1, y = 2, width = 3, height = 4 } }]
"##,
        );
        let scenes = laid_out(
            &setup,
            (1920, 1080),
            &mut NodeIds::default(),
            &mut Contents::default(),
            &DataStore::default(),
        );
        assert_eq!(scenes, setup.scenes);
    }
}
