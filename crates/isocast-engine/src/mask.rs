//! Mask filters' passes (#115): shapes and gradients drawn as signed
//! distances and ramps in a fragment shader, and other sources' or scenes'
//! pictures read as masks, all on the GPU. Each pass multiplies the
//! picture by the mask, or (with `combine`) adds it to or takes it from
//! what the masks before it left of the picture before them.
//!
//! Their numbers can be animated by stops written in the filter; they are
//! evaluated here each frame, without allocating.

use std::f32::consts::PI;

use isocast_protocol::{
    Animation, GradientMask, GradientShape, MaskAnimation, MaskChannel, MaskCombine, MaskFit,
    MaskKeyframe, MaskShape, ShapeMask, SourceMask,
};

use crate::Time;
use crate::animate::Timing;

/// Where a mask pass is, on the clocks its animations run on.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Clock {
    pub at: Time,
    /// When the source's filters first ran (`on_air` and `shown`).
    pub started: Time,
    /// When the engine's clock started (`clock`).
    pub epoch: Time,
    /// Every animation is at its start (snapshots, which show one moment
    /// with no clock running).
    pub still: bool,
}

impl Clock {
    /// No animation has started yet (sizes don't depend on them).
    pub(crate) const ZERO: Clock = Clock {
        at: Time::from_nanos(0),
        started: Time::from_nanos(0),
        epoch: Time::from_nanos(0),
        still: true,
    };
}

/// A number masks animate.
#[derive(Debug, Clone, Copy)]
enum Num {
    X,
    Y,
    Width,
    Height,
    Rotation,
    Radius(usize),
    InnerRadius,
    Feather,
    Angle,
    Length,
    Scale,
    Threshold,
    Softness,
}

impl Num {
    fn read(self, frame: &MaskKeyframe) -> Option<f32> {
        match self {
            Self::X => frame.x,
            Self::Y => frame.y,
            Self::Width => frame.width,
            Self::Height => frame.height,
            Self::Rotation => frame.rotation,
            Self::Radius(corner) => frame.radius.map(|radius| radius.corners()[corner]),
            Self::InnerRadius => frame.inner_radius,
            Self::Feather => frame.feather,
            Self::Angle => frame.angle,
            Self::Length => frame.length,
            Self::Scale => frame.scale,
            Self::Threshold => frame.threshold,
            Self::Softness => frame.softness,
        }
    }
}

/// `num`'s value at `clock` with `animations` playing over its own value
/// `base` (later animations over earlier ones, as CSS's do).
fn animated(animations: &[MaskAnimation], num: Num, base: f32, clock: &Clock) -> f32 {
    let mut value = base;
    for animation in animations {
        if !animation
            .frames
            .iter()
            .any(|frame| num.read(frame).is_some())
        {
            continue;
        }
        let timing = Timing::of(&Animation {
            keyframes: String::new(),
            duration_ms: animation.duration_ms,
            delay_ms: animation.delay_ms,
            easing: animation.easing,
            iterations: animation.iterations,
            direction: animation.direction,
            fill: animation.fill,
            start: animation.start,
        });
        let start = match animation.start {
            isocast_protocol::AnimationStart::Clock => clock.epoch,
            _ => clock.started,
        };
        let elapsed = match clock.still {
            true => 0,
            false => clock.at.as_nanos().saturating_sub(start.as_nanos()).max(0),
        };
        let Some(progress) = timing.progress(elapsed) else {
            continue;
        };
        value = sample(animation, num, progress as f32, value);
    }
    value
}

/// `num` `progress` (0 to 1) of the way through a run of `animation`,
/// over `under` (what implicit first and last stops take).
fn sample(animation: &MaskAnimation, num: Num, progress: f32, under: f32) -> f32 {
    // The stop at or before `progress`, and the one after it, of those
    // that set `num`; implicit ones at 0 and 1 take `under`.
    let mut before = (0.0, under, animation.easing);
    let mut after = None;
    for frame in &animation.frames {
        let Some(value) = num.read(frame) else {
            continue;
        };
        let at = frame.at.clamp(0.0, 1.0);
        let easing = frame.easing.unwrap_or(animation.easing);
        if at <= progress {
            before = (at, value, easing);
        } else {
            after = Some((at, value));
            break;
        }
    }
    let (to_at, to) = match after {
        Some(after) => after,
        // The last stop that sets it is at the end: it holds there.
        None if before.0 >= 1.0 => return before.1,
        None => (1.0, under),
    };
    let span = to_at - before.0;
    let t = match span > 0.0 {
        true => before.2.apply((progress - before.0) / span),
        false => 1.0,
    };
    before.1 + (to - before.1) * t
}

/// What a mask pass needs: its fragment shader and parameters.
pub(crate) struct MaskPass {
    pub entry: &'static str,
    pub params: [[f32; 4]; 8],
}

fn combine(combine: MaskCombine) -> f32 {
    match combine {
        MaskCombine::Intersect => 0.0,
        MaskCombine::Add => 1.0,
        MaskCombine::Subtract => 2.0,
    }
}

fn flag(on: bool) -> f32 {
    if on { 1.0 } else { 0.0 }
}

/// A shape mask's pass on a picture of `size`, at `clock`.
pub(crate) fn shape(settings: &ShapeMask, (w, h): (u32, u32), clock: &Clock) -> MaskPass {
    let a = &settings.animations;
    let num = |num, base| animated(a, num, base, clock);
    let (w, h) = (w as f32, h as f32);
    let x = num(Num::X, settings.x) * w;
    let y = num(Num::Y, settings.y) * h;
    let half = [
        (num(Num::Width, settings.width) * w / 2.0).max(0.0),
        (num(Num::Height, settings.height) * h / 2.0).max(0.0),
    ];
    let rotation = num(Num::Rotation, settings.rotation).to_radians();
    let corners = settings.radius.corners();
    let radius: [f32; 4] = std::array::from_fn(|k| num(Num::Radius(k), corners[k]).max(0.0));
    let feather = num(Num::Feather, settings.feather).max(0.0);
    let inner = num(Num::InnerRadius, settings.inner_radius).clamp(0.01, 1.0);
    let (id, count) = match settings.shape {
        MaskShape::Rectangle => (0.0, 0),
        MaskShape::Ellipse => (1.0, 0),
        MaskShape::Circle => (2.0, 0),
        MaskShape::Polygon => (3.0, settings.sides.clamp(3, 64)),
        MaskShape::Star => (4.0, settings.points.clamp(3, 64)),
        MaskShape::Heart => (5.0, 0),
    };
    // The shape's size (circumradius, or the heart's square side) and
    // where its middle is, so the largest one fits the box, centred.
    let (size, shift) = match settings.shape {
        MaskShape::Rectangle | MaskShape::Ellipse => (0.0, [0.0; 2]),
        MaskShape::Circle => (half[0].min(half[1]), [0.0; 2]),
        MaskShape::Polygon | MaskShape::Star => {
            // A point at the top, the points every 360/n degrees.
            let n = count as f32;
            let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
            // A star's notches too: deep ones (a large inner radius) can
            // reach past its points' bounds.
            let notch = match settings.shape {
                MaskShape::Star => inner,
                _ => 0.0,
            };
            for k in 0..count {
                let angle = 2.0 * PI * k as f32 / n;
                let half = angle + PI / n;
                for corner in [
                    [angle.sin(), -angle.cos()],
                    [notch * half.sin(), -notch * half.cos()],
                ] {
                    for i in 0..2 {
                        lo[i] = lo[i].min(corner[i]);
                        hi[i] = hi[i].max(corner[i]);
                    }
                }
            }
            let r = (2.0 * half[0] / (hi[0] - lo[0])).min(2.0 * half[1] / (hi[1] - lo[1]));
            (r, [-(lo[0] + hi[0]) / 2.0 * r, -(lo[1] + hi[1]) / 2.0 * r])
        }
        MaskShape::Heart => {
            // A square of side s turned 45° with circles on its upper
            // sides: 1.7071 s wide, 1.5607 s tall.
            let reach = 0.5 + std::f32::consts::FRAC_1_SQRT_2 / 2.0;
            let top = reach;
            let bottom = std::f32::consts::FRAC_1_SQRT_2;
            let s = (2.0 * half[0] / (2.0 * reach)).min(2.0 * half[1] / (top + bottom));
            // y down: its middle is (top - bottom) / 2 · s above the
            // square's centre.
            (s, [0.0, (top - bottom) / 2.0 * s])
        }
    };
    let mut params = [[0.0; 4]; 8];
    params[0] = [x, y, half[0], half[1]];
    params[1] = [rotation, feather, id, flag(settings.invert)];
    params[2] = radius;
    params[3] = [count as f32, inner, combine(settings.combine), size];
    params[4] = [shift[0], shift[1], 0.0, 0.0];
    MaskPass {
        entry: "shape_mask",
        params,
    }
}

/// A gradient mask's pass on a picture of `size`, at `clock`.
pub(crate) fn gradient(settings: &GradientMask, (w, h): (u32, u32), clock: &Clock) -> MaskPass {
    let a = &settings.animations;
    let num = |num, base| animated(a, num, base, clock);
    let (w, h) = (w as f32, h as f32);
    let x = num(Num::X, settings.x) * w;
    let y = num(Num::Y, settings.y) * h;
    let angle = num(Num::Angle, settings.angle).to_radians();
    let (sin, cos) = angle.sin_cos();
    let length = num(Num::Length, settings.length).max(0.0001);
    let (shape, length) = match settings.gradient {
        GradientShape::Linear => (0.0, length * (w * cos.abs() + h * sin.abs())),
        GradientShape::Radial => (1.0, length * (w * w + h * h).sqrt() / 2.0),
    };
    let feather = num(Num::Feather, settings.feather).max(0.0);
    let mut params = [[0.0; 4]; 8];
    params[0] = [x, y, cos, sin];
    params[1] = [length.max(0.0001), feather, shape, flag(settings.invert)];
    let stops = &settings.stops[..settings.stops.len().min(8)];
    params[2] = [stops.len() as f32, combine(settings.combine), 0.0, 0.0];
    for (k, stop) in stops.iter().enumerate() {
        let slot = &mut params[4 + k / 2];
        slot[(k % 2) * 2] = stop.at;
        slot[(k % 2) * 2 + 1] = stop.opacity;
    }
    MaskPass {
        entry: "gradient_mask",
        params,
    }
}

/// A source mask's pass on a picture of `size`, the mask's picture being
/// `mask` big, at `clock`.
pub(crate) fn source(
    settings: &SourceMask,
    (w, h): (u32, u32),
    mask: (u32, u32),
    clock: &Clock,
) -> MaskPass {
    let a = &settings.animations;
    let num = |num, base| animated(a, num, base, clock);
    let (w, h) = (w as f32, h as f32);
    let (mw, mh) = (mask.0.max(1) as f32, mask.1.max(1) as f32);
    let size = match settings.fit {
        MaskFit::Stretch => [w, h],
        MaskFit::Contain => {
            let k = (w / mw).min(h / mh);
            [mw * k, mh * k]
        }
        MaskFit::Cover => {
            let k = (w / mw).max(h / mh);
            [mw * k, mh * k]
        }
        MaskFit::None => [mw, mh],
    };
    let scale = num(Num::Scale, settings.scale).max(0.0001);
    let channel = match settings.channel {
        MaskChannel::Luminance => 0.0,
        MaskChannel::Alpha => 1.0,
        MaskChannel::Red => 2.0,
        MaskChannel::Green => 3.0,
        MaskChannel::Blue => 4.0,
    };
    let mut params = [[0.0; 4]; 8];
    params[0] = [
        num(Num::X, settings.x) * w,
        num(Num::Y, settings.y) * h,
        size[0] * scale / 2.0,
        size[1] * scale / 2.0,
    ];
    params[1] = [
        channel,
        num(Num::Threshold, settings.threshold).clamp(0.0, 1.0),
        num(Num::Softness, settings.softness).clamp(0.0, 1.0),
        flag(settings.invert),
    ];
    params[2] = [combine(settings.combine), 0.0, 0.0, 0.0];
    MaskPass {
        entry: "source_mask",
        params,
    }
}

/// The mask passes' fragment shaders, added to the filters' (they use its
/// bindings: `input`, `params`, `mask` for a source mask's picture, and
/// `base`, the picture before a run of masks, for `combine`).
pub(crate) const SHADER: &str = r#"
// The input's premultiplied pixel under `in`, through the mask `m` (0 to
// 1), combined as `mode` says (0 intersect, 1 add, 2 subtract) with what
// the masks before it left of `base`.
fn through(in: Varyings, m: f32, mode: f32) -> vec4<f32> {
    let at = vec2<i32>(floor(in.uv * vec2<f32>(textureDimensions(input))));
    let c = texel(at);
    let mode_i = i32(mode + 0.5);
    if (mode_i == 0) {
        return c * m;
    }
    let bsize = vec2<i32>(textureDimensions(base));
    let b = textureLoad(base, clamp(at, vec2<i32>(0), bsize - 1), 0);
    let before = select(0.0, clamp(c.a / b.a, 0.0, 1.0), b.a > 0.0);
    var cover = clamp(before + m, 0.0, 1.0);
    if (mode_i == 2) {
        cover = clamp(before - m, 0.0, 1.0);
    }
    return b * cover;
}

// How much shows `d` pixels outside an edge (inside is negative), its
// edge softened over `feather` pixels (at least one, for smooth edges).
fn edge(d: f32, feather: f32) -> f32 {
    return clamp(0.5 - d / max(feather, 1.0), 0.0, 1.0);
}

// A rounded box `half` big about the origin (y down), each corner's
// radius in r: top left, top right, bottom right, bottom left.
fn sd_box(p: vec2<f32>, half: vec2<f32>, r: vec4<f32>) -> f32 {
    let bottom = select(r.w, r.z, p.x > 0.0);
    let corner = select(bottom, select(r.x, r.y, p.x > 0.0), p.y < 0.0);
    let rr = min(corner, min(half.x, half.y));
    let q = abs(p) - half + rr;
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0))) - rr;
}

// An ellipse with half-axes `ab` (an approximation, close near its edge).
fn sd_ellipse(p: vec2<f32>, ab: vec2<f32>) -> f32 {
    let r = max(ab, vec2<f32>(1e-3));
    let k0 = length(p / r);
    let k1 = length(p / (r * r));
    if (k1 < 1e-6) {
        return -min(r.x, r.y);
    }
    return k0 * (k0 - 1.0) / k1;
}

// `q` (y up) folded into the sector of angle `sector` about the up axis:
// x across it (0 or more), y along it.
fn fold(q: vec2<f32>, sector: f32, offset: f32) -> vec2<f32> {
    let a = atan2(q.x, q.y) - offset;
    let folded = a - sector * round(a / sector);
    return length(q) * vec2<f32>(abs(sin(folded)), cos(folded));
}

// A regular polygon with `n` sides, corners `big` from its centre, one at
// the top (y up).
fn sd_polygon(q: vec2<f32>, n: f32, big: f32) -> f32 {
    let sector = 6.2831853 / n;
    let half = sector * 0.5;
    // Folded about the middle of a side.
    let f = fold(q, sector, half);
    let apothem = big * cos(half);
    let side = big * sin(half);
    if (f.y < apothem) {
        return f.y - apothem;
    }
    return length(vec2<f32>(f.x - min(f.x, side), f.y - apothem));
}

// A star with `n` points `big` from its centre and notches `inner` from
// it, a point at the top (y up).
fn sd_star(q: vec2<f32>, n: f32, big: f32, inner: f32) -> f32 {
    let sector = 6.2831853 / n;
    let half = sector * 0.5;
    let f = fold(q, sector, 0.0);
    let tip = vec2<f32>(0.0, big);
    let notch = inner * vec2<f32>(sin(half), cos(half));
    let e = notch - tip;
    let w = f - tip;
    // A star with no size (its box is 0 wide): no division by 0.
    let t = clamp(dot(w, e) / max(dot(e, e), 1e-6), 0.0, 1.0);
    let d = length(w - e * t);
    let side = e.x * w.y - e.y * w.x;
    return select(d, -d, side < 0.0);
}

// A heart made of a square of side `s` turned 45° and two circles on its
// upper sides (y up, the square's centre at the origin).
fn sd_heart(q: vec2<f32>, s: f32) -> f32 {
    let k = 0.70710678;
    let u = vec2<f32>((q.x + q.y) * k, (q.y - q.x) * k);
    let square = sd_box(u, vec2<f32>(s * 0.5), vec4<f32>(0.0));
    let c = s * k * 0.5;
    let left = length(q - vec2<f32>(-c, c)) - s * 0.5;
    let right = length(q - vec2<f32>(c, c)) - s * 0.5;
    return min(square, min(left, right));
}

// v0: centre and half its box, in pixels; v1: rotation (radians,
// clockwise), feather, shape (0 rectangle, 1 ellipse, 2 circle, 3
// polygon, 4 star, 5 heart), invert; v2: corner radii; v3: sides or
// points, inner radius, combine, size; v4.xy: the shape's shift in its
// box.
@fragment
fn shape_mask(in: Varyings) -> @location(0) vec4<f32> {
    let p = params.v;
    let px = in.uv * vec2<f32>(textureDimensions(input)) - p[0].xy;
    let s = sin(p[1].x);
    let c = cos(p[1].x);
    // Into the shape's own frame (turned back), y down.
    let local = vec2<f32>(px.x * c + px.y * s, -px.x * s + px.y * c);
    let kind = i32(p[1].z + 0.5);
    let half = p[0].zw;
    var d = 0.0;
    if (kind == 0) {
        d = sd_box(local, half, p[2]);
    } else if (kind == 1) {
        d = sd_ellipse(local, half);
    } else if (kind == 2) {
        d = length(local) - p[3].w;
    } else {
        // y up, about the shape's own middle.
        let q = vec2<f32>(local.x - p[4].x, -(local.y - p[4].y));
        let r = min(p[2].x, p[3].w * 0.9);
        if (kind == 3) {
            let n = p[3].x;
            d = sd_polygon(q, n, max(p[3].w - r / cos(3.14159265 / n), 0.0)) - r;
        } else if (kind == 4) {
            let big = max(p[3].w - r, 0.0);
            d = sd_star(q, p[3].x, big, max(p[3].y * p[3].w - r, 0.0)) - r;
        } else {
            d = sd_heart(q, p[3].w);
        }
    }
    var m = edge(d, p[1].y);
    if (p[1].w > 0.5) {
        m = 1.0 - m;
    }
    return through(in, m, p[3].z);
}

// The opacity at `t` along stops (at, opacity) in v4..v7, `count` of them.
fn ramp(t: f32, count: i32) -> f32 {
    let p = params.v;
    var last = vec2<f32>(p[4].x, p[4].y);
    if (t <= last.x) {
        return last.y;
    }
    for (var k = 1; k < count; k++) {
        let slot = p[4 + k / 2];
        let stop = select(slot.xy, slot.zw, k % 2 == 1);
        if (t <= stop.x) {
            let span = stop.x - last.x;
            if (span <= 0.0) {
                return stop.y;
            }
            return mix(last.y, stop.y, (t - last.x) / span);
        }
        last = stop;
    }
    return last.y;
}

// v0: centre (pixels), direction (cos, sin); v1: length (pixels),
// feather, shape (0 linear, 1 radial), invert; v2.x: stops, v2.y:
// combine; v4..v7: stops.
@fragment
fn gradient_mask(in: Varyings) -> @location(0) vec4<f32> {
    let p = params.v;
    let px = in.uv * vec2<f32>(textureDimensions(input)) - p[0].xy;
    var t = length(px) / p[1].x;
    if (p[1].z < 0.5) {
        t = dot(px, p[0].zw) / p[1].x + 0.5;
    }
    let count = i32(p[2].x + 0.5);
    var m = ramp(t, count);
    if (p[1].y > 0.0) {
        // Averaged across the feather.
        let spread = p[1].y / p[1].x;
        m = 0.0;
        for (var k = 0; k < 5; k++) {
            m += ramp(t + (f32(k) / 4.0 - 0.5) * spread, count);
        }
        m = m / 5.0;
    }
    if (p[1].w > 0.5) {
        m = 1.0 - m;
    }
    return through(in, m, p[2].y);
}

// v0: where the mask's picture is: its centre and half its size, in
// pixels; v1: channel (0 luminance, 1 alpha, 2 red, 3 green, 4 blue),
// threshold, softness, invert; v2.x: combine; v2.y: 1 while the mask has
// no picture, when nothing shows.
@fragment
fn source_mask(in: Varyings) -> @location(0) vec4<f32> {
    let p = params.v;
    if (p[2].y > 0.5) {
        return vec4<f32>(0.0);
    }
    let px = in.uv * vec2<f32>(textureDimensions(input));
    let at = (px - (p[0].xy - p[0].zw)) / (2.0 * p[0].zw);
    var v = 0.0;
    if (all(at >= vec2<f32>(0.0)) && all(at <= vec2<f32>(1.0))) {
        // Premultiplied: transparent reads as black.
        let s = textureSampleLevel(mask, smp, at, 0.0);
        let channel = i32(p[1].x + 0.5);
        if (channel == 0) {
            v = dot(s.rgb, LUMA);
        } else if (channel == 1) {
            v = s.a;
        } else if (channel == 2) {
            v = s.r;
        } else if (channel == 3) {
            v = s.g;
        } else {
            v = s.b;
        }
    }
    let threshold = p[1].y;
    let soft = p[1].z;
    var m = select(0.0, 1.0, v >= threshold);
    if (soft > 0.0) {
        m = clamp((v - (threshold - soft * 0.5)) / soft, 0.0, 1.0);
    }
    if (p[1].w > 0.5) {
        m = 1.0 - m;
    }
    return through(in, m, p[2].x);
}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn clock(ms: i64) -> Clock {
        Clock {
            at: Time::from_nanos(ms * 1_000_000),
            started: Time::from_nanos(0),
            epoch: Time::from_nanos(0),
            still: false,
        }
    }

    #[test]
    fn animations_move_numbers_between_their_stops_and_hold_the_last() {
        let mask: ShapeMask = serde_json::from_value(json!({
            "rotation": 10,
            "animations": [{"duration_ms": 1000, "easing": "linear", "frames": [
                {"at": 0.5, "rotation": 30, "x": 0.25},
                {"at": 1, "rotation": 90},
            ]}],
        }))
        .unwrap();
        let a = &mask.animations;
        // From its own value at the implicit first stop.
        assert!((animated(a, Num::Rotation, 10.0, &clock(250)) - 20.0).abs() < 1e-4);
        assert!((animated(a, Num::Rotation, 10.0, &clock(750)) - 60.0).abs() < 1e-4);
        // Held at its last stop after the run (fill both).
        assert_eq!(animated(a, Num::Rotation, 10.0, &clock(5000)), 90.0);
        // x goes back to its own value at the implicit last stop.
        assert!((animated(a, Num::X, 0.5, &clock(750)) - 0.375).abs() < 1e-4);
        // Numbers no stop sets keep their own value.
        assert_eq!(animated(a, Num::Width, 0.8, &clock(750)), 0.8);
    }

    #[test]
    fn shapes_fit_their_box() {
        let mut mask = ShapeMask {
            shape: MaskShape::Polygon,
            sides: 4,
            width: 0.5,
            height: 1.0,
            ..ShapeMask::default()
        };
        let pass = shape(&mask, (400, 200), &Clock::ZERO);
        // A diamond 100 px wide fits a 200 × 200 box: corners 100 px out.
        assert!((pass.params[3][3] - 100.0).abs() < 1e-3);
        mask.shape = MaskShape::Heart;
        let pass = shape(&mask, (400, 200), &Clock::ZERO);
        let s = pass.params[3][3];
        assert!((s * 1.7071 - 200.0).abs() < 0.1, "{s}");
    }
}
