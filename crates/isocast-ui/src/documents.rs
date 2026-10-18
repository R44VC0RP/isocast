//! Project files (`.isoprj`, #88) opened from the Finder: Isocast.app
//! declares the document type (Info.plist), and macOS hands the files to
//! the app in an "open documents" Apple event, at launch or later. They
//! wait in [`take`] until the Projects area opens them (once the engine is
//! connected).
//!
//! And [`order_behind`]: a window shown without taking the keyboard from
//! another (the editor opened after a recording while a stream runs).

use std::path::PathBuf;
use std::sync::Mutex;

/// Files macOS asked the app to open, not yet opened.
static OPENED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// The files asked for since the last call.
pub fn take() -> Vec<PathBuf> {
    std::mem::take(&mut *OPENED.lock().unwrap_or_else(|e| e.into_inner()))
}

#[cfg(target_os = "macos")]
fn opened(paths: Vec<PathBuf>) {
    isocast_diagnostics::diagnostic!("info"; "documents: asked to open {paths:?}");
    OPENED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend(paths);
}

#[cfg(target_os = "macos")]
mod mac {
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObject};
    use objc2::{AllocAnyThread, ClassType, class, define_class, msg_send, sel};

    use crate::platform::{ns_string, rust_string};

    /// 'aevt' and 'odoc': the core suite's open documents event.
    const CORE_SUITE: u32 = u32::from_be_bytes(*b"aevt");
    const OPEN_DOCUMENTS: u32 = u32::from_be_bytes(*b"odoc");
    /// '----', the event's direct object (the files), and 'list'.
    const DIRECT_OBJECT: u32 = u32::from_be_bytes(*b"----");
    const LIST: u32 = u32::from_be_bytes(*b"list");

    define_class!(
        // SAFETY: NSObject has no subclassing requirements and the class
        // adds no Drop or ivars.
        #[unsafe(super(NSObject))]
        #[name = "IsocastOpenDocuments"]
        struct Handler;

        impl Handler {
            /// AppKit installs its own handlers as the app finishes
            /// launching, before it hands over the files it was launched
            /// with: ours goes in after them.
            #[unsafe(method(willFinishLaunching:))]
            fn will_finish(&self, _note: &AnyObject) {
                install(self);
            }

            #[unsafe(method(openDocuments:withReply:))]
            fn open_documents(&self, event: &AnyObject, _reply: &AnyObject) {
                // SAFETY: the event AppKit is delivering, read on the main
                // thread; every descriptor is retained while it is used.
                let paths = unsafe { paths(event) };
                super::opened(paths);
            }
        }
    );

    /// The file paths in an open documents event.
    unsafe fn paths(event: &AnyObject) -> Vec<std::path::PathBuf> {
        unsafe {
            let direct: Option<Retained<AnyObject>> =
                msg_send![event, paramDescriptorForKeyword: DIRECT_OBJECT];
            let Some(direct) = direct else {
                return Vec::new();
            };
            let kind: u32 = msg_send![&*direct, descriptorType];
            let items: Vec<Retained<AnyObject>> = if kind == LIST {
                let count: isize = msg_send![&*direct, numberOfItems];
                (1..=count)
                    .filter_map(|i| msg_send![&*direct, descriptorAtIndex: i])
                    .collect()
            } else {
                vec![direct]
            };
            items
                .iter()
                .filter_map(|item| {
                    let url: Option<Retained<AnyObject>> = msg_send![&**item, fileURLValue];
                    let path: Option<Retained<AnyObject>> = msg_send![&*url?, path];
                    Some(std::path::PathBuf::from(rust_string(&*path?)))
                })
                .collect()
        }
    }

    fn install(handler: &Handler) {
        // SAFETY: NSAppleEventManager's shared object on the main thread;
        // the handler lives for the rest of the process (leaked below).
        unsafe {
            let manager: Retained<AnyObject> =
                msg_send![class!(NSAppleEventManager), sharedAppleEventManager];
            let _: () = msg_send![
                &*manager,
                setEventHandler: handler,
                andSelector: sel!(openDocuments:withReply:),
                forEventClass: CORE_SUITE,
                andEventID: OPEN_DOCUMENTS
            ];
        }
    }

    /// Take the open documents event from AppKit: now, and again once
    /// AppKit has put in its own as the app finishes launching.
    pub fn listen() {
        // SAFETY: plain NSObject subclass made and used on the main
        // thread; NSNotificationCenter keeps a weak reference, so the
        // handler is leaked to live as long as the app.
        unsafe {
            let handler: Retained<Handler> = msg_send![Handler::alloc(), init];
            install(&handler);
            let center: Retained<AnyObject> =
                msg_send![class!(NSNotificationCenter), defaultCenter];
            let name = ns_string("NSApplicationWillFinishLaunchingNotification");
            let none: *const AnyObject = std::ptr::null();
            let _: () = msg_send![
                &*center,
                addObserver: &*handler,
                selector: sel!(willFinishLaunching:),
                name: &*name,
                object: none
            ];
            let _ = Retained::into_raw(handler);
            let _ = Handler::class();
        }
    }

    /// Show `window` without taking the keyboard from `keep`: ordered just
    /// behind it, and `keep` stays the key window.
    pub fn order_behind(
        window: &slint::winit_030::winit::window::Window,
        keep: &slint::winit_030::winit::window::Window,
    ) {
        let (Some(window), Some(keep)) = (ns_window(window), ns_window(keep)) else {
            return;
        };
        // SAFETY: winit's windows, alive while their Slint windows are;
        // main thread. NSWindowBelow is -1.
        unsafe {
            let number: isize = msg_send![&*keep, windowNumber];
            let _: () = msg_send![&*window, orderWindow: -1isize, relativeTo: number];
            let _: () = msg_send![&*keep, makeKeyWindow];
        }
    }

    fn ns_window(window: &slint::winit_030::winit::window::Window) -> Option<Retained<AnyObject>> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let Ok(RawWindowHandle::AppKit(handle)) = window.window_handle().map(|h| h.as_raw()) else {
            return None;
        };
        // SAFETY: winit's view, alive as long as the window; main thread.
        let view: &AnyObject = unsafe { handle.ns_view.cast::<AnyObject>().as_ref() };
        unsafe { msg_send![view, window] }
    }
}

/// Start taking the files macOS asks the app to open (before the event
/// loop runs, so those it was launched with aren't missed).
pub fn listen() {
    #[cfg(target_os = "macos")]
    mac::listen();
}

/// Show `window` behind `keep`, leaving `keep` the key window (macOS);
/// elsewhere nothing more than showing it.
pub fn order_behind(window: &slint::Window, keep: &slint::Window) {
    #[cfg(target_os = "macos")]
    {
        use slint::winit_030::WinitWindowAccessor;
        window.with_winit_window(|w| {
            keep.with_winit_window(|k| mac::order_behind(w, k));
        });
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (window, keep);
}
