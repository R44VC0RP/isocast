//! The floating helper for granting a permission by hand: a small
//! borderless panel that floats beside System Settings with the app's icon
//! as a real drag source (a file URL of the bundle macOS lists, which the
//! Privacy & Security list accepts) and one line saying what to do. It
//! never takes focus (a non-activating panel), follows the System Settings
//! window, and is closed by the permissions step (src/areas/permissions.rs)
//! once the permission is granted or the person comes back to Isocast.
//!
//! The System Settings window is found with public API only:
//! `CGWindowListCopyWindowInfo` gives every on-screen window's owner pid
//! and bounds without screen recording permission (only titles need it),
//! and `NSRunningApplication` gives System Settings' pid by bundle id.

#[cfg(target_os = "macos")]
pub use mac::{Helper, render_png};

#[cfg(not(target_os = "macos"))]
pub use other::{Helper, render_png};

/// The helper's size in points.
const WIDTH: f64 = 300.0;
const HEIGHT: f64 = 76.0;
/// The gap between it and the System Settings window.
const GAP: f64 = 12.0;

#[cfg(target_os = "macos")]
mod mac {
    use std::cell::{Cell, RefCell};
    use std::ffi::c_void;
    use std::path::Path;
    use std::rc::Rc;
    use std::sync::OnceLock;
    use std::time::Duration;

    use anyhow::{Context, Result, bail};
    use objc2::rc::{Allocated, Retained};
    use objc2::runtime::{AnyClass, AnyObject, AnyProtocol, Bool, ClassBuilder, Sel};
    use objc2::{class, msg_send, sel};
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};

    use super::{GAP, HEIGHT, WIDTH};
    use crate::platform::ns_string;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        /// A +1 CFArray of CFDictionaries (toll-free NSArray/NSDictionary).
        fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> *mut AnyObject;
    }

    #[link(name = "CoreText", kind = "framework")]
    unsafe extern "C" {
        fn CTFontManagerCreateFontDescriptorFromData(data: *const AnyObject) -> *mut c_void;
        fn CTFontCreateWithFontDescriptor(
            descriptor: *const c_void,
            size: f64,
            matrix: *const c_void,
        ) -> *mut AnyObject;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(object: *const c_void);
    }

    const ON_SCREEN_ONLY: u32 = 1 << 0;
    const EXCLUDE_DESKTOP: u32 = 1 << 4;
    const BORDERLESS: usize = 0;
    const NON_ACTIVATING_PANEL: usize = 1 << 7;
    const BUFFERED: usize = 2;
    const FLOATING_LEVEL: isize = 3;
    const CAN_JOIN_ALL_SPACES: usize = 1 << 0;
    const FULL_SCREEN_AUXILIARY: usize = 1 << 8;
    const SYSTEM_SETTINGS: &str = "com.apple.systempreferences";

    thread_local! {
        /// The bundle being dragged (one helper at a time).
        static DRAG_PATH: RefCell<String> = const { RefCell::new(String::new()) };
        /// The press already started a drag.
        static DRAGGING: Cell<bool> = const { Cell::new(false) };
        /// The bundle's icon, drawn by the view.
        static ICON: RefCell<Option<Retained<AnyObject>>> = const { RefCell::new(None) };
    }

    /// The helper's view: the whole helper is the drag source (its icon
    /// and its line don't take the mouse), drawn as Console's floating
    /// surface. An NSView subclass made at run time (the crate has no
    /// AppKit bindings, only the Objective-C runtime).
    fn drag_view_class() -> &'static AnyClass {
        static CLASS: OnceLock<&'static AnyClass> = OnceLock::new();
        CLASS.get_or_init(|| {
            let superclass = AnyClass::get(c"NSView").expect("AppKit is loaded");
            let mut builder = ClassBuilder::new(c"IsocastPermissionDragView", superclass)
                .expect("the class is made once");
            if let Some(source) = AnyProtocol::get(c"NSDraggingSource") {
                builder.add_protocol(source);
            }
            // SAFETY: each function's signature matches its selector's
            // (receiver, _cmd, then the arguments AppKit passes).
            unsafe {
                builder.add_method(
                    sel!(isFlipped),
                    yes as unsafe extern "C-unwind" fn(*mut AnyObject, Sel) -> Bool,
                );
                builder.add_method(
                    sel!(hitTest:),
                    hit_test
                        as unsafe extern "C-unwind" fn(
                            *mut AnyObject,
                            Sel,
                            CGPoint,
                        ) -> *mut AnyObject,
                );
                builder.add_method(
                    sel!(acceptsFirstMouse:),
                    yes_for
                        as unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> Bool,
                );
                builder.add_method(
                    sel!(mouseDownCanMoveWindow),
                    no as unsafe extern "C-unwind" fn(*mut AnyObject, Sel) -> Bool,
                );
                builder.add_method(
                    sel!(mouseDown:),
                    mouse_down as unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject),
                );
                builder.add_method(
                    sel!(mouseDragged:),
                    mouse_dragged
                        as unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject),
                );
                builder.add_method(
                    sel!(mouseUp:),
                    mouse_down as unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject),
                );
                builder.add_method(
                    sel!(draggingSession:sourceOperationMaskForDraggingContext:),
                    operation_mask
                        as unsafe extern "C-unwind" fn(
                            *mut AnyObject,
                            Sel,
                            *mut AnyObject,
                            isize,
                        ) -> usize,
                );
                builder.add_method(
                    sel!(drawRect:),
                    draw_rect as unsafe extern "C-unwind" fn(*mut AnyObject, Sel, CGRect),
                );
            }
            builder.register()
        })
    }

    unsafe extern "C-unwind" fn yes(_this: *mut AnyObject, _cmd: Sel) -> Bool {
        Bool::YES
    }

    unsafe extern "C-unwind" fn no(_this: *mut AnyObject, _cmd: Sel) -> Bool {
        Bool::NO
    }

    /// A click on the (inactive) panel starts the drag at once.
    unsafe extern "C-unwind" fn yes_for(
        _this: *mut AnyObject,
        _cmd: Sel,
        _event: *mut AnyObject,
    ) -> Bool {
        Bool::YES
    }

    /// Every point inside the helper is the view's own.
    unsafe extern "C-unwind" fn hit_test(
        this: *mut AnyObject,
        _cmd: Sel,
        point: CGPoint,
    ) -> *mut AnyObject {
        // SAFETY: AppKit passes the live view; plain geometry, main thread.
        let frame: CGRect = unsafe { msg_send![&*this, frame] };
        let inside = point.x >= frame.origin.x
            && point.y >= frame.origin.y
            && point.x <= frame.origin.x + frame.size.width
            && point.y <= frame.origin.y + frame.size.height;
        if inside { this } else { std::ptr::null_mut() }
    }

    unsafe extern "C-unwind" fn mouse_down(
        _this: *mut AnyObject,
        _cmd: Sel,
        _event: *mut AnyObject,
    ) {
        DRAGGING.set(false);
    }

    unsafe extern "C-unwind" fn mouse_dragged(
        this: *mut AnyObject,
        _cmd: Sel,
        event: *mut AnyObject,
    ) {
        if DRAGGING.replace(true) || event.is_null() || this.is_null() {
            return;
        }
        let path = DRAG_PATH.with(|p| p.borrow().clone());
        // SAFETY: AppKit objects made and used on the main thread; the
        // event is the one AppKit is delivering.
        unsafe { begin_drag(&*this, &*event, &path) };
    }

    /// NSDraggingSource: the list in System Settings takes a copy (or a
    /// link); offer every operation and let it choose.
    unsafe extern "C-unwind" fn operation_mask(
        _this: *mut AnyObject,
        _cmd: Sel,
        _session: *mut AnyObject,
        _context: isize,
    ) -> usize {
        usize::MAX
    }

    /// Console's floating surface: overlay grey, a 1px `line-strong` edge,
    /// the menus' 8px corners.
    unsafe extern "C-unwind" fn draw_rect(this: *mut AnyObject, _cmd: Sel, _dirty: CGRect) {
        // SAFETY: AppKit passes the live view; drawing into the current
        // context, main thread.
        unsafe {
            let bounds: CGRect = msg_send![&*this, bounds];
            let inset = CGRect::new(
                CGPoint::new(bounds.origin.x + 0.5, bounds.origin.y + 0.5),
                CGSize::new(bounds.size.width - 1.0, bounds.size.height - 1.0),
            );
            let path: Retained<AnyObject> = msg_send![
                class!(NSBezierPath),
                bezierPathWithRoundedRect: inset,
                xRadius: 8.0f64,
                yRadius: 8.0f64
            ];
            let fill = color(0x1e, 0x1e, 0x1e);
            let _: () = msg_send![&*fill, setFill];
            let _: () = msg_send![&*path, fill];
            let edge = color(0x33, 0x33, 0x33);
            let _: () = msg_send![&*edge, setStroke];
            let _: () = msg_send![&*path, setLineWidth: 1.0f64];
            let _: () = msg_send![&*path, stroke];
            ICON.with(|icon| {
                if let Some(icon) = icon.borrow().as_ref() {
                    const SOURCE_OVER: usize = 2;
                    let none: Option<&AnyObject> = None;
                    let _: () = msg_send![
                        &**icon,
                        drawInRect: ICON_FRAME,
                        fromRect: CGRect::ZERO,
                        operation: SOURCE_OVER,
                        fraction: 1.0f64,
                        respectFlipped: true,
                        hints: none
                    ];
                }
            });
        }
    }

    /// Where the icon sits (flipped view coordinates).
    const ICON_FRAME: CGRect = CGRect::new(CGPoint::new(14.0, 14.0), CGSize::new(48.0, 48.0));

    /// Start dragging a file URL of `path`, its icon under the pointer.
    unsafe fn begin_drag(view: &AnyObject, event: &AnyObject, path: &str) {
        unsafe {
            let url: Retained<AnyObject> =
                msg_send![class!(NSURL), fileURLWithPath: &*ns_string(path)];
            let allocated: Allocated<AnyObject> = msg_send![class!(NSDraggingItem), alloc];
            let item: Retained<AnyObject> = msg_send![allocated, initWithPasteboardWriter: &*url];
            let icon = icon(path);
            let _: () = msg_send![&*item, setDraggingFrame: ICON_FRAME, contents: &*icon];
            let items: Retained<AnyObject> = msg_send![class!(NSArray), arrayWithObject: &*item];
            let _: Retained<AnyObject> = msg_send![
                view,
                beginDraggingSessionWithItems: &*items,
                event: event,
                source: view
            ];
        }
    }

    unsafe fn color(r: u8, g: u8, b: u8) -> Retained<AnyObject> {
        unsafe {
            msg_send![
                class!(NSColor),
                colorWithSRGBRed: f64::from(r) / 255.0,
                green: f64::from(g) / 255.0,
                blue: f64::from(b) / 255.0,
                alpha: 1.0f64
            ]
        }
    }

    /// The bundle's icon, as Finder shows it.
    unsafe fn icon(path: &str) -> Retained<AnyObject> {
        unsafe {
            let workspace: Retained<AnyObject> = msg_send![class!(NSWorkspace), sharedWorkspace];
            let image: Retained<AnyObject> = msg_send![&*workspace, iconForFile: &*ns_string(path)];
            let _: () = msg_send![&*image, setSize: CGSize::new(48.0, 48.0)];
            image
        }
    }

    /// Mona Sans Medium at `size` (the interface's face), from the copy
    /// built into the app; the system font if it can't be read.
    unsafe fn font(size: f64) -> Retained<AnyObject> {
        static MONA: &[u8] = include_bytes!("../ui/fonts/MonaSans-Medium.ttf");
        unsafe {
            let data: Retained<AnyObject> = msg_send![
                class!(NSData),
                dataWithBytes: MONA.as_ptr().cast::<c_void>(),
                length: MONA.len()
            ];
            let descriptor = CTFontManagerCreateFontDescriptorFromData(&*data);
            if !descriptor.is_null() {
                let font = CTFontCreateWithFontDescriptor(descriptor, size, std::ptr::null());
                CFRelease(descriptor);
                if let Some(font) = Retained::from_raw(font) {
                    return font;
                }
            }
            msg_send![class!(NSFont), systemFontOfSize: size, weight: 0.23f64]
        }
    }

    /// The helper's content: the icon and its line, in a drag view.
    unsafe fn content(path: &str, text: &str) -> Retained<AnyObject> {
        unsafe {
            let frame = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(WIDTH, HEIGHT));
            let allocated: Allocated<AnyObject> = msg_send![drag_view_class(), alloc];
            let view: Retained<AnyObject> = msg_send![allocated, initWithFrame: frame];

            ICON.with(|i| *i.borrow_mut() = Some(icon(path)));

            let label: Retained<AnyObject> =
                msg_send![class!(NSTextField), wrappingLabelWithString: &*ns_string(text)];
            let _: () = msg_send![&*label, setFont: &*font(13.0)];
            let _: () = msg_send![&*label, setTextColor: &*color(0xf5, 0xf5, 0xf5)];
            let _: () = msg_send![&*label, setDrawsBackground: false];
            let _: () = msg_send![&*label, setSelectable: false];
            // Centred on the icon, however many lines it takes.
            let width = WIDTH - 74.0 - 16.0;
            let _: () = msg_send![&*label, setPreferredMaxLayoutWidth: width];
            let fitting: CGSize = msg_send![&*label, fittingSize];
            let height = fitting.height.min(HEIGHT - 16.0);
            let _: () = msg_send![
                &*label,
                setFrame: CGRect::new(CGPoint::new(74.0, (HEIGHT - height) / 2.0), CGSize::new(width, height))
            ];
            let _: () = msg_send![&*view, addSubview: &*label];
            view
        }
    }

    /// Where System Settings' main window is, in Core Graphics' global
    /// coordinates (top-left origin, y down), if it has one on screen: its
    /// largest normal-level window, not the frontmost, because its sheets
    /// (the password prompt, "Quit & Reopen") are windows of their own in
    /// front of it, and following them put the helper over the list.
    fn settings_window() -> Option<CGRect> {
        // SAFETY: Foundation/AppKit queries on the main thread; the window
        // list is +1 and released by `Retained`.
        unsafe {
            let apps: Retained<AnyObject> = msg_send![
                class!(NSRunningApplication),
                runningApplicationsWithBundleIdentifier: &*ns_string(SYSTEM_SETTINGS)
            ];
            let first: Option<Retained<AnyObject>> = msg_send![&*apps, firstObject];
            let pid: i32 = msg_send![&*first?, processIdentifier];
            let list = Retained::from_raw(CGWindowListCopyWindowInfo(
                ON_SCREEN_ONLY | EXCLUDE_DESKTOP,
                0,
            ))?;
            let count: usize = msg_send![&*list, count];
            let number = |info: &AnyObject, key: &str| -> Option<f64> {
                let value: Option<Retained<AnyObject>> =
                    msg_send![info, objectForKey: &*ns_string(key)];
                Some(msg_send![&*value?, doubleValue])
            };
            let mut largest: Option<CGRect> = None;
            for i in 0..count {
                let info: Retained<AnyObject> = msg_send![&*list, objectAtIndex: i];
                if number(&info, "kCGWindowOwnerPID") != Some(f64::from(pid))
                    || number(&info, "kCGWindowLayer") != Some(0.0)
                {
                    continue;
                }
                let bounds: Option<Retained<AnyObject>> =
                    msg_send![&*info, objectForKey: &*ns_string("kCGWindowBounds")];
                let Some(rect) = bounds.and_then(|b| {
                    Some(CGRect::new(
                        CGPoint::new(number(&b, "X")?, number(&b, "Y")?),
                        CGSize::new(number(&b, "Width")?, number(&b, "Height")?),
                    ))
                }) else {
                    continue;
                };
                let area = |r: &CGRect| r.size.width * r.size.height;
                if rect.size.width > 100.0
                    && rect.size.height > 100.0
                    && largest.is_none_or(|l| area(&rect) > area(&l))
                {
                    largest = Some(rect);
                }
            }
            largest
        }
    }

    /// The screens' visible frames (Cocoa coordinates) and the primary
    /// screen's height (to turn Core Graphics' y into Cocoa's).
    unsafe fn screens() -> (Vec<CGRect>, f64) {
        unsafe {
            let screens: Retained<AnyObject> = msg_send![class!(NSScreen), screens];
            let count: usize = msg_send![&*screens, count];
            let mut frames = Vec::new();
            let mut primary = 0.0;
            for i in 0..count {
                let screen: Retained<AnyObject> = msg_send![&*screens, objectAtIndex: i];
                if i == 0 {
                    let frame: CGRect = msg_send![&*screen, frame];
                    primary = frame.size.height;
                }
                frames.push(msg_send![&*screen, visibleFrame]);
            }
            (frames, primary)
        }
    }

    /// Where the helper goes (its frame's origin, Cocoa coordinates):
    /// beside the System Settings window, on the side with room, level
    /// with its list; else the main screen's bottom-right corner.
    fn place(settings: Option<CGRect>) -> CGPoint {
        // SAFETY: NSScreen on the main thread.
        let (frames, primary) = unsafe { screens() };
        let fits = |x: f64, y: f64| {
            frames.iter().any(|f| {
                x >= f.origin.x
                    && y >= f.origin.y
                    && x + WIDTH <= f.origin.x + f.size.width
                    && y + HEIGHT <= f.origin.y + f.size.height
            })
        };
        if let Some(s) = settings {
            // Level with the list, a little under the window's top.
            let top = s.origin.y + (s.size.height * 0.3).min(160.0);
            let y = primary - top - HEIGHT;
            let right = s.origin.x + s.size.width + GAP;
            let left = s.origin.x - GAP - WIDTH;
            for x in [right, left] {
                if fits(x, y) {
                    return CGPoint::new(x, y);
                }
            }
            // No room either side: over the window's bottom-right corner.
            let x = s.origin.x + s.size.width - WIDTH - GAP;
            let y = primary - (s.origin.y + s.size.height) + GAP;
            if fits(x, y) {
                return CGPoint::new(x, y);
            }
        }
        let main = frames.first().copied().unwrap_or(CGRect::new(
            CGPoint::new(0.0, 0.0),
            CGSize::new(1024.0, 768.0),
        ));
        CGPoint::new(
            main.origin.x + main.size.width - WIDTH - 24.0,
            main.origin.y + 24.0,
        )
    }

    fn app_active() -> bool {
        // SAFETY: NSApplication's shared object on the main thread.
        unsafe {
            let app: Retained<AnyObject> = msg_send![class!(NSApplication), sharedApplication];
            let active: Bool = msg_send![&*app, isActive];
            active.as_bool()
        }
    }

    /// The helper while it shows. Dropping it closes it.
    pub struct Helper {
        panel: Retained<AnyObject>,
        _timer: slint::Timer,
    }

    impl Helper {
        /// Show the helper for dragging `app` (the bundle's path), with
        /// `text`. `returned` is called once when the person comes back
        /// to Isocast (it was left for System Settings, then made active
        /// again) or System Settings quits.
        pub fn show(app: &Path, text: &str, returned: impl Fn() + 'static) -> Result<Helper> {
            let path = app
                .to_str()
                .context("the app's path isn't UTF-8")?
                .to_string();
            if !app.exists() {
                bail!("{path} doesn't exist");
            }
            DRAG_PATH.with(|p| *p.borrow_mut() = path.clone());
            // SAFETY: AppKit objects made and shown on the main thread.
            let panel = unsafe {
                let origin = place(settings_window());
                let frame = CGRect::new(origin, CGSize::new(WIDTH, HEIGHT));
                let allocated: Allocated<AnyObject> = msg_send![class!(NSPanel), alloc];
                let panel: Retained<AnyObject> = msg_send![
                    allocated,
                    initWithContentRect: frame,
                    styleMask: BORDERLESS | NON_ACTIVATING_PANEL,
                    backing: BUFFERED,
                    defer: false
                ];
                let _: () = msg_send![&*panel, setReleasedWhenClosed: false];
                let _: () = msg_send![&*panel, setOpaque: false];
                let clear: Retained<AnyObject> = msg_send![class!(NSColor), clearColor];
                let _: () = msg_send![&*panel, setBackgroundColor: &*clear];
                let _: () = msg_send![&*panel, setHasShadow: true];
                let _: () = msg_send![&*panel, setLevel: FLOATING_LEVEL];
                let _: () = msg_send![&*panel, setFloatingPanel: true];
                let _: () = msg_send![&*panel, setHidesOnDeactivate: false];
                let _: () = msg_send![&*panel, setBecomesKeyOnlyIfNeeded: true];
                let _: () = msg_send![
                    &*panel,
                    setCollectionBehavior: CAN_JOIN_ALL_SPACES | FULL_SCREEN_AUXILIARY
                ];
                let _: () = msg_send![&*panel, setContentView: &*content(&path, text)];
                let _: () = msg_send![&*panel, orderFrontRegardless];
                panel
            };
            isocast_diagnostics::diagnostic!("info"; "permissions: helper shown for {path}");

            // Follow the System Settings window; notice the person coming
            // back to Isocast.
            let timer = slint::Timer::default();
            let follow = panel.clone();
            let left = Cell::new(!app_active());
            let seen_settings = Cell::new(false);
            let returned = Rc::new(returned);
            let done = Cell::new(false);
            timer.start(
                slint::TimerMode::Repeated,
                Duration::from_millis(250),
                move || {
                    if done.get() {
                        return;
                    }
                    let settings = settings_window();
                    seen_settings.set(seen_settings.get() || settings.is_some());
                    let active = app_active();
                    if !active {
                        left.set(true);
                    }
                    // SAFETY: NSRunningApplication on the main thread.
                    let running = unsafe {
                        let apps: Retained<AnyObject> = msg_send![
                            class!(NSRunningApplication),
                            runningApplicationsWithBundleIdentifier: &*ns_string(SYSTEM_SETTINGS)
                        ];
                        let count: usize = msg_send![&*apps, count];
                        count > 0
                    };
                    if (left.get() && active) || (seen_settings.get() && !running) {
                        done.set(true);
                        returned();
                        return;
                    }
                    let origin = place(settings);
                    // SAFETY: the panel is ours, main thread.
                    unsafe {
                        let frame: CGRect = msg_send![&*follow, frame];
                        if (frame.origin.x - origin.x).abs() > 0.5
                            || (frame.origin.y - origin.y).abs() > 0.5
                        {
                            let _: () = msg_send![&*follow, setFrameOrigin: origin];
                        }
                    }
                },
            );
            Ok(Helper {
                panel,
                _timer: timer,
            })
        }
    }

    impl Drop for Helper {
        fn drop(&mut self) {
            // SAFETY: the panel is ours, main thread.
            unsafe {
                let _: () = msg_send![&*self.panel, orderOut: Option::<&AnyObject>::None];
                let _: () = msg_send![&*self.panel, close];
            }
            isocast_diagnostics::diagnostic!("info"; "permissions: helper closed");
        }
    }

    /// Draw the helper (the same view, never shown) into a PNG at `scale`,
    /// for snapshots.
    pub fn render_png(app: &Path, text: &str, out: &Path, scale: f32) -> Result<()> {
        let path = app.to_str().context("the app's path isn't UTF-8")?;
        let out = out.to_str().context("the output path isn't UTF-8")?;
        let scale = f64::from(scale.max(1.0));
        // SAFETY: AppKit drawing on the main thread into our own bitmap.
        unsafe {
            let _: Retained<AnyObject> = msg_send![class!(NSApplication), sharedApplication];
            let view = content(path, text);
            let bounds: CGRect = msg_send![&*view, bounds];
            let allocated: Allocated<AnyObject> = msg_send![class!(NSBitmapImageRep), alloc];
            let null: *mut *mut u8 = std::ptr::null_mut();
            let color_space = ns_string("NSCalibratedRGBColorSpace");
            let rep: Option<Retained<AnyObject>> = msg_send![
                allocated,
                initWithBitmapDataPlanes: null,
                pixelsWide: (WIDTH * scale) as isize,
                pixelsHigh: (HEIGHT * scale) as isize,
                bitsPerSample: 8isize,
                samplesPerPixel: 4isize,
                hasAlpha: true,
                isPlanar: false,
                colorSpaceName: &*color_space,
                bytesPerRow: 0isize,
                bitsPerPixel: 0isize
            ];
            let rep = rep.context("could not make a bitmap")?;
            let _: () = msg_send![&*rep, setSize: CGSize::new(WIDTH, HEIGHT)];
            let _: () = msg_send![&*view, cacheDisplayInRect: bounds, toBitmapImageRep: &*rep];
            let properties: Retained<AnyObject> = msg_send![class!(NSDictionary), dictionary];
            const PNG: usize = 4;
            let data: Option<Retained<AnyObject>> =
                msg_send![&*rep, representationUsingType: PNG, properties: &*properties];
            let data = data.context("could not encode the PNG")?;
            let ok: Bool = msg_send![&*data, writeToFile: &*ns_string(out), atomically: true];
            if !ok.as_bool() {
                bail!("could not write {out}");
            }
        }
        Ok(())
    }
}

#[cfg(not(target_os = "macos"))]
mod other {
    use std::path::Path;

    use anyhow::{Result, bail};

    /// No helper outside macOS: System Settings is macOS's.
    pub struct Helper;

    impl Helper {
        pub fn show(_app: &Path, _text: &str, _returned: impl Fn() + 'static) -> Result<Helper> {
            bail!("the permission helper is macOS's")
        }
    }

    pub fn render_png(_app: &Path, _text: &str, _out: &Path, _scale: f32) -> Result<()> {
        bail!("the permission helper is macOS's")
    }
}
