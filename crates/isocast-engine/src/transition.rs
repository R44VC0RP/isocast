//! Scene transitions on the GPU. The program draws the scene it changes
//! from and the one it changes to on canvases of their own, then one
//! full-screen pass here combines them by the transition's progress: a
//! fade, a swipe or slide (the scenes shifted), a fade through a colour, a
//! luma wipe (a pattern's brightness says where the next scene shows
//! first), or a stinger (a video over the switch, with an optional matte).
//! Move is different: it draws the items themselves on their way, so it
//! works on the scenes' layers ([`moved`]).

use std::collections::HashMap;

use isocast_protocol::{Direction, LumaPattern, Scene, SceneItem, Swipe, TransitionKind};
use wgpu::util::DeviceExt;

use crate::gpu::Gpu;
use crate::render::{Affine, Layer, LayerClip, LayerShadow};

/// How many vec4 parameters a pass gets.
const PARAMS: usize = 4;

const SHADER: &str = r#"
struct Params {
    v: array<vec4<f32>, 4>,
}

// The scene changed from and the one changed to, drawn on canvases.
@group(0) @binding(0) var before: texture_2d<f32>;
@group(0) @binding(1) var after: texture_2d<f32>;
// A luma wipe's picture, or a stinger's matte (drawn on a canvas).
@group(0) @binding(2) var pattern: texture_2d<f32>;
// A stinger's picture, premultiplied.
@group(0) @binding(3) var overlay: texture_2d<f32>;
@group(0) @binding(4) var smp: sampler;
@group(0) @binding(5) var<uniform> params: Params;

struct Varyings {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

// One triangle covering the target.
@vertex
fn vs(@builtin(vertex_index) index: u32) -> Varyings {
    let xy = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    var out: Varyings;
    out.position = vec4<f32>(xy * 2.0 - 1.0, 0.0, 1.0);
    out.uv = vec2<f32>(xy.x, 1.0 - xy.y);
    return out;
}

const LUMA: vec3<f32> = vec3<f32>(0.2126, 0.7152, 0.0722);
const TAU: f32 = 6.28318530718;

// v0.x: progress.
@fragment
fn fade(in: Varyings) -> @location(0) vec4<f32> {
    let a = textureSampleLevel(before, smp, in.uv, 0.0);
    let b = textureSampleLevel(after, smp, in.uv, 0.0);
    return mix(a, b, params.v[0].x);
}

// The scene changed from (`which` 0) or to (1), moved by `offset` (in
// canvas widths and heights), at `uv`; transparent where it isn't.
fn shifted(uv: vec2<f32>, offset: vec2<f32>, which: u32) -> vec4<f32> {
    let at = uv - offset;
    let inside = all(at >= vec2<f32>(0.0)) && all(at <= vec2<f32>(1.0));
    var c: vec4<f32>;
    if (which == 0u) {
        c = textureSampleLevel(before, smp, clamp(at, vec2<f32>(0.0), vec2<f32>(1.0)), 0.0);
    } else {
        c = textureSampleLevel(after, smp, clamp(at, vec2<f32>(0.0), vec2<f32>(1.0)), 0.0);
    }
    return select(vec4<f32>(0.0), c, inside);
}

// v0.xy: where the scene changed from is moved to, v0.zw: where the next
// is; v1.x: 1 if the next is drawn on top. Where neither is: black.
@fragment
fn shift(in: Varyings) -> @location(0) vec4<f32> {
    let a = shifted(in.uv, params.v[0].xy, 0u);
    let b = shifted(in.uv, params.v[0].zw, 1u);
    let next_on_top = params.v[1].x > 0.5;
    let top = select(a, b, next_on_top);
    let bottom = select(b, a, next_on_top);
    let under = select(vec4<f32>(0.0, 0.0, 0.0, 1.0), bottom, bottom.a > 0.0);
    return top + under * (1.0 - top.a);
}

// v0.x: progress, v0.y: switch point (0 to 1); v1.rgb: the colour.
@fragment
fn fade_to_color(in: Varyings) -> @location(0) vec4<f32> {
    let p = params.v[0].x;
    let switch_at = params.v[0].y;
    let color = vec4<f32>(params.v[1].rgb, 1.0);
    if (p < switch_at) {
        return mix(textureSampleLevel(before, smp, in.uv, 0.0), color, p / switch_at);
    }
    let rest = max(1.0 - switch_at, 1e-6);
    return mix(color, textureSampleLevel(after, smp, in.uv, 0.0), clamp((p - switch_at) / rest, 0.0, 1.0));
}

// A built-in pattern's brightness at `uv` on a canvas of `size`.
fn built_in(kind: i32, uv: vec2<f32>, size: vec2<f32>) -> f32 {
    let d = abs(uv - 0.5);
    switch kind {
        case 0: { return uv.x; }
        case 1: { return uv.y; }
        case 2: { return (uv.x + uv.y) * 0.5; }
        case 3: { return d.x * 2.0; }
        case 4: { return d.y * 2.0; }
        case 5: { return max(d.x, d.y) * 2.0; }
        case 6: { return length((uv - 0.5) * size) / length(size * 0.5); }
        case 7: { return d.x + d.y; }
        case 8: {
            let p = (uv - 0.5) * size;
            // Clockwise from twelve o'clock (y points down).
            return fract(atan2(p.x, -p.y) / TAU + 1.0);
        }
        case 9: { return fract(uv.y * 8.0); }
        case 10: { return fract(uv.x * 8.0); }
        default: {
            let cells = vec2<f32>(8.0, max(round(8.0 * size.y / size.x), 1.0));
            let cell = floor(uv * cells);
            let odd = (i32(cell.x) + i32(cell.y)) & 1;
            return (fract(uv.x * cells.x) + f32(odd)) * 0.5;
        }
    }
}

// v0.x: progress, v0.y: softness, v0.z: 1 to invert, v0.w: the built-in
// pattern, or -1 for the picture; v1.xy: the canvas size.
@fragment
fn luma_wipe(in: Varyings) -> @location(0) vec4<f32> {
    let p = params.v[0];
    var level: f32;
    if (p.w < 0.0) {
        level = dot(textureSampleLevel(pattern, smp, in.uv, 0.0).rgb, LUMA);
    } else {
        level = built_in(i32(p.w), in.uv, params.v[1].xy);
    }
    level = clamp(select(level, 1.0 - level, p.z > 0.5), 0.0, 1.0);
    // The edge sweeps from below 0 to past 1 + softness, so the wipe
    // starts and ends whole.
    let t = p.x * (1.0 + p.y);
    let shown = select(f32(level < t), clamp((t - level) / p.y, 0.0, 1.0), p.y > 0.0);
    return mix(textureSampleLevel(before, smp, in.uv, 0.0), textureSampleLevel(after, smp, in.uv, 0.0), shown);
}

// v0.x: 1 once past the cut point, v0.y: 1 with a matte, v0.z: 1 to
// invert the matte, v0.w: 1 if there is a picture to draw on top.
@fragment
fn stinger(in: Varyings) -> @location(0) vec4<f32> {
    let p = params.v[0];
    let a = textureSampleLevel(before, smp, in.uv, 0.0);
    let b = textureSampleLevel(after, smp, in.uv, 0.0);
    var base = select(a, b, p.x > 0.5);
    if (p.y > 0.5) {
        let m = dot(textureSampleLevel(pattern, smp, in.uv, 0.0).rgb, LUMA);
        base = mix(a, b, clamp(select(m, 1.0 - m, p.z > 0.5), 0.0, 1.0));
    }
    let top = select(vec4<f32>(0.0), textureSampleLevel(overlay, smp, in.uv, 0.0), p.w > 0.5);
    return vec4<f32>(top.rgb + base.rgb * (1.0 - top.a), 1.0);
}
"#;

/// The fragment shaders, by entry point.
const PASSES: &[&str] = &["fade", "shift", "fade_to_color", "luma_wipe", "stinger"];

/// A stinger's video at this moment, drawn on canvases: its picture
/// (premultiplied), and its matte if it has one.
pub struct Overlay<'a> {
    pub picture: Option<&'a wgpu::Texture>,
    pub matte: Option<&'a wgpu::Texture>,
    /// The scenes have switched behind it (without a matte).
    pub cut: bool,
    pub invert: bool,
}

/// The id a stinger's video plays under in a session (not a valid source
/// id, so it can't clash with one).
pub(crate) fn stinger_id(file: &str) -> String {
    format!("stinger:{file}")
}

/// The transition passes for targets of one format, and the luma wipe
/// pictures they read.
pub struct Transitions {
    layout: wgpu::BindGroupLayout,
    pipelines: HashMap<&'static str, wgpu::RenderPipeline>,
    sampler: wgpu::Sampler,
    /// Luma wipe pictures by path; `None` if one could not be read (the
    /// pattern is used instead).
    pictures: HashMap<String, Option<wgpu::Texture>>,
    /// Bound when a pass reads no picture.
    blank: wgpu::Texture,
}

impl Transitions {
    pub fn new(gpu: &Gpu, format: wgpu::TextureFormat) -> Self {
        let device = &gpu.device;
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("transitions"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let texture = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("transition"),
            entries: &[
                texture(0),
                texture(1),
                texture(2),
                texture(3),
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 5,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("transition"),
            bind_group_layouts: &[Some(&layout)],
            ..Default::default()
        });
        let pipelines = PASSES
            .iter()
            .map(|&entry| {
                let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some(entry),
                    layout: Some(&pipeline_layout),
                    vertex: wgpu::VertexState {
                        module: &shader,
                        entry_point: Some("vs"),
                        compilation_options: Default::default(),
                        buffers: &[],
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &shader,
                        entry_point: Some(entry),
                        compilation_options: Default::default(),
                        targets: &[Some(wgpu::ColorTargetState {
                            format,
                            blend: None,
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                    }),
                    primitive: Default::default(),
                    depth_stencil: None,
                    multisample: Default::default(),
                    multiview_mask: None,
                    cache: None,
                });
                (entry, pipeline)
            })
            .collect();
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("transition"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Self {
            layout,
            pipelines,
            sampler,
            pictures: HashMap::new(),
            blank: crate::filter::upload_image(gpu, 1, 1, &[0; 4]),
        }
    }

    /// Draw `kind` `progress` (0 to 1) of the way from the canvas `before`
    /// to `after` into `target` (all `canvas` sized), with a stinger's
    /// `overlay`. Move is drawn from layers instead ([`moved`]); here it
    /// fades. Returns the submission, to wait for.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        gpu: &Gpu,
        target: &wgpu::Texture,
        before: &wgpu::Texture,
        after: &wgpu::Texture,
        kind: &TransitionKind,
        progress: f32,
        overlay: Option<Overlay<'_>>,
        canvas: (u32, u32),
    ) -> wgpu::SubmissionIndex {
        let p = progress.clamp(0.0, 1.0);
        let mut params = [[0.0f32; 4]; PARAMS];
        let mut pattern = None;
        let mut picture = None;
        let entry = match kind {
            TransitionKind::Cut | TransitionKind::Fade | TransitionKind::Move(_) => {
                params[0][0] = p;
                "fade"
            }
            TransitionKind::Swipe(swipe) => {
                params = swipe_params(swipe, p);
                "shift"
            }
            TransitionKind::Slide(slide) => {
                let t = slide.easing.apply(p);
                let way = direction(slide.direction);
                // Both move: the old one out, the next one in behind it.
                params[0] = [
                    way[0] * t,
                    way[1] * t,
                    -way[0] * (1.0 - t),
                    -way[1] * (1.0 - t),
                ];
                params[1][0] = 1.0;
                "shift"
            }
            TransitionKind::FadeToColor(fade) => {
                let [r, g, b, _] = fade.color.rgba().unwrap_or([0.0, 0.0, 0.0, 1.0]);
                params[0] = [p, fade.switch_point / 100.0, 0.0, 0.0];
                params[1] = [r, g, b, 1.0];
                "fade_to_color"
            }
            TransitionKind::LumaWipe(wipe) => {
                let image = wipe
                    .image
                    .as_deref()
                    .and_then(|path| self.picture(gpu, path));
                let kind = match image {
                    Some(_) => -1.0,
                    None => pattern_index(wipe.pattern) as f32,
                };
                pattern = image.cloned();
                params[0] = [p, wipe.softness, f32::from(u8::from(wipe.invert)), kind];
                params[1] = [canvas.0 as f32, canvas.1 as f32, 0.0, 0.0];
                "luma_wipe"
            }
            TransitionKind::Stinger(_) => {
                let overlay = overlay.unwrap_or(Overlay {
                    picture: None,
                    matte: None,
                    cut: p >= 0.5,
                    invert: false,
                });
                let flag = |on: bool| f32::from(u8::from(on));
                params[0] = [
                    flag(overlay.cut),
                    flag(overlay.matte.is_some()),
                    flag(overlay.invert),
                    flag(overlay.picture.is_some()),
                ];
                pattern = overlay.matte.cloned();
                picture = overlay.picture.cloned();
                "stinger"
            }
        };
        let uniform: Vec<u8> = params
            .iter()
            .flatten()
            .flat_map(|v| v.to_ne_bytes())
            .collect();
        let buffer = gpu
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("transition"),
                contents: &uniform,
                usage: wgpu::BufferUsages::UNIFORM,
            });
        let view = |texture: &wgpu::Texture| texture.create_view(&Default::default());
        let views = [
            view(before),
            view(after),
            view(pattern.as_ref().unwrap_or(&self.blank)),
            view(picture.as_ref().unwrap_or(&self.blank)),
        ];
        let mut entries: Vec<wgpu::BindGroupEntry> = views
            .iter()
            .enumerate()
            .map(|(binding, view)| wgpu::BindGroupEntry {
                binding: binding as u32,
                resource: wgpu::BindingResource::TextureView(view),
            })
            .collect();
        entries.push(wgpu::BindGroupEntry {
            binding: 4,
            resource: wgpu::BindingResource::Sampler(&self.sampler),
        });
        entries.push(wgpu::BindGroupEntry {
            binding: 5,
            resource: buffer.as_entire_binding(),
        });
        let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("transition"),
            layout: &self.layout,
            entries: &entries,
        });
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        {
            let target = view(target);
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some(entry),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&self.pipelines[entry]);
            pass.set_bind_group(0, &group, &[]);
            pass.draw(0..3, 0..1);
        }
        gpu.queue.submit([encoder.finish()])
    }

    /// The luma wipe picture at `path` on the GPU, read on first use;
    /// `None` (said once) if it can't be.
    fn picture(&mut self, gpu: &Gpu, path: &str) -> Option<&wgpu::Texture> {
        self.pictures
            .entry(path.to_string())
            .or_insert_with(|| {
                let file = crate::record::expand_home(path);
                match crate::picture::load(&file, None) {
                    Ok(mut frames) if !frames.is_empty() => {
                        let (picture, _) = frames.remove(0);
                        Some(crate::filter::upload_image(
                            gpu,
                            picture.width,
                            picture.height,
                            &picture.rgba,
                        ))
                    }
                    Ok(_) => None,
                    Err(err) => {
                        isocast_diagnostics::diagnostic!("warn";
                            "isocastd: a luma wipe uses its pattern instead: {err:#}"
                        );
                        None
                    }
                }
            })
            .as_ref()
    }
}

/// One canvas width or height in `direction`, in texture coordinates (y
/// down).
fn direction(direction: Direction) -> [f32; 2] {
    match direction {
        Direction::Left => [-1.0, 0.0],
        Direction::Right => [1.0, 0.0],
        Direction::Up => [0.0, -1.0],
        Direction::Down => [0.0, 1.0],
    }
}

/// The `shift` pass's parameters for a swipe `p` of the way through.
fn swipe_params(swipe: &Swipe, p: f32) -> [[f32; 4]; PARAMS] {
    let t = swipe.easing.apply(p);
    let way = direction(swipe.direction);
    let mut params = [[0.0; 4]; PARAMS];
    if swipe.swipe_in {
        // The next scene comes in from the far edge, over the old one.
        params[0] = [0.0, 0.0, -way[0] * (1.0 - t), -way[1] * (1.0 - t)];
        params[1][0] = 1.0;
    } else {
        // The old scene leaves, uncovering the next.
        params[0] = [way[0] * t, way[1] * t, 0.0, 0.0];
    }
    params
}

/// The shader's number for a built-in luma pattern.
fn pattern_index(pattern: LumaPattern) -> u32 {
    match pattern {
        LumaPattern::Horizontal => 0,
        LumaPattern::Vertical => 1,
        LumaPattern::Diagonal => 2,
        LumaPattern::BarnDoorHorizontal => 3,
        LumaPattern::BarnDoorVertical => 4,
        LumaPattern::Box => 5,
        LumaPattern::Circle => 6,
        LumaPattern::Diamond => 7,
        LumaPattern::Clock => 8,
        LumaPattern::BlindsHorizontal => 9,
        LumaPattern::BlindsVertical => 10,
        LumaPattern::Checkerboard => 11,
    }
}

/// What a layer of a scene is, for Move to find it in the other scene:
/// the item's key, and what it shows (its source or scene). Items without
/// a key are never matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    key: Option<String>,
    source: Option<String>,
    scene: Option<String>,
}

/// The identities of the layers `crate::scene::layers` makes for `scene`,
/// in the same order: one per visible item, parents before their
/// children.
pub fn identities(scene: &Scene) -> Vec<Identity> {
    fn walk(items: &[SceneItem], out: &mut Vec<Identity>) {
        for item in items.iter().filter(|item| item.visible) {
            out.push(Identity {
                key: item.key().map(str::to_string),
                source: item.source.clone(),
                scene: item.scene.clone(),
            });
            walk(&item.children, out);
        }
    }
    let mut out = Vec::new();
    walk(&scene.items, &mut out);
    out
}

/// The layers of a Move `t` (eased, 0 to 1) of the way from the layers
/// `before` to `after` (each with its identity): an item in both scenes
/// moves, resizes, turns and fades between its two looks; an item in only
/// one fades out or in. Drawn in the next scene's order, each item only
/// in the old scene just above the item it was above there.
pub fn moved<'a>(
    before: Vec<(Identity, Layer<'a>)>,
    after: Vec<(Identity, Layer<'a>)>,
    t: f32,
) -> Vec<Layer<'a>> {
    let matched = |identity: &Identity| identity.key.is_some();
    // For each old layer, the new layer it matches.
    let partner: Vec<Option<usize>> = before
        .iter()
        .map(|(identity, _)| {
            matched(identity)
                .then(|| after.iter().position(|(other, _)| other == identity))
                .flatten()
        })
        .collect();
    let mut before: Vec<Option<Layer<'a>>> =
        before.into_iter().map(|(_, layer)| Some(layer)).collect();
    // Old layers only in the old scene, each after the matched layer below
    // it there (or first).
    let mut follows: Vec<Vec<usize>> = vec![Vec::new(); after.len() + 1];
    let mut below = after.len();
    for (i, partner) in partner.iter().enumerate() {
        match partner {
            Some(j) => below = *j,
            None => follows[below].push(i),
        }
    }
    let mut out = Vec::new();
    let fade_out =
        |out: &mut Vec<Layer<'a>>, indices: &[usize], before: &mut [Option<Layer<'a>>]| {
            for &i in indices {
                if let Some(mut layer) = before[i].take() {
                    layer.opacity *= 1.0 - t;
                    out.push(layer);
                }
            }
        };
    fade_out(&mut out, &follows[after.len()], &mut before);
    for (j, (_, layer)) in after.into_iter().enumerate() {
        let from = partner
            .iter()
            .position(|partner| *partner == Some(j))
            .and_then(|i| before[i].take());
        match from {
            Some(from) => out.push(between(&from, layer, t)),
            None => {
                let mut layer = layer;
                layer.opacity *= t;
                out.push(layer);
            }
        }
        fade_out(&mut out, &follows[j], &mut before);
    }
    out
}

/// `to` with its place, size, crop, opacity and style `t` of the way from
/// `from`'s.
fn between<'a>(from: &Layer<'_>, to: Layer<'a>, t: f32) -> Layer<'a> {
    let mix = |a: f32, b: f32| a + (b - a) * t;
    let mix4 = |a: [f32; 4], b: [f32; 4]| [0, 1, 2, 3].map(|i| mix(a[i], b[i]));
    let (size, transform) = moved_box((from.transform, from.size), (to.transform, to.size), t);
    // A clip in both scenes moves with it; one in either stays.
    let clip = match (from.clip, to.clip) {
        (Some(a), Some(b)) => {
            let (size, transform) = moved_box((a.transform, a.size), (b.transform, b.size), t);
            Some(LayerClip {
                transform,
                size,
                radius: mix4(a.radius, b.radius),
            })
        }
        (a, b) => b.or(a),
    };
    let crop = |a: u32, b: u32| mix(a as f32, b as f32).round().max(0.0) as u32;
    let clear = |color: [f32; 4]| [color[0], color[1], color[2], 0.0];
    let background = match (from.background, to.background) {
        (None, None) => None,
        (a, b) => {
            let (a, b) = (
                a.unwrap_or_else(|| clear(b.unwrap_or_default())),
                b.unwrap_or_else(|| clear(a.unwrap_or_default())),
            );
            Some(mix4(a, b))
        }
    };
    let border = match (from.border, to.border) {
        (None, None) => None,
        (a, b) => {
            let (wa, ca) = a.unwrap_or((0.0, b.map_or([0.0; 4], |b| b.1)));
            let (wb, cb) = b.unwrap_or((0.0, ca));
            Some((mix(wa, wb), mix4(ca, cb)))
        }
    };
    let shadow = match (from.shadow, to.shadow) {
        (None, None) => None,
        (a, b) => {
            let none = |other: LayerShadow| LayerShadow {
                color: clear(other.color),
                ..other
            };
            let a = a.unwrap_or_else(|| none(b.expect("one shadow")));
            let b = b.unwrap_or_else(|| none(a));
            Some(LayerShadow {
                offset: [mix(a.offset[0], b.offset[0]), mix(a.offset[1], b.offset[1])],
                blur: mix(a.blur, b.blur),
                color: mix4(a.color, b.color),
            })
        }
    };
    Layer {
        crop: isocast_protocol::Crop {
            left: crop(from.crop.left, to.crop.left),
            top: crop(from.crop.top, to.crop.top),
            right: crop(from.crop.right, to.crop.right),
            bottom: crop(from.crop.bottom, to.crop.bottom),
        },
        size,
        transform,
        opacity: mix(from.opacity, to.opacity),
        background,
        radius: mix4(from.radius, to.radius),
        border,
        shadow,
        clip,
        ..to
    }
}

/// A box (its transform and size) `t` of the way from one place to
/// another: moving its centre, turning the shorter way round, scaling and
/// resizing. A box flipped at only one end turns over about its upright
/// axis, edge on halfway.
fn moved_box(from: (Affine, [f32; 2]), to: (Affine, [f32; 2]), t: f32) -> ([f32; 2], Affine) {
    let mix = |a: f32, b: f32| a + (b - a) * t;
    let (a, b) = (Placement::of(from.0, from.1), Placement::of(to.0, to.1));
    let mut turn = b.angle - a.angle;
    if turn > std::f32::consts::PI {
        turn -= std::f32::consts::TAU;
    } else if turn < -std::f32::consts::PI {
        turn += std::f32::consts::TAU;
    }
    let size = [mix(from.1[0], to.1[0]), mix(from.1[1], to.1[1])];
    let center = [mix(a.center[0], b.center[0]), mix(a.center[1], b.center[1])];
    let transform = Affine::translate(center[0], center[1])
        .after(Affine::rotate((a.angle + turn * t).to_degrees()))
        .after(Affine::scale(mix(a.scale, b.scale)))
        .after(Affine::scale_xy(mix(a.across, b.across), 1.0))
        .after(Affine::translate(-size[0] / 2.0, -size[1] / 2.0));
    (size, transform)
}

/// Where a layer is on the canvas: its centre, how far it is turned
/// (radians, clockwise), how much it is scaled, and whether it is
/// mirrored left to right before that (`across` −1, else 1). Layers are
/// only moved, turned, scaled evenly and flipped, so this describes their
/// transform whole: a flip top to bottom reads as one left to right,
/// turned half way round.
struct Placement {
    center: [f32; 2],
    angle: f32,
    scale: f32,
    across: f32,
}

impl Placement {
    fn of(transform: Affine, size: [f32; 2]) -> Self {
        let [a, b, tx, c, d, ty] = transform.0;
        let (w, h) = (size[0] / 2.0, size[1] / 2.0);
        // A mirror negates the first column (a, c), so undo it before
        // reading the angle.
        let across = if a * d - b * c < 0.0 { -1.0 } else { 1.0 };
        Self {
            center: [a * w + b * h + tx, c * w + d * h + ty],
            angle: (across * c).atan2(across * a),
            scale: a.hypot(c),
            across,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use isocast_protocol::{Align, Blend, Crop, Fit, ScaleFilter};

    fn layer(x: f32, y: f32, size: f32, rotate: f32, opacity: f32) -> Layer<'static> {
        Layer {
            frame: None,
            crop: Crop::default(),
            fit: Fit::Stretch,
            align: Align::Center,
            scale_filter: ScaleFilter::Bilinear,
            blend: Blend::Normal,
            size: [size, size],
            transform: Affine::translate(x + size / 2.0, y + size / 2.0)
                .after(Affine::rotate(rotate))
                .after(Affine::translate(-size / 2.0, -size / 2.0)),
            opacity,
            background: Some([1.0, 0.0, 0.0, 1.0]),
            radius: [0.0; 4],
            border: None,
            shadow: None,
            shift: [0.0; 2],
            clip: None,
        }
    }

    fn id(key: &str) -> Identity {
        Identity {
            key: Some(key.into()),
            source: Some(key.into()),
            scene: None,
        }
    }

    #[test]
    fn matched_items_move_between_their_places() {
        let before = vec![(id("a"), layer(0.0, 0.0, 100.0, 0.0, 1.0))];
        let after = vec![(id("a"), layer(200.0, 100.0, 50.0, 90.0, 0.5))];
        let out = moved(before, after, 0.5);
        assert_eq!(out.len(), 1);
        let place = Placement::of(out[0].transform, out[0].size);
        // Centres (50, 50) and (225, 125); sizes 100 and 50.
        assert!((place.center[0] - 137.5).abs() < 1e-3, "{:?}", place.center);
        assert!((place.center[1] - 87.5).abs() < 1e-3, "{:?}", place.center);
        assert!((place.angle.to_degrees() - 45.0).abs() < 1e-3);
        assert_eq!(out[0].size, [75.0, 75.0]);
        assert_eq!(out[0].opacity, 0.75);
    }

    /// A 100-pixel `layer` at `x`, turned `rotate` degrees and flipped
    /// about its centre.
    fn flipped(x: f32, rotate: f32, horizontal: bool, vertical: bool) -> Layer<'static> {
        let mut out = layer(x, 0.0, 100.0, 0.0, 1.0);
        out.transform = Affine::translate(x + 50.0, 50.0)
            .after(Affine::rotate(rotate))
            .after(Affine::flip(horizontal, vertical))
            .after(Affine::translate(-50.0, -50.0));
        out
    }

    fn determinant(transform: Affine) -> f32 {
        let [a, b, _, c, d, _] = transform.0;
        a * d - b * c
    }

    #[test]
    fn flipped_items_stay_flipped_as_they_move() {
        // Left to right at one end, top to bottom (and turned) at the other.
        let from = || flipped(0.0, 0.0, true, false);
        let to = || flipped(200.0, 90.0, false, true);
        let at = |t| moved(vec![(id("a"), from())], vec![(id("a"), to())], t)[0].transform;
        for (t, want) in [(0.0, from().transform), (1.0, to().transform)] {
            let got = at(t);
            for (g, w) in got.0.iter().zip(want.0) {
                assert!((g - w).abs() < 1e-3, "at {t}: {got:?}");
            }
        }
        assert!(determinant(at(0.5)) < 0.0, "{:?}", at(0.5));
    }

    #[test]
    fn an_item_flipped_at_one_end_turns_over() {
        let at = |t| {
            let before = vec![(id("a"), flipped(0.0, 0.0, false, false))];
            let after = vec![(id("a"), flipped(0.0, 0.0, true, false))];
            determinant(moved(before, after, t)[0].transform)
        };
        assert!(at(0.25) > 0.0);
        assert!(at(0.5).abs() < 1e-6, "edge on halfway");
        assert!(at(0.75) < 0.0);
    }

    #[test]
    fn unmatched_items_fade_and_keep_their_place_in_the_order() {
        // Old: bg-old, a, logo; new: bg-new, a.
        let before = vec![
            (id("bg-old"), layer(0.0, 0.0, 10.0, 0.0, 1.0)),
            (id("a"), layer(0.0, 0.0, 10.0, 0.0, 1.0)),
            (id("logo"), layer(0.0, 0.0, 10.0, 0.0, 1.0)),
        ];
        let after = vec![
            (id("bg-new"), layer(0.0, 0.0, 10.0, 0.0, 1.0)),
            (id("a"), layer(0.0, 0.0, 10.0, 0.0, 1.0)),
        ];
        let out = moved(before, after, 0.25);
        let opacities: Vec<f32> = out.iter().map(|l| l.opacity).collect();
        // bg-old fading out first, bg-new fading in, a, then the logo
        // still above a.
        assert_eq!(opacities, [0.75, 0.25, 1.0, 0.75]);
    }

    #[test]
    fn the_same_key_showing_something_else_is_not_matched() {
        let mut other = id("a");
        other.source = Some("b".into());
        let before = vec![(id("a"), layer(0.0, 0.0, 10.0, 0.0, 1.0))];
        let after = vec![(other, layer(50.0, 0.0, 10.0, 0.0, 1.0))];
        let out = moved(before, after, 0.5);
        assert_eq!(out.len(), 2);
    }
}
