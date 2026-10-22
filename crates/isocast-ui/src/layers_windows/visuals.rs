//! Windows: the stage, the selected item's box, the master level and the
//! `--drag-test` pointer as DirectComposition visuals under the window's
//! interface, as `layers.rs` builds them from Core Animation layers on
//! macOS (same layout, same sizes in logical points, scaled to physical
//! pixels by `dpi`).
//!
//! Nothing here draws pixels per frame. Every flat shape is a leaf visual
//! whose content is one shared atlas: a 1-row composition surface of flat colour
//! cells (`palette`) that the backend fills once. A shape's transform
//! stretches one cell's middle texel over the shape's rectangle (nearest
//! neighbour, so no colours mix) and its clip, in texel space, cuts that
//! texel out, with rounded corners where the Mac layer has a corner
//! radius. Borders are four edge shapes that don't overlap (so a
//! translucent ring isn't doubled at the corners), and a handle's 1px edge
//! is an opaque disc in the edge-over-fill colour under the fill's disc.
//!
//! Containers (stage, pictures, box, meter, tracks) carry offsets and clips
//! only, so their children are placed in plain physical pixels. Nothing is
//! committed here: the caller batches these changes with its own and calls
//! `IDCompositionDevice::Commit`.

use std::cell::Cell;

use anyhow::Result;
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows::Win32::Graphics::DirectComposition::{
    DCOMPOSITION_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR, IDCompositionClip,
    IDCompositionDevice, IDCompositionRectangleClip, IDCompositionSurface, IDCompositionVisual,
};
use windows::core::{IUnknown, Interface};
use windows_numerics::Matrix3x2;

use super::{BoxShape, BoxStyle, Colours, Geometry, MeterLevel};

/// The atlas's cells, in order. Each is `CELL` texels of one colour; a
/// shape samples only the middle one, so its neighbours never bleed in.
const STAGE: u32 = 0;
const MONITOR: u32 = 1;
const ACCENT: u32 = 2;
const RING: u32 = 3;
const LOCKED: u32 = 4;
const BONE: u32 = 5;
/// The handle's edge over a bone handle, and over a cropping (accent) one.
const BONE_EDGE: u32 = 6;
const ACCENT_EDGE: u32 = 7;
const TEST_OUTLINE: u32 = 8;
const TEST_POINTER: u32 = 9;
const INSET: u32 = 10;
const TRACK: u32 = 11;
const ZONES: [u32; 3] = [12, 13, 14];
const PEAKS: [u32; 3] = [15, 16, 17];
const WINDOW: u32 = 18;
const FRAME: u32 = 19;
const LIVE: u32 = 20;
const CELLS: u32 = 21;
const CELL: u32 = 3;

/// The atlas's width in texels (its height is 1).
pub const PALETTE_WIDTH: u32 = CELLS * CELL;

const HANDLE: f64 = 8.0;
const METER_BAR: f64 = 4.0;
const METER_INSET: f64 = 3.0;
const METER_GAP: f64 = 2.0;

/// The atlas's texels: `PALETTE_WIDTH` × 1, RGBA (R first), 8 bits per
/// channel, premultiplied alpha, sRGB-encoded as the theme gives them.
pub fn palette(colours: &Colours, test_outline: bool) -> Vec<u8> {
    let opaque = |[r, g, b]: [u8; 3]| [r, g, b, 255];
    // Premultiplied, as DirectComposition composes the surface.
    let premultiplied = |[r, g, b, a]: [u8; 4]| {
        let k = |c: u8| ((c as u32 * a as u32 + 127) / 255) as u8;
        [k(r), k(g), k(b), a]
    };
    // `edge` drawn over the opaque `fill` (a Core Animation border over
    // the layer's background): opaque.
    let over = |[r, g, b, a]: [u8; 4], fill: [u8; 3]| {
        let a = a as f64 / 255.0;
        let mix = |e: u8, f: u8| (e as f64 * a + f as f64 * (1.0 - a)).round() as u8;
        [mix(r, fill[0]), mix(g, fill[1]), mix(b, fill[2]), 255]
    };
    let test = |rgb: [u8; 3]| if test_outline { opaque(rgb) } else { [0; 4] };
    let meter = &colours.meter;
    let mut cells = [[0u8; 4]; CELLS as usize];
    cells[STAGE as usize] = opaque(colours.stage);
    cells[MONITOR as usize] = opaque(colours.monitor);
    cells[ACCENT as usize] = opaque(colours.accent);
    cells[RING as usize] = premultiplied(colours.ring);
    cells[LOCKED as usize] = opaque(colours.locked);
    cells[BONE as usize] = opaque(colours.bone);
    cells[BONE_EDGE as usize] = over(colours.handle_edge, colours.bone);
    cells[ACCENT_EDGE as usize] = over(colours.handle_edge, colours.accent);
    cells[TEST_OUTLINE as usize] = test([0, 255, 0]);
    cells[TEST_POINTER as usize] = test([0, 255, 255]);
    cells[INSET as usize] = opaque(meter.inset);
    cells[TRACK as usize] = opaque(meter.track);
    cells[WINDOW as usize] = opaque(colours.window);
    cells[FRAME as usize] = opaque(colours.frame);
    cells[LIVE as usize] = opaque(colours.live);
    for (cell, zone) in ZONES.into_iter().zip(meter.zones) {
        cells[cell as usize] = opaque(zone);
    }
    for (cell, peak) in PEAKS.into_iter().zip(meter.peaks) {
        cells[cell as usize] = premultiplied(peak);
    }
    cells
        .iter()
        .flat_map(|cell| std::iter::repeat_n(*cell, CELL as usize))
        .flatten()
        .collect()
}

/// A 2D affine map: x' = a·x + c·y + tx, y' = b·x + d·y + ty (the
/// `Matrix3x2` and `CGAffineTransform` layout).
#[derive(Debug, Clone, Copy)]
struct Affine {
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    tx: f64,
    ty: f64,
}

impl Affine {
    const IDENTITY: Self = Self::translate(0.0, 0.0);

    const fn translate(tx: f64, ty: f64) -> Self {
        Self {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            tx,
            ty,
        }
    }

    /// Clockwise by `angle` radians (y points down), as layers.rs turns.
    fn turn(angle: f64) -> Self {
        let (sin, cos) = angle.sin_cos();
        Self {
            a: cos,
            b: sin,
            c: -sin,
            d: cos,
            tx: 0.0,
            ty: 0.0,
        }
    }

    /// `self`, then `next`.
    fn then(self, next: Self) -> Self {
        Self {
            a: self.a * next.a + self.b * next.c,
            b: self.a * next.b + self.b * next.d,
            c: self.c * next.a + self.d * next.c,
            d: self.c * next.b + self.d * next.d,
            tx: self.tx * next.a + self.ty * next.c + next.tx,
            ty: self.tx * next.b + self.ty * next.d + next.ty,
        }
    }

    fn matrix(self) -> Matrix3x2 {
        Matrix3x2 {
            M11: self.a as f32,
            M12: self.b as f32,
            M21: self.c as f32,
            M22: self.d as f32,
            M31: self.tx as f32,
            M32: self.ty as f32,
        }
    }
}

fn clip_rect(w: f64, h: f64) -> D2D_RECT_F {
    D2D_RECT_F {
        left: 0.0,
        top: 0.0,
        right: w.max(0.0) as f32,
        bottom: h.max(0.0) as f32,
    }
}

/// Append `child` over `parent`'s other children.
fn add(parent: &IDCompositionVisual, child: &IDCompositionVisual) -> Result<()> {
    // No reference and insertAbove FALSE: "above all of its siblings".
    unsafe { parent.AddVisual(child, false, None::<&IDCompositionVisual>)? };
    Ok(())
}

/// A container: an offset and a clip, no content.
fn container(
    device: &IDCompositionDevice,
    parent: &IDCompositionVisual,
) -> Result<IDCompositionVisual> {
    let visual = unsafe { device.CreateVisual()? };
    // Empty (hidden) until placed.
    unsafe { visual.SetClip2(&clip_rect(0.0, 0.0))? };
    add(parent, &visual)?;
    Ok(visual)
}

/// Put a container at (`x`, `y`) in its parent, clipped to `w` × `h`
/// (nothing shows when either is zero).
fn place_container(visual: &IDCompositionVisual, [x, y, w, h]: [f64; 4]) -> Result<()> {
    unsafe {
        visual.SetOffsetX2(x as f32)?;
        visual.SetOffsetY2(y as f32)?;
        visual.SetClip2(&clip_rect(w, h))?;
    }
    Ok(())
}

/// A flat rectangle (optionally rounded) of one atlas cell. A leaf: its
/// transform maps the atlas's texels, so it must have no children.
struct Solid {
    visual: IDCompositionVisual,
    clip: IDCompositionRectangleClip,
    atlas: IUnknown,
    shown: Cell<bool>,
    cell: Cell<Option<u32>>,
    radii: Cell<(f32, f32)>,
}

impl Solid {
    fn new(
        device: &IDCompositionDevice,
        atlas: &IDCompositionSurface,
        parent: Option<&IDCompositionVisual>,
    ) -> Result<Self> {
        let visual = unsafe { device.CreateVisual()? };
        let clip = unsafe { device.CreateRectangleClip()? };
        unsafe {
            visual.SetBitmapInterpolationMode(
                DCOMPOSITION_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
            )?;
            clip.SetTop2(0.0)?;
            clip.SetBottom2(1.0)?;
            visual.SetClip(&clip)?;
        }
        if let Some(parent) = parent {
            add(parent, &visual)?;
        }
        Ok(Self {
            visual,
            clip,
            atlas: atlas.cast()?,
            shown: Cell::new(false),
            cell: Cell::new(None),
            radii: Cell::new((0.0, 0.0)),
        })
    }

    fn hide(&self) -> Result<()> {
        if self.shown.replace(false) {
            unsafe { self.visual.SetContent(None::<&IUnknown>)? };
        }
        Ok(())
    }

    /// Fill `[x, y, w, h]` with `cell`, corners rounded by `radius`, then
    /// map it by `to_parent` (all in physical pixels).
    fn paint(
        &self,
        cell: u32,
        [x, y, w, h]: [f64; 4],
        radius: f64,
        to_parent: Affine,
    ) -> Result<()> {
        if w <= 0.0 || h <= 0.0 {
            return self.hide();
        }
        // The cell's middle texel, [t0, t0 + 1] × [0, 1], stretched over
        // the rectangle.
        let t0 = (cell * CELL + CELL / 2) as f64;
        let texels = Affine {
            a: w,
            b: 0.0,
            c: 0.0,
            d: h,
            tx: x - t0 * w,
            ty: y,
        };
        unsafe {
            self.visual
                .SetTransform2(&texels.then(to_parent).matrix())?
        };
        if self.cell.replace(Some(cell)) != Some(cell) {
            unsafe {
                self.clip.SetLeft2(t0 as f32)?;
                self.clip.SetRight2((t0 + 1.0) as f32)?;
            }
        }
        // The clip is in texel space: a texel is `w` × `h` pixels.
        let radius = radius.clamp(0.0, w.min(h) / 2.0);
        let radii = ((radius / w) as f32, (radius / h) as f32);
        if self.radii.replace(radii) != radii {
            let (rx, ry) = radii;
            let clip = &self.clip;
            unsafe {
                clip.SetTopLeftRadiusX2(rx)?;
                clip.SetTopLeftRadiusY2(ry)?;
                clip.SetTopRightRadiusX2(rx)?;
                clip.SetTopRightRadiusY2(ry)?;
                clip.SetBottomLeftRadiusX2(rx)?;
                clip.SetBottomLeftRadiusY2(ry)?;
                clip.SetBottomRightRadiusX2(rx)?;
                clip.SetBottomRightRadiusY2(ry)?;
            }
        }
        if !self.shown.replace(true) {
            unsafe { self.visual.SetContent(&self.atlas)? };
        }
        Ok(())
    }
}

/// A border `width` thick just inside `[x, y, w, h]`: four edges that
/// don't overlap.
fn paint_border(
    edges: &[Solid; 4],
    cell: u32,
    [x, y, w, h]: [f64; 4],
    width: f64,
    to_parent: Affine,
) -> Result<()> {
    let side = (h - 2.0 * width).max(0.0);
    let rects = [
        [x, y, w, width],
        [x, y + h - width, w, width],
        [x, y + width, width, side],
        [x + w - width, y + width, width, side],
    ];
    for (edge, rect) in edges.iter().zip(rects) {
        edge.paint(cell, rect, 0.0, to_parent)?;
    }
    Ok(())
}

/// The monitor's 1pt frame or 2pt live tally, outside its picture rectangle.
fn monitor_ring([x, y, w, h]: [f64; 4], dpi: f64, live: bool) -> Option<([f64; 4], f64)> {
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let edge = if live { 2.0 } else { 1.0 } * dpi;
    Some(([x - edge, y - edge, w + 2.0 * edge, h + 2.0 * edge], edge))
}

struct Picture {
    frame: [Solid; 4],
    container: IDCompositionVisual,
    content: IDCompositionVisual,
    background: Solid,
}

impl Picture {
    fn new(
        device: &IDCompositionDevice,
        atlas: &IDCompositionSurface,
        stage: &IDCompositionVisual,
    ) -> Result<Self> {
        let container = container(device, stage)?;
        let edge = || Solid::new(device, atlas, Some(stage));
        let frame = [edge()?, edge()?, edge()?, edge()?];
        let background = Solid::new(device, atlas, Some(&container))?;
        let content = unsafe { device.CreateVisual()? };
        add(&container, &content)?;
        Ok(Self {
            container,
            content,
            background,
            frame,
        })
    }

    fn place(&mut self, rect: [f64; 4], shown: bool, live: bool, dpi: f64) -> Result<()> {
        if let Some((ring, width)) = monitor_ring(rect, dpi, live).filter(|_| shown) {
            paint_border(
                &self.frame,
                if live { LIVE } else { FRAME },
                ring,
                width,
                Affine::IDENTITY,
            )?;
        } else {
            for edge in &self.frame {
                edge.hide()?;
            }
        }
        let [_, _, w, h] = rect;
        place_container(&self.container, rect)?;
        self.background
            .paint(MONITOR, [0.0, 0.0, w, h], 0.0, Affine::IDENTITY)?;
        Ok(())
    }
}

/// One channel's bar: its track (a rounded clip over the track colour)
/// with the zones, their peaks and the hold tick.
struct Bar {
    track: IDCompositionVisual,
    clip: IDCompositionRectangleClip,
    background: Solid,
    peaks: [Solid; 3],
    levels: [Solid; 3],
    hold: Solid,
}

/// The master level: the capsule's inside and two bars, as layers.rs's.
struct Meter {
    inside: IDCompositionVisual,
    background: Solid,
    bars: [Bar; 2],
    warn_db: f32,
    clip_db: f32,
    radius: f64,
    /// The dpi it was placed at (0 until placed) and its bars' span in
    /// physical pixels.
    dpi: f64,
    span: f64,
    shown: Option<[MeterLevel; 2]>,
}

impl Meter {
    fn new(
        device: &IDCompositionDevice,
        atlas: &IDCompositionSurface,
        colours: &Colours,
    ) -> Result<Self> {
        let inside = unsafe { device.CreateVisual()? };
        unsafe { inside.SetClip2(&clip_rect(0.0, 0.0))? };
        let background = Solid::new(device, atlas, Some(&inside))?;
        let bar = || -> Result<Bar> {
            let track = container(device, &inside)?;
            let clip = unsafe { device.CreateRectangleClip()? };
            unsafe { track.SetClip(&clip)? };
            let solid = || Solid::new(device, atlas, Some(&track));
            Ok(Bar {
                background: solid()?,
                peaks: [solid()?, solid()?, solid()?],
                levels: [solid()?, solid()?, solid()?],
                hold: solid()?,
                track,
                clip,
            })
        };
        let bars = [bar()?, bar()?];
        Ok(Self {
            inside,
            background,
            bars,
            warn_db: colours.meter.warn_db,
            clip_db: colours.meter.clip_db,
            radius: colours.meter.radius,
            dpi: 0.0,
            span: 0.0,
            shown: None,
        })
    }

    /// Put the inside at `rect` (window points).
    fn place(&mut self, [x, y, w, h]: [f64; 4], dpi: f64) -> Result<()> {
        let (w, h) = (w.max(0.0) * dpi, h.max(0.0) * dpi);
        place_container(&self.inside, [x * dpi, y * dpi, w, h])?;
        self.background
            .paint(INSET, [0.0, 0.0, w, h], self.radius * dpi, Affine::IDENTITY)?;
        let span = (w - 2.0 * METER_INSET * dpi).max(0.0);
        let bar = METER_BAR * dpi;
        for (i, track) in self.bars.iter().enumerate() {
            let y = (METER_INSET + i as f64 * (METER_BAR + METER_GAP)) * dpi;
            unsafe {
                track.track.SetOffsetX2((METER_INSET * dpi) as f32)?;
                track.track.SetOffsetY2(y as f32)?;
                let clip = &track.clip;
                clip.SetLeft2(0.0)?;
                clip.SetTop2(0.0)?;
                clip.SetRight2(span as f32)?;
                clip.SetBottom2(bar as f32)?;
                let r = (bar / 2.0) as f32;
                clip.SetTopLeftRadiusX2(r)?;
                clip.SetTopLeftRadiusY2(r)?;
                clip.SetTopRightRadiusX2(r)?;
                clip.SetTopRightRadiusY2(r)?;
                clip.SetBottomLeftRadiusX2(r)?;
                clip.SetBottomLeftRadiusY2(r)?;
                clip.SetBottomRightRadiusX2(r)?;
                clip.SetBottomRightRadiusY2(r)?;
            }
            track
                .background
                .paint(TRACK, [0.0, 0.0, span, bar], 0.0, Affine::IDENTITY)?;
        }
        self.dpi = dpi;
        self.span = span;
        Ok(())
    }

    /// Each bar's zones as the Slint meter lays them out: [from, to] dB,
    /// lit up to the level, over −60..0 across the bar.
    fn show(&mut self, levels: [MeterLevel; 2]) -> Result<()> {
        let ranges = [
            (-60.0, self.warn_db),
            (self.warn_db, self.clip_db),
            (self.clip_db, 0.0),
        ];
        let (span, dpi) = (self.span, self.dpi);
        let bar = METER_BAR * dpi;
        let x = |db: f32| span * f64::from(db + 60.0) / 60.0;
        let lit = |zone: &Solid, cell: u32, (from, to): (f32, f32), at: f32| {
            let top = to.min(at);
            if top > from {
                zone.paint(
                    cell,
                    [x(from), 0.0, x(top) - x(from), bar],
                    0.0,
                    Affine::IDENTITY,
                )
            } else {
                zone.hide()
            }
        };
        for (track, level) in self.bars.iter().zip(levels) {
            for ((zone, range), cell) in track.peaks.iter().zip(ranges).zip(PEAKS) {
                lit(zone, cell, range, level.peak)?;
            }
            for ((zone, range), cell) in track.levels.iter().zip(ranges).zip(ZONES) {
                lit(zone, cell, range, level.rms)?;
            }
            let hold = level.hold.min(0.0);
            if level.hold > -59.5 {
                let zone = if hold > self.clip_db {
                    2
                } else if hold > self.warn_db {
                    1
                } else {
                    0
                };
                track.hold.paint(
                    ZONES[zone],
                    [(x(hold) - dpi).max(0.0), 0.0, dpi, bar],
                    0.0,
                    Affine::IDENTITY,
                )?;
            } else {
                track.hold.hide()?;
            }
        }
        self.shown = Some(levels);
        Ok(())
    }
}

/// The stage, its pictures, the selected item's box, the master level and
/// the `--drag-test` pointer, under the window's interface.
///
/// `stage` is placed at the area's origin and clipped to it; `picture`,
/// `program` and `extra` are containers in it (offset to their place,
/// clipped to their size, hidden at zero size) with the monitor colour
/// under their `*_content` child, which the caller gives its content and a
/// transform from the source's pixels to the container's physical size
/// (`picture_sizes`). The box is drawn over all three.
pub struct Visuals {
    pub stage: IDCompositionVisual,
    pub picture_content: IDCompositionVisual,
    pub program_content: IDCompositionVisual,
    pub extra_content: IDCompositionVisual,
    root: IDCompositionVisual,
    window: Solid,
    window_size: Cell<(f64, f64)>,
    background: Solid,
    pictures: [Picture; 3],
    /// The box: a dark ring outside the line, the handles over them, in a
    /// container centred on the item and turned with it.
    selection: IDCompositionVisual,
    ring: [Solid; 4],
    outline: [Solid; 4],
    /// Each handle's edge disc and its fill disc over it.
    handles: Vec<[Solid; 2]>,
    test_outline: bool,
    test_pointer: Option<Solid>,
    meter: Meter,
    geometry: Geometry,
    dpi: f64,
    shape: Option<BoxShape>,
    style: BoxStyle,
    /// Whether `place` has run (everything is hidden until it does).
    placed: bool,
}

impl Visuals {
    /// Build the visuals and put them under `root`'s children (the
    /// interface's visual), bottom to top: the meter, the stage, the test
    /// pointer. `atlas` holds `palette(colours, test_outline)`.
    pub fn new(
        device: &IDCompositionDevice,
        root: &IDCompositionVisual,
        atlas: &IDCompositionSurface,
        colours: &Colours,
        test_outline: bool,
    ) -> Result<Self> {
        let stage = unsafe { device.CreateVisual()? };
        unsafe { stage.SetClip2(&clip_rect(0.0, 0.0))? };
        let background = Solid::new(device, atlas, Some(&stage))?;
        let pictures = [
            Picture::new(device, atlas, &stage)?,
            Picture::new(device, atlas, &stage)?,
            Picture::new(device, atlas, &stage)?,
        ];

        let selection = container(device, &stage)?;
        let solid = || Solid::new(device, atlas, Some(&selection));
        let ring = [solid()?, solid()?, solid()?, solid()?];
        let outline = [solid()?, solid()?, solid()?, solid()?];
        let mut handles = Vec::new();
        if !test_outline {
            for _ in 0..8 {
                handles.push([solid()?, solid()?]);
            }
        }
        let meter = Meter::new(device, atlas, colours)?;
        let test_pointer = if test_outline {
            Some(Solid::new(device, atlas, None)?)
        } else {
            None
        };

        // No reference and insertAbove TRUE: "rendered below all of its
        // siblings". So the last added is lowest.
        let under = |visual: &IDCompositionVisual| -> Result<()> {
            unsafe { root.AddVisual(visual, true, None::<&IDCompositionVisual>)? };
            Ok(())
        };
        if let Some(mark) = &test_pointer {
            under(&mark.visual)?;
        }
        under(&stage)?;
        under(&meter.inside)?;
        let window = Solid::new(device, atlas, None)?;
        under(&window.visual)?;

        let [picture, program, extra] = &pictures;
        Ok(Self {
            picture_content: picture.content.clone(),
            program_content: program.content.clone(),
            extra_content: extra.content.clone(),
            stage,
            root: root.clone(),
            window,
            window_size: Cell::new((0.0, 0.0)),
            background,
            pictures,
            selection,
            ring,
            outline,
            handles,
            test_outline,
            test_pointer,
            meter,
            geometry: Geometry::default(),
            dpi: 0.0,
            shape: None,
            style: BoxStyle::default(),
            placed: false,
        })
    }

    /// Keep the window opaque behind the transparent interface, as the Mac
    /// root NSWindow is. Unlike stage geometry these are physical pixels.
    pub fn window_size(&self, width: f64, height: f64) -> Result<()> {
        if self.window_size.get() != (width, height) {
            self.window
                .paint(WINDOW, [0.0, 0.0, width, height], 0.0, Affine::IDENTITY)?;
            self.window_size.set((width, height));
        }
        Ok(())
    }

    /// Place everything at `geometry` (window points) with the box at
    /// `shape`, at `dpi` physical pixels per point. Unchanged parts are
    /// left alone.
    pub fn place(
        &mut self,
        geometry: Geometry,
        shape: Option<BoxShape>,
        style: BoxStyle,
        dpi: f64,
    ) -> Result<()> {
        let rescaled = !self.placed || dpi != self.dpi;
        let moved = rescaled || geometry != self.geometry;
        if moved {
            self.place_stage(geometry, dpi)?;
        }
        if geometry.meter != self.geometry.meter || dpi != self.meter.dpi {
            self.meter.place(geometry.meter, dpi)?;
            if let Some(levels) = self.meter.shown {
                self.meter.show(levels)?;
            }
        }
        if moved || shape != self.shape || style != self.style {
            self.place_box(shape, style, geometry, dpi)?;
        }
        self.geometry = geometry;
        self.dpi = dpi;
        self.shape = shape;
        self.style = style;
        self.placed = true;
        Ok(())
    }

    /// Show the master level, left and right, at `rect` (the meter's
    /// inside, window points); nothing changes when neither did.
    pub fn show_meter(&mut self, rect: [f64; 4], levels: [MeterLevel; 2], dpi: f64) -> Result<()> {
        let placed = rect != self.geometry.meter || dpi != self.meter.dpi;
        if !placed && self.meter.shown == Some(levels) {
            return Ok(());
        }
        if placed {
            self.geometry.meter = rect;
            self.meter.place(rect, dpi)?;
        }
        self.meter.show(levels)
    }

    /// `--drag-test`: put the pointer's mark at `rect` (window points).
    /// False when this isn't a drag test.
    pub fn show_test_pointer(&self, [x, y, w, h]: [f64; 4], dpi: f64) -> Result<bool> {
        let Some(mark) = &self.test_pointer else {
            return Ok(false);
        };
        mark.paint(
            TEST_POINTER,
            [x * dpi, y * dpi, w * dpi, h * dpi],
            0.0,
            Affine::IDENTITY,
        )?;
        Ok(true)
    }

    fn place_stage(&mut self, geometry: Geometry, dpi: f64) -> Result<()> {
        let physical = |rect: [f64; 4]| rect.map(|v| v * dpi);
        let area @ [_, _, w, h] = physical(geometry.area);
        place_container(&self.stage, area)?;
        self.background
            .paint(STAGE, [0.0, 0.0, w, h], 0.0, Affine::IDENTITY)?;
        let rects = [geometry.picture, geometry.program, geometry.extra];
        for ((picture, rect), live) in self.pictures.iter_mut().zip(rects).zip(geometry.live) {
            picture.place(physical(rect), geometry.has_setup, live, dpi)?;
        }
        Ok(())
    }

    fn place_box(
        &self,
        shape: Option<BoxShape>,
        style: BoxStyle,
        geometry: Geometry,
        dpi: f64,
    ) -> Result<()> {
        let Some(BoxShape {
            rect: [x, y, w, h],
            rotate,
        }) = shape
        else {
            unsafe { self.selection.SetClip2(&clip_rect(0.0, 0.0))? };
            return Ok(());
        };
        // Stage pixels per canvas pixel.
        let k = geometry.scale * dpi;
        let [px, py, ..] = geometry.picture.map(|v| v * dpi);
        let (w, h) = (w as f64 * k, h as f64 * k);
        let centre = (px + x as f64 * k + w / 2.0, py + y as f64 * k + h / 2.0);
        // Centred on the item's centre, turned about it.
        let turn =
            Affine::turn((rotate as f64).to_radians()).then(Affine::translate(centre.0, centre.1));
        unsafe {
            self.selection.SetTransform2(&turn.matrix())?;
            self.selection.SetClip(None::<&IDCompositionClip>)?;
        }
        // A rect of the item's size grown by `grow` points, centred.
        let around = |grow: f64| {
            let grow = grow * dpi;
            [
                -w / 2.0 - grow,
                -h / 2.0 - grow,
                w + 2.0 * grow,
                h + 2.0 * grow,
            ]
        };
        if self.test_outline {
            paint_border(
                &self.outline,
                TEST_OUTLINE,
                around(3.0),
                3.0 * dpi,
                Affine::IDENTITY,
            )?;
            for edge in &self.ring {
                edge.hide()?;
            }
            return Ok(());
        }
        // The line just outside the item, the dark ring outside that.
        let edge = if style.locked { LOCKED } else { ACCENT };
        paint_border(&self.outline, edge, around(1.0), dpi, Affine::IDENTITY)?;
        paint_border(&self.ring, RING, around(2.0), dpi, Affine::IDENTITY)?;
        let (fill, rim) = if style.cropping {
            (ACCENT, ACCENT_EDGE)
        } else {
            (BONE, BONE_EDGE)
        };
        let spots = [
            (-0.5, -0.5),
            (0.0, -0.5),
            (0.5, -0.5),
            (-0.5, 0.0),
            (0.5, 0.0),
            (-0.5, 0.5),
            (0.0, 0.5),
            (0.5, 0.5),
        ];
        let size = HANDLE * dpi;
        for ([outer, inner], (fx, fy)) in self.handles.iter().zip(spots) {
            if style.locked {
                outer.hide()?;
                inner.hide()?;
                continue;
            }
            // Round, with a 1pt edge (layers.rs's border over its
            // background, corner radius half the handle).
            let (cx, cy) = (w * fx, h * fy);
            let disc = |size: f64| [cx - size / 2.0, cy - size / 2.0, size, size];
            outer.paint(rim, disc(size), size / 2.0, Affine::IDENTITY)?;
            let inside = size - 2.0 * dpi;
            inner.paint(fill, disc(inside), inside / 2.0, Affine::IDENTITY)?;
        }
        Ok(())
    }
}

impl Drop for Visuals {
    /// Take the visuals out of the window's tree (shown at the caller's
    /// next commit).
    fn drop(&mut self) {
        self.detach();
    }
}

impl Visuals {
    pub fn detach(&self) {
        unsafe {
            let _ = self.root.RemoveVisual(&self.stage);
            let _ = self.root.RemoveVisual(&self.meter.inside);
            let _ = self.root.RemoveVisual(&self.window.visual);
            if let Some(mark) = &self.test_pointer {
                let _ = self.root.RemoveVisual(&mark.visual);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_ring_tracks_dpi_and_live_tally_without_showing_hidden_monitors() {
        assert_eq!(
            monitor_ring([10.0, 20.0, 300.0, 200.0], 1.0, false),
            Some(([9.0, 19.0, 302.0, 202.0], 1.0))
        );
        assert_eq!(
            monitor_ring([10.0, 20.0, 300.0, 200.0], 1.5, true),
            Some(([7.0, 17.0, 306.0, 206.0], 3.0))
        );
        assert_eq!(monitor_ring([10.0, 20.0, 0.0, 200.0], 2.0, true), None);
        assert_eq!(monitor_ring([10.0, 20.0, 300.0, 0.0], 2.0, false), None);
    }

    #[test]
    fn rotation_then_translation_matches_canvas_coordinates() {
        let matrix = Affine::turn(std::f64::consts::FRAC_PI_2)
            .then(Affine::translate(40.0, 50.0))
            .matrix();
        let x = 3.0 * matrix.M11 + 7.0 * matrix.M21 + matrix.M31;
        let y = 3.0 * matrix.M12 + 7.0 * matrix.M22 + matrix.M32;
        assert!((x - 33.0).abs() < 0.001);
        assert!((y - 53.0).abs() < 0.001);
    }

    #[test]
    fn palette_premultiplies_translucent_cells_and_keeps_guard_texels() {
        let colours = Colours {
            window: [10, 20, 30],
            stage: [20, 30, 40],
            monitor: [0, 0, 0],
            accent: [255, 200, 0],
            ring: [255, 0, 0, 128],
            locked: [80, 80, 80],
            bone: [255, 255, 255],
            handle_edge: [0, 0, 0, 128],
            frame: [50, 50, 50],
            live: [255, 0, 0],
            meter: super::super::MeterColours {
                inset: [0, 0, 0],
                track: [30, 30, 30],
                zones: [[0, 255, 0]; 3],
                peaks: [[0, 255, 0, 128]; 3],
                warn_db: -12.0,
                clip_db: -3.0,
                radius: 4.0,
            },
        };
        let bytes = palette(&colours, false);
        assert_eq!(bytes.len(), PALETTE_WIDTH as usize * 4);
        for x in 0..CELL {
            let at = ((RING * CELL + x) * 4) as usize;
            assert_eq!(&bytes[at..at + 4], &[128, 0, 0, 128]);
            let at = ((PEAKS[0] * CELL + x) * 4) as usize;
            assert_eq!(&bytes[at..at + 4], &[0, 128, 0, 128]);
        }
    }
}
