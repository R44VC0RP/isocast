//! Keyframe animations (5.2a). Items' `animations` are compiled with the
//! scenes into tracks of floats (stops, easings, timings), and the
//! compositor evaluates them at each tick: for each animated item, the
//! time on its animations' clocks gives each property's value, which the
//! draw list composes into the records of the item and of the items inside
//! it. Nothing is laid out, parsed or allocated per tick, and a frame is a
//! pure function of the compiled scenes and the tick's time.

use std::collections::HashMap;
use std::ops::Range;

use isocast_protocol::{
    Animation, AnimationStart, Color, Easing, Fill, Iterations, Keyframe, Keyframes, PlayDirection,
    SceneItem, Transform, translate_px,
};

use crate::render::{Affine, LayerShadow, Placed};

/// What an item's animations can change, as floats: each [`Property`]'s
/// span. Colours are premultiplied, so they interpolate as CSS's do.
pub(crate) type Values = [f32; VALUES];

const VALUES: usize = 29;

/// The widest property (a shadow: x, y, blur and a colour).
const WIDEST: usize = 7;

/// A property animations can change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Property {
    Translate,
    Scale,
    Rotate,
    Opacity,
    Background,
    BorderColor,
    BorderWidth,
    Radius,
    Shadow,
    Crop,
}

impl Property {
    const ALL: [Self; 10] = [
        Self::Translate,
        Self::Scale,
        Self::Rotate,
        Self::Opacity,
        Self::Background,
        Self::BorderColor,
        Self::BorderWidth,
        Self::Radius,
        Self::Shadow,
        Self::Crop,
    ];

    /// Where its floats are in [`Values`].
    const fn span(self) -> Range<usize> {
        match self {
            Self::Translate => 0..2,
            Self::Scale => 2..3,
            Self::Rotate => 3..4,
            Self::Opacity => 4..5,
            Self::Background => 5..9,
            Self::BorderColor => 9..13,
            Self::BorderWidth => 13..14,
            Self::Radius => 14..18,
            Self::Shadow => 18..25,
            Self::Crop => 25..29,
        }
    }

    const fn bit(self) -> u16 {
        1 << self as u16
    }

    /// Its value at a keyframe stop, for an item `size` pixels big, if the
    /// stop sets it.
    fn at(self, frame: &Keyframe, size: [f32; 2]) -> Option<[f32; WIDEST]> {
        let mut out = [0.0; WIDEST];
        match self {
            Self::Translate => {
                let [x, y] = frame.translate.as_ref()?;
                out[0] = translate_px(x, size[0]).unwrap_or(0.0);
                out[1] = translate_px(y, size[1]).unwrap_or(0.0);
            }
            Self::Scale => out[0] = frame.scale?,
            Self::Rotate => out[0] = frame.rotate?,
            Self::Opacity => out[0] = frame.opacity?,
            Self::Background => {
                out[..4].copy_from_slice(&premultiplied(frame.background.as_ref()?))
            }
            Self::BorderColor => {
                out[..4].copy_from_slice(&premultiplied(frame.border_color.as_ref()?));
            }
            Self::BorderWidth => out[0] = frame.border_width?,
            Self::Radius => out[..4].copy_from_slice(&frame.radius?.corners()),
            Self::Shadow => {
                let shadow = frame.shadow.as_ref()?;
                out[..3].copy_from_slice(&[shadow.x, shadow.y, shadow.blur]);
                out[3..7].copy_from_slice(&premultiplied(&shadow.color));
            }
            Self::Crop => {
                let crop = frame.crop?;
                out[..4].copy_from_slice(
                    &[crop.left, crop.top, crop.right, crop.bottom].map(|v| v as f32),
                );
            }
        }
        Some(out)
    }
}

/// Properties that change where an item is drawn: its transform.
pub(crate) const TRANSFORMS: u16 =
    Property::Translate.bit() | Property::Scale.bit() | Property::Rotate.bit();

/// Properties that change an item and everything inside it: its transform
/// and opacity.
pub(crate) const MOVES: u16 = TRANSFORMS | Property::Opacity.bit();

/// The animations of a setup's scenes, compiled.
#[derive(Default)]
pub(crate) struct Animations {
    /// Animated items, parents before children, in ranges by scene
    /// (`CompiledScene::animated`).
    pub items: Vec<AnimatedItem>,
    runs: Vec<Run>,
    tracks: Vec<Track>,
    stops: Vec<Stop>,
    /// Items whose records animations change (animated ones, and those
    /// inside an item whose transform or opacity animates), parents before
    /// children, in ranges by scene (`CompiledScene::touched`): what the
    /// compositor walks each tick, with all it needs from their nodes.
    pub touched: Vec<Touched>,
}

/// An item whose records animations change, with what the compositor needs
/// of its node each tick (kept together, so the pass reads little memory).
pub(crate) struct Touched {
    /// Its node.
    pub node: u32,
    /// Its parent (node index + 1) if animations touch it too, or 0: then
    /// it is placed in `outer`.
    pub parent: u32,
    /// Its animations (their index + 1 in `Animations::items`), or 0.
    pub item: u32,
    /// Its own draws.
    pub draws: Range<u32>,
    /// The clip it makes for its children (index + 1), or 0.
    pub clip: u32,
    /// An animation of its own or of an item it is inside changes its
    /// transform or opacity.
    pub moved: bool,
    /// Its animations change its style, so its records are written whole.
    pub styled: bool,
    /// Its parent's transform (or its scene's root), as compiled.
    pub outer: Affine,
    /// Where it is placed in its parent, as compiled.
    pub place: Affine,
}

/// An item with animations.
pub(crate) struct AnimatedItem {
    /// Its node id, if an animation runs on the `shown` clock (which is
    /// kept by node id).
    pub shown: Option<u32>,
    /// Its values with no animation: its own style and crop, not moved.
    pub base: Values,
    /// Where its centre is in its parent.
    center: [f32; 2],
    /// Its own transform about its centre, from its own pixels: what the
    /// animated translate, rotate and scale go around (as CSS's
    /// `translate`, `rotate` and `scale` go around `transform`).
    own: Affine,
    /// Its animations, in `Animations::runs`.
    runs: Range<u32>,
    /// What its animations change ([`Property::bit`]s).
    pub props: u16,
    /// How many properties its animations animate (each animation's
    /// count), for the compositor's status.
    pub tracks: u32,
}

/// One animation of an item.
struct Run {
    timing: Timing,
    /// Its tracks, in `Animations::tracks`.
    tracks: Range<u32>,
}

/// When an animation runs, on which clock.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Timing {
    /// One run, in nanoseconds.
    duration: i64,
    delay: i64,
    /// `f64::INFINITY` for infinite.
    iterations: f64,
    direction: PlayDirection,
    fill: Fill,
    pub clock: AnimationStart,
}

/// One property of one animation: its stops.
struct Track {
    property: Property,
    /// In `Animations::stops`, in order of `at`, the first at 0 and the
    /// last at 1.
    stops: Range<u32>,
}

#[derive(Debug, Clone, Copy)]
struct Stop {
    at: f32,
    /// Its value, or `None` for the value underneath (the item's own, or
    /// an earlier animation's): a property's implicit first or last stop.
    value: Option<[f32; WIDEST]>,
    /// How the stretch to the next stop goes.
    easing: Easing,
}

/// The times an item's animation clocks started.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Clocks {
    /// When its scene came on air (for the feed being drawn).
    pub on_air: crate::Time,
    /// When it was shown.
    pub shown: crate::Time,
    /// When the engine started.
    pub epoch: crate::Time,
}

impl Animations {
    /// Compile `item`'s animations (its node id is `id`, its rect at `rect`
    /// in its parent), with the setup's keyframe sets `sets`: its index
    /// among the animated items, or `None` if they change nothing.
    pub(crate) fn add(
        &mut self,
        item: &SceneItem,
        id: u32,
        rect: [f32; 4],
        sets: &HashMap<&str, &Keyframes>,
    ) -> Option<u32> {
        if item.animations.is_empty() {
            return None;
        }
        let [x, y, width, height] = rect;
        let size = [width, height];
        let runs = self.runs.len() as u32;
        let first_track = self.tracks.len();
        let mut props = 0;
        for animation in &item.animations {
            let Some(set) = sets.get(animation.keyframes.as_str()) else {
                continue;
            };
            let tracks = self.tracks.len() as u32;
            for property in Property::ALL {
                if self.track(property, set, animation, size) {
                    props |= property.bit();
                }
            }
            if self.tracks.len() as u32 > tracks {
                self.runs.push(Run {
                    timing: Timing::of(animation),
                    tracks: tracks..self.tracks.len() as u32,
                });
            }
        }
        if props == 0 {
            return None;
        }
        let index = self.items.len() as u32;
        let shown = self.runs[runs as usize..]
            .iter()
            .any(|run| run.timing.clock == AnimationStart::Shown);
        self.items.push(AnimatedItem {
            shown: shown.then_some(id),
            base: base(item),
            center: [x + width / 2.0, y + height / 2.0],
            own: own_transform(item.style.transform, size),
            runs: runs..self.runs.len() as u32,
            props,
            tracks: (self.tracks.len() - first_track) as u32,
        });
        Some(index)
    }

    /// Add `property`'s track of `animation` playing `set` on an item
    /// `size` pixels big, if any stop sets it; returns whether it did.
    fn track(
        &mut self,
        property: Property,
        set: &Keyframes,
        animation: &Animation,
        size: [f32; 2],
    ) -> bool {
        let first = self.stops.len() as u32;
        for frame in &set.frames {
            if let Some(value) = property.at(frame, size) {
                self.stops.push(Stop {
                    at: frame.at.clamp(0.0, 1.0),
                    value: Some(value),
                    easing: frame.easing.unwrap_or(animation.easing),
                });
            }
        }
        let stops = &mut self.stops;
        if stops.len() as u32 == first {
            return false;
        }
        // From and to the value underneath where no stop sets it (CSS's
        // implicit 0% and 100% keyframes).
        let implicit = |at| Stop {
            at,
            value: None,
            easing: animation.easing,
        };
        if stops[first as usize].at > 0.0 {
            stops.insert(first as usize, implicit(0.0));
        }
        if stops.last().is_some_and(|stop| stop.at < 1.0) {
            stops.push(implicit(1.0));
        }
        self.tracks.push(Track {
            property,
            stops: first..self.stops.len() as u32,
        });
        true
    }

    /// Animated item `index`'s values at `at` (its clocks started at
    /// `clocks`), into `values`.
    pub(crate) fn evaluate(
        &self,
        index: usize,
        at: crate::Time,
        clocks: &Clocks,
        values: &mut Values,
    ) {
        let item = &self.items[index];
        *values = item.base;
        for run in &self.runs[range(&item.runs)] {
            let start = match run.timing.clock {
                AnimationStart::OnAir => clocks.on_air,
                AnimationStart::Shown => clocks.shown,
                AnimationStart::Clock => clocks.epoch,
            };
            let elapsed = at.as_nanos().saturating_sub(start.as_nanos()).max(0);
            let Some(progress) = run.timing.progress(elapsed) else {
                continue;
            };
            for track in &self.tracks[range(&run.tracks)] {
                let stops = &self.stops[range(&track.stops)];
                sample(stops, progress as f32, &mut values[track.property.span()]);
            }
        }
    }
}

impl AnimatedItem {
    /// Where it is placed in its parent with `values`: its own transform,
    /// with the animated translate, rotate and scale around it.
    pub(crate) fn place(&self, values: &Values) -> Affine {
        let mut place = Affine::translate(self.center[0] + values[0], self.center[1] + values[1]);
        if values[3] != 0.0 {
            place = place.after(Affine::rotate(values[3]));
        }
        if values[2] != 1.0 {
            place = place.after(Affine::scale(values[2]));
        }
        place.after(self.own)
    }

    /// Whether its transform animates.
    pub(crate) fn moves(&self) -> bool {
        self.props & TRANSFORMS != 0
    }

    /// Its animated opacity, which multiplies its own and its children's.
    pub(crate) fn opacity(&self, values: &Values) -> f32 {
        match self.props & Property::Opacity.bit() {
            0 => 1.0,
            _ => values[4].clamp(0.0, 1.0),
        }
    }

    /// Put the style properties it animates into `placed`.
    pub(crate) fn style(&self, values: &Values, placed: &mut Placed) {
        let has = |property: Property| self.props & property.bit() != 0;
        let color = |property: Property| straight(&values[property.span()]);
        if has(Property::Background) {
            placed.background = Some(color(Property::Background));
        }
        if has(Property::BorderColor) || has(Property::BorderWidth) {
            placed.border = Some((values[13].max(0.0), color(Property::BorderColor)));
        }
        if has(Property::Radius) {
            placed.radius = std::array::from_fn(|i| values[14 + i].max(0.0));
        }
        if has(Property::Shadow) {
            placed.shadow = Some(LayerShadow {
                offset: [values[18], values[19]],
                blur: values[20].max(0.0),
                color: straight(&values[21..25]),
            });
        }
        if has(Property::Crop) {
            placed.crop = std::array::from_fn(|i| values[25 + i].max(0.0));
        }
    }

    /// Its animated radius, if it animates one (for the clip it makes).
    pub(crate) fn radius(&self, values: &Values) -> Option<[f32; 4]> {
        (self.props & Property::Radius.bit() != 0)
            .then(|| std::array::from_fn(|i| values[14 + i].max(0.0)))
    }

    /// Whether it animates its background or border (so it needs a fill
    /// draw), or its shadow (so it needs a shadow draw).
    pub(crate) fn fills(&self) -> bool {
        let fill =
            Property::Background.bit() | Property::BorderColor.bit() | Property::BorderWidth.bit();
        self.props & fill != 0
    }
}

impl Animations {
    /// Whether animated item `index` animates its shadow, and the margin its
    /// shadow draw needs for the largest shadow it reaches.
    pub(crate) fn shadow_margin(&self, index: usize) -> Option<f32> {
        let item = &self.items[index];
        if item.props & Property::Shadow.bit() == 0 {
            return None;
        }
        let reach = |value: &[f32]| 1.5 * value[2].max(0.0) + value[0].abs().max(value[1].abs());
        let mut most = reach(&item.base[Property::Shadow.span()]);
        for run in &self.runs[range(&item.runs)] {
            for track in &self.tracks[range(&run.tracks)] {
                if track.property != Property::Shadow {
                    continue;
                }
                for stop in &self.stops[range(&track.stops)] {
                    if let Some(value) = &stop.value {
                        most = most.max(reach(value));
                    }
                }
            }
        }
        // As `Placed::shadow_margin` (cubic Béziers may overshoot a little).
        Some(most * 1.25 + 2.0)
    }
}

impl Timing {
    pub(crate) fn of(animation: &Animation) -> Self {
        const MS: i64 = 1_000_000;
        Self {
            duration: i64::from(animation.duration_ms) * MS,
            delay: i64::from(animation.delay_ms) * MS,
            iterations: match animation.iterations {
                Iterations::Count(count) => f64::from(count.max(0.0)),
                Iterations::Infinite => f64::INFINITY,
            },
            direction: animation.direction,
            fill: animation.fill,
            clock: animation.start,
        }
    }

    /// How far through a run (0 to 1, after its direction) it is
    /// `elapsed` nanoseconds after its clock started; `None` while it has
    /// no effect (before or after its runs without a fill). Whole
    /// nanoseconds throughout, so a loop stays on schedule however long it
    /// runs.
    pub(crate) fn progress(&self, elapsed: i64) -> Option<f64> {
        let local = elapsed - self.delay;
        let holds_start = matches!(self.fill, Fill::Backwards | Fill::Both);
        let holds_end = matches!(self.fill, Fill::Forwards | Fill::Both);
        if local < 0 {
            return holds_start.then(|| self.directed(0, 0.0));
        }
        let ended = self.duration == 0
            || (self.iterations.is_finite()
                && local as f64 >= self.duration as f64 * self.iterations);
        if ended {
            if !holds_end {
                return None;
            }
            // Where the last run ends: part of the way through it if the
            // count isn't whole.
            let (iteration, simple) = match self.iterations {
                count if !count.is_finite() => (0, 1.0),
                0.0 => (0, 0.0),
                count if count.fract() == 0.0 => (count as i64 - 1, 1.0),
                count => (count.floor() as i64, count.fract()),
            };
            return Some(self.directed(iteration, simple));
        }
        let iteration = local / self.duration;
        let simple = (local % self.duration) as f64 / self.duration as f64;
        Some(self.directed(iteration, simple))
    }

    /// `simple` (0 to 1) of the way through run `iteration`, after the
    /// direction.
    fn directed(&self, iteration: i64, simple: f64) -> f64 {
        let forwards = match self.direction {
            PlayDirection::Normal => true,
            PlayDirection::Reverse => false,
            PlayDirection::Alternate => iteration % 2 == 0,
            PlayDirection::AlternateReverse => iteration % 2 != 0,
        };
        if forwards { simple } else { 1.0 - simple }
    }
}

/// A track's value `progress` (0 to 1) of the way through a run, into
/// `values` (its property's span), which hold the value under it (what its
/// implicit stops take).
fn sample(stops: &[Stop], progress: f32, values: &mut [f32]) {
    let mut at = 0;
    while at + 2 < stops.len() && stops[at + 1].at <= progress {
        at += 1;
    }
    let (from, to) = (&stops[at], &stops[(at + 1).min(stops.len() - 1)]);
    let span = to.at - from.at;
    let t = match span > 0.0 {
        true => from.easing.apply((progress - from.at) / span),
        false => 1.0,
    };
    let width = values.len();
    let mut out = [0.0; WIDEST];
    let under = &*values;
    let a = from.value.as_ref().map_or(under, |value| &value[..width]);
    let b = to.value.as_ref().map_or(under, |value| &value[..width]);
    for ((out, a), b) in out.iter_mut().zip(a).zip(b) {
        *out = a + (b - a) * t;
    }
    values.copy_from_slice(&out[..width]);
}

/// An item's own transform (`style.transform`) about the centre of its
/// `size`, from its own pixels.
fn own_transform(own: Transform, size: [f32; 2]) -> Affine {
    Affine::translate(own.translate[0], own.translate[1])
        .after(Affine::rotate(own.rotate))
        .after(Affine::scale(own.scale))
        .after(Affine::flip(own.flip_horizontal, own.flip_vertical))
        .after(Affine::translate(-size[0] / 2.0, -size[1] / 2.0))
}

/// `range` as `usize`s, to index with.
fn range(range: &Range<u32>) -> Range<usize> {
    range.start as usize..range.end as usize
}

/// `item`'s values with no animation running.
fn base(item: &SceneItem) -> Values {
    let style = &item.style;
    let mut values = [0.0; VALUES];
    values[2] = 1.0;
    values[4] = 1.0;
    if let Some(color) = &style.background {
        values[5..9].copy_from_slice(&premultiplied(color));
    }
    if let Some(border) = &style.border {
        values[9..13].copy_from_slice(&premultiplied(&border.color));
        values[13] = border.width;
    }
    values[14..18].copy_from_slice(&style.radius.corners());
    if let Some(shadow) = &style.shadow {
        values[18..21].copy_from_slice(&[shadow.x, shadow.y, shadow.blur]);
        values[21..25].copy_from_slice(&premultiplied(&shadow.color));
    }
    let crop = item.crop;
    values[25..29]
        .copy_from_slice(&[crop.left, crop.top, crop.right, crop.bottom].map(|v| v as f32));
    values
}

/// `color` with its alpha multiplied in (transparent if it isn't one).
fn premultiplied(color: &Color) -> [f32; 4] {
    let [r, g, b, a] = color.rgba().unwrap_or([0.0; 4]);
    [r * a, g * a, b * a, a]
}

/// A premultiplied colour as straight RGBA, each within 0 to 1.
fn straight(premultiplied: &[f32]) -> [f32; 4] {
    let a = premultiplied[3].clamp(0.0, 1.0);
    if a <= 0.0 {
        return [0.0; 4];
    }
    let channel = |c: f32| (c / a).clamp(0.0, 1.0);
    [
        channel(premultiplied[0]),
        channel(premultiplied[1]),
        channel(premultiplied[2]),
        a,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Time;
    use serde_json::{Value, json};

    fn animated(set: Value, animation: Value) -> (Animations, Clocks) {
        let set: Keyframes = serde_json::from_value(set).unwrap();
        let animation: Animation = serde_json::from_value(animation).unwrap();
        let item: SceneItem = serde_json::from_value(json!({
            "rect": {"x": 0, "y": 0, "width": 200, "height": 100},
            "style": {"opacity": 0.5},
            "animations": [animation],
        }))
        .unwrap();
        let mut animations = Animations::default();
        let sets = HashMap::from([(set.id.as_str(), &set)]);
        animations
            .add(&item, 0, [0.0, 0.0, 200.0, 100.0], &sets)
            .unwrap();
        let start = Time::from_nanos(1_000_000_000);
        let clocks = Clocks {
            on_air: start,
            shown: start,
            epoch: start,
        };
        (animations, clocks)
    }

    fn at_ms(animations: &Animations, clocks: &Clocks, ms: i64) -> Values {
        let at = Time::from_nanos(clocks.on_air.as_nanos() + ms * 1_000_000);
        evaluate(animations, at, clocks)
    }

    fn evaluate(animations: &Animations, at: Time, clocks: &Clocks) -> Values {
        let mut values = animations.items[0].base;
        animations.evaluate(0, at, clocks, &mut values);
        values
    }

    #[test]
    fn linear_translate_moves_evenly_and_holds_its_end() {
        let (animations, clocks) = animated(
            json!({"id": "slide", "frames": [
                {"at": 0.0, "translate": [0, 0]},
                {"at": 1.0, "translate": [300, "50%"]},
            ]}),
            json!({"keyframes": "slide", "duration_ms": 1000, "easing": "linear"}),
        );
        for frame in 0..=60 {
            // 60 fps: exactly 5 px a frame.
            let ns = frame * 1_000_000_000 / 60;
            let at = Time::from_nanos(clocks.on_air.as_nanos() + ns);
            let values = evaluate(&animations, at, &clocks);
            assert!(
                (values[0] - 5.0 * frame as f32).abs() < 1e-3,
                "{frame}: {values:?}"
            );
            assert!((values[1] - 50.0 * frame as f32 / 60.0).abs() < 1e-3);
        }
        // Filled forwards (the default, both).
        assert_eq!(at_ms(&animations, &clocks, 5000)[0], 300.0);
        // Nothing else moves.
        assert_eq!(animations.items[0].props, Property::Translate.bit());
        assert_eq!(animations.items[0].tracks, 1);
    }

    #[test]
    fn implicit_stops_start_from_the_items_own_values() {
        let (animations, clocks) = animated(
            json!({"id": "glow", "frames": [{"at": 0.5, "background": "#ff0000", "opacity": 0.0}]}),
            json!({"keyframes": "glow", "duration_ms": 1000, "easing": "linear"}),
        );
        // No background of its own: from transparent, to red at the middle
        // and back.
        let quarter = at_ms(&animations, &clocks, 250);
        assert!((quarter[8] - 0.5).abs() < 1e-6, "{quarter:?}");
        assert!((quarter[5] - 0.5).abs() < 1e-6, "premultiplied red");
        assert!((quarter[4] - 0.5).abs() < 1e-6, "opacity from 1 to 0");
        let middle = at_ms(&animations, &clocks, 500);
        assert_eq!(&middle[5..9], &[1.0, 0.0, 0.0, 1.0]);
        assert_eq!(animations.items[0].tracks, 2);
    }

    #[test]
    fn delays_iterations_directions_and_fills_follow_css() {
        let set =
            json!({"id": "a", "frames": [{"at": 0.0, "scale": 0.0}, {"at": 1.0, "scale": 1.0}]});
        let (alternate, clocks) = animated(
            set.clone(),
            json!({"keyframes": "a", "duration_ms": 1000, "easing": "linear",
                   "iterations": "infinite", "direction": "alternate"}),
        );
        assert!((at_ms(&alternate, &clocks, 250)[2] - 0.25).abs() < 1e-6);
        assert!((at_ms(&alternate, &clocks, 1250)[2] - 0.75).abs() < 1e-6);
        // After 60 s (60 runs), exactly at the start of a forward run.
        assert_eq!(at_ms(&alternate, &clocks, 60_000)[2], 0.0);
        assert_eq!(at_ms(&alternate, &clocks, 61_000)[2], 1.0);
        let (delayed, clocks) = animated(
            set.clone(),
            json!({"keyframes": "a", "duration_ms": 1000, "delay_ms": 500, "easing": "linear",
                   "iterations": 1.5, "fill": "none"}),
        );
        // Before its delay with no fill: its own value (a scale of 1).
        assert_eq!(at_ms(&delayed, &clocks, 100)[2], 1.0);
        assert!((at_ms(&delayed, &clocks, 750)[2] - 0.25).abs() < 1e-6);
        // Ended half way through its second run, without a fill.
        assert_eq!(at_ms(&delayed, &clocks, 3000)[2], 1.0);
        let (negative, clocks) = animated(
            set,
            json!({"keyframes": "a", "duration_ms": 1000, "delay_ms": -250, "easing": "linear",
                   "iterations": 1.5, "direction": "reverse"}),
        );
        // Started a quarter of the way through, backwards.
        assert!((at_ms(&negative, &clocks, 0)[2] - 0.75).abs() < 1e-6);
        // Held where the half run ended.
        assert!((at_ms(&negative, &clocks, 9000)[2] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn steps_jump_exactly_where_they_should() {
        let (animations, clocks) = animated(
            json!({"id": "s", "frames": [{"at": 0.0, "translate": [0, 0]}, {"at": 1.0, "translate": [300, 0]}]}),
            json!({"keyframes": "s", "duration_ms": 1000, "easing": {"steps": 4}}),
        );
        let mut jumps = 0;
        let mut last = 0.0;
        for frame in 0..=70 {
            let at = Time::from_nanos(clocks.on_air.as_nanos() + frame * 1_000_000_000 / 60);
            let x = evaluate(&animations, at, &clocks)[0];
            if x != last {
                jumps += 1;
                // At a quarter second's frame exactly.
                assert_eq!(frame % 15, 0, "{frame}: {x}");
            }
            last = x;
        }
        assert_eq!(jumps, 4);
    }
}
