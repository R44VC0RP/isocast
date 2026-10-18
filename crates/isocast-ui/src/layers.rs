//! macOS: the preview's pictures and the selected item's box as Core
//! Animation layers under the window's interface.
//!
//! Slint redraws the whole window whenever anything in it changes, so a
//! picture drawn by Slint costs a full redraw of every dock for every video
//! frame. Here each picture is a layer whose contents are the daemon's
//! shared surface itself (the window server composites it, nothing is
//! drawn or copied in this process), with the selected item's box in
//! layers over the edited one, both changed in one Core Animation
//! transaction: they reach the screen in the same frame. They sit under
//! the layer Slint draws into, which is transparent over the stage
//! (`native-picture`), so anything Slint draws over the preview (the
//! hovered item, guides, a dialog, a menu) still covers them. Slint only
//! redraws when the interface itself changes.
//!
//! The control bar's master level is a layer too (`Meter`): the bar
//! leaves the meter's inside unpainted, and the level moves there without
//! Slint redrawing the window (a meter Slint draws costs a whole-window
//! redraw per step).
//!
//! The stage layer covers the whole preview area and paints its
//! background; in Studio Mode it holds both pictures (Preview, edited,
//! and Program). The surfaces are tagged sRGB, so the window server
//! converts them to the display as it does the web app's monitor (measured
//! the same within 1/255 on a test Mac; a BT.709 tag darkened them).

use std::collections::HashSet;
use std::ffi::c_void;
use std::ptr::NonNull;

use anyhow::{Context, Result, bail};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_core_foundation::{CFRetained, CFType, CGAffineTransform, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGColor, CGColorSpace, kCGColorSpaceSRGB};
use objc2_io_surface::kIOSurfaceColorSpace;
use objc2_quartz_core::{CALayer, CATransaction, kCAFilterLinear, kCAGravityResize};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::winit_030::WinitWindowAccessor;

use isocast_client::shared::SharedFrame;

/// Where things are, in the window's points (top-left origin).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Geometry {
    /// The stage (the whole preview area).
    pub area: [f64; 4],
    /// The edited picture, relative to the area.
    pub picture: [f64; 4],
    /// Studio Mode's program picture, relative to the area (zero size
    /// when not in Studio Mode).
    pub program: [f64; 4],
    /// The chosen extra canvas's program picture, relative to the area (zero
    /// size when it isn't shown, #86).
    pub extra: [f64; 4],
    /// Points per canvas pixel.
    pub scale: f64,
    /// The control bar's master level's inside (window points).
    pub meter: [f64; 4],
}

/// One channel of the master level, in dBFS (−60 to 0): the average, the
/// peak (drawn at half strength past it) and the held peak (a 1px tick),
/// as the Slint meter (ui/widgets/meter.slint) draws them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeterLevel {
    pub rms: f32,
    pub peak: f32,
    pub hold: f32,
}

/// The master level's colours (the theme's `meter-*`, `surface-inset`).
pub struct MeterColours {
    pub inset: [u8; 3],
    pub track: [u8; 3],
    /// Safe, warning and clipping zones, and their peaks (with alpha).
    pub zones: [[u8; 3]; 3],
    pub peaks: [[u8; 4]; 3],
    /// The dB where the warning and clipping zones start.
    pub warn_db: f32,
    pub clip_db: f32,
    /// The inside's corners (the capsule's, less its 1px edge).
    pub radius: f64,
}

/// The master level: the capsule's inside, two 4px bars (2px apart, 3px
/// in) on `track`, each with its zones, peaks and hold tick.
struct Meter {
    inside: Retained<CALayer>,
    bars: [Bar; 2],
    zones: [CFRetained<CGColor>; 3],
    warn_db: f32,
    clip_db: f32,
    shown: Option<[MeterLevel; 2]>,
}

struct Bar {
    track: Retained<CALayer>,
    peaks: [Retained<CALayer>; 3],
    levels: [Retained<CALayer>; 3],
    hold: Retained<CALayer>,
}

const METER_BAR: f64 = 4.0;
const METER_INSET: f64 = 3.0;
const METER_GAP: f64 = 2.0;

/// The selected item's box in canvas pixels: its placement before
/// rotation (top-left, size) and its rotation in degrees, clockwise.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BoxShape {
    pub rect: [f32; 4],
    pub rotate: f32,
}

/// How the box looks: locked (a grey outline, no handles) or cropping
/// (yellow handles instead of bone ones).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BoxStyle {
    pub locked: bool,
    pub cropping: bool,
}

/// The colours the layers use: the theme's (ui/theme.slint, read from the
/// window's `Theme` global by preview.rs), so they follow it.
pub struct Colours {
    /// The window behind the interface (`surface-chrome`).
    pub window: [u8; 3],
    /// The stage (`surface-stage`).
    pub stage: [u8; 3],
    /// Behind the picture (`surface-monitor`).
    pub monitor: [u8; 3],
    /// The box's line, and handles while cropping (`selection-ring`).
    pub accent: [u8; 3],
    /// The ring outside the box's line (`box-ring`), with its alpha.
    pub ring: [u8; 4],
    /// A locked item's box (`text-2`).
    pub locked: [u8; 3],
    /// The handles (`bone`).
    pub bone: [u8; 3],
    /// A handle's 1px edge (`handle-edge`), with its alpha.
    pub handle_edge: [u8; 4],
    pub meter: MeterColours,
}

pub struct Layers {
    stage: Retained<CALayer>,
    picture: Retained<CALayer>,
    program: Retained<CALayer>,
    extra: Retained<CALayer>,
    /// The box: a dark ring outside a yellow one, the handles over them.
    ring: Retained<CALayer>,
    outline: Retained<CALayer>,
    handles: Vec<Retained<CALayer>>,
    accent: CFRetained<CGColor>,
    locked: CFRetained<CGColor>,
    bone: CFRetained<CGColor>,
    /// Whether the window's root layer has its origin at the bottom.
    bottom_up: bool,
    geometry: Geometry,
    shape: Option<BoxShape>,
    style: BoxStyle,
    test_outline: bool,
    /// `--drag-test`: the scripted pointer, a pure cyan mark placed the
    /// moment the pointer moves, as the box is (window captures leave the
    /// real pointer out).
    test_pointer: Option<Retained<CALayer>>,
    /// Surfaces already tagged sRGB (by ring generation and slot).
    tagged: HashSet<(u64, u32)>,
    srgb: Option<CFRetained<CFType>>,
    meter: Meter,
}

const HANDLE: f64 = 8.0;

fn color(rgb: [u8; 3], alpha: f64) -> CFRetained<CGColor> {
    let [r, g, b] = rgb.map(|c| c as f64 / 255.0);
    CGColor::new_srgb(r, g, b, alpha)
}

fn rgba([r, g, b, a]: [u8; 4]) -> CFRetained<CGColor> {
    color([r, g, b], a as f64 / 255.0)
}

/// A layer. Changes are made in transactions without animation.
fn layer() -> Retained<CALayer> {
    CALayer::new()
}

fn picture_layer(background: [u8; 3]) -> Retained<CALayer> {
    let picture = layer();
    picture.setContentsGravity(unsafe { kCAGravityResize });
    picture.setMinificationFilter(unsafe { kCAFilterLinear });
    picture.setMagnificationFilter(unsafe { kCAFilterLinear });
    picture.setBackgroundColor(Some(&color(background, 1.0)));
    picture
}

impl Layers {
    /// Put the layers under `window`'s interface, keeping the window itself
    /// opaque in the window colour (Slint asked for a transparent one,
    /// which would also leave the title bar unpainted).
    pub fn attach(window: &slint::Window, colours: Colours, test_outline: bool) -> Result<Self> {
        let view = window
            .with_winit_window(|window| {
                match window.window_handle().map(|handle| handle.as_raw()) {
                    Ok(RawWindowHandle::AppKit(handle)) => Some(handle.ns_view),
                    _ => None,
                }
            })
            .flatten()
            .context("the window has no AppKit view")?;
        let view: NonNull<c_void> = view;
        // SAFETY: winit's view, alive as long as the window; used on the
        // main thread (Slint's event loop) only.
        let view: &AnyObject = unsafe { view.cast::<AnyObject>().as_ref() };
        let root: Option<Retained<CALayer>> = unsafe { msg_send![view, layer] };
        let Some(root) = root else {
            bail!("the window's view has no layer");
        };
        let ns_window: Option<Retained<AnyObject>> = unsafe { msg_send![view, window] };
        if let Some(ns_window) = ns_window {
            let [r, g, b] = colours.window.map(|c| c as f64 / 255.0);
            unsafe {
                let colour: Retained<AnyObject> = msg_send![
                    class!(NSColor),
                    colorWithSRGBRed: r, green: g, blue: b, alpha: 1.0f64
                ];
                let _: () = msg_send![&*ns_window, setBackgroundColor: &*colour];
                let _: () = msg_send![&*ns_window, setOpaque: true];
            }
        }

        let stage = layer();
        stage.setMasksToBounds(true);
        stage.setBackgroundColor(Some(&color(colours.stage, 1.0)));

        let picture = picture_layer(colours.monitor);
        stage.addSublayer(&picture);
        let program = picture_layer(colours.monitor);
        program.setHidden(true);
        stage.addSublayer(&program);
        let extra = picture_layer(colours.monitor);
        extra.setHidden(true);
        stage.addSublayer(&extra);

        // The dark ring keeps the yellow edge visible on bright pictures.
        let ring = layer();
        ring.setBorderWidth(1.0);
        ring.setBorderColor(Some(&rgba(colours.ring)));
        ring.setHidden(true);
        stage.addSublayer(&ring);

        let outline = layer();
        let (width, colour) = if test_outline {
            (3.0, color([0, 255, 0], 1.0))
        } else {
            (1.0, color(colours.accent, 1.0))
        };
        outline.setBorderWidth(width);
        outline.setBorderColor(Some(&colour));
        outline.setHidden(true);
        stage.addSublayer(&outline);

        let mut handles = Vec::new();
        if !test_outline {
            for _ in 0..8 {
                // Round, bone with a dark edge (Paper's Main window).
                let handle = layer();
                handle.setBackgroundColor(Some(&color(colours.bone, 1.0)));
                handle.setBorderWidth(1.0);
                handle.setBorderColor(Some(&rgba(colours.handle_edge)));
                handle.setCornerRadius(HANDLE / 2.0);
                handle.setHidden(true);
                stage.addSublayer(&handle);
                handles.push(handle);
            }
        }

        let meter = Meter::new(&colours.meter);

        // Under Slint's layer (the window's Metal layer, a sublayer too).
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        root.insertSublayer_atIndex(&stage, 0);
        root.insertSublayer_atIndex(&meter.inside, 0);
        let test_pointer = test_outline.then(|| {
            let mark = layer();
            mark.setBackgroundColor(Some(&color([0, 255, 255], 1.0)));
            mark.setHidden(true);
            // Over the pictures, still under Slint's layer.
            root.insertSublayer_atIndex(&mark, 2);
            mark
        });
        CATransaction::commit();
        // AppKit flips the root layer of a flipped view (winit's is), so
        // its sublayers are placed top-down, as Slint's are; the stage's
        // own sublayers are made top-down either way.
        let bottom_up = !root.isGeometryFlipped();
        stage.setGeometryFlipped(bottom_up);
        meter.inside.setGeometryFlipped(bottom_up);
        let srgb = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))
            .and_then(|space| space.property_list());
        Ok(Self {
            stage,
            picture,
            program,
            extra,
            ring,
            outline,
            handles,
            accent: color(colours.accent, 1.0),
            locked: color(colours.locked, 1.0),
            bone: color(colours.bone, 1.0),
            bottom_up,
            geometry: Geometry::default(),
            shape: None,
            style: BoxStyle::default(),
            test_outline,
            test_pointer,
            tagged: HashSet::new(),
            srgb,
            meter,
        })
    }

    /// Show the master level, left and right, at `rect` (the meter's
    /// inside, window points), in one transaction; nothing changes when
    /// neither did.
    pub fn show_meter(&mut self, rect: [f64; 4], levels: [MeterLevel; 2]) {
        let placed = rect != self.geometry.meter;
        if !placed && self.meter.shown == Some(levels) {
            return;
        }
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        if placed {
            self.geometry.meter = rect;
            self.place_meter(rect);
        }
        self.meter.show(levels);
        CATransaction::commit();
    }

    /// Show `frame` in the edited picture with the box at `shape`,
    /// together.
    pub fn show(
        &mut self,
        frame: Option<&SharedFrame>,
        shape: Option<BoxShape>,
        style: BoxStyle,
        geometry: Geometry,
    ) {
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        if geometry != self.geometry {
            self.place(geometry);
        }
        if let Some(frame) = frame {
            self.set_contents(&self.picture.clone(), frame);
        }
        if shape != self.shape || style != self.style || geometry != self.geometry {
            self.place_box(shape, style, geometry);
        }
        self.geometry = geometry;
        self.shape = shape;
        self.style = style;
        CATransaction::commit();
    }

    /// `--drag-test`: put the pointer's mark at `rect` (window points).
    pub fn show_test_pointer(&self, rect: [f64; 4]) -> bool {
        let Some(mark) = &self.test_pointer else {
            return false;
        };
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        mark.setFrame(self.in_root(rect));
        mark.setHidden(false);
        CATransaction::commit();
        true
    }

    /// Show `frame` in Studio Mode's program picture.
    pub fn show_program(&mut self, frame: &SharedFrame) {
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        self.set_contents(&self.program.clone(), frame);
        CATransaction::commit();
    }

    /// Show `frame` in the chosen extra canvas's program picture (#86).
    pub fn show_extra(&mut self, frame: &SharedFrame) {
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        self.set_contents(&self.extra.clone(), frame);
        CATransaction::commit();
    }

    /// Where the layers are now.
    pub fn geometry(&self) -> Geometry {
        self.geometry
    }

    /// No picture to show (the daemon is away) in `role`'s monitor.
    pub fn clear(&mut self, role: crate::preview::Role) {
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        let layer = match role {
            crate::preview::Role::Editor => &self.picture,
            crate::preview::Role::Program => &self.program,
            crate::preview::Role::Extra => &self.extra,
        };
        unsafe { layer.setContents(None) };
        CATransaction::commit();
    }

    fn set_contents(&mut self, layer: &CALayer, frame: &SharedFrame) {
        let surface: &objc2_io_surface::IOSurfaceRef = frame.surface().io_surface();
        // Tag the picture sRGB (once per surface), so the window server
        // converts it to the display as the web app's monitor and the
        // image path are: untagged, a surface is taken as the display's own
        // RGB (not converted, so a wide-gamut display shows it differently),
        // and tagged BT.709 (as the
        // engine's H.264 monitor stream is) the window server applies the
        // video transfer curve and darkens it (#1f3a4a showed as #122f41).
        if let Some(space) = &self.srgb
            && self.tagged.insert(frame.slot())
        {
            // SAFETY: a valid surface and a colour space property list.
            unsafe { surface.set_value(kIOSurfaceColorSpace, space) };
            if self.tagged.len() > 64 {
                // Old rings' surfaces are gone; keep the newest ring's.
                let generation = frame.slot().0;
                self.tagged.retain(|(g, _)| *g == generation);
            }
        }
        let surface: *const objc2_io_surface::IOSurfaceRef = surface;
        // SAFETY: an IOSurfaceRef is toll-free bridged to IOSurface, which
        // a layer takes as its contents. The layer retains it.
        let contents: &AnyObject = unsafe { &*(surface as *const AnyObject) };
        unsafe { layer.setContents(Some(contents)) };
    }

    /// A window rect (top-left origin) as a frame in the root layer.
    fn in_root(&self, [x, y, w, h]: [f64; 4]) -> CGRect {
        let y = if self.bottom_up {
            // The root layer's height is the view's.
            let bounds = self.stage.superlayer().map(|s| s.bounds().size.height);
            bounds.unwrap_or(0.0) - y - h
        } else {
            y
        };
        rect(x, y, w, h)
    }

    fn place_meter(&self, meter: [f64; 4]) {
        self.meter.inside.setFrame(self.in_root(meter));
        self.meter
            .inside
            .setHidden(meter[2] <= 0.0 || meter[3] <= 0.0);
        let width = (meter[2] - 2.0 * METER_INSET).max(0.0);
        for (i, bar) in self.meter.bars.iter().enumerate() {
            let y = METER_INSET + i as f64 * (METER_BAR + METER_GAP);
            bar.track.setFrame(rect(METER_INSET, y, width, METER_BAR));
        }
    }

    fn place(&self, geometry: Geometry) {
        self.stage.setFrame(self.in_root(geometry.area));
        if geometry.meter != self.geometry.meter {
            self.place_meter(geometry.meter);
        }
        let [px, py, pw, ph] = geometry.picture;
        self.picture.setFrame(rect(px, py, pw, ph));
        let [qx, qy, qw, qh] = geometry.program;
        self.program.setFrame(rect(qx, qy, qw, qh));
        self.program.setHidden(qw <= 0.0 || qh <= 0.0);
        let [vx, vy, vw, vh] = geometry.extra;
        self.extra.setFrame(rect(vx, vy, vw, vh));
        self.extra.setHidden(vw <= 0.0 || vh <= 0.0);
    }

    fn place_box(&self, shape: Option<BoxShape>, style: BoxStyle, geometry: Geometry) {
        let Some(BoxShape {
            rect: [x, y, w, h],
            rotate,
        }) = shape
        else {
            self.ring.setHidden(true);
            self.outline.setHidden(true);
            for handle in &self.handles {
                handle.setHidden(true);
            }
            return;
        };
        let k = geometry.scale;
        let [px, py, ..] = geometry.picture;
        let (w, h) = (w as f64 * k, h as f64 * k);
        let centre = (px + (x as f64) * k + w / 2.0, py + (y as f64) * k + h / 2.0);
        let angle = (rotate as f64).to_radians();
        let turn = CGAffineTransform {
            a: angle.cos(),
            b: angle.sin(),
            c: -angle.sin(),
            d: angle.cos(),
            tx: 0.0,
            ty: 0.0,
        };
        // A layer of size (w, h) centred on the item's centre, turned
        // about it.
        let around = |layer: &CALayer, grow: f64| {
            layer.setAffineTransform(CGAffineTransform {
                a: 1.0,
                b: 0.0,
                c: 0.0,
                d: 1.0,
                tx: 0.0,
                ty: 0.0,
            });
            layer.setBounds(rect(0.0, 0.0, w + 2.0 * grow, h + 2.0 * grow));
            layer.setPosition(CGPoint::new(centre.0, centre.1));
            layer.setAffineTransform(turn);
            layer.setHidden(false);
        };
        if self.test_outline {
            around(&self.outline, 3.0);
            return;
        }
        // Yellow just outside the item, the dark ring outside that.
        around(&self.outline, 1.0);
        around(&self.ring, 2.0);
        let edge = if style.locked {
            &self.locked
        } else {
            &self.accent
        };
        self.outline.setBorderColor(Some(edge));
        let fill = if style.cropping {
            &self.accent
        } else {
            &self.bone
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
        let (sin, cos) = angle.sin_cos();
        for (handle, (fx, fy)) in self.handles.iter().zip(spots) {
            if style.locked {
                handle.setHidden(true);
                continue;
            }
            let (lx, ly) = (w * fx, h * fy);
            let (hx, hy) = (
                centre.0 + lx * cos - ly * sin,
                centre.1 + lx * sin + ly * cos,
            );
            handle.setAffineTransform(turn);
            handle.setBounds(rect(0.0, 0.0, HANDLE, HANDLE));
            handle.setPosition(CGPoint::new(hx, hy));
            handle.setBackgroundColor(Some(fill));
            handle.setHidden(false);
        }
    }
}

impl Meter {
    fn new(colours: &MeterColours) -> Self {
        let inside = layer();
        inside.setBackgroundColor(Some(&color(colours.inset, 1.0)));
        inside.setCornerRadius(colours.radius);
        inside.setHidden(true);
        let zones = colours.zones.map(|c| color(c, 1.0));
        let bars = [(); 2].map(|_| {
            let track = layer();
            track.setBackgroundColor(Some(&color(colours.track, 1.0)));
            track.setCornerRadius(METER_BAR / 2.0);
            track.setMasksToBounds(true);
            let peaks = colours.peaks.map(|c| {
                let zone = layer();
                zone.setBackgroundColor(Some(&rgba(c)));
                zone.setHidden(true);
                track.addSublayer(&zone);
                zone
            });
            let levels = zones.clone().map(|c| {
                let zone = layer();
                zone.setBackgroundColor(Some(&c));
                zone.setHidden(true);
                track.addSublayer(&zone);
                zone
            });
            let hold = layer();
            hold.setHidden(true);
            track.addSublayer(&hold);
            inside.addSublayer(&track);
            Bar {
                track,
                peaks,
                levels,
                hold,
            }
        });
        Self {
            inside,
            bars,
            zones,
            warn_db: colours.warn_db,
            clip_db: colours.clip_db,
            shown: None,
        }
    }

    /// Each bar's zones as the Slint meter lays them out: [from, to] dB,
    /// lit up to the level, over −60..0 across the bar.
    fn show(&mut self, levels: [MeterLevel; 2]) {
        let ranges = [
            (-60.0, self.warn_db),
            (self.warn_db, self.clip_db),
            (self.clip_db, 0.0),
        ];
        for (bar, level) in self.bars.iter().zip(levels) {
            let span = bar.track.bounds().size.width;
            let x = |db: f32| span * f64::from(db + 60.0) / 60.0;
            let lit = |zone: &CALayer, (from, to): (f32, f32), at: f32| {
                let top = to.min(at);
                zone.setHidden(top <= from);
                if top > from {
                    zone.setFrame(rect(x(from), 0.0, x(top) - x(from), METER_BAR));
                }
            };
            for (zone, range) in bar.peaks.iter().zip(ranges) {
                lit(zone, range, level.peak);
            }
            for (zone, range) in bar.levels.iter().zip(ranges) {
                lit(zone, range, level.rms);
            }
            let hold = level.hold.min(0.0);
            bar.hold.setHidden(level.hold <= -59.5);
            if level.hold > -59.5 {
                let zone = if hold > self.clip_db {
                    2
                } else if hold > self.warn_db {
                    1
                } else {
                    0
                };
                bar.hold.setBackgroundColor(Some(&self.zones[zone]));
                bar.hold
                    .setFrame(rect((x(hold) - 1.0).max(0.0), 0.0, 1.0, METER_BAR));
            }
        }
        self.shown = Some(levels);
    }
}

impl Drop for Layers {
    fn drop(&mut self) {
        self.stage.removeFromSuperlayer();
        self.meter.inside.removeFromSuperlayer();
    }
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
    CGRect::new(CGPoint::new(x, y), CGSize::new(w.max(0.0), h.max(0.0)))
}
