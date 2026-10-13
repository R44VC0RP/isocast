//! Layout: CSS's box model for scene items, in setup terms (snake case).
//! An item gives either `rect` (absolute, in pixels) or `layout`; a scene's
//! `layout` lays out its items as the root container, the size of the
//! canvas. The engine lays items out (flexbox, grid, block and absolute
//! positioning) whenever the scenes or the sizes of the pictures they show
//! change, never per frame.
//!
//! Values are kept as written (a length stays `"12px"` or `12`), so setups
//! save back as they were; [`Setup::validate`](crate::Setup::validate)
//! reports any that can't be read, and the engine reads them with the
//! parsers here.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::Issues;

/// Where an item goes in its parent, and how its children are laid out:
/// CSS's properties of the same names in snake case. All are optional. A
/// scene's layout takes only the fields for children.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    /// `relative` (in its parent's flow, the default) or `absolute` (placed
    /// by `left`, `top`, `right` and `bottom` within its parent, taking no
    /// room from its siblings).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<Position>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left: Option<Length>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top: Option<Length>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub right: Option<Length>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bottom: Option<Length>,
    /// `top`, `right`, `bottom` and `left` at once (each one given wins).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inset: Option<Sides>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<Length>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<Length>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_width: Option<Length>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_height: Option<Length>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_width: Option<Length>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_height: Option<Length>,
    /// Width divided by height, used where only one of them is known.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "whole_option"
    )]
    pub aspect_ratio: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub margin: Option<Sides>,
    /// How much of the free room in a flex row or column it takes, against
    /// its siblings' (0, the default, takes none).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "whole_option"
    )]
    pub flex_grow: Option<f32>,
    /// How much it gives up when a flex row or column is too full (1 by
    /// default).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "whole_option"
    )]
    pub flex_shrink: Option<f32>,
    /// Its size along a flex row or column before growing or shrinking:
    /// `auto` (the default) is its width or height, else what it shows
    /// (a picture's own size). `flex_basis = 0` with `flex_grow` shares the
    /// room by `flex_grow` alone (CSS's `flex: 1`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flex_basis: Option<Length>,
    /// Its own `align_items` in a flex or grid parent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align_self: Option<AlignItems>,
    /// Where it sits across its grid cell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub justify_self: Option<AlignItems>,
    /// The grid rows it spans: a line (`2`), `"1 / 3"`, `"span 2"` or
    /// `"2 / span 3"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grid_row: Option<GridLine>,
    /// The grid columns it spans, as `grid_row`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grid_column: Option<GridLine>,
    /// How its children are laid out: `block` (stacked top to bottom, the
    /// default), `flex` or `grid`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<LayoutDisplay>,
    /// A flex container's main axis: `row` (the default), `column`,
    /// `row_reverse` or `column_reverse`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<FlexDirection>,
    /// Whether a flex container's children wrap onto more lines:
    /// `nowrap` (the default), `wrap` or `wrap_reverse`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wrap: Option<FlexWrap>,
    /// Room between a flex or grid container's children: one length, or
    /// [row gap, column gap].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap: Option<Gap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub padding: Option<Sides>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub justify_content: Option<JustifyContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align_items: Option<AlignItems>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align_content: Option<JustifyContent>,
    /// A grid's columns, as in CSS: `"1fr 2fr 200px"`,
    /// `"repeat(3, 1fr)"`, `"minmax(200px, 1fr) auto"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grid_template_columns: Option<GridTracks>,
    /// A grid's rows, as `grid_template_columns`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grid_template_rows: Option<GridTracks>,
    /// How a grid places children without `grid_row`/`grid_column`: `row`
    /// (the default), `column`, `row_dense` or `column_dense`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grid_auto_flow: Option<GridAutoFlow>,
    /// `hidden` clips its children (shadows too) to its rounded rect;
    /// `visible`, the default, doesn't.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overflow: Option<Overflow>,
}

/// The fields that place an item in its parent (a scene has none).
pub const ITEM_FIELDS: &[&str] = &[
    "position",
    "left",
    "top",
    "right",
    "bottom",
    "inset",
    "width",
    "height",
    "min_width",
    "min_height",
    "max_width",
    "max_height",
    "aspect_ratio",
    "margin",
    "flex_grow",
    "flex_shrink",
    "flex_basis",
    "align_self",
    "justify_self",
    "grid_row",
    "grid_column",
];

/// The fields that lay out an item's (or a scene's) children.
pub const CONTAINER_FIELDS: &[&str] = &[
    "display",
    "direction",
    "wrap",
    "gap",
    "padding",
    "justify_content",
    "align_items",
    "align_content",
    "grid_template_columns",
    "grid_template_rows",
    "grid_auto_flow",
    "overflow",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Position {
    Relative,
    Absolute,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LayoutDisplay {
    Block,
    Flex,
    Grid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FlexDirection {
    Row,
    Column,
    RowReverse,
    ColumnReverse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FlexWrap {
    Nowrap,
    Wrap,
    WrapReverse,
}

/// How children share the room along an axis (CSS `justify-content` and
/// `align-content`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JustifyContent {
    Start,
    End,
    FlexStart,
    FlexEnd,
    Center,
    Stretch,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
}

/// Where children sit across an axis (CSS `align-items`, `align-self`,
/// `justify-self`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AlignItems {
    Start,
    End,
    FlexStart,
    FlexEnd,
    Center,
    Baseline,
    Stretch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GridAutoFlow {
    Row,
    Column,
    RowDense,
    ColumnDense,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Overflow {
    Visible,
    Hidden,
}

/// A length: a number of pixels (`12`, or `"12px"`), a percentage of the
/// parent's content box (`"50%"`, CSS's rules), or `"auto"`.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(
    untagged,
    expecting = "a length: a number of pixels, \"50%\", or \"auto\""
)]
pub enum Length {
    Px(f32),
    Text(#[schemars(regex(pattern = r"^\s*(auto|-?[0-9.]+(px|%)?)\s*$"))] String),
}

impl Serialize for Length {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Px(px) => whole(px, serializer),
            Self::Text(text) => serializer.serialize_str(text),
        }
    }
}

/// A [`Length`] read.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LengthValue {
    Px(f32),
    /// Percent (50 for `"50%"`).
    Percent(f32),
    Auto,
}

impl Length {
    /// Its value, or why it isn't a length.
    pub fn value(&self) -> Result<LengthValue, String> {
        match self {
            Self::Px(px) if px.is_finite() => Ok(LengthValue::Px(*px)),
            Self::Px(px) => Err(format!("{px} is not a length")),
            Self::Text(text) => parse_length(text).ok_or_else(|| {
                format!("{text:?} is not a length; use a number of pixels, \"50%\", or \"auto\"")
            }),
        }
    }
}

/// `12`, `"12px"`, `"50%"` or `"auto"`.
fn parse_length(text: &str) -> Option<LengthValue> {
    let text = text.trim();
    if text == "auto" {
        return Some(LengthValue::Auto);
    }
    let (number, percent) = match text.strip_suffix('%') {
        Some(number) => (number, true),
        None => (text.strip_suffix("px").unwrap_or(text), false),
    };
    let value = parse_number(number)?;
    Some(match percent {
        true => LengthValue::Percent(value),
        false => LengthValue::Px(value),
    })
}

/// A plain decimal number (no spaces, exponents, `inf` or `NaN`).
fn parse_number(text: &str) -> Option<f32> {
    let digits = text.strip_prefix('-').unwrap_or(text);
    let plain = !digits.is_empty()
        && digits.chars().all(|c| c.is_ascii_digit() || c == '.')
        && digits.chars().filter(|c| *c == '.').count() <= 1
        && digits != ".";
    plain.then(|| text.parse().ok()).flatten()
}

/// Lengths for the four sides, as CSS's `margin`, `padding` and `inset`
/// take them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    untagged,
    expecting = "a length, [vertical, horizontal], or [top, right, bottom, left]"
)]
pub enum Sides {
    All(Length),
    /// Vertical, horizontal.
    Pair([Length; 2]),
    /// Top, right, bottom, left.
    Each([Length; 4]),
}

impl Sides {
    /// Top, right, bottom and left.
    pub fn sides(&self) -> [&Length; 4] {
        match self {
            Self::All(all) => [all; 4],
            Self::Pair([vertical, horizontal]) => [vertical, horizontal, vertical, horizontal],
            Self::Each([top, right, bottom, left]) => [top, right, bottom, left],
        }
    }
}

/// The room between children: one length for both, or [row gap, column
/// gap].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged, expecting = "a length, or [row gap, column gap]")]
pub enum Gap {
    All(Length),
    Each([Length; 2]),
}

impl Gap {
    /// Between rows, then between columns.
    pub fn gaps(&self) -> [&Length; 2] {
        match self {
            Self::All(all) => [all; 2],
            Self::Each([rows, columns]) => [rows, columns],
        }
    }
}

/// A grid track list in CSS's syntax: sizes (`200px`, `25%`, `1fr`,
/// `auto`, `min-content`, `max-content`, `minmax(MIN, MAX)`) and
/// `repeat(N, ...)` (or `repeat(auto-fill, ...)`, `repeat(auto-fit,
/// ...)`), separated by spaces.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct GridTracks(pub String);

/// One track's size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TrackSize {
    Px(f32),
    Percent(f32),
    Fr(f32),
    Auto,
    MinContent,
    MaxContent,
    /// The smallest and largest it may be (the smallest is never `fr`).
    MinMax(TrackMin, TrackMax),
}

/// The least a `minmax()` track may be.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TrackMin {
    Px(f32),
    Percent(f32),
    Auto,
    MinContent,
    MaxContent,
}

/// The most a `minmax()` track may be.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TrackMax {
    Px(f32),
    Percent(f32),
    Fr(f32),
    Auto,
    MinContent,
    MaxContent,
}

/// An entry of a track list.
#[derive(Debug, Clone, PartialEq)]
pub enum GridTrack {
    Size(TrackSize),
    Repeat(GridRepeat, Vec<TrackSize>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridRepeat {
    Count(u16),
    AutoFill,
    AutoFit,
}

impl GridTracks {
    /// The track list, or what is wrong with it.
    pub fn tracks(&self) -> Result<Vec<GridTrack>, String> {
        parse_tracks(&self.0).map_err(|problem| {
            format!(
                "{:?}: {problem}; give sizes like \"1fr 2fr 200px\", \"repeat(3, 1fr)\" or \
                 \"minmax(200px, 1fr) auto\"",
                self.0
            )
        })
    }
}

fn parse_tracks(text: &str) -> Result<Vec<GridTrack>, String> {
    let mut out = Vec::new();
    let mut rest = text.trim();
    if rest.is_empty() {
        return Err("no tracks".into());
    }
    while !rest.is_empty() {
        let (word, after) = next_word(rest)?;
        rest = after.trim_start();
        match word.strip_prefix("repeat(") {
            Some(inner) => {
                let inner = inner
                    .strip_suffix(')')
                    .expect("a word ends with its parenthesis");
                let (count, sizes) = inner
                    .split_once(',')
                    .ok_or_else(|| format!("{word:?} needs a count and sizes"))?;
                let count = match count.trim() {
                    "auto-fill" => GridRepeat::AutoFill,
                    "auto-fit" => GridRepeat::AutoFit,
                    n => match n.parse::<u16>() {
                        Ok(n) if n >= 1 => GridRepeat::Count(n),
                        _ => {
                            return Err(format!(
                                "{n:?} is not a repeat count (a whole number from 1, \
                                 auto-fill or auto-fit)"
                            ));
                        }
                    },
                };
                let mut tracks = Vec::new();
                let mut sizes = sizes.trim();
                if sizes.is_empty() {
                    return Err(format!("{word:?} repeats no sizes"));
                }
                while !sizes.is_empty() {
                    let (size, after) = next_word(sizes)?;
                    tracks.push(parse_track(size)?);
                    sizes = after.trim_start();
                }
                out.push(GridTrack::Repeat(count, tracks));
            }
            None => out.push(GridTrack::Size(parse_track(word)?)),
        }
    }
    Ok(out)
}

/// The first word of `text` (which starts with one), keeping a
/// parenthesised group whole, and the rest.
fn next_word(text: &str) -> Result<(&str, &str), String> {
    let mut depth = 0usize;
    for (at, c) in text.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| format!("an unmatched \")\" in {text:?}"))?;
            }
            c if c.is_whitespace() && depth == 0 => return Ok((&text[..at], &text[at..])),
            _ => {}
        }
    }
    match depth {
        0 => Ok((text, "")),
        _ => Err(format!("an unclosed \"(\" in {text:?}")),
    }
}

fn parse_track(word: &str) -> Result<TrackSize, String> {
    if let Some(inner) = word.strip_prefix("minmax(") {
        let inner = inner
            .strip_suffix(')')
            .ok_or_else(|| format!("{word:?} is not a track size"))?;
        let (min, max) = inner
            .split_once(',')
            .ok_or_else(|| format!("{word:?} needs a least and a most size"))?;
        let min = match parse_track(min.trim())? {
            TrackSize::Px(px) => TrackMin::Px(px),
            TrackSize::Percent(percent) => TrackMin::Percent(percent),
            TrackSize::Auto => TrackMin::Auto,
            TrackSize::MinContent => TrackMin::MinContent,
            TrackSize::MaxContent => TrackMin::MaxContent,
            TrackSize::Fr(_) | TrackSize::MinMax(..) => {
                return Err(format!(
                    "the least size in {word:?} can't be a fraction (fr) or minmax()"
                ));
            }
        };
        let max = match parse_track(max.trim())? {
            TrackSize::Px(px) => TrackMax::Px(px),
            TrackSize::Percent(percent) => TrackMax::Percent(percent),
            TrackSize::Fr(fr) => TrackMax::Fr(fr),
            TrackSize::Auto => TrackMax::Auto,
            TrackSize::MinContent => TrackMax::MinContent,
            TrackSize::MaxContent => TrackMax::MaxContent,
            TrackSize::MinMax(..) => return Err(format!("minmax() inside {word:?}")),
        };
        return Ok(TrackSize::MinMax(min, max));
    }
    let size = match word {
        "auto" => TrackSize::Auto,
        "min-content" => TrackSize::MinContent,
        "max-content" => TrackSize::MaxContent,
        _ => match word.strip_suffix("fr") {
            Some(fr) => TrackSize::Fr(parse_number(fr).ok_or_else(|| not_track(word))?),
            None => match parse_length(word) {
                Some(LengthValue::Px(px)) => TrackSize::Px(px),
                Some(LengthValue::Percent(percent)) => TrackSize::Percent(percent),
                _ => return Err(not_track(word)),
            },
        },
    };
    match size {
        TrackSize::Px(v) | TrackSize::Percent(v) | TrackSize::Fr(v) if v < 0.0 => {
            Err(format!("{word:?} is negative"))
        }
        size => Ok(size),
    }
}

fn not_track(word: &str) -> String {
    format!("{word:?} is not a track size")
}

/// Where a grid item starts and ends along one axis: a line number, or
/// a string as CSS writes `grid-row`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    untagged,
    expecting = "a grid line like 2, or \"1 / 3\", \"span 2\" or \"2 / span 3\""
)]
pub enum GridLine {
    Line(i16),
    Text(String),
}

/// One end of a [`GridLine`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridPlace {
    Auto,
    /// A line, from 1 (or from the end, from -1).
    Line(i16),
    Span(u16),
}

impl GridLine {
    /// Its start and end, or what is wrong with it.
    pub fn places(&self) -> Result<(GridPlace, GridPlace), String> {
        let problem = || {
            format!(
                "{} is not a grid placement; use a line like 2, or \"1 / 3\", \"span 2\" or \
                 \"2 / span 3\" (lines count from 1, or from -1 at the end)",
                match self {
                    Self::Line(line) => line.to_string(),
                    Self::Text(text) => format!("{text:?}"),
                }
            )
        };
        let place = |text: &str| -> Option<GridPlace> {
            let text = text.trim();
            if text == "auto" {
                return Some(GridPlace::Auto);
            }
            if let Some(span) = text.strip_prefix("span ") {
                return match span.trim().parse::<u16>() {
                    Ok(span) if span >= 1 => Some(GridPlace::Span(span)),
                    _ => None,
                };
            }
            match text.parse::<i16>() {
                Ok(0) | Err(_) => None,
                Ok(line) => Some(GridPlace::Line(line)),
            }
        };
        match self {
            Self::Line(0) => Err(problem()),
            Self::Line(line) => Ok((GridPlace::Line(*line), GridPlace::Auto)),
            Self::Text(text) => {
                let (start, end) = match text.split_once('/') {
                    Some((start, end)) => (place(start), place(end)),
                    None => (place(text), Some(GridPlace::Auto)),
                };
                match (start, end) {
                    (Some(start), Some(end)) => Ok((start, end)),
                    _ => Err(problem()),
                }
            }
        }
    }
}

/// Write a number as an integer when it is a whole one, so `gap = 24`
/// saves as it was written rather than as `24.0`.
pub(crate) fn whole<S: serde::Serializer>(value: &f32, serializer: S) -> Result<S::Ok, S::Error> {
    if value.fract() == 0.0 && value.abs() < 1e9 {
        serializer.serialize_i64(*value as i64)
    } else {
        serializer.serialize_f32(*value)
    }
}

fn whole_option<S: serde::Serializer>(
    value: &Option<f32>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Some(value) => whole(value, serializer),
        None => serializer.serialize_none(),
    }
}

/// Check a layout at `path`: a scene's (`scene`), which has only the fields
/// for children, or an item's.
pub(crate) fn check(path: &str, layout: &Layout, scene: bool, issues: &mut Issues) {
    let json = serde_json::to_value(layout).unwrap_or_default();
    if scene && let Some(fields) = json.as_object() {
        for field in fields
            .keys()
            .filter(|field| ITEM_FIELDS.contains(&field.as_str()))
        {
            issues.add(
                format!("{path}.{field}"),
                format!(
                    "a scene is the canvas's size and has no place of its own; its layout \
                     takes only: {}",
                    CONTAINER_FIELDS.join(", ")
                ),
            );
        }
    }
    // What each length may be: (auto allowed, negative allowed).
    let lengths: [(&str, Option<&Length>, bool, bool); 11] = [
        ("left", layout.left.as_ref(), true, true),
        ("top", layout.top.as_ref(), true, true),
        ("right", layout.right.as_ref(), true, true),
        ("bottom", layout.bottom.as_ref(), true, true),
        ("width", layout.width.as_ref(), true, false),
        ("height", layout.height.as_ref(), true, false),
        ("min_width", layout.min_width.as_ref(), true, false),
        ("min_height", layout.min_height.as_ref(), true, false),
        ("max_width", layout.max_width.as_ref(), true, false),
        ("max_height", layout.max_height.as_ref(), true, false),
        ("flex_basis", layout.flex_basis.as_ref(), true, false),
    ];
    for (field, length, auto, negative) in lengths {
        if let Some(length) = length {
            check_length(&format!("{path}.{field}"), length, auto, negative, issues);
        }
    }
    let sides = [
        ("inset", &layout.inset, true, true),
        ("margin", &layout.margin, true, true),
        ("padding", &layout.padding, false, false),
    ];
    for (field, value, auto, negative) in sides {
        let each: &[Length] = match value {
            None => &[],
            Some(Sides::All(all)) => std::slice::from_ref(all),
            Some(Sides::Pair(pair)) => pair,
            Some(Sides::Each(four)) => four,
        };
        for (i, length) in each.iter().enumerate() {
            let at = match value {
                Some(Sides::All(_)) => format!("{path}.{field}"),
                _ => format!("{path}.{field}[{i}]"),
            };
            check_length(&at, length, auto, negative, issues);
        }
    }
    let gaps: &[Length] = match &layout.gap {
        None => &[],
        Some(Gap::All(all)) => std::slice::from_ref(all),
        Some(Gap::Each(pair)) => pair,
    };
    for (i, length) in gaps.iter().enumerate() {
        let at = match &layout.gap {
            Some(Gap::Each(_)) => format!("{path}.gap[{i}]"),
            _ => format!("{path}.gap"),
        };
        check_length(&at, length, false, false, issues);
    }
    for (field, value) in [
        ("aspect_ratio", layout.aspect_ratio),
        ("flex_grow", layout.flex_grow),
        ("flex_shrink", layout.flex_shrink),
    ] {
        let Some(value) = value else { continue };
        let ok = match field {
            "aspect_ratio" => value.is_finite() && value > 0.0,
            _ => value.is_finite() && value >= 0.0,
        };
        if !ok {
            let rule = match field {
                "aspect_ratio" => {
                    "must be a width divided by a height, more than 0 (16:9 is 1.7778)"
                }
                _ => "must be 0 or more",
            };
            issues.add(format!("{path}.{field}"), format!("{value} {rule}"));
        }
    }
    for (field, tracks) in [
        ("grid_template_columns", &layout.grid_template_columns),
        ("grid_template_rows", &layout.grid_template_rows),
    ] {
        if let Some(tracks) = tracks
            && let Err(problem) = tracks.tracks()
        {
            issues.add(format!("{path}.{field}"), problem);
        }
    }
    for (field, line) in [
        ("grid_row", &layout.grid_row),
        ("grid_column", &layout.grid_column),
    ] {
        if let Some(line) = line
            && let Err(problem) = line.places()
        {
            issues.add(format!("{path}.{field}"), problem);
        }
    }
}

fn check_length(path: &str, length: &Length, auto: bool, negative: bool, issues: &mut Issues) {
    let use_instead = match auto {
        true => "use a number of pixels, \"50%\", or \"auto\"",
        false => "use a number of pixels or \"50%\"",
    };
    match length.value() {
        Err(_) => issues.add(
            path,
            match length {
                Length::Px(px) => format!("{px} is not a length; {use_instead}"),
                Length::Text(text) => format!("{text:?} is not a length; {use_instead}"),
            },
        ),
        Ok(LengthValue::Auto) if !auto => {
            issues.add(path, format!("\"auto\" doesn't apply here; {use_instead}"))
        }
        Ok(LengthValue::Px(value) | LengthValue::Percent(value)) if value < 0.0 && !negative => {
            issues.add(path, "must be 0 or more")
        }
        Ok(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths_read_as_css_writes_them() {
        let value = |text: &str| Length::Text(text.into()).value();
        assert_eq!(value("50%"), Ok(LengthValue::Percent(50.0)));
        assert_eq!(value("12px"), Ok(LengthValue::Px(12.0)));
        assert_eq!(value(" auto "), Ok(LengthValue::Auto));
        assert_eq!(value("-4.5px"), Ok(LengthValue::Px(-4.5)));
        assert_eq!(Length::Px(320.0).value(), Ok(LengthValue::Px(320.0)));
        assert!(
            value("50 %")
                .unwrap_err()
                .contains("\"50 %\" is not a length")
        );
        for bad in ["", "px", "1e3", "inf", "NaN", "12em", "1.2.3"] {
            assert!(value(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn track_lists_parse() {
        let tracks = |text: &str| GridTracks(text.into()).tracks();
        assert_eq!(
            tracks("1fr 2fr 200px").unwrap(),
            [
                GridTrack::Size(TrackSize::Fr(1.0)),
                GridTrack::Size(TrackSize::Fr(2.0)),
                GridTrack::Size(TrackSize::Px(200.0))
            ]
        );
        assert_eq!(
            tracks("repeat(3, 1fr)").unwrap(),
            [GridTrack::Repeat(
                GridRepeat::Count(3),
                vec![TrackSize::Fr(1.0)]
            )]
        );
        assert_eq!(
            tracks("minmax(200px, 1fr) auto 25%").unwrap(),
            [
                GridTrack::Size(TrackSize::MinMax(TrackMin::Px(200.0), TrackMax::Fr(1.0))),
                GridTrack::Size(TrackSize::Auto),
                GridTrack::Size(TrackSize::Percent(25.0))
            ]
        );
        assert_eq!(
            tracks("repeat(auto-fill, minmax(100px, 1fr) 10px)").unwrap(),
            [GridTrack::Repeat(
                GridRepeat::AutoFill,
                vec![
                    TrackSize::MinMax(TrackMin::Px(100.0), TrackMax::Fr(1.0)),
                    TrackSize::Px(10.0)
                ]
            )]
        );
        for bad in [
            "",
            "1fr 2xx",
            "repeat(0, 1fr)",
            "repeat(3)",
            "minmax(1fr, 2fr)",
            "(1fr",
            "1fr)",
            "-1fr",
        ] {
            assert!(tracks(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn grid_lines_parse() {
        let places = |text: &str| GridLine::Text(text.into()).places();
        assert_eq!(
            places("1 / 3"),
            Ok((GridPlace::Line(1), GridPlace::Line(3)))
        );
        assert_eq!(places("span 2"), Ok((GridPlace::Span(2), GridPlace::Auto)));
        assert_eq!(
            places("2 / span 3"),
            Ok((GridPlace::Line(2), GridPlace::Span(3)))
        );
        assert_eq!(places("-1"), Ok((GridPlace::Line(-1), GridPlace::Auto)));
        assert_eq!(
            GridLine::Line(2).places(),
            Ok((GridPlace::Line(2), GridPlace::Auto))
        );
        for bad in ["0", "span 0", "a / b", "1 / 2 / 3", "span"] {
            assert!(places(bad).is_err(), "{bad:?}");
        }
        assert!(GridLine::Line(0).places().is_err());
    }

    #[test]
    fn whole_numbers_save_as_written() {
        let layout: Layout = toml::from_str(
            r#"gap = 24
               flex_grow = 1
               aspect_ratio = 1.7778
               width = "50%"
               padding = [12, "5%"]"#,
        )
        .unwrap();
        let text = toml::to_string(&layout).unwrap();
        assert!(text.contains("gap = 24\n"), "{text}");
        assert!(text.contains("flex_grow = 1\n"), "{text}");
        assert!(text.contains("aspect_ratio = 1.7778"), "{text}");
        assert!(text.contains("width = \"50%\""), "{text}");
        assert!(text.contains("padding = [12, \"5%\"]"), "{text}");
    }
}
