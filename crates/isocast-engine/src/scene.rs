//! Scenes as layers: the item tree flattened into drawing order, each item
//! placed by its rect and transform within its parents. The program and
//! snapshots draw compiled scenes ([`crate::compiled`]), which place items
//! with the same arithmetic; Move transitions blend these layers. Scenes
//! are laid out first ([`crate::layout`]), so every item has a rect.

use isocast_protocol::layout::Overflow;
use isocast_protocol::{Rect, Scene, SceneItem};

use crate::gpu::{Gpu, GpuFrame};
use crate::render::{Affine, Layer, LayerClip, LayerShadow};

/// The layers that draw `scene`, given its sources' `frames` (by source
/// id), the canvases of the scenes it shows (`nested`, by scene id) and its
/// content items' pictures (`contents`, by [`item_path`]), with the canvas
/// mapped onto the target by `root` (to scale it). An item whose source
/// has no frame yet draws only its style. A group's opacity multiplies its
/// children's.
pub fn layers<'a, S: AsRef<str>>(
    scene: &Scene,
    frames: &'a [(S, GpuFrame)],
    nested: &'a [(String, GpuFrame)],
    contents: &'a [(String, GpuFrame)],
    root: Affine,
) -> Vec<Layer<'a>> {
    layers_with(scene, frames, nested, contents, root, &|_| None)
}

/// How an item looks part of the way through its show or hide transition.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ItemLook {
    pub opacity: f32,
    /// How far its picture is moved out of its rect, as fractions of its
    /// width and height.
    pub shift: [f32; 2],
}

/// The looks of items whose show or hide transition is under way, by
/// [`item_path`]; an item hiding is drawn until its transition ends.
pub type Looks<'l> = &'l dyn Fn(&str) -> Option<ItemLook>;

/// What names an item for show and hide transitions: its scene's id, then
/// its key (or `#index`) at each level of the tree.
pub fn item_path(parent: &str, item: &SceneItem, index: usize) -> String {
    match item.key() {
        Some(key) => format!("{parent}/{key}"),
        None => format!("{parent}/#{index}"),
    }
}

/// Add `item`'s part of its path ([`item_path`]) to `path`, its parent's
/// path: the same path, without making a string for every item.
pub(crate) fn push_item_path(path: &mut String, item: &SceneItem, index: usize) {
    path.push('/');
    match item.key() {
        Some(key) => path.push_str(key),
        None => {
            use std::fmt::Write;
            let _ = write!(path, "#{index}");
        }
    }
}

/// [`layers`], with items part of the way through their show or hide
/// transitions looking as `looks` says.
pub fn layers_with<'a, S: AsRef<str>>(
    scene: &Scene,
    frames: &'a [(S, GpuFrame)],
    nested: &'a [(String, GpuFrame)],
    contents: &'a [(String, GpuFrame)],
    root: Affine,
    looks: Looks<'_>,
) -> Vec<Layer<'a>> {
    let mut out = Vec::new();
    let place = Place {
        frames,
        nested,
        contents,
        looks,
    };
    place.walk(&scene.items, &scene.id, root, 1.0, None, &mut out);
    out
}

/// Where `item` is in its parent: its rect (every item has one once its
/// scene is laid out; none means no size).
pub(crate) fn rect_of(item: &SceneItem) -> Rect {
    item.rect.unwrap_or(Rect {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
    })
}

/// Whether `item` clips its children to its rounded rect.
pub(crate) fn clips(item: &SceneItem) -> bool {
    item.layout
        .as_ref()
        .is_some_and(|layout| layout.overflow == Some(Overflow::Hidden))
}

/// A texture to draw a canvas on and then draw from.
pub(crate) fn canvas_texture(
    gpu: &Gpu,
    format: wgpu::TextureFormat,
    (width, height): (u32, u32),
) -> wgpu::Texture {
    gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("canvas"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

/// What items are drawn with.
struct Place<'a, 'l, S> {
    frames: &'a [(S, GpuFrame)],
    nested: &'a [(String, GpuFrame)],
    contents: &'a [(String, GpuFrame)],
    looks: Looks<'l>,
}

impl<'a, S: AsRef<str>> Place<'a, '_, S> {
    fn walk(
        &self,
        items: &[SceneItem],
        path: &str,
        parent: Affine,
        opacity: f32,
        clip: Option<LayerClip>,
        out: &mut Vec<Layer<'a>>,
    ) {
        let (frames, nested, contents) = (self.frames, self.nested, self.contents);
        for (index, item) in items.iter().enumerate() {
            let path = item_path(path, item, index);
            let look = (self.looks)(&path);
            if !item.visible && look.is_none() {
                continue;
            }
            let look = look.unwrap_or(ItemLook {
                opacity: 1.0,
                shift: [0.0; 2],
            });
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
            let transform = parent.after(place);
            let opacity = opacity * style.opacity * look.opacity;
            let frame = match (&item.source, &item.scene) {
                (Some(source), _) => frames
                    .iter()
                    .find(|(id, _)| id.as_ref() == source)
                    .map(|(_, frame)| frame),
                (None, Some(scene)) => nested
                    .iter()
                    .find(|(id, _)| id == scene)
                    .map(|(_, frame)| frame),
                (None, None) => contents
                    .iter()
                    .find(|(at, _)| *at == path)
                    .map(|(_, frame)| frame),
            };
            let rgba = |color: &isocast_protocol::Color| color.rgba().unwrap_or([0.0; 4]);
            out.push(Layer {
                frame,
                crop: item.crop,
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
                shift: [look.shift[0] * width, look.shift[1] * height],
                clip,
            });
            // Its children are clipped by it (only the nearest clip is
            // kept here; compiled scenes keep them all).
            let inner = match clips(item) {
                true => Some(LayerClip {
                    transform,
                    size: [width, height],
                    radius: style.radius.corners(),
                }),
                false => clip,
            };
            self.walk(&item.children, &path, transform, opacity, inner, out);
        }
    }
}
