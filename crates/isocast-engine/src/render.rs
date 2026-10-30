//! Drawing frames and styled boxes with the GPU, and reading the result
//! back as a PNG.
//!
//! Every draw reads its parameters, a 272-byte draw record, from one
//! storage buffer by its instance index; what it samples is bound
//! separately (bind group 1), so a list of draws needs no buffer or bind
//! group of its own per draw. A draw inside an item that clips its
//! children (`overflow = "hidden"`) names that item's clip, a 64-byte
//! record in a second storage buffer. Compiled scenes ([`crate::draw_list`]) keep
//! their records uploaded between frames; [`Renderer::render`] draws a
//! one-off list of [`Layer`]s the same way.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::num::NonZeroU64;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use isocast_protocol::{Align, Blend, Crop, Fit, ScaleFilter};

use crate::gpu::{Gpu, GpuFrame, GpuPixels, Matrix, YCbCr};

const SHADER: &str = r#"
struct Draw {
    // Maps the item's own pixels (0,0 to its size) to the target's: the
    // rows of a 2×3 affine matrix.
    row0: vec4<f32>,
    row1: vec4<f32>,
    // Target width and height, then the item's.
    sizes: vec4<f32>,
    // The part of the frame drawn, in texture coordinates.
    src: vec4<f32>,
    // Y'CbCr to RGB: what each of Y', Cb and Cr adds to R, G and B, after
    // subtracting `offset`.
    col_y: vec4<f32>,
    col_cb: vec4<f32>,
    col_cr: vec4<f32>,
    offset: vec4<f32>,
    // Opacity, the margin drawn around the item, border width, shadow blur.
    style: vec4<f32>,
    // Corner radii: top left, top right, bottom right, bottom left.
    radius: vec4<f32>,
    background: vec4<f32>,
    border: vec4<f32>,
    shadow_color: vec4<f32>,
    // Shadow offset.
    shadow: vec4<f32>,
    // Where the picture is drawn, in the item's own pixels: x, y, width,
    // height (all of the item unless it is fitted).
    picture: vec4<f32>,
    // The picture's edges that lie inside the item, which are smoothed:
    // left, top, right, bottom (far out where it reaches the item's edge).
    bounds: vec4<f32>,
    // x: what the frame's texels are: 0 RGB (straight alpha), 1 RGB with
    // premultiplied alpha, 2 Y'CbCr; y: how it is resampled: 0 point, 1
    // bilinear, 2 bicubic, 3 lanczos, 4 area; z: the grey a partly
    // transparent colour is completed with before blending (1 for darken,
    // so transparent parts darken nothing); w: the clip it is drawn inside
    // (its index in `clips` + 1; 0 for none).
    sampling: vec4<f32>,
}

// An item that clips its children to its rounded rect.
struct Clip {
    // Maps the target's pixels to the item's own: the rows of a 2×3
    // affine matrix.
    row0: vec4<f32>,
    row1: vec4<f32>,
    // Its width and height; the clip it is itself inside (index + 1, 0 for
    // none); how many of its pixels one target pixel spans.
    size: vec4<f32>,
    // Corner radii: top left, top right, bottom right, bottom left.
    radius: vec4<f32>,
}

// Every draw's record; a draw's instance index says which is its.
@group(0) @binding(0) var<storage, read> draws: array<Draw>;
@group(0) @binding(1) var smp: sampler;
@group(0) @binding(2) var nearest: sampler;
@group(0) @binding(3) var<storage, read> clips: array<Clip>;
// What the draw samples.
@group(1) @binding(0) var plane0: texture_2d<f32>;
@group(1) @binding(1) var plane1: texture_2d<f32>;

// The record of the draw being drawn, read at the start of each stage.
var<private> draw: Draw;

struct Varyings {
    @builtin(position) position: vec4<f32>,
    // The position in the item's own pixels.
    @location(0) local: vec2<f32>,
    // Its draw record.
    @location(1) @interpolate(flat) record: u32,
}

@vertex
fn vs(@builtin(vertex_index) i: u32, @builtin(instance_index) record: u32) -> Varyings {
    draw = draws[record];
    // A quad as a triangle strip: top left, top right, bottom left, bottom
    // right; grown by the margin for antialiasing and shadows.
    let corner = vec2<f32>(f32(i & 1u), f32(i >> 1u));
    let margin = vec2<f32>(draw.style.y);
    let local = mix(-margin, draw.sizes.zw + margin, corner);
    let at = vec3<f32>(local, 1.0);
    let target_px = vec2<f32>(dot(draw.row0.xyz, at), dot(draw.row1.xyz, at));
    var out: Varyings;
    out.position = vec4<f32>(
        target_px.x / draw.sizes.x * 2.0 - 1.0,
        1.0 - target_px.y / draw.sizes.y * 2.0,
        0.0,
        1.0,
    );
    out.local = local;
    out.record = record;
    return out;
}

// Signed distance from `p` (relative to the box's centre) to a box of half
// size `half` with the radius of p's corner: negative inside.
fn rounded_box(p: vec2<f32>, half: vec2<f32>, radius: vec4<f32>) -> f32 {
    let top = select(radius.x, radius.y, p.x >= 0.0);
    let bottom = select(radius.w, radius.z, p.x >= 0.0);
    let r = min(select(top, bottom, p.y >= 0.0), min(half.x, half.y));
    let q = abs(p) - half + vec2<f32>(r);
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0))) - r;
}

// How much of a pixel at signed distance `d` is inside, antialiased over
// one target pixel.
fn coverage(d: f32) -> f32 {
    return clamp(0.5 - d / max(fwidth(d), 1e-4), 0.0, 1.0);
}

// How much of the target pixel at `p` the draw's clips leave (1 with
// none): inside each clipping item's rounded rect, antialiased.
fn clipped(local: vec2<f32>) -> f32 {
    // Layout pixels, not framebuffer pixels: scene monitors may rasterize
    // the same records into a smaller target.
    let point = vec3<f32>(local, 1.0);
    let p = vec2<f32>(dot(draw.row0.xyz, point), dot(draw.row1.xyz, point));
    var next = u32(draw.sampling.w + 0.5);
    var covered = 1.0;
    for (var depth = 0; depth < 16 && next > 0u; depth++) {
        let clip = clips[next - 1u];
        let at = vec3<f32>(p, 1.0);
        let local = vec2<f32>(dot(clip.row0.xyz, at), dot(clip.row1.xyz, at));
        let half = clip.size.xy * 0.5;
        let d = rounded_box(local - half, half, clip.radius);
        covered *= clamp(0.5 - d / max(clip.size.w, 1e-4), 0.0, 1.0);
        next = u32(clip.size.z + 0.5);
    }
    return covered;
}

fn premultiply(color: vec4<f32>) -> vec4<f32> {
    return vec4<f32>(color.rgb * color.a, color.a);
}

// What a fragment shader returns for premultiplied `color`, ready for the
// layer's blend mode.
fn blended(color: vec4<f32>) -> vec4<f32> {
    return vec4<f32>(color.rgb + (1.0 - color.a) * draw.sampling.z, color.a);
}

// `content` (premultiplied) inside the item's rounded rect and border,
// faded by opacity.
fn shade(content: vec4<f32>, local: vec2<f32>) -> vec4<f32> {
    let half = draw.sizes.zw * 0.5;
    let d = rounded_box(local - half, half, draw.radius);
    let inside = coverage(d);
    let within_border = coverage(d + draw.style.z);
    let bordered = mix(premultiply(draw.border), content, within_border);
    let color = select(content, bordered, draw.style.z > 0.0);
    return color * (inside * draw.style.x);
}

// The error function, within 5e-4 (Abramowitz and Stegun 7.1.27).
fn erf(x: f32) -> f32 {
    let a = abs(x);
    let t = 1.0 + a * (0.278393 + a * (0.230389 + a * (0.000972 + a * 0.078108)));
    let t2 = t * t;
    return sign(x) * (1.0 - 1.0 / (t2 * t2));
}

// Where the item's pixel `local` falls in the frame, in texture
// coordinates.
fn uv(local: vec2<f32>) -> vec2<f32> {
    return mix(draw.src.xy, draw.src.zw, (local - draw.picture.xy) / draw.picture.zw);
}

// `at` kept half a texel inside the crop, so nothing cut off bleeds in.
fn inside_crop(at: vec2<f32>) -> vec2<f32> {
    let half_texel = 0.5 / vec2<f32>(textureDimensions(plane0));
    return clamp(at, draw.src.xy + half_texel, draw.src.zw - half_texel);
}

// The frame's colour at `at` (texture coordinates), premultiplied, read
// from the nearest texel or between texels. RGB frames keep their alpha
// (images, colours); captures are opaque.
fn picture_at(at: vec2<f32>, point: bool) -> vec4<f32> {
    var a: vec4<f32>;
    if (point) {
        a = textureSampleLevel(plane0, nearest, at, 0.0);
    } else {
        a = textureSampleLevel(plane0, smp, at, 0.0);
    }
    let kind = i32(draw.sampling.x + 0.5);
    if (kind == 2) {
        var c: vec2<f32>;
        if (point) {
            c = textureSampleLevel(plane1, nearest, at, 0.0).rg;
        } else {
            c = textureSampleLevel(plane1, smp, at, 0.0).rg;
        }
        let y = a.r - draw.offset.x;
        let cbcr = c - draw.offset.yz;
        let rgb = draw.col_y.rgb * y + draw.col_cb.rgb * cbcr.x + draw.col_cr.rgb * cbcr.y;
        return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
    }
    if (kind == 1) {
        return a;
    }
    return premultiply(a);
}

// Texel `t` of the frame (its column and row, kept inside the crop),
// premultiplied.
fn tap(t: vec2<f32>, size: vec2<f32>) -> vec4<f32> {
    let at = clamp(t, round(draw.src.xy * size), round(draw.src.zw * size) - 1.0);
    return picture_at((at + 0.5) / size, false);
}

// The frame around `at` (texture coordinates) by the item's scale filter,
// premultiplied; `reach` is half the texels one target pixel spans.
fn resample(at: vec2<f32>, reach: vec2<f32>) -> vec4<f32> {
    let method = i32(draw.sampling.y + 0.5);
    if (method == 0) {
        return picture_at(at, true);
    }
    if (method == 1 || (method == 4 && all(reach <= vec2<f32>(0.5)))) {
        return picture_at(at, false);
    }
    let size = vec2<f32>(textureDimensions(plane0));
    var sum = vec4<f32>(0.0);
    var weight = 0.0;
    if (method == 4) {
        // Every texel the target pixel covers, by how much it covers it.
        let lo = at * size - reach;
        let hi = at * size + reach;
        let first = floor(lo);
        let count = min(vec2<i32>(ceil(hi) - first), vec2<i32>(64));
        for (var j = 0; j < count.y; j++) {
            let y = first.y + f32(j);
            let wy = min(y + 1.0, hi.y) - max(y, lo.y);
            for (var i = 0; i < count.x; i++) {
                let x = first.x + f32(i);
                let w = (min(x + 1.0, hi.x) - max(x, lo.x)) * wy;
                sum += tap(vec2<f32>(x, y), size) * w;
                weight += w;
            }
        }
        return sum / max(weight, 1e-6);
    }
    let texels = at * size - 0.5;
    let base = floor(texels);
    let f = texels - base;
    let taps = select(2, 3, method == 3);
    for (var j = 1 - taps; j <= taps; j++) {
        for (var i = 1 - taps; i <= taps; i++) {
            let d = vec2<f32>(f32(i), f32(j)) - f;
            var w = cubic(d.x) * cubic(d.y);
            if (method == 3) {
                w = lanczos(d.x) * lanczos(d.y);
            }
            sum += tap(base + vec2<f32>(f32(i), f32(j)), size) * w;
            weight += w;
        }
    }
    // Sharp kernels overshoot; keep colours valid for their alpha.
    let c = sum / weight;
    let a = clamp(c.a, 0.0, 1.0);
    return vec4<f32>(clamp(c.rgb, vec3<f32>(0.0), vec3<f32>(a)), a);
}

// The frame, placed in the item over its background, inside its rounded
// rect and border.
@fragment
fn fs_picture(in: Varyings) -> @location(0) vec4<f32> {
    draw = draws[in.record];
    let p = in.local;
    let at = uv(p);
    let reach = 0.5 * fwidth(at * vec2<f32>(textureDimensions(plane0)));
    // How much of the pixel the picture covers, along each axis (so a
    // picture at whole pixels has hard edges, corners too).
    let outside = max(draw.bounds.xy - p, p - draw.bounds.zw);
    let across = clamp(0.5 - outside / max(fwidth(p), vec2<f32>(1e-4)), vec2<f32>(0.0), vec2<f32>(1.0));
    let covered = across.x * across.y;
    var color = vec4<f32>(0.0);
    if (covered > 0.0) {
        color = resample(inside_crop(at), reach) * covered;
    }
    var shaded = shade(color + premultiply(draw.background) * (1.0 - color.a), p);
    if (draw.sampling.w > 0.5) {
        shaded *= clipped(in.local);
    }
    return blended(shaded);
}

@fragment
fn fs_fill(in: Varyings) -> @location(0) vec4<f32> {
    draw = draws[in.record];
    var shaded = shade(premultiply(draw.background), in.local);
    if (draw.sampling.w > 0.5) {
        shaded *= clipped(in.local);
    }
    return blended(shaded);
}

// A CSS box shadow: the rounded rect, offset and blurred with a Gaussian of
// σ = blur / 2 (exact along straight edges), not drawn under the item.
@fragment
fn fs_shadow(in: Varyings) -> @location(0) vec4<f32> {
    draw = draws[in.record];
    let half = draw.sizes.zw * 0.5;
    let d = rounded_box(in.local - half - draw.shadow.xy, half, draw.radius);
    let sigma = max(draw.style.w * 0.5, 0.01);
    let soft = 0.5 - 0.5 * erf(d / (sigma * 1.41421356));
    let alpha = select(coverage(d), soft, draw.style.w > 0.0);
    let under = coverage(rounded_box(in.local - half, half, draw.radius));
    var a = draw.shadow_color.a * alpha * (1.0 - under) * draw.style.x;
    if (draw.sampling.w > 0.5) {
        a *= clipped(in.local);
    }
    return blended(vec4<f32>(draw.shadow_color.rgb * a, a));
}
"#;

/// Resampling kernels in WGSL, shared with the scaling filter.
pub(crate) const KERNELS: &str = r#"
// Catmull-Rom.
fn cubic(x: f32) -> f32 {
    let a = abs(x);
    if (a < 1.0) {
        return (1.5 * a - 2.5) * a * a + 1.0;
    }
    if (a < 2.0) {
        return ((-0.5 * a + 2.5) * a - 4.0) * a + 2.0;
    }
    return 0.0;
}

// Three lobes.
fn lanczos(x: f32) -> f32 {
    let a = abs(x);
    if (a < 1e-5) {
        return 1.0;
    }
    if (a >= 3.0) {
        return 0.0;
    }
    let px = 3.14159265 * a;
    return 3.0 * sin(px) * sin(px / 3.0) / (px * px);
}
"#;

/// How the shaders number a scale filter.
pub(crate) fn scale_method(filter: ScaleFilter) -> f32 {
    match filter {
        ScaleFilter::Point => 0.0,
        ScaleFilter::Bilinear => 1.0,
        ScaleFilter::Bicubic => 2.0,
        ScaleFilter::Lanczos => 3.0,
        ScaleFilter::Area => 4.0,
    }
}

/// A rectangle in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// A 2D affine transform: (x, y) becomes (a·x + b·y + tx, c·x + d·y + ty),
/// stored as [a, b, tx, c, d, ty].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Affine(pub [f32; 6]);

impl Affine {
    pub const IDENTITY: Self = Self([1.0, 0.0, 0.0, 0.0, 1.0, 0.0]);

    pub fn translate(x: f32, y: f32) -> Self {
        Self([1.0, 0.0, x, 0.0, 1.0, y])
    }

    pub fn scale(s: f32) -> Self {
        Self([s, 0.0, 0.0, 0.0, s, 0.0])
    }

    /// Scaled by `x` across and `y` down (negative mirrors).
    pub fn scale_xy(x: f32, y: f32) -> Self {
        Self([x, 0.0, 0.0, 0.0, y, 0.0])
    }

    /// Mirrored left to right, top to bottom, or both, about the origin.
    pub fn flip(horizontal: bool, vertical: bool) -> Self {
        let sign = |flip: bool| if flip { -1.0 } else { 1.0 };
        Self::scale_xy(sign(horizontal), sign(vertical))
    }

    /// Clockwise by `degrees` (y points down).
    pub fn rotate(degrees: f32) -> Self {
        let (sin, cos) = degrees.to_radians().sin_cos();
        Self([cos, -sin, 0.0, sin, cos, 0.0])
    }

    /// The transform that undoes this one (the identity if it can't be
    /// undone).
    pub fn inverse(self) -> Self {
        let [a, b, tx, c, d, ty] = self.0;
        let det = a * d - b * c;
        if det == 0.0 || !det.is_finite() {
            return Self::IDENTITY;
        }
        Self([
            d / det,
            -b / det,
            (b * ty - d * tx) / det,
            -c / det,
            a / det,
            (c * tx - a * ty) / det,
        ])
    }

    /// `other` first, then this.
    pub fn after(self, other: Self) -> Self {
        let [a, b, tx, c, d, ty] = self.0;
        let [e, f, ux, g, h, uy] = other.0;
        Self([
            a * e + b * g,
            a * f + b * h,
            a * ux + b * uy + tx,
            c * e + d * g,
            c * f + d * h,
            c * ux + d * uy + ty,
        ])
    }
}

/// The rounded rect of an item that clips what is drawn inside it
/// (`overflow = "hidden"`): `size` pixels with corner `radius`, placed on
/// the target by `transform`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayerClip {
    pub transform: Affine,
    pub size: [f32; 2],
    pub radius: [f32; 4],
}

/// A drop shadow: the layer's rounded rect, offset, blurred and coloured.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayerShadow {
    pub offset: [f32; 2],
    pub blur: f32,
    pub color: [f32; 4],
}

/// Something to draw: an item `size` pixels big, placed on the target by
/// `transform`, showing the part of `frame` left after `crop` placed by
/// `fit` and `align` (and resampled by `scale_filter`) over its
/// `background` (or just filled with it), with rounded corners, a border
/// and a shadow, faded by `opacity` (0 to 1) and combined with what is
/// below by `blend`.
pub struct Layer<'a> {
    pub frame: Option<&'a GpuFrame>,
    pub crop: Crop,
    pub fit: Fit,
    pub align: Align,
    pub scale_filter: ScaleFilter,
    pub size: [f32; 2],
    pub transform: Affine,
    pub opacity: f32,
    pub blend: Blend,
    pub background: Option<[f32; 4]>,
    /// Top left, top right, bottom right, bottom left.
    pub radius: [f32; 4],
    /// Width in pixels and colour.
    pub border: Option<(f32, [f32; 4])>,
    pub shadow: Option<LayerShadow>,
    /// How far its picture is moved within its rect (which clips it), in
    /// the layer's pixels: a show or hide slide.
    pub shift: [f32; 2],
    /// The item it is inside that clips it, if any (the nearest).
    pub clip: Option<LayerClip>,
}

impl<'a> Layer<'a> {
    /// The part of `frame` left after `crop`, filling `dest`.
    pub fn rect(frame: &'a GpuFrame, crop: Crop, dest: Rect, opacity: f32) -> Self {
        Self {
            frame: Some(frame),
            crop,
            fit: Fit::Stretch,
            align: Align::Center,
            scale_filter: ScaleFilter::Bilinear,
            size: [dest.width, dest.height],
            transform: Affine::translate(dest.x, dest.y),
            opacity,
            blend: Blend::Normal,
            background: None,
            radius: [0.0; 4],
            border: None,
            shadow: None,
            shift: [0.0; 2],
            clip: None,
        }
    }

    /// All of `frame`, filling a `width`×`height` target.
    pub fn full(frame: &'a GpuFrame, width: u32, height: u32) -> Self {
        let dest = Rect {
            x: 0.0,
            y: 0.0,
            width: width as f32,
            height: height as f32,
        };
        Self::rect(frame, Crop::default(), dest, 1.0)
    }

    /// The layer without its frame.
    pub(crate) fn placed(&self) -> Placed {
        let crop = self.crop;
        Placed {
            crop: [crop.left, crop.top, crop.right, crop.bottom].map(|v| v as f32),
            fit: self.fit,
            align: self.align,
            scale_filter: self.scale_filter,
            size: self.size,
            transform: self.transform,
            opacity: self.opacity,
            blend: self.blend,
            background: self.background,
            radius: self.radius,
            border: self.border,
            shadow: self.shadow,
            shift: self.shift,
            clip: 0,
        }
    }
}

/// A [`Layer`] without its frame: everything its draw records say besides
/// the picture's size and texels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Placed {
    /// Pixels cut from the picture's left, top, right and bottom (whole
    /// pixels but while an animation changes them).
    pub crop: [f32; 4],
    pub fit: Fit,
    pub align: Align,
    pub scale_filter: ScaleFilter,
    pub size: [f32; 2],
    pub transform: Affine,
    pub opacity: f32,
    pub blend: Blend,
    pub background: Option<[f32; 4]>,
    pub radius: [f32; 4],
    pub border: Option<(f32, [f32; 4])>,
    pub shadow: Option<LayerShadow>,
    pub shift: [f32; 2],
    /// The clip it is drawn inside: its index among the clips + 1, 0 for
    /// none.
    pub clip: u32,
}

impl Placed {
    /// It draws nothing at all: invisible, or no size.
    pub(crate) fn is_empty(&self) -> bool {
        self.opacity <= 0.0 || self.size[0] <= 0.0 || self.size[1] <= 0.0
    }

    /// Whether it draws a fill where it has no picture.
    pub(crate) fn fills(&self) -> bool {
        self.background.is_some() || self.border.is_some()
    }

    /// The margin its shadow is drawn with, if it has one.
    pub(crate) fn shadow_margin(&self) -> Option<f32> {
        self.shadow.map(|shadow| {
            // Three σ (σ = blur / 2) holds all but 0.1% of the blur.
            let reach = 1.5 * shadow.blur + shadow.offset[0].abs().max(shadow.offset[1].abs());
            reach + 2.0
        })
    }
}

/// The margin drawn around an item's picture or fill, for antialiasing.
pub(crate) const MARGIN: f32 = 2.0;

/// One draw's record: the shader's `Draw` struct, 17 `vec4<f32>`s.
pub(crate) type Record = [[f32; 4]; 17];

/// The size of a [`Record`] in the storage buffer.
pub(crate) const RECORD_BYTES: u64 = 272;

/// A clip's record: the shader's `Clip` struct, 4 `vec4<f32>`s.
pub(crate) type ClipRecord = [[f32; 4]; 4];

/// The size of a [`ClipRecord`] in its storage buffer.
pub(crate) const CLIP_BYTES: u64 = 64;

/// The record of a clip to `clip`'s rounded rect, itself inside clip
/// `parent` (index + 1, 0 for none).
pub(crate) fn clip_record(clip: &LayerClip, parent: u32) -> ClipRecord {
    let inverse = clip.transform.inverse();
    let [a, b, tx, c, d, ty] = inverse.0;
    // Its pixels per target pixel (exact without a stretch).
    let scale = (a * d - b * c).abs().sqrt();
    [
        [a, b, tx, 0.0],
        [c, d, ty, 0.0],
        [clip.size[0], clip.size[1], parent as f32, scale],
        fit_radii(clip.radius, clip.size),
    ]
}

/// Which fragment shader draws a layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Picture,
    Fill,
    Shadow,
}

impl Mode {
    const ALL: [Self; 3] = [Self::Picture, Self::Fill, Self::Shadow];

    fn entry_point(self) -> &'static str {
        match self {
            Self::Picture => "fs_picture",
            Self::Fill => "fs_fill",
            Self::Shadow => "fs_shadow",
        }
    }
}

/// What a picture's texels are, as far as its draw records care: their
/// size and how they encode colour.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Texels {
    pub width: u32,
    pub height: u32,
    pub kind: TexelKind,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum TexelKind {
    Rgb,
    Premultiplied,
    YCbCr(YCbCr),
}

/// The record that draws `p` in `mode` with `margin` pixels around it into
/// a `target`-sized target; in `Picture` mode showing a picture of
/// `texels`. `None` if its crop leaves nothing of the picture.
pub(crate) fn record(
    p: &Placed,
    texels: Option<Texels>,
    mode: Mode,
    target: (u32, u32),
    margin: f32,
) -> Option<Record> {
    let mut src = [0.0, 0.0, 1.0, 1.0];
    let mut csc = [[0.0; 4]; 4];
    let mut picture = [0.0, 0.0, p.size[0], p.size[1]];
    let mut kind = 0.0;
    if let (Mode::Picture, Some(texels)) = (mode, texels) {
        let crop = p.crop;
        let (fw, fh) = (texels.width as f32, texels.height as f32);
        src = [
            crop[0] / fw,
            crop[1] / fh,
            1.0 - crop[2] / fw,
            1.0 - crop[3] / fh,
        ];
        if src[0] >= src[2] || src[1] >= src[3] {
            return None;
        }
        let cropped = [(src[2] - src[0]) * fw, (src[3] - src[1]) * fh];
        picture = place(p.fit, p.align, p.size, cropped);
        picture[0] += p.shift[0];
        picture[1] += p.shift[1];
        match texels.kind {
            TexelKind::Rgb => {}
            TexelKind::Premultiplied => kind = 1.0,
            TexelKind::YCbCr(coding) => {
                csc = to_rgb(coding);
                kind = 2.0;
            }
        }
    }
    let complete = if p.blend == Blend::Darken { 1.0 } else { 0.0 };
    let [a, b, tx, c, d, ty] = p.transform.0;
    let (border_width, border) = p.border.unwrap_or((0.0, [0.0; 4]));
    let shadow = p.shadow.unwrap_or(LayerShadow {
        offset: [0.0; 2],
        blur: 0.0,
        color: [0.0; 4],
    });
    Some([
        [a, b, tx, 0.0],
        [c, d, ty, 0.0],
        [target.0 as f32, target.1 as f32, p.size[0], p.size[1]],
        src,
        csc[0],
        csc[1],
        csc[2],
        csc[3],
        [p.opacity.clamp(0.0, 1.0), margin, border_width, shadow.blur],
        fit_radii(p.radius, p.size),
        p.background.unwrap_or([0.0; 4]),
        border,
        shadow.color,
        [shadow.offset[0], shadow.offset[1], 0.0, 0.0],
        picture,
        smoothed_edges(picture, p.size),
        [kind, scale_method(p.scale_filter), complete, p.clip as f32],
    ])
}

/// `records` (draw or clip records) as the shader reads them, into
/// `bytes` (emptied first).
pub(crate) fn record_bytes<const N: usize>(records: &[[[f32; 4]; N]], bytes: &mut Vec<u8>) {
    let values = records.as_flattened().as_flattened();
    bytes.clear();
    bytes.resize(values.len() * 4, 0);
    for (out, value) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(values) {
        *out = value.to_ne_bytes();
    }
}

/// The blend modes, in the order of [`Renderer`]'s pipelines.
const BLENDS: [Blend; 7] = [
    Blend::Normal,
    Blend::Additive,
    Blend::Subtract,
    Blend::Screen,
    Blend::Multiply,
    Blend::Lighten,
    Blend::Darken,
];

/// Where `blend` is in [`BLENDS`].
fn blend_index(blend: Blend) -> usize {
    match blend {
        Blend::Normal => 0,
        Blend::Additive => 1,
        Blend::Subtract => 2,
        Blend::Screen => 3,
        Blend::Multiply => 4,
        Blend::Lighten => 5,
        Blend::Darken => 6,
    }
}

/// How the GPU combines a layer's premultiplied colour (src) with the
/// target's (dst) for each blend mode. Modes that only darken keep the
/// target's alpha, so on a transparent canvas (a nested scene's) they draw
/// nothing rather than black.
fn blend_state(blend: Blend) -> wgpu::BlendState {
    use wgpu::{BlendComponent, BlendFactor as F, BlendOperation as Op};
    let part = |src_factor, dst_factor, operation| BlendComponent {
        src_factor,
        dst_factor,
        operation,
    };
    let over = part(F::One, F::OneMinusSrcAlpha, Op::Add);
    let keep = part(F::Zero, F::One, Op::Add);
    let (color, alpha) = match blend {
        Blend::Normal => (over, over),
        // src + dst
        Blend::Additive => (part(F::One, F::One, Op::Add), over),
        // dst − src
        Blend::Subtract => (part(F::One, F::One, Op::ReverseSubtract), keep),
        // src + dst·(1 − src)
        Blend::Screen => (part(F::One, F::OneMinusSrc, Op::Add), over),
        // src·dst + dst·(1 − src alpha)
        Blend::Multiply => (part(F::Dst, F::OneMinusSrcAlpha, Op::Add), keep),
        // max(src, dst), the src as if over black
        Blend::Lighten => (part(F::One, F::One, Op::Max), over),
        // min(src, dst), the src as if over white (see `blended`)
        Blend::Darken => (part(F::One, F::One, Op::Min), keep),
    };
    wgpu::BlendState { color, alpha }
}

/// Draws layers into render targets of one texture format.
pub struct Renderer {
    format: wgpu::TextureFormat,
    /// By [`Mode`] and blend mode (in [`BLENDS`] order), all made up front
    /// so a scene never waits for one.
    pipelines: [[wgpu::RenderPipeline; BLENDS.len()]; Mode::ALL.len()],
    /// Bind group 0: the draw records and the samplers.
    records_layout: wgpu::BindGroupLayout,
    /// Bind group 1: what a draw samples.
    planes_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    nearest: wgpu::Sampler,
    /// Bound in place of a frame for fills and shadows.
    blank: wgpu::BindGroup,
    /// What [`Renderer::render`] draws with, kept between calls.
    immediate: Mutex<Immediate>,
}

/// The records of a one-off list of layers, and the buffer they go in.
#[derive(Default)]
struct Immediate {
    records: Vec<Record>,
    clips: Vec<ClipRecord>,
    bytes: Vec<u8>,
    /// Each record's mode, blend and (for pictures) layer.
    draws: Vec<(Mode, Blend, Option<usize>)>,
    buffer: Option<RecordBuffer>,
    cached: Option<CachedFrames>,
}

/// Only for pooled imports that do not retain a frame lease. Recording's
/// uncached imports must not be kept alive by their texture bindings.
#[derive(Default)]
struct CachedFrames {
    uploaded: Option<Vec<u8>>,
    uploaded_clips: Option<Vec<u8>>,
    groups: HashMap<(wgpu::Texture, wgpu::Texture), wgpu::BindGroup>,
    views: HashMap<wgpu::Texture, wgpu::TextureView>,
    #[cfg(test)]
    uploads: usize,
}

const CACHED_FRAMES: usize = 64;

/// Storage buffers of draw records and clip records, and the bind group
/// that reads them.
pub(crate) struct RecordBuffer {
    pub buffer: wgpu::Buffer,
    pub clips: wgpu::Buffer,
    pub group: wgpu::BindGroup,
    /// How many records it holds.
    pub capacity: usize,
    /// How many clip records it holds.
    pub clip_capacity: usize,
}

impl Renderer {
    /// Reuse monitor draw parameters and pooled texture bindings. The caller
    /// must keep each source frame alive until its submitted GPU read finishes.
    pub(crate) fn cache_pooled_frames(&mut self, enabled: bool) {
        let cached = &mut self.immediate.get_mut().cached;
        if enabled {
            cached.get_or_insert_default();
        } else {
            *cached = None;
        }
    }

    pub fn new(gpu: &Gpu, format: wgpu::TextureFormat) -> Self {
        let device = &gpu.device;
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("draw"),
            source: wgpu::ShaderSource::Wgsl(format!("{SHADER}{KERNELS}").into()),
        });
        let sampler_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        };
        let records_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("draw records"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: NonZeroU64::new(RECORD_BYTES),
                    },
                    count: None,
                },
                sampler_entry(1),
                sampler_entry(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: NonZeroU64::new(CLIP_BYTES),
                    },
                    count: None,
                },
            ],
        });
        let texture_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let planes_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("draw planes"),
            entries: &[texture_entry(0), texture_entry(1)],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("draw"),
            bind_group_layouts: &[Some(&records_layout), Some(&planes_layout)],
            ..Default::default()
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("draw"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let nearest = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("nearest"),
            ..Default::default()
        });
        let blank = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("blank"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let pipeline = |mode: Mode, blend: Blend| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(mode.entry_point()),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(mode.entry_point()),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(blend_state(blend)),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let blank = planes_group(gpu, &planes_layout, &blank, &blank);
        Self {
            format,
            pipelines: Mode::ALL.map(|mode| BLENDS.map(|blend| pipeline(mode, blend))),
            records_layout,
            planes_layout,
            sampler,
            nearest,
            blank,
            immediate: Mutex::default(),
        }
    }

    /// The pipeline that draws in `mode` with `blend`.
    pub(crate) fn pipeline(&self, mode: Mode, blend: Blend) -> &wgpu::RenderPipeline {
        &self.pipelines[mode as usize][blend_index(blend)]
    }

    /// Storage buffers for at least `records` draw records and `clips`
    /// clip records, with the bind group (0) that reads them.
    pub(crate) fn record_buffer(&self, gpu: &Gpu, records: usize, clips: usize) -> RecordBuffer {
        let capacity = records.max(16).next_power_of_two();
        let clip_capacity = clips.max(1).next_power_of_two();
        let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("draw records"),
            size: capacity as u64 * RECORD_BYTES,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let clip_buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("clips"),
            size: clip_capacity as u64 * CLIP_BYTES,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("draw records"),
            layout: &self.records_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.nearest),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: clip_buffer.as_entire_binding(),
                },
            ],
        });
        RecordBuffer {
            buffer,
            clips: clip_buffer,
            group,
            capacity,
            clip_capacity,
        }
    }

    /// The bind group (1) a draw of `frame`'s picture samples.
    pub(crate) fn frame_group(&self, gpu: &Gpu, frame: &GpuFrame) -> wgpu::BindGroup {
        if let GpuPixels::YCbCrBiPlanar { texture, .. } = &frame.pixels {
            let luma = texture.create_view(&wgpu::TextureViewDescriptor {
                format: Some(wgpu::TextureFormat::R8Unorm),
                aspect: wgpu::TextureAspect::Plane0,
                ..Default::default()
            });
            let chroma = texture.create_view(&wgpu::TextureViewDescriptor {
                format: Some(wgpu::TextureFormat::Rg8Unorm),
                aspect: wgpu::TextureAspect::Plane1,
                ..Default::default()
            });
            return views_group(gpu, &self.planes_layout, &luma, &chroma);
        }
        let (plane0, plane1) = frame.planes();
        planes_group(gpu, &self.planes_layout, plane0, plane1)
    }

    /// The bind group (1) a draw sampling `plane0` and `plane1` uses.
    pub(crate) fn planes_group(
        &self,
        gpu: &Gpu,
        plane0: &wgpu::Texture,
        plane1: &wgpu::Texture,
    ) -> wgpu::BindGroup {
        planes_group(gpu, &self.planes_layout, plane0, plane1)
    }

    /// The bind group (1) of a draw that samples nothing.
    pub(crate) fn blank_group(&self) -> &wgpu::BindGroup {
        &self.blank
    }

    /// The format of the targets it draws into.
    pub fn format(&self) -> wgpu::TextureFormat {
        self.format
    }

    /// Draw `layers`, back to front on black, into `target` (of `size`
    /// pixels) and submit the work. Returns the submission, to wait for
    /// before handing the target on.
    pub fn render(
        &self,
        gpu: &Gpu,
        target: &wgpu::Texture,
        layers: &[Layer<'_>],
        size: (u32, u32),
    ) -> wgpu::SubmissionIndex {
        self.render_on(gpu, target, layers, size, wgpu::Color::BLACK)
    }

    /// [`Renderer::render`] on `clear` (premultiplied) instead of black.
    pub fn render_on(
        &self,
        gpu: &Gpu,
        target: &wgpu::Texture,
        layers: &[Layer<'_>],
        size: (u32, u32),
        clear: wgpu::Color,
    ) -> wgpu::SubmissionIndex {
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        self.encode(gpu, &mut encoder, target, layers, size, clear);
        gpu.queue.submit([encoder.finish()])
    }

    /// Encode drawing `layers` into `target` ([`Renderer::render_on`] as
    /// part of `encoder`'s work). Their records are written to the queue
    /// now, so the encoder must be submitted before this renderer draws
    /// another list.
    pub(crate) fn encode(
        &self,
        gpu: &Gpu,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::Texture,
        layers: &[Layer<'_>],
        size: (u32, u32),
        clear: wgpu::Color,
    ) {
        let mut immediate = self.immediate.lock();
        let Immediate {
            records,
            clips,
            bytes,
            draws,
            buffer,
            cached,
        } = &mut *immediate;
        records.clear();
        clips.clear();
        draws.clear();
        for (index, layer) in layers.iter().enumerate() {
            let mut placed = layer.placed();
            if placed.is_empty() {
                continue;
            }
            if let Some(clip) = &layer.clip {
                clips.push(clip_record(clip, 0));
                placed.clip = clips.len() as u32;
            }
            let texels = layer.frame.map(GpuFrame::texels);
            if let Some(margin) = placed.shadow_margin()
                && let Some(record) = record(&placed, None, Mode::Shadow, size, margin)
            {
                records.push(record);
                draws.push((Mode::Shadow, layer.blend, None));
            }
            let mode = match layer.frame {
                Some(_) => Mode::Picture,
                None if placed.fills() => Mode::Fill,
                None => continue,
            };
            if let Some(record) = record(&placed, texels, mode, size, MARGIN) {
                records.push(record);
                let frame = (mode == Mode::Picture).then_some(index);
                draws.push((mode, layer.blend, frame));
            }
        }
        if !records.is_empty() {
            if buffer
                .as_ref()
                .is_none_or(|b| b.capacity < records.len() || b.clip_capacity < clips.len())
            {
                *buffer = Some(self.record_buffer(gpu, records.len(), clips.len()));
                if let Some(cached) = cached {
                    cached.uploaded = None;
                    cached.uploaded_clips = None;
                }
            }
            if let Some(buffer) = buffer.as_ref().filter(|_| !clips.is_empty()) {
                record_bytes(clips, bytes);
                if cached
                    .as_ref()
                    .is_none_or(|c| c.uploaded_clips.as_deref() != Some(bytes.as_slice()))
                {
                    gpu.queue.write_buffer(&buffer.clips, 0, bytes);
                    if let Some(cached) = cached {
                        cached
                            .uploaded_clips
                            .get_or_insert_default()
                            .clone_from(bytes);
                    }
                }
            }
            record_bytes(records, bytes);
        }
        let view = if let Some(cached) = cached {
            if cached.views.len() >= CACHED_FRAMES && !cached.views.contains_key(target) {
                cached.views.clear();
            }
            cached
                .views
                .entry(target.clone())
                .or_insert_with(|| target.create_view(&Default::default()))
                .clone()
        } else {
            target.create_view(&Default::default())
        };
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("layers"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(clear),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        let Some(buffer) = buffer.as_ref().filter(|_| !records.is_empty()) else {
            return;
        };
        if cached
            .as_ref()
            .is_none_or(|c| c.uploaded.as_deref() != Some(bytes.as_slice()))
        {
            gpu.queue.write_buffer(&buffer.buffer, 0, bytes);
            if let Some(cached) = cached {
                cached.uploaded.get_or_insert_default().clone_from(bytes);
                #[cfg(test)]
                {
                    cached.uploads += 1;
                }
            }
        }
        pass.set_bind_group(0, &buffer.group, &[]);
        for (record, &(mode, blend, frame)) in draws.iter().enumerate() {
            pass.set_pipeline(self.pipeline(mode, blend));
            match frame.and_then(|index| layers[index].frame) {
                Some(frame) => {
                    let group = if let Some(cached) = cached {
                        let (plane0, plane1) = frame.planes();
                        let key = (plane0.clone(), plane1.clone());
                        if cached.groups.len() >= CACHED_FRAMES && !cached.groups.contains_key(&key)
                        {
                            cached.groups.clear();
                        }
                        cached
                            .groups
                            .entry(key)
                            .or_insert_with(|| self.frame_group(gpu, frame))
                            .clone()
                    } else {
                        self.frame_group(gpu, frame)
                    };
                    pass.set_bind_group(1, &group, &[]);
                }
                None => pass.set_bind_group(1, &self.blank, &[]),
            }
            let record = record as u32;
            pass.draw(0..4, record..record + 1);
        }
    }

    /// Draw `layers`, back to front on black, into a `width`×`height`
    /// picture and encode it as a PNG.
    pub fn png(&self, gpu: &Gpu, layers: &[Layer<'_>], width: u32, height: u32) -> Result<Vec<u8>> {
        let target = self.png_target(gpu, width, height)?;
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        self.encode(
            gpu,
            &mut encoder,
            &target,
            layers,
            (width, height),
            wgpu::Color::BLACK,
        );
        read_png(gpu, encoder, &target)
    }

    /// A `width`×`height` target to draw a PNG into.
    pub(crate) fn png_target(&self, gpu: &Gpu, width: u32, height: u32) -> Result<wgpu::Texture> {
        if self.format != wgpu::TextureFormat::Rgba8Unorm {
            bail!("PNGs are drawn by an Rgba8Unorm renderer");
        }
        Ok(gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("snapshot"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        }))
    }
}

/// A bind group (1) sampling `plane0` and `plane1`.
fn planes_group(
    gpu: &Gpu,
    layout: &wgpu::BindGroupLayout,
    plane0: &wgpu::Texture,
    plane1: &wgpu::Texture,
) -> wgpu::BindGroup {
    let view0 = plane0.create_view(&Default::default());
    let view1 = plane1.create_view(&Default::default());
    views_group(gpu, layout, &view0, &view1)
}

fn views_group(
    gpu: &Gpu,
    layout: &wgpu::BindGroupLayout,
    view0: &wgpu::TextureView,
    view1: &wgpu::TextureView,
) -> wgpu::BindGroup {
    gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("draw planes"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(view0),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(view1),
            },
        ],
    })
}

/// Finish `encoder` (which draws `target`, an Rgba8Unorm texture), read
/// `target` back and encode it as a PNG.
pub(crate) fn read_png(
    gpu: &Gpu,
    mut encoder: wgpu::CommandEncoder,
    target: &wgpu::Texture,
) -> Result<Vec<u8>> {
    let device = &gpu.device;
    let (width, height) = (target.width(), target.height());
    let size = target.size();
    // Rows of a texture copy are padded to 256 bytes.
    let row = (width * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("snapshot"),
        size: row as u64 * height as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        target.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row),
                rows_per_image: Some(height),
            },
        },
        size,
    );
    let submission = gpu.queue.submit([encoder.finish()]);
    let slice = buffer.slice(..);
    let (tx, rx) = mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    device
        .poll(wgpu::PollType::Wait {
            submission_index: Some(submission),
            timeout: Some(Duration::from_secs(5)),
        })
        .context("the GPU did not finish drawing")?;
    rx.recv()
        .context("the GPU did not return the picture")?
        .context("could not read the picture back from the GPU")?;

    let mapped = slice
        .get_mapped_range()
        .context("could not read the picture back from the GPU")?;
    let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
    for y in 0..height as usize {
        let start = y * row as usize;
        let line = &mapped[start..start + width as usize * 4];
        for pixel in line.as_chunks::<4>().0 {
            rgb.extend_from_slice(&pixel[..3]);
        }
    }
    drop(mapped);
    buffer.unmap();

    let mut png = Vec::new();
    let mut encoder = png::Encoder::new(&mut png, width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&rgb)?;
    writer.finish()?;
    Ok(png)
}

/// Where a `picture`-sized picture goes in an item of `size` pixels,
/// fitted and aligned: x, y, width and height in the item's pixels. The
/// offsets are whole pixels, so a picture at its own size stays sharp.
pub fn place(fit: Fit, align: Align, size: [f32; 2], picture: [f32; 2]) -> [f32; 4] {
    let [width, height] = size;
    let (across, down) = (width / picture[0], height / picture[1]);
    let scale = match fit {
        Fit::Legacy | Fit::Stretch => return [0.0, 0.0, width, height],
        Fit::Contain => across.min(down),
        Fit::Cover => across.max(down),
        Fit::Width => across,
        Fit::Height => down,
        Fit::ScaleDown => across.min(down).min(1.0),
        Fit::None => 1.0,
    };
    let (w, h) = (picture[0] * scale, picture[1] * scale);
    let [ax, ay] = align.factors();
    [
        ((width - w) * ax).round(),
        ((height - h) * ay).round(),
        w,
        h,
    ]
}

/// The edges of `picture` (x, y, width, height) that lie inside an item of
/// `size`, as left, top, right and bottom; edges at or past the item's
/// own are moved far out, since the item's edges clip there.
fn smoothed_edges(picture: [f32; 4], size: [f32; 2]) -> [f32; 4] {
    const FAR: f32 = 1e9;
    let [x, y, w, h] = picture;
    let reaches = |edge: f32, limit: f32| edge >= limit - 1e-3;
    [
        if x <= 1e-3 { -FAR } else { x },
        if y <= 1e-3 { -FAR } else { y },
        if reaches(x + w, size[0]) { FAR } else { x + w },
        if reaches(y + h, size[1]) { FAR } else { y + h },
    ]
}

/// Corner radii scaled down together where adjacent ones would overlap,
/// as CSS does.
fn fit_radii(radius: [f32; 4], size: [f32; 2]) -> [f32; 4] {
    let [tl, tr, br, bl] = radius.map(|r| r.max(0.0));
    let [w, h] = size;
    let fit = |side: f32, a: f32, b: f32| if a + b > side { side / (a + b) } else { 1.0 };
    let f = fit(w, tl, tr)
        .min(fit(w, bl, br))
        .min(fit(h, tl, bl))
        .min(fit(h, tr, br));
    [tl * f, tr * f, br * f, bl * f]
}

/// The columns of the Y'CbCr to RGB matrix (for Y', Cb, Cr) and the offsets
/// subtracted first, in texel values.
fn to_rgb(coding: YCbCr) -> [[f32; 4]; 4] {
    let (kr, kb) = match coding.matrix {
        Matrix::Bt601 => (0.299, 0.114),
        Matrix::Bt709 => (0.2126, 0.0722),
        Matrix::Bt2020 => (0.2627, 0.0593),
    };
    let kg = 1.0 - kr - kb;
    let bits = coding.bits.clamp(8, 16);
    // The texel value of one code, for samples in the high bits of 8- or
    // 16-bit texels.
    let unit = if bits == 8 {
        1.0 / 255.0
    } else {
        f64::from(1u32 << (16 - bits)) / 65535.0
    };
    // Codes per 8-bit code.
    let step = f64::from(1u32 << (bits - 8));
    let max = f64::from((1u32 << bits) - 1);
    let (y_offset, y_span, c_span) = if coding.full_range {
        (0.0, max, max)
    } else {
        (16.0 * step, 219.0 * step, 224.0 * step)
    };
    let c_offset = 128.0 * step;
    let ys = 1.0 / (y_span * unit);
    let cs = 1.0 / (c_span * unit);
    let column = |r: f64, g: f64, b: f64| [r as f32, g as f32, b as f32, 0.0];
    [
        column(ys, ys, ys),
        column(0.0, -2.0 * kb * (1.0 - kb) / kg * cs, 2.0 * (1.0 - kb) * cs),
        column(2.0 * (1.0 - kr) * cs, -2.0 * kr * (1.0 - kr) / kg * cs, 0.0),
        column(y_offset * unit, c_offset * unit, c_offset * unit),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a native GPU; run under the hardware lock"]
    fn pooled_monitor_draws_reuse_bindings_and_refresh_changed_parameters() -> Result<()> {
        let gpu = Gpu::new()?;
        let mut renderer = Renderer::new(&gpu, wgpu::TextureFormat::Rgba8Unorm);
        renderer.cache_pooled_frames(true);
        let source = |size, color| {
            let texture = crate::scene::canvas_texture(&gpu, renderer.format(), size);
            let view = texture.create_view(&Default::default());
            let mut encoder = gpu.device.create_command_encoder(&Default::default());
            {
                let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(color),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                });
            }
            gpu.queue.submit([encoder.finish()]);
            GpuFrame {
                width: size.0,
                height: size.1,
                pixels: GpuPixels::Rgb(texture),
            }
        };
        let red = source((16, 16), wgpu::Color::RED);
        let blue = source((16, 16), wgpu::Color::BLUE);
        let targets = [
            renderer.png_target(&gpu, 8, 8)?,
            renderer.png_target(&gpu, 8, 8)?,
        ];
        let draw = |frame: &GpuFrame, target: &wgpu::Texture, filter, expected| -> Result<()> {
            let size = (target.width(), target.height());
            let mut layer = Layer::full(frame, size.0, size.1);
            layer.scale_filter = filter;
            let mut encoder = gpu.device.create_command_encoder(&Default::default());
            renderer.encode(
                &gpu,
                &mut encoder,
                target,
                &[layer],
                size,
                wgpu::Color::BLACK,
            );
            let png = read_png(&gpu, encoder, target)?;
            let image = image::load_from_memory(&png)?.to_rgba8();
            assert_eq!(image.get_pixel(size.0 / 2, size.1 / 2).0, expected);
            Ok(())
        };
        for i in 0..32 {
            let (frame, color) = if i % 2 == 0 {
                (&red, [255, 0, 0, 255])
            } else {
                (&blue, [0, 0, 255, 255])
            };
            draw(frame, &targets[i % 2], ScaleFilter::Area, color)?;
        }
        {
            let state = renderer.immediate.lock();
            let cached = state.cached.as_ref().unwrap();
            assert_eq!(cached.uploads, 1);
            assert_eq!(cached.groups.len(), 2);
            assert_eq!(cached.views.len(), 2);
        }
        let green = source((32, 16), wgpu::Color::GREEN);
        draw(&green, &targets[0], ScaleFilter::Area, [0, 255, 0, 255])?;
        // Stretching still uses the same normalized record; the new source
        // gets its own binding and the shader reads its texture dimensions.
        assert_eq!(
            renderer.immediate.lock().cached.as_ref().unwrap().uploads,
            1
        );
        let larger = renderer.png_target(&gpu, 16, 8)?;
        draw(&green, &larger, ScaleFilter::Area, [0, 255, 0, 255])?;
        draw(&green, &larger, ScaleFilter::Point, [0, 255, 0, 255])?;
        // A replacement buffer must upload even when the record bytes match.
        renderer.immediate.lock().buffer = None;
        draw(&green, &larger, ScaleFilter::Point, [0, 255, 0, 255])?;
        assert_eq!(
            renderer.immediate.lock().cached.as_ref().unwrap().uploads,
            4
        );
        renderer.cache_pooled_frames(false);
        assert!(renderer.immediate.lock().cached.is_none());
        Ok(())
    }

    fn apply(t: Affine, x: f32, y: f32) -> (f32, f32) {
        let [a, b, tx, c, d, ty] = t.0;
        (a * x + b * y + tx, c * x + d * y + ty)
    }

    #[test]
    fn transforms_apply_right_to_left() {
        // Rotate a quarter turn clockwise (y points down), then move.
        let t = Affine::translate(10.0, 0.0).after(Affine::rotate(90.0));
        let (x, y) = apply(t, 1.0, 0.0);
        assert!(
            (x - 10.0).abs() < 1e-5 && (y - 1.0).abs() < 1e-5,
            "{x}, {y}"
        );
        let (x, y) = apply(
            Affine::scale(2.0).after(Affine::translate(1.0, 1.0)),
            1.0,
            2.0,
        );
        assert_eq!((x, y), (4.0, 6.0));
    }

    #[test]
    fn flips_mirror_about_the_origin() {
        assert_eq!(apply(Affine::flip(true, false), 3.0, 4.0), (-3.0, 4.0));
        assert_eq!(apply(Affine::flip(false, true), 3.0, 4.0), (3.0, -4.0));
        assert_eq!(Affine::flip(false, false), Affine::IDENTITY);
        // Both ways is a half turn.
        assert_eq!(apply(Affine::flip(true, true), 3.0, 4.0), (-3.0, -4.0));
        let (x, y) = apply(Affine::rotate(180.0), 3.0, 4.0);
        assert!((x + 3.0).abs() < 1e-5 && (y + 4.0).abs() < 1e-5, "{x}, {y}");
    }

    #[test]
    fn pictures_are_fitted_and_aligned_like_obs_bounds() {
        // A 2:1 picture in a 100×100 item.
        let at = |fit, align| place(fit, align, [100.0, 100.0], [40.0, 20.0]);
        assert_eq!(at(Fit::Stretch, Align::Bottom), [0.0, 0.0, 100.0, 100.0]);
        assert_eq!(at(Fit::Contain, Align::Center), [0.0, 25.0, 100.0, 50.0]);
        assert_eq!(at(Fit::Contain, Align::Bottom), [0.0, 50.0, 100.0, 50.0]);
        assert_eq!(at(Fit::Cover, Align::Center), [-50.0, 0.0, 200.0, 100.0]);
        assert_eq!(at(Fit::Cover, Align::Right), [-100.0, 0.0, 200.0, 100.0]);
        assert_eq!(at(Fit::Width, Align::Center), [0.0, 25.0, 100.0, 50.0]);
        assert_eq!(at(Fit::Height, Align::TopLeft), [0.0, 0.0, 200.0, 100.0]);
        assert_eq!(at(Fit::ScaleDown, Align::Center), [30.0, 40.0, 40.0, 20.0]);
        assert_eq!(at(Fit::None, Align::BottomRight), [60.0, 80.0, 40.0, 20.0]);
        // Larger than the item: scale_down shrinks it, none overflows.
        let big = |fit| place(fit, Align::Center, [100.0, 100.0], [400.0, 200.0]);
        assert_eq!(big(Fit::ScaleDown), [0.0, 25.0, 100.0, 50.0]);
        assert_eq!(big(Fit::None), [-150.0, -50.0, 400.0, 200.0]);
        // Half-pixel offsets round to whole pixels.
        assert_eq!(
            place(Fit::None, Align::Center, [101.0, 100.0], [50.0, 50.0]),
            [26.0, 25.0, 50.0, 50.0]
        );
        assert_eq!(
            smoothed_edges([0.0, 25.0, 100.0, 50.0], [100.0, 100.0]),
            [-1e9, 25.0, 1e9, 75.0]
        );
    }

    #[test]
    fn each_blend_mode_finds_its_own_pipeline() {
        assert_eq!(BLENDS.map(blend_index), [0, 1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn overlapping_radii_shrink_together_as_in_css() {
        assert_eq!(
            fit_radii([100.0, 100.0, 0.0, 0.0], [100.0, 50.0]),
            [50.0, 50.0, 0.0, 0.0]
        );
        assert_eq!(fit_radii([10.0; 4], [100.0, 50.0]), [10.0; 4]);
        assert_eq!(fit_radii([-5.0, 0.0, 0.0, 0.0], [10.0, 10.0]), [0.0; 4]);
    }

    #[test]
    fn records_are_the_shaders_struct() {
        let placed = Placed {
            crop: [0.0; 4],
            fit: Fit::Stretch,
            align: Align::Center,
            scale_filter: ScaleFilter::Bilinear,
            size: [30.0, 40.0],
            transform: Affine::translate(1.0, 2.0),
            opacity: 0.5,
            blend: Blend::Normal,
            background: None,
            radius: [0.0; 4],
            border: None,
            shadow: None,
            shift: [0.0; 2],
            clip: 3,
        };
        let texels = Texels {
            width: 60,
            height: 80,
            kind: TexelKind::Premultiplied,
        };
        let drawn = record(&placed, Some(texels), Mode::Picture, (100, 200), 2.0).unwrap();
        let mut bytes = Vec::new();
        record_bytes(&[drawn, drawn], &mut bytes);
        assert_eq!(bytes.len() as u64, 2 * RECORD_BYTES);
        assert_eq!(drawn[2], [100.0, 200.0, 30.0, 40.0]);
        assert_eq!(drawn[8], [0.5, 2.0, 0.0, 0.0]);
        assert_eq!(drawn[16][0], 1.0);
        // The clip it is inside, as an index + 1.
        assert_eq!(drawn[16][3], 3.0);
        // A clip maps target pixels back to its item's own.
        let clip = clip_record(
            &LayerClip {
                transform: Affine::translate(10.0, 20.0).after(Affine::scale(2.0)),
                size: [50.0, 40.0],
                radius: [8.0; 4],
            },
            1,
        );
        record_bytes(&[clip], &mut bytes);
        assert_eq!(bytes.len() as u64, CLIP_BYTES);
        assert_eq!(clip[0], [0.5, 0.0, -5.0, 0.0]);
        assert_eq!(clip[1], [0.0, 0.5, -10.0, 0.0]);
        assert_eq!(clip[2], [50.0, 40.0, 1.0, 0.5]);
        assert_eq!(clip[3], [8.0; 4]);
        // A crop that leaves nothing draws nothing.
        let mut cropped = placed;
        cropped.crop[0] = 60.0;
        assert!(record(&cropped, Some(texels), Mode::Picture, (100, 200), 2.0).is_none());
        // A fill doesn't look at the picture.
        assert!(record(&cropped, Some(texels), Mode::Fill, (100, 200), 2.0).is_some());
    }
}
