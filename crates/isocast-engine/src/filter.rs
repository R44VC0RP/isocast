//! Effect filters on the GPU. A source with filters gets a canvas for its
//! frame (premultiplied RGBA) and one for each enabled filter, which is a
//! full-screen pass from the canvas before; the last is shown in place of
//! the frame. Images and tables that filters read are loaded once.
//!
//! To add a filter: its settings in `isocast_protocol::filter`, a fragment
//! shader in [`SHADER`] and its entry in [`pass`].

use std::collections::{HashMap, VecDeque};
use std::f32::consts::PI;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use isocast_protocol::{
    ColorCorrection, CropPad, Filter, FilterKind, KeyAdjust, MaskMode, Scaling, Scroll,
};
use wgpu::util::DeviceExt;

use crate::Time;
use crate::gpu::{Gpu, GpuFrame, GpuPixels};
use crate::picture::{self, Picture};
use crate::record::expand_home;
use crate::render::{KERNELS, Layer, Renderer, scale_method};
use crate::scene::canvas_texture;

/// How many vec4 parameters a pass gets.
const PARAMS: usize = 8;

const SHADER: &str = r#"
struct Params {
    v: array<vec4<f32>, 8>,
}

@group(0) @binding(0) var input: texture_2d<f32>;
@group(0) @binding(1) var smp: sampler;
@group(0) @binding(2) var<uniform> params: Params;
// Like `smp` but wrapping around, for scrolling.
@group(0) @binding(3) var wrap: sampler;
// A mask image (straight alpha), and a colour lookup table.
@group(0) @binding(4) var mask: texture_2d<f32>;
@group(0) @binding(5) var lut: texture_3d<f32>;

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

fn straight(c: vec4<f32>) -> vec4<f32> {
    return select(vec4<f32>(0.0), vec4<f32>(c.rgb / c.a, c.a), c.a > 0.0);
}

fn premultiplied(c: vec4<f32>) -> vec4<f32> {
    let clamped = clamp(c, vec4<f32>(0.0), vec4<f32>(1.0));
    return vec4<f32>(clamped.rgb * clamped.a, clamped.a);
}

// v0: gamma exponent, contrast factor, brightness, saturation factor;
// v1..v3: rows of the hue rotation; v4: multiply; v5: add; v6.x: opacity.
@fragment
fn color_correction(in: Varyings) -> @location(0) vec4<f32> {
    let c = straight(textureSample(input, smp, in.uv));
    let p = params.v;
    var rgb = pow(max(c.rgb, vec3<f32>(0.0)), vec3<f32>(p[0].x));
    rgb = (rgb - 0.5) * p[0].y + 0.5 + p[0].z;
    let luma = dot(rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    rgb = mix(vec3<f32>(luma), rgb, p[0].w);
    rgb = vec3<f32>(dot(p[1].xyz, rgb), dot(p[2].xyz, rgb), dot(p[3].xyz, rgb));
    rgb = rgb * p[4].rgb + p[5].rgb;
    return premultiplied(vec4<f32>(rgb, c.a * p[4].a * p[6].x));
}

const LUMA: vec3<f32> = vec3<f32>(0.2126, 0.7152, 0.0722);

// BT.709 Cb and Cr, each −0.5 to 0.5.
fn chroma(rgb: vec3<f32>) -> vec2<f32> {
    let y = dot(rgb, LUMA);
    return vec2<f32>((rgb.b - y) / 1.8556, (rgb.r - y) / 1.5748);
}

fn from_ycbcr(y: f32, c: vec2<f32>) -> vec3<f32> {
    let r = y + 1.5748 * c.y;
    let b = y + 1.8556 * c.x;
    return vec3<f32>(r, (y - 0.2126 * r - 0.0722 * b) / 0.7152, b);
}

// The keys' picture adjustments: (gamma exponent, contrast factor,
// brightness, unused).
fn adjust(rgb: vec3<f32>, p: vec4<f32>) -> vec3<f32> {
    let lifted = pow(max(rgb, vec3<f32>(0.0)), vec3<f32>(p.x));
    return (lifted - 0.5) * p.y + 0.5 + p.z;
}

// v0: key Cb, key Cr, similarity, smoothness; v1.x: spill, v1.y: opacity;
// v2: adjustments.
@fragment
fn chroma_key(in: Varyings) -> @location(0) vec4<f32> {
    let c = straight(textureSample(input, smp, in.uv));
    let p = params.v;
    let key = p[0].xy;
    var ch = chroma(c.rgb);
    let d = distance(ch, key);
    let keep = smoothstep(p[0].z, p[0].z + p[0].w, d);
    // Near the key, take the key's direction out of what remains.
    let near = 1.0 - smoothstep(p[0].z, p[0].z + p[0].w + p[1].x, d);
    if (length(key) > 1e-4) {
        let k = normalize(key);
        ch = ch - k * max(dot(ch, k), 0.0) * near;
    }
    let rgb = adjust(from_ycbcr(dot(c.rgb, LUMA), ch), p[2]);
    return premultiplied(vec4<f32>(rgb, c.a * keep * p[1].y));
}

// v0: key RGB, similarity; v1.x: smoothness, v1.y: opacity; v2:
// adjustments.
@fragment
fn color_key(in: Varyings) -> @location(0) vec4<f32> {
    let c = straight(textureSample(input, smp, in.uv));
    let p = params.v;
    let d = distance(c.rgb, p[0].rgb);
    let keep = smoothstep(p[0].w, p[0].w + p[1].x, d);
    return premultiplied(vec4<f32>(adjust(c.rgb, p[2]), c.a * keep * p[1].y));
}

// The input's texel at `at`, clamped to its edges.
fn texel(at: vec2<i32>) -> vec4<f32> {
    let size = vec2<i32>(textureDimensions(input));
    return textureLoad(input, clamp(at, vec2<i32>(0), size - 1), 0);
}

// v0.xy: the output's size; v0.zw: where its top-left is in the input,
// in texels. Outside the input is transparent.
@fragment
fn crop_pad(in: Varyings) -> @location(0) vec4<f32> {
    let p = params.v[0];
    let at = floor(in.uv * p.xy) + p.zw;
    let size = vec2<f32>(textureDimensions(input));
    if (any(at < vec2<f32>(0.0)) || any(at >= size)) {
        return vec4<f32>(0.0);
    }
    return textureLoad(input, vec2<i32>(at), 0);
}

// v0.x: method (0 point, 1 bilinear, 2 bicubic, 3 lanczos, 4 area); v0.y:
// the horizontal slope at the centre when undistorting (1: none); v0.zw:
// the output's size.
@fragment
fn scale(in: Varyings) -> @location(0) vec4<f32> {
    let p = params.v[0];
    var uv = in.uv;
    if (p.y < 1.0) {
        let x = uv.x * 2.0 - 1.0;
        uv.x = (p.y * x + (1.0 - p.y) * x * x * x + 1.0) * 0.5;
    }
    let size = vec2<f32>(textureDimensions(input));
    let method = i32(p.x + 0.5);
    let reach = size / p.zw * 0.5;
    if (method == 0) {
        return texel(vec2<i32>(floor(uv * size)));
    }
    if (method == 1 || (method == 4 && all(reach <= vec2<f32>(0.5)))) {
        return textureSampleLevel(input, smp, uv, 0.0);
    }
    var sum = vec4<f32>(0.0);
    var weight = 0.0;
    if (method == 4) {
        // Every texel the output pixel covers, by how much it covers it.
        let lo = uv * size - reach;
        let hi = uv * size + reach;
        let first = vec2<i32>(floor(lo));
        let count = min(vec2<i32>(ceil(hi)) - first, vec2<i32>(64));
        for (var j = 0; j < count.y; j++) {
            let y = f32(first.y + j);
            let wy = min(y + 1.0, hi.y) - max(y, lo.y);
            for (var i = 0; i < count.x; i++) {
                let x = f32(first.x + i);
                let w = (min(x + 1.0, hi.x) - max(x, lo.x)) * wy;
                sum += texel(first + vec2<i32>(i, j)) * w;
                weight += w;
            }
        }
        return sum / max(weight, 1e-6);
    }
    let at = uv * size - 0.5;
    let base = floor(at);
    let f = at - base;
    let taps = select(2, 3, method == 3);
    for (var j = 1 - taps; j <= taps; j++) {
        for (var i = 1 - taps; i <= taps; i++) {
            let d = vec2<f32>(f32(i), f32(j)) - f;
            var w = cubic(d.x) * cubic(d.y);
            if (method == 3) {
                w = lanczos(d.x) * lanczos(d.y);
            }
            sum += texel(vec2<i32>(base) + vec2<i32>(i, j)) * w;
            weight += w;
        }
    }
    // Sharp kernels overshoot; keep colours valid for their alpha.
    let c = sum / weight;
    let a = clamp(c.a, 0.0, 1.0);
    return vec4<f32>(clamp(c.rgb, vec3<f32>(0.0), vec3<f32>(a)), a);
}

// v0.xy: the output's size as a fraction of the input's; v0.zw: the
// shift, in input coordinates; v1.x: 1 to repeat the picture.
@fragment
fn scroll(in: Varyings) -> @location(0) vec4<f32> {
    let p = params.v;
    let at = in.uv * p[0].xy + p[0].zw;
    if (p[1].x < 0.5 && (any(at < vec2<f32>(0.0)) || any(at > vec2<f32>(1.0)))) {
        return vec4<f32>(0.0);
    }
    return textureSampleLevel(input, wrap, at, 0.0);
}

// v0.x: strength.
@fragment
fn sharpen(in: Varyings) -> @location(0) vec4<f32> {
    let at = vec2<i32>(floor(in.uv * vec2<f32>(textureDimensions(input))));
    let c = texel(at);
    let around = texel(at + vec2<i32>(1, 0)) + texel(at - vec2<i32>(1, 0))
        + texel(at + vec2<i32>(0, 1)) + texel(at - vec2<i32>(0, 1));
    let sharp = c.rgb + (c.rgb * 4.0 - around.rgb) * params.v[0].x;
    return vec4<f32>(clamp(sharp, vec3<f32>(0.0), vec3<f32>(c.a)), c.a);
}

// v0.x: mode (0 colour mask, 1 alpha mask, 2 multiply, 3 add, 4
// subtract); v0.y: opacity; v1: colour; v2.xy: the image's scale about
// its centre.
@fragment
fn image_mask(in: Varyings) -> @location(0) vec4<f32> {
    let c = straight(textureSample(input, smp, in.uv));
    let p = params.v;
    let m = textureSampleLevel(mask, smp, (in.uv - 0.5) * p[2].xy + 0.5, 0.0);
    let mode = i32(p[0].x + 0.5);
    let strength = p[0].y;
    var rgb = c.rgb;
    var a = c.a;
    if (mode == 0) {
        let weights = p[1].rgb / max(dot(p[1].rgb, vec3<f32>(1.0)), 1e-6);
        a = a * (1.0 - strength * (1.0 - dot(m.rgb, weights) * m.a));
    } else if (mode == 1) {
        a = a * (1.0 - strength * (1.0 - m.a));
    } else {
        let tint = m.rgb * p[1].rgb;
        let k = strength * m.a;
        if (mode == 2) {
            rgb = rgb * mix(vec3<f32>(1.0), tint, k);
        } else if (mode == 3) {
            rgb = rgb + tint * k;
        } else {
            rgb = rgb - tint * k;
        }
    }
    return premultiplied(vec4<f32>(rgb, a));
}

// v0.x: amount; v0.y: the table's size; v1.xyz, v2.xyz: its domain.
@fragment
fn apply_lut(in: Varyings) -> @location(0) vec4<f32> {
    let c = straight(textureSample(input, smp, in.uv));
    let p = params.v;
    let n = p[0].y;
    let t = clamp((c.rgb - p[1].xyz) / (p[2].xyz - p[1].xyz), vec3<f32>(0.0), vec3<f32>(1.0));
    let graded = textureSampleLevel(lut, smp, t * (n - 1.0) / n + 0.5 / n, 0.0).rgb;
    return premultiplied(vec4<f32>(mix(c.rgb, graded, p[0].x), c.a));
}

// v0: luma max, its smoothing, luma min, its smoothing.
@fragment
fn luma_key(in: Varyings) -> @location(0) vec4<f32> {
    let c = straight(textureSample(input, smp, in.uv));
    let p = params.v[0];
    let y = dot(c.rgb, LUMA);
    let below_max = select(f32(y <= p.x), 1.0 - smoothstep(p.x - p.y, p.x, y), p.y > 0.0);
    let above_min = select(f32(y >= p.z), smoothstep(p.z, p.z + p.w, y), p.w > 0.0);
    return premultiplied(vec4<f32>(c.rgb, c.a * below_max * above_min));
}
"#;

/// The fragment shaders, by entry point.
const PASSES: &[&str] = &[
    "color_correction",
    "chroma_key",
    "color_key",
    "luma_key",
    "crop_pad",
    "scale",
    "scroll",
    "sharpen",
    "image_mask",
    "apply_lut",
];

/// The filter passes for targets of one format, and each source's
/// canvases.
pub struct Filters {
    format: wgpu::TextureFormat,
    layout: wgpu::BindGroupLayout,
    pipelines: HashMap<&'static str, wgpu::RenderPipeline>,
    sampler: wgpu::Sampler,
    wrap: wgpu::Sampler,
    /// Each source's chain of passes, by source id.
    chains: HashMap<String, Chain>,
    /// When each source's filters first ran, which moving filters count
    /// from.
    started: HashMap<String, Time>,
    /// Images and tables filters read; `None` if one could not be loaded
    /// (its filter is skipped).
    assets: HashMap<File, Option<Asset>>,
    /// Bound when a pass reads no image or table.
    blank_image: wgpu::Texture,
    blank_table: wgpu::Texture,
    /// Recent pictures for render delays, by source id and pass; only
    /// kept when drawing over time (not for snapshots).
    delays: Option<HashMap<(String, usize), Delay>>,
}

/// One source's filters on the GPU.
#[derive(Default)]
struct Chain {
    /// Its frame, then each pass's output.
    canvases: Vec<wgpu::Texture>,
    /// Each drawing pass's parameters and bindings, kept from frame to
    /// frame (made again when the pass, its file or the canvases change).
    bound: Vec<Option<Bound>>,
}

/// What a pass reads: its parameters, input, and file.
struct Bound {
    entry: &'static str,
    file: Option<File>,
    params: wgpu::Buffer,
    group: wgpu::BindGroup,
}

/// The pictures a render delay holds back, oldest first, each with the
/// time it was drawn for, and spare canvases to reuse.
#[derive(Default)]
struct Delay {
    held: VecDeque<(Time, wgpu::Texture)>,
    spare: Vec<wgpu::Texture>,
}

/// One filter's pass: its fragment shader, parameters and output size,
/// and the file it reads, if any.
struct Pass {
    entry: &'static str,
    params: [[f32; 4]; PARAMS],
    size: (u32, u32),
    file: Option<File>,
    /// A render delay, which copies instead of drawing.
    delay: Option<Duration>,
}

/// A file a filter reads, by what it is read as.
#[derive(Clone, PartialEq, Eq, Hash)]
enum File {
    Image(String),
    /// A colour lookup table (.cube or .png).
    Table(String),
}

/// A file a filter reads, on the GPU.
enum Asset {
    Image(wgpu::Texture),
    /// A 3D colour table, its size, and the input range it covers.
    Table {
        texture: wgpu::Texture,
        size: u32,
        domain: [[f32; 3]; 2],
    },
}

impl Filters {
    /// Passes for targets of `format`. `over_time`: frames are drawn one
    /// after another (the program), so render delays hold pictures back;
    /// otherwise (snapshots) they are skipped.
    pub fn new(gpu: &Gpu, format: wgpu::TextureFormat, over_time: bool) -> Self {
        let device = &gpu.device;
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("filters"),
            source: wgpu::ShaderSource::Wgsl(format!("{SHADER}{KERNELS}").into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("filter"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 5,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D3,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("filter"),
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
            label: Some("filter"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let wrap = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("filter wrap"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Self {
            format,
            layout,
            pipelines,
            sampler,
            wrap,
            chains: HashMap::new(),
            started: HashMap::new(),
            assets: HashMap::new(),
            blank_image: upload_image(gpu, 1, 1, &[255; 4]),
            blank_table: upload_table(gpu, 1, &[255; 4]),
            delays: over_time.then(HashMap::new),
        }
    }

    /// `file` on the GPU, loaded on first use; `None` (said once) if it
    /// can't be.
    fn asset(&mut self, gpu: &Gpu, file: &File) -> Option<&Asset> {
        self.assets
            .entry(file.clone())
            .or_insert_with(|| match load_asset(gpu, file) {
                Ok(asset) => Some(asset),
                Err(err) => {
                    isocast_diagnostics::diagnostic!("warn"; "isocastd: a filter is skipped: {err:#}");
                    None
                }
            })
            .as_ref()
    }

    /// `frame` of source `id`, shown at `at`, with its enabled `filters`
    /// applied in order; `frame` itself if none is on. `renderer` draws
    /// for targets of this format.
    pub fn apply(
        &mut self,
        gpu: &Gpu,
        renderer: &Renderer,
        id: &str,
        frame: GpuFrame,
        filters: &[Filter],
        at: Time,
    ) -> GpuFrame {
        let started = *self.started.entry(id.to_string()).or_insert(at);
        let seconds = at.saturating_since(started).as_secs_f64();
        let mut sizes = vec![(frame.width, frame.height)];
        let mut passes = Vec::new();
        for filter in filters.iter().filter(|filter| filter.enabled) {
            let Some(mut pass) = pass(&filter.kind, sizes[sizes.len() - 1], seconds) else {
                continue;
            };
            if pass.delay.is_some() && self.delays.is_none() {
                continue;
            }
            if let Some(file) = &pass.file {
                match self.asset(gpu, file) {
                    // Passes that read files keep the size, so skipping one
                    // changes no other.
                    None => continue,
                    Some(Asset::Image(image)) => {
                        if pass.params[2] == [0.0; 4] {
                            pass.params[2] = cover(pass.size, (image.width(), image.height()));
                        }
                    }
                    Some(Asset::Table { size, domain, .. }) => {
                        pass.params[0][1] = *size as f32;
                        pass.params[1] = [domain[0][0], domain[0][1], domain[0][2], 0.0];
                        pass.params[2] = [domain[1][0], domain[1][1], domain[1][2], 0.0];
                    }
                }
            }
            sizes.push(pass.size);
            passes.push(pass);
        }
        if passes.is_empty() {
            return frame;
        }
        let format = self.format;
        let chain = self.chains.entry(id.to_string()).or_default();
        let fits = chain.canvases.len() == sizes.len()
            && (chain.canvases.iter())
                .zip(&sizes)
                .all(|(canvas, &size)| (canvas.width(), canvas.height()) == size);
        if !fits {
            chain.canvases = sizes
                .iter()
                .map(|&size| canvas_texture(gpu, format, size))
                .collect();
            chain.bound.clear();
        }
        let canvases = chain.canvases.clone();
        // The frame and every pass go to the GPU in one submission.
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        let layer = Layer::full(&frame, sizes[0].0, sizes[0].1);
        renderer.encode(
            gpu,
            &mut encoder,
            &canvases[0],
            &[layer],
            sizes[0],
            wgpu::Color::TRANSPARENT,
        );
        for (i, pass) in passes.iter().enumerate() {
            match pass.delay {
                Some(delay) => {
                    let key = (id.to_string(), i);
                    self.hold_back(
                        gpu,
                        &mut encoder,
                        key,
                        at,
                        delay,
                        &canvases[i],
                        &canvases[i + 1],
                    );
                }
                None => {
                    let group = self.bind(gpu, id, i, pass, &canvases[i]);
                    encode_pass(
                        &mut encoder,
                        pass.entry,
                        &self.pipelines[pass.entry],
                        &group,
                        &canvases[i + 1],
                    );
                }
            }
        }
        gpu.queue.submit([encoder.finish()]);
        let (width, height) = sizes[sizes.len() - 1];
        GpuFrame {
            width,
            height,
            pixels: GpuPixels::Premultiplied(canvases[canvases.len() - 1].clone()),
        }
    }

    /// Keep a copy of `from`, drawn for `at`, and copy into `to` the
    /// picture held from `delay` earlier (the oldest until there is one).
    #[allow(clippy::too_many_arguments)]
    fn hold_back(
        &mut self,
        gpu: &Gpu,
        encoder: &mut wgpu::CommandEncoder,
        key: (String, usize),
        at: Time,
        delay: Duration,
        from: &wgpu::Texture,
        to: &wgpu::Texture,
    ) {
        let format = self.format;
        let Some(held) = self
            .delays
            .as_mut()
            .map(|delays| delays.entry(key).or_default())
        else {
            return;
        };
        let size = (from.width(), from.height());
        let fits = |texture: &wgpu::Texture| (texture.width(), texture.height()) == size;
        if held.held.iter().any(|(_, texture)| !fits(texture)) {
            *held = Delay::default();
        }
        // A picture is drawn more than once for the same time during a
        // fade; keep one copy.
        if held.held.back().is_none_or(|(time, _)| *time < at) {
            let copy = match held.spare.pop() {
                Some(texture) if fits(&texture) => texture,
                _ => canvas_texture(gpu, format, size),
            };
            encoder.copy_texture_to_texture(
                from.as_image_copy(),
                copy.as_image_copy(),
                from.size(),
            );
            held.held.push_back((at, copy));
        }
        // The newest picture at least `delay` old; older ones are spare.
        let due = at - delay;
        while held.held.len() > 1 && held.held[1].0 <= due {
            let (_, texture) = held.held.pop_front().expect("two held");
            held.spare.push(texture);
        }
        let (_, shown) = &held.held[0];
        encoder.copy_texture_to_texture(shown.as_image_copy(), to.as_image_copy(), to.size());
    }

    /// What pass `index` of source `id`'s chain reads, with `pass`'s
    /// parameters: kept from the last frame and updated, or made.
    fn bind(
        &mut self,
        gpu: &Gpu,
        id: &str,
        index: usize,
        pass: &Pass,
        from: &wgpu::Texture,
    ) -> wgpu::BindGroup {
        let uniform: Vec<u8> = pass
            .params
            .iter()
            .flatten()
            .flat_map(|v| v.to_ne_bytes())
            .collect();
        let chain = self.chains.get_mut(id).expect("made by apply");
        if chain.bound.len() <= index {
            chain.bound.resize_with(index + 1, || None);
        }
        if let Some(bound) = &chain.bound[index]
            && bound.entry == pass.entry
            && bound.file == pass.file
        {
            gpu.queue.write_buffer(&bound.params, 0, &uniform);
            return bound.group.clone();
        }
        let params = gpu
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("filter"),
                contents: &uniform,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            });
        let input = from.create_view(&Default::default());
        let (mut image, mut table) = (&self.blank_image, &self.blank_table);
        match pass
            .file
            .as_ref()
            .and_then(|file| self.assets.get(file)?.as_ref())
        {
            Some(Asset::Image(texture)) => image = texture,
            Some(Asset::Table { texture, .. }) => table = texture,
            None => {}
        }
        let image = image.create_view(&Default::default());
        let table = table.create_view(&Default::default());
        let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("filter"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&input),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: params.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&self.wrap),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(&image),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::TextureView(&table),
                },
            ],
        });
        let chain = self.chains.get_mut(id).expect("made by apply");
        chain.bound[index] = Some(Bound {
            entry: pass.entry,
            file: pass.file.clone(),
            params,
            group: group.clone(),
        });
        group
    }
}

/// Draw one pass, reading what `group` binds, into `to`.
fn encode_pass(
    encoder: &mut wgpu::CommandEncoder,
    entry: &'static str,
    pipeline: &wgpu::RenderPipeline,
    group: &wgpu::BindGroup,
    to: &wgpu::Texture,
) {
    let view = to.create_view(&Default::default());
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some(entry),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: &view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                store: wgpu::StoreOp::Store,
            },
        })],
        ..Default::default()
    });
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, group, &[]);
    pass.draw(0..3, 0..1);
}

/// The size of a picture of `size` after `filters` (those that are on).
pub fn output_size(filters: &[Filter], size: (u32, u32)) -> (u32, u32) {
    filters
        .iter()
        .filter(|filter| filter.enabled)
        .fold(size, |size, filter| {
            pass(&filter.kind, size, 0.0).map_or(size, |pass| pass.size)
        })
}

/// The pass that runs `kind` on a picture of `size`, `seconds` after the
/// source's filters started; `None` if it would change nothing.
fn pass(kind: &FilterKind, size: (u32, u32), seconds: f64) -> Option<Pass> {
    let (entry, params) = match kind {
        FilterKind::ColorCorrection(settings) => ("color_correction", color_correction(settings)),
        FilterKind::ChromaKey(settings) => {
            let [r, g, b] = settings.key_color_type.rgb(&settings.key_color);
            let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            let mut params = [[0.0; 4]; PARAMS];
            params[0] = [
                (b - y) / 1.8556,
                (r - y) / 1.5748,
                settings.similarity as f32 / 1000.0,
                settings.smoothness as f32 / 1000.0,
            ];
            params[1] = [settings.spill as f32 / 1000.0, settings.opacity, 0.0, 0.0];
            params[2] = key_adjust(&settings.adjust);
            ("chroma_key", params)
        }
        FilterKind::ColorKey(settings) => {
            let [r, g, b] = settings.key_color_type.rgb(&settings.key_color);
            let mut params = [[0.0; 4]; PARAMS];
            params[0] = [r, g, b, settings.similarity as f32 / 1000.0];
            params[1] = [
                settings.smoothness as f32 / 1000.0,
                settings.opacity,
                0.0,
                0.0,
            ];
            params[2] = key_adjust(&settings.adjust);
            ("color_key", params)
        }
        FilterKind::LumaKey(settings) => {
            let mut params = [[0.0; 4]; PARAMS];
            params[0] = [
                settings.luma_max,
                settings.luma_max_smooth,
                settings.luma_min,
                settings.luma_min_smooth,
            ];
            ("luma_key", params)
        }
        FilterKind::Sharpen(settings) => {
            let mut params = [[0.0; 4]; PARAMS];
            params[0][0] = settings.sharpness * 2.0;
            ("sharpen", params)
        }
        FilterKind::ImageMask(settings) => {
            let mode = match settings.mode {
                MaskMode::AlphaMaskColor => 0.0,
                MaskMode::AlphaMaskAlpha => 1.0,
                MaskMode::BlendMultiply => 2.0,
                MaskMode::BlendAdd => 3.0,
                MaskMode::BlendSubtract => 4.0,
            };
            let mut params = [[0.0; 4]; PARAMS];
            params[0] = [mode, settings.opacity, 0.0, 0.0];
            params[1] = settings.color.rgba().unwrap_or([1.0; 4]);
            // Otherwise the image's scale is worked out once it is loaded.
            if settings.stretch {
                params[2] = [1.0, 1.0, 0.0, 0.0];
            }
            return Some(Pass {
                entry: "image_mask",
                params,
                size,
                file: Some(File::Image(settings.path.clone())),
                delay: None,
            });
        }
        FilterKind::ApplyLut(settings) => {
            let mut params = [[0.0; 4]; PARAMS];
            params[0][0] = settings.amount;
            return Some(Pass {
                entry: "apply_lut",
                params,
                size,
                file: Some(File::Table(settings.path.clone())),
                delay: None,
            });
        }
        FilterKind::RenderDelay(settings) => {
            if settings.delay_ms == 0 {
                return None;
            }
            return Some(Pass {
                entry: "render_delay",
                params: [[0.0; 4]; PARAMS],
                size,
                file: None,
                delay: Some(Duration::from_millis(settings.delay_ms.into())),
            });
        }
        FilterKind::CropPad(settings) => return Some(crop_pad(settings, size)),
        FilterKind::Scaling(settings) => return scaling(settings, size),
        FilterKind::Scroll(settings) => return Some(scroll(settings, size, seconds)),
    };
    Some(Pass {
        entry,
        params,
        size,
        file: None,
        delay: None,
    })
}

/// How an image of `image` size is scaled about its centre to cover a
/// picture of `size`, keeping its shape.
fn cover(size: (u32, u32), image: (u32, u32)) -> [f32; 4] {
    let picture = size.0 as f32 / size.1 as f32;
    let shape = image.0 as f32 / image.1 as f32;
    if shape > picture {
        [picture / shape, 1.0, 0.0, 0.0]
    } else {
        [1.0, shape / picture, 0.0, 0.0]
    }
}

fn load_asset(gpu: &Gpu, file: &File) -> Result<Asset> {
    let (File::Image(path) | File::Table(path)) = file;
    let full = expand_home(path);
    let shown = full.display();
    if let File::Table(_) = file {
        let (size, domain, rgba) = if path.to_ascii_lowercase().ends_with(".cube") {
            let text = std::fs::read_to_string(&full)
                .with_context(|| format!("could not read the LUT {shown}"))?;
            parse_cube(&text).with_context(|| format!("{shown} is not a 3D .cube LUT"))?
        } else {
            let picture = first_picture(&full)?;
            let (size, rgba) =
                png_table(&picture).with_context(|| format!("{shown} is not a LUT"))?;
            (size, [[0.0; 3], [1.0; 3]], rgba)
        };
        return Ok(Asset::Table {
            texture: upload_table(gpu, size, &rgba),
            size,
            domain,
        });
    }
    let picture = first_picture(&full)?;
    Ok(Asset::Image(upload_image(
        gpu,
        picture.width,
        picture.height,
        &picture.rgba,
    )))
}

fn first_picture(path: &std::path::Path) -> Result<Picture> {
    let frames = picture::load(path, None)?;
    let (picture, _) = frames
        .into_iter()
        .next()
        .context("the image has no frames")?;
    Ok(picture)
}

/// A .cube file's table: its size, domain and RGBA bytes, red fastest.
fn parse_cube(text: &str) -> Result<(u32, [[f32; 3]; 2], Vec<u8>)> {
    fn three<'a>(words: impl Iterator<Item = &'a str>) -> Result<[f32; 3]> {
        let values: Vec<f32> = words.map(str::parse).collect::<Result<_, _>>()?;
        values
            .try_into()
            .map_err(|_| anyhow::anyhow!("expected three numbers"))
    }
    let mut size = None;
    let mut domain = [[0.0; 3], [1.0; 3]];
    let mut rgba = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') || line.starts_with("TITLE") {
            continue;
        }
        let mut words = line.split_whitespace();
        match words.next().unwrap_or_default() {
            "LUT_3D_SIZE" => size = Some(words.next().context("no size")?.parse::<u32>()?),
            "LUT_1D_SIZE" => bail!("1D LUTs are not supported"),
            "DOMAIN_MIN" => domain[0] = three(words)?,
            "DOMAIN_MAX" => domain[1] = three(words)?,
            _ => {
                let [r, g, b] = three(line.split_whitespace())
                    .with_context(|| format!("unexpected line {line:?}"))?;
                let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
                rgba.extend([byte(r), byte(g), byte(b), 255]);
            }
        }
    }
    let size = size.context("no LUT_3D_SIZE")?;
    if !(2..=256).contains(&size) || rgba.len() != (size.pow(3) * 4) as usize {
        bail!("a size-{size} table needs {} entries", size.pow(3));
    }
    Ok((size, domain, rgba))
}

/// A .png LUT's table: tiles of red across and green down, one tile per
/// blue level, left to right and top to bottom (512×512 for 64 levels, or
/// a strip like 1024×32).
fn png_table(picture: &Picture) -> Result<(u32, Vec<u8>)> {
    let (w, h) = (picture.width, picture.height);
    let size = (f64::from(w) * f64::from(h)).cbrt().round() as u32;
    if size < 2 || size.pow(3) != w * h || w % size != 0 || h % size != 0 {
        bail!("{w}×{h} holds no cube of colours");
    }
    let across = w / size;
    let mut rgba = Vec::with_capacity((size.pow(3) * 4) as usize);
    for b in 0..size {
        for g in 0..size {
            for r in 0..size {
                let x = (b % across) * size + r;
                let y = (b / across) * size + g;
                let i = ((y * w + x) * 4) as usize;
                rgba.extend([
                    picture.rgba[i],
                    picture.rgba[i + 1],
                    picture.rgba[i + 2],
                    255,
                ]);
            }
        }
    }
    Ok((size, rgba))
}

pub(crate) fn upload_image(gpu: &Gpu, width: u32, height: u32, rgba: &[u8]) -> wgpu::Texture {
    upload(gpu, (width, height, 1), wgpu::TextureDimension::D2, rgba)
}

fn upload_table(gpu: &Gpu, size: u32, rgba: &[u8]) -> wgpu::Texture {
    upload(gpu, (size, size, size), wgpu::TextureDimension::D3, rgba)
}

fn upload(
    gpu: &Gpu,
    (width, height, depth): (u32, u32, u32),
    dimension: wgpu::TextureDimension,
    rgba: &[u8],
) -> wgpu::Texture {
    gpu.device.create_texture_with_data(
        &gpu.queue,
        &wgpu::TextureDescriptor {
            label: Some("filter file"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: depth,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        },
        wgpu::util::TextureDataOrder::LayerMajor,
        rgba,
    )
}

fn crop_pad(settings: &CropPad, (width, height): (u32, u32)) -> Pass {
    let (width, height) = (width as i32, height as i32);
    let (x, y, w, h) = match settings.area {
        Some(area) => {
            let reach = |size: u32, edge: i32, whole: i32| match size {
                0 => whole - edge,
                size => size as i32,
            };
            (
                area.x,
                area.y,
                reach(area.width, area.x, width),
                reach(area.height, area.y, height),
            )
        }
        None => (
            settings.left,
            settings.top,
            width - settings.left - settings.right,
            height - settings.top - settings.bottom,
        ),
    };
    let size = (w.max(1) as u32, h.max(1) as u32);
    let mut params = [[0.0; 4]; PARAMS];
    params[0] = [size.0 as f32, size.1 as f32, x as f32, y as f32];
    Pass {
        entry: "crop_pad",
        params,
        size,
        file: None,
        delay: None,
    }
}

fn scaling(settings: &Scaling, (width, height): (u32, u32)) -> Option<Pass> {
    let size = match (settings.width, settings.height, settings.ratio()) {
        (Some(w), Some(h), _) => (w, h),
        (_, _, Some(ratio)) => (((height as f64 * ratio).round() as u32).max(1), height),
        _ => return None,
    };
    if size == (width, height) {
        return None;
    }
    let method = scale_method(settings.scale_filter);
    // Narrowing, the middle keeps its proportions if it is squeezed as
    // little as the height is.
    let slope = (height as f32 / size.1 as f32) * (size.0 as f32 / width as f32);
    let slope = if settings.undistort {
        slope.min(1.0)
    } else {
        1.0
    };
    let mut params = [[0.0; 4]; PARAMS];
    params[0] = [method, slope, size.0 as f32, size.1 as f32];
    Some(Pass {
        entry: "scale",
        params,
        size,
        file: None,
        delay: None,
    })
}

fn scroll(settings: &Scroll, (width, height): (u32, u32), seconds: f64) -> Pass {
    let size = (
        settings.width.unwrap_or(width),
        settings.height.unwrap_or(height),
    );
    // Moving right shows what was to the left; moving up, what was below.
    let mut x = -f64::from(settings.horizontal_speed) * seconds / f64::from(width);
    let mut y = f64::from(settings.vertical_speed) * seconds / f64::from(height);
    if settings.repeat {
        (x, y) = (x.rem_euclid(1.0), y.rem_euclid(1.0));
    }
    let mut params = [[0.0; 4]; PARAMS];
    params[0] = [
        size.0 as f32 / width as f32,
        size.1 as f32 / height as f32,
        x as f32,
        y as f32,
    ];
    params[1][0] = if settings.repeat { 1.0 } else { 0.0 };
    Pass {
        entry: "scroll",
        params,
        size,
        file: None,
        delay: None,
    }
}

/// The exponent that `gamma` (above 0 lifts the midtones) raises colours
/// to.
fn gamma_exponent(gamma: f32) -> f32 {
    if gamma >= 0.0 {
        1.0 / (1.0 + gamma)
    } else {
        1.0 - gamma
    }
}

/// How far `contrast` scales colours from middle grey.
fn contrast_factor(contrast: f32) -> f32 {
    if contrast >= 0.0 {
        1.0 + contrast
    } else {
        1.0 / (1.0 - contrast)
    }
}

fn key_adjust(adjust: &KeyAdjust) -> [f32; 4] {
    [
        gamma_exponent(adjust.gamma),
        contrast_factor(adjust.contrast),
        adjust.brightness,
        0.0,
    ]
}

fn color_correction(settings: &ColorCorrection) -> [[f32; 4]; PARAMS] {
    let exponent = gamma_exponent(settings.gamma);
    let factor = contrast_factor(settings.contrast);
    let [h0, h1, h2] = hue_rotation(settings.hue_shift);
    let multiply = settings.color_multiply.rgba().unwrap_or([1.0; 4]);
    let [r, g, b, a] = settings.color_add.rgba().unwrap_or([0.0; 4]);
    let mut params = [[0.0; 4]; PARAMS];
    params[0] = [
        exponent,
        factor,
        settings.brightness,
        1.0 + settings.saturation,
    ];
    params[1] = h0;
    params[2] = h1;
    params[3] = h2;
    params[4] = multiply;
    params[5] = [r * a, g * a, b * a, 0.0];
    params[6][0] = settings.opacity;
    params
}

/// The rows of a rotation by `degrees` about the grey axis of RGB space,
/// which turns hues and keeps greys.
fn hue_rotation(degrees: f32) -> [[f32; 4]; 3] {
    let (sin, cos) = (degrees * PI / 180.0).sin_cos();
    let k = 1.0 / 3.0f32.sqrt();
    let same = cos + (1.0 - cos) / 3.0;
    let ahead = (1.0 - cos) / 3.0 + sin * k;
    let behind = (1.0 - cos) / 3.0 - sin * k;
    [
        [same, behind, ahead, 0.0],
        [ahead, same, behind, 0.0],
        [behind, ahead, same, 0.0],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(degrees: f32, rgb: [f32; 3]) -> [f32; 3] {
        hue_rotation(degrees).map(|row| row[0] * rgb[0] + row[1] * rgb[1] + row[2] * rgb[2])
    }

    fn close(a: [f32; 3], b: [f32; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-5)
    }

    #[test]
    fn hue_rotation_turns_colours_and_keeps_greys() {
        assert!(close(turn(0.0, [0.2, 0.5, 0.9]), [0.2, 0.5, 0.9]));
        // A third of the way round, red becomes green and green blue.
        assert!(close(turn(120.0, [1.0, 0.0, 0.0]), [0.0, 1.0, 0.0]));
        assert!(close(turn(120.0, [0.0, 1.0, 0.0]), [0.0, 0.0, 1.0]));
        assert!(close(turn(73.0, [0.4, 0.4, 0.4]), [0.4, 0.4, 0.4]));
    }
}
