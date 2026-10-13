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
    /// Points per canvas pixel.
    pub scale: f64,
}

/// The selected item's box in canvas pixels: its placement before
/// rotation (top-left, size) and its rotation in degrees, clockwise.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BoxShape {
    pub rect: [f32; 4],
    pub rotate: f32,
}

/// How the box looks: locked (a bone-grey outline, no handles) or
/// cropping (bone handles).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BoxStyle {
    pub locked: bool,
    pub cropping: bool,
}

/// The colours the layers use (docs/design.md tokens).
pub struct Colours {
    /// The window behind the interface (`--surface-chrome`).
    pub window: [u8; 3],
    /// The stage (`--surface-stage`).
    pub stage: [u8; 3],
    /// Behind the picture, and handle borders (`--surface-monitor`).
    pub monitor: [u8; 3],
    /// The box and handles (`--accent`).
    pub accent: [u8; 3],
    /// A locked item's box (`--text-2`).
    pub locked: [u8; 3],
    /// Handles while cropping (`--bone`).
    pub bone: [u8; 3],
}

pub struct Layers {
    stage: Retained<CALayer>,
    picture: Retained<CALayer>,
    program: Retained<CALayer>,
    /// The box: a dark ring outside a copper one, the handles over them.
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
    /// Surfaces already tagged sRGB (by ring generation and slot).
    tagged: HashSet<(u64, u32)>,
    srgb: Option<CFRetained<CFType>>,
}

const HANDLE: f64 = 8.0;

fn color(rgb: [u8; 3], alpha: f64) -> CFRetained<CGColor> {
    let [r, g, b] = rgb.map(|c| c as f64 / 255.0);
    CGColor::new_srgb(r, g, b, alpha)
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

        // The dark ring keeps the copper edge visible on bright pictures.
        let ring = layer();
        ring.setBorderWidth(1.0);
        ring.setBorderColor(Some(&color([0, 0, 0], 0.5)));
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
                let handle = layer();
                handle.setBackgroundColor(Some(&color(colours.accent, 1.0)));
                handle.setBorderWidth(1.0);
                handle.setBorderColor(Some(&color(colours.monitor, 1.0)));
                handle.setCornerRadius(1.0);
                handle.setHidden(true);
                stage.addSublayer(&handle);
                handles.push(handle);
            }
        }

        // Under Slint's layer (the window's Metal layer, a sublayer too).
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        root.insertSublayer_atIndex(&stage, 0);
        CATransaction::commit();
        // AppKit flips the root layer of a flipped view (winit's is), so
        // its sublayers are placed top-down, as Slint's are; the stage's
        // own sublayers are made top-down either way.
        let bottom_up = !root.isGeometryFlipped();
        stage.setGeometryFlipped(bottom_up);
        let srgb = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))
            .and_then(|space| space.property_list());
        Ok(Self {
            stage,
            picture,
            program,
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
            tagged: HashSet::new(),
            srgb,
        })
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

    /// Show `frame` in Studio Mode's program picture.
    pub fn show_program(&mut self, frame: &SharedFrame) {
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        self.set_contents(&self.program.clone(), frame);
        CATransaction::commit();
    }

    /// Where the layers are now.
    pub fn geometry(&self) -> Geometry {
        self.geometry
    }

    /// No picture to show (the daemon is away): the edited one, or the
    /// program's.
    pub fn clear(&mut self, program: bool) {
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        let layer = if program {
            &self.program
        } else {
            &self.picture
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

    fn place(&self, geometry: Geometry) {
        let [x, y, w, h] = geometry.area;
        let y = if self.bottom_up {
            // The root layer's height is the view's.
            let bounds = self.stage.superlayer().map(|s| s.bounds().size.height);
            bounds.unwrap_or(0.0) - y - h
        } else {
            y
        };
        self.stage.setFrame(rect(x, y, w, h));
        let [px, py, pw, ph] = geometry.picture;
        self.picture.setFrame(rect(px, py, pw, ph));
        let [qx, qy, qw, qh] = geometry.program;
        self.program.setFrame(rect(qx, qy, qw, qh));
        self.program.setHidden(qw <= 0.0 || qh <= 0.0);
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
        // Copper just outside the item, the dark ring outside that.
        around(&self.outline, 1.0);
        around(&self.ring, 2.0);
        let edge = if style.locked {
            &self.locked
        } else {
            &self.accent
        };
        self.outline.setBorderColor(Some(edge));
        let fill = if style.cropping {
            &self.bone
        } else {
            &self.accent
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

impl Drop for Layers {
    fn drop(&mut self) {
        self.stage.removeFromSuperlayer();
    }
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
    CGRect::new(CGPoint::new(x, y), CGSize::new(w.max(0.0), h.max(0.0)))
}
