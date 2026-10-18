//! Background mode, for test and measurement runs: the window shows (so it
//! keeps drawing) without ever taking focus from the app Ryan is working
//! in. On with `ISOCAST_BACKGROUND` set, and always for `--bench` and
//! `--drag-test`.
//!
//! macOS: the app starts with activation prohibited, so neither winit's
//! activation at launch nor showing the window can bring it to the front
//! (asking winit not to activate isn't enough: the app still came to the
//! front as its window showed). Once the window is shown it becomes an
//! accessory app (no Dock icon, no menu bar), and the window sits in the
//! bottom-right corner of its screen, in front of other apps' windows but
//! the app inactive. Windows asks winit not to activate the initial window;
//! its normal interactive launch is unchanged. Elsewhere this is a no-op.

use crate::Options;

pub fn enabled(options: &Options) -> bool {
    std::env::var_os("ISOCAST_BACKGROUND").is_some()
        || options.bench.is_some()
        || options.drag_test.is_some()
}

/// Starts the app with activation prohibited (see the module's notes).
pub fn configure(selector: slint::BackendSelector) -> slint::BackendSelector {
    #[cfg(target_os = "macos")]
    {
        use slint::winit_030::winit::event_loop::EventLoop;
        use slint::winit_030::winit::platform::macos::{
            ActivationPolicy, EventLoopBuilderExtMacOS,
        };
        let mut builder = EventLoop::with_user_event();
        builder
            .with_activation_policy(ActivationPolicy::Prohibited)
            .with_activate_ignoring_other_apps(false);
        selector.with_winit_event_loop_builder(builder)
    }
    #[cfg(windows)]
    {
        selector.with_winit_window_attributes_hook(|attributes| attributes.with_active(false))
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        selector
    }
}

/// Once the window is shown: an accessory app, the window in its screen's
/// bottom-right corner and in front of other apps' windows, the app still
/// inactive.
#[cfg(target_os = "macos")]
pub fn place(window: &slint::winit_030::winit::window::Window) {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};

    // NSApplicationActivationPolicyAccessory.
    const ACCESSORY: isize = 1;
    unsafe {
        let app: Retained<AnyObject> = msg_send![class!(NSApplication), sharedApplication];
        let _: bool = msg_send![&*app, setActivationPolicy: ACCESSORY];
    }
    if let Some(ns_window) = ns_window(window) {
        anchor(&ns_window);
        let _: () = unsafe { msg_send![&*ns_window, orderFrontRegardless] };
    }
}

/// Keeps the window in its screen's bottom-right corner when it resizes
/// (the interface settles its size after the window is shown).
#[cfg(target_os = "macos")]
pub fn keep_in_corner(window: &slint::winit_030::winit::window::Window) {
    if let Some(ns_window) = ns_window(window) {
        anchor(&ns_window);
    }
}

#[cfg(target_os = "macos")]
fn ns_window(
    window: &slint::winit_030::winit::window::Window,
) -> Option<objc2::rc::Retained<objc2::runtime::AnyObject>> {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Ok(RawWindowHandle::AppKit(handle)) = window.window_handle().map(|h| h.as_raw()) else {
        return None;
    };
    // SAFETY: winit's view, alive as long as the window; called on the
    // main thread (Slint's event loop).
    let view: &AnyObject = unsafe { handle.ns_view.cast::<AnyObject>().as_ref() };
    unsafe { msg_send![view, window] }
}

#[cfg(target_os = "macos")]
fn anchor(ns_window: &objc2::runtime::AnyObject) {
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_core_foundation::{CGPoint, CGRect};

    unsafe {
        let screen: Option<Retained<AnyObject>> = msg_send![ns_window, screen];
        let Some(screen) = screen else { return };
        let area: CGRect = msg_send![&*screen, visibleFrame];
        let frame: CGRect = msg_send![ns_window, frame];
        let origin = CGPoint::new(
            area.origin.x + area.size.width - frame.size.width,
            area.origin.y,
        );
        if origin != frame.origin {
            let _: () = msg_send![ns_window, setFrameOrigin: origin];
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn place(_window: &slint::winit_030::winit::window::Window) {}

#[cfg(not(target_os = "macos"))]
pub fn keep_in_corner(_window: &slint::winit_030::winit::window::Window) {}
