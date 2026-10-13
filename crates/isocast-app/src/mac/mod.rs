//! The macOS shell: the application, its menus and its one window, whose
//! content is a `WKWebView` showing the interface (see [`web`]).
//!
//! AppKit and WebKit live on the main thread. Work for the daemon runs on
//! other threads and comes back through the main dispatch queue; the
//! objects it needs are kept in [`MAIN`].

mod snapshot;
mod web;

use std::cell::RefCell;
use std::process::ExitCode;
use std::sync::{Arc, LazyLock};

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{MainThreadMarker, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAppearance, NSAppearanceNameDarkAqua, NSApplication, NSApplicationActivationPolicy,
    NSApplicationDelegate, NSBackingStoreType, NSColor, NSControlStateValueOff,
    NSControlStateValueOn, NSEventModifierFlags, NSMenu, NSMenuItem, NSWindow, NSWindowDelegate,
    NSWindowStyleMask, NSWindowTabbingMode, NSWindowTitleVisibility,
};
use objc2_foundation::{
    NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, ns_string,
};
use objc2_web_kit::WKWebView;
use serde_json::{Value, json};

use crate::daemon::Daemon;
use crate::options::Options;

/// The daemon, shared by every thread.
pub static DAEMON: LazyLock<Arc<Daemon>> = LazyLock::new(Default::default);

const DEFAULT_SIZE: (f64, f64) = (1440.0, 900.0);
/// Small enough for a 13-inch screen; the docks still fit side by side.
const MIN_SIZE: (f64, f64) = (960.0, 600.0);
/// The app background (`--bg-app` in the interface), shown before the page
/// paints.
const BACKGROUND: (f64, f64, f64) = (
    0x14 as f64 / 255.0,
    0x16 as f64 / 255.0,
    0x19 as f64 / 255.0,
);

/// Main-thread objects.
pub(crate) struct Main {
    pub window: Retained<NSWindow>,
    pub web: Retained<WKWebView>,
    /// Keeps the delegate alive (windows and the app hold it weakly).
    _delegate: Retained<AppDelegate>,
    pub state: web::State,
    pub snapshot: Option<snapshot::Pending>,
}

thread_local! {
    pub(crate) static MAIN: RefCell<Option<Main>> = const { RefCell::new(None) };
}

/// Run `f` with the main-thread objects, if the window exists. Don't call
/// AppKit from `f` in ways that could call back into this module (resizing
/// the window, say); take what it needs out first.
pub(crate) fn with_main<R>(f: impl FnOnce(&mut Main) -> R) -> Option<R> {
    MAIN.with(|main| main.borrow_mut().as_mut().map(f))
}

/// Run `f` on the main thread, soon.
pub(crate) fn on_main(f: impl FnOnce() + Send + 'static) {
    DispatchQueue::main().exec_async(f);
}

pub fn run(options: Options) -> ExitCode {
    let mtm = MainThreadMarker::new().expect("the app starts on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    let snapshot = options.snapshot.is_some();
    let options_route_is_icon = options
        .snapshot
        .as_ref()
        .is_some_and(|s| s.route.as_deref() == Some("icon"));
    // ISOCAST_BACKGROUND (test runs, e.g. scripts/drag-lag.py): show the
    // window without ever activating the app — no Dock icon, no focus taken
    // from whatever Ryan is working in.
    let background = std::env::var_os("ISOCAST_BACKGROUND").is_some();
    app.setActivationPolicy(if snapshot {
        NSApplicationActivationPolicy::Prohibited
    } else if background {
        NSApplicationActivationPolicy::Accessory
    } else {
        NSApplicationActivationPolicy::Regular
    });
    app.setAppearance(
        NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua }).as_deref(),
    );
    let delegate = AppDelegate::new(mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));

    let (window, query) = match &options.snapshot {
        Some(snapshot) => (snapshot::window(mtm, snapshot), snapshot::query(snapshot)),
        None => {
            app.setMainMenu(Some(&menu(mtm, &delegate)));
            (main_window(mtm, &delegate), String::new())
        }
    };
    let web = web::web_view(mtm, &window, &query);
    MAIN.with(|main| {
        *main.borrow_mut() = Some(Main {
            window: window.clone(),
            web,
            _delegate: delegate,
            state: web::State::default(),
            snapshot: options.snapshot.map(snapshot::Pending::new),
        })
    });

    // The icon (for packaging) needs no daemon.
    let icon = options_route_is_icon;
    if !icon {
        DAEMON.watch(web::emit);
    }
    if snapshot {
        snapshot::arm(mtm);
    } else if background {
        // In the screen's bottom-right corner, visible (so it keeps
        // drawing) but never key or active.
        if let Some(screen) = window.screen() {
            let area = screen.visibleFrame();
            let size = window.frame().size;
            window.setFrameOrigin(NSPoint::new(
                area.origin.x + area.size.width - size.width,
                area.origin.y,
            ));
        }
        window.orderFrontRegardless();
    } else {
        window.makeKeyAndOrderFront(None);
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }
    app.run();
    ExitCode::SUCCESS
}

/// The main window: a plain title bar saying "Isocast — <setup>" (as OBS
/// names its window), in the interface's colour, over the web view.
fn main_window(mtm: MainThreadMarker, delegate: &AppDelegate) -> Retained<NSWindow> {
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable;
    let frame = NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(DEFAULT_SIZE.0, DEFAULT_SIZE.1),
    );
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(ns_string!("Isocast"));
    window.setTitleVisibility(NSWindowTitleVisibility::Visible);
    window.setTitlebarAppearsTransparent(true);
    window.setTabbingMode(NSWindowTabbingMode::Disallowed);
    window.setContentMinSize(NSSize::new(MIN_SIZE.0, MIN_SIZE.1));
    window.setBackgroundColor(Some(&background_color()));
    window.setDelegate(Some(ProtocolObject::from_ref(delegate)));
    // Remember where the window was; the first time, centre it, no larger
    // than the screen.
    let name = ns_string!("Isocast Main Window");
    if !window.setFrameUsingName(name) {
        if let Some(screen) = window.screen() {
            let visible = screen.visibleFrame();
            let size = NSSize::new(
                DEFAULT_SIZE.0.min(visible.size.width),
                DEFAULT_SIZE.1.min(visible.size.height),
            );
            window.setContentSize(size);
        }
        window.center();
    }
    window.setFrameAutosaveName(name);
    window
}

pub(crate) fn background_color() -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(BACKGROUND.0, BACKGROUND.1, BACKGROUND.2, 1.0)
}

/// Menu commands the interface handles, by tag (the names the interface's
/// `MENU` table in ui/ui/app.js knows).
const MENU_ACTIONS: &[&str] = &[
    "settings",
    "hotkeys",
    "studio_mode",
    "new_scene",
    "add_source",
    "show_recordings",
    "start_over",
    "undo",
    "redo",
    "paste_duplicate",
    "transform_edit",
    "transform_reset",
    "rotate_cw",
    "rotate_ccw",
    "rotate_180",
    "fit_screen",
    "stretch_screen",
    "center_screen",
    "center_vertical",
    "center_horizontal",
    "order_up",
    "order_down",
    "order_top",
    "order_bottom",
    "filters",
    "properties",
    "lock_preview",
    "advanced_audio",
    "source_toolbar",
    "status_bar",
    "reset_ui",
    "lock_docks",
    "reset_docks",
    "dock_scenes",
    "dock_sources",
    "dock_mixer",
    "dock_transitions",
    "dock_controls",
];

/// The menu bar, after OBS's where Isocast has the function: the app menu,
/// File, Edit, View, Docks, Profile, Scene Collection, Tools, Window and
/// Help. Profile and Scene Collection list what the library holds, so the
/// interface fills them (`menu_items`).
fn menu(mtm: MainThreadMarker, delegate: &AppDelegate) -> Retained<NSMenu> {
    let item = |title: &str, action: Option<Sel>, key: &str| unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            action,
            &NSString::from_str(key),
        )
    };
    let with_modifiers = |item: Retained<NSMenuItem>, flags: NSEventModifierFlags| {
        item.setKeyEquivalentModifierMask(flags);
        item
    };
    // Items the interface handles: sent to the delegate, which passes the
    // action name on.
    let ui_item = |title: &str, name: &str, key: &str| {
        let item = item(title, Some(sel!(menuAction:)), key);
        unsafe { item.setTarget(Some(delegate)) };
        let tag = MENU_ACTIONS.iter().position(|a| *a == name).unwrap_or(0);
        item.setTag(tag as isize);
        item
    };
    let submenu = |title: &str, items: Vec<Retained<NSMenuItem>>| {
        let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(title));
        for item in &items {
            menu.addItem(item);
        }
        let holder = item(title, None, "");
        holder.setSubmenu(Some(&menu));
        (holder, menu)
    };
    let separator = || NSMenuItem::separatorItem(mtm);
    let command = NSEventModifierFlags::Command;
    let option = NSEventModifierFlags::Option;
    let shift = NSEventModifierFlags::Shift;
    let control = NSEventModifierFlags::Control;

    let (app_menu, _) = submenu(
        "Isocast",
        vec![
            item(
                "About Isocast",
                Some(sel!(orderFrontStandardAboutPanel:)),
                "",
            ),
            separator(),
            ui_item("Settings…", "settings", ","),
            separator(),
            item("Hide Isocast", Some(sel!(hide:)), "h"),
            with_modifiers(
                item("Hide Others", Some(sel!(hideOtherApplications:)), "h"),
                command | option,
            ),
            item("Show All", Some(sel!(unhideAllApplications:)), ""),
            separator(),
            item("Quit Isocast", Some(sel!(terminate:)), "q"),
        ],
    );
    let (file_menu, _) = submenu(
        "File",
        vec![
            ui_item("Show Recordings", "show_recordings", ""),
            separator(),
            with_modifiers(ui_item("Add Scene", "new_scene", "n"), command | shift),
            with_modifiers(ui_item("Add Source…", "add_source", "a"), command | shift),
            separator(),
            ui_item("Settings", "settings", ""),
            separator(),
            item("Close Window", Some(sel!(performClose:)), "w"),
        ],
    );
    let (transform, _) = submenu(
        "Transform",
        vec![
            ui_item("Edit Transform…", "transform_edit", "e"),
            ui_item("Reset Transform", "transform_reset", "r"),
            separator(),
            ui_item("Rotate 90° Clockwise", "rotate_cw", ""),
            ui_item("Rotate 90° Counterclockwise", "rotate_ccw", ""),
            ui_item("Rotate 180°", "rotate_180", ""),
            separator(),
            ui_item("Fit to Screen", "fit_screen", "f"),
            ui_item("Stretch to Screen", "stretch_screen", "s"),
            ui_item("Center to Screen", "center_screen", "d"),
            ui_item("Center Vertically", "center_vertical", ""),
            ui_item("Center Horizontally", "center_horizontal", ""),
        ],
    );
    let (order, _) = submenu(
        "Order",
        vec![
            ui_item("Move Up", "order_up", ""),
            ui_item("Move Down", "order_down", ""),
            separator(),
            ui_item("Move to Top", "order_top", ""),
            ui_item("Move to Bottom", "order_bottom", ""),
        ],
    );
    // Undo and Redo undo setup changes, or a text field's typing while one
    // is being edited; Cut, Copy and Paste are the text's own (⌘C and ⌘V
    // on a selected source copy and paste it).
    let (edit_menu, _) = submenu(
        "Edit",
        vec![
            ui_item("Undo", "undo", "z"),
            with_modifiers(ui_item("Redo", "redo", "z"), command | shift),
            separator(),
            item("Cut", Some(sel!(cut:)), "x"),
            item("Copy", Some(sel!(copy:)), "c"),
            item("Paste", Some(sel!(paste:)), "v"),
            ui_item("Paste (Duplicate)", "paste_duplicate", ""),
            item("Select All", Some(sel!(selectAll:)), "a"),
            separator(),
            transform,
            order,
            separator(),
            ui_item("Filters…", "filters", ""),
            ui_item("Properties…", "properties", ""),
            separator(),
            ui_item("Lock Preview", "lock_preview", ""),
            separator(),
            ui_item("Advanced Audio Properties", "advanced_audio", ""),
        ],
    );
    let (view_menu, _) = submenu(
        "View",
        vec![
            with_modifiers(ui_item("Studio Mode", "studio_mode", "s"), command | shift),
            separator(),
            ui_item("Source Toolbar", "source_toolbar", ""),
            ui_item("Status Bar", "status_bar", ""),
            separator(),
            ui_item("Reset UI", "reset_ui", ""),
            separator(),
            with_modifiers(
                item("Enter Full Screen", Some(sel!(toggleFullScreen:)), "f"),
                command | control,
            ),
        ],
    );
    let (docks_menu, _) = submenu(
        "Docks",
        vec![
            ui_item("Lock Docks", "lock_docks", ""),
            ui_item("Reset Docks", "reset_docks", ""),
            separator(),
            ui_item("Scenes", "dock_scenes", ""),
            ui_item("Sources", "dock_sources", ""),
            ui_item("Audio Mixer", "dock_mixer", ""),
            ui_item("Scene Transitions", "dock_transitions", ""),
            ui_item("Controls", "dock_controls", ""),
        ],
    );
    // Filled by the interface: their items change with the library.
    let filled = |title: &str| {
        let (holder, menu) = submenu(title, Vec::new());
        menu.setAutoenablesItems(false);
        DYNAMIC.with(|d| d.borrow_mut().menus.push((title.to_string(), menu)));
        holder
    };
    let profile_menu = filled("Profile");
    let collection_menu = filled("Scene Collection");
    DYNAMIC.with(|d| d.borrow_mut().delegate = Some(delegate.retain()));
    let (tools_menu, _) = submenu(
        "Tools",
        vec![ui_item("Auto-Configuration Wizard…", "start_over", "")],
    );
    let (window_item, window_menu) = submenu(
        "Window",
        vec![
            item("Minimize", Some(sel!(performMiniaturize:)), "m"),
            item("Zoom", Some(sel!(performZoom:)), ""),
            separator(),
            item("Bring All to Front", Some(sel!(arrangeInFront:)), ""),
        ],
    );
    NSApplication::sharedApplication(mtm).setWindowsMenu(Some(&window_menu));
    let (help_item, help_menu) =
        submenu("Help", vec![ui_item("Keyboard Shortcuts", "hotkeys", "/")]);
    NSApplication::sharedApplication(mtm).setHelpMenu(Some(&help_menu));

    let bar = NSMenu::new(mtm);
    for item in [
        app_menu,
        file_menu,
        edit_menu,
        view_menu,
        docks_menu,
        profile_menu,
        collection_menu,
        tools_menu,
        window_item,
        help_item,
    ] {
        bar.addItem(&item);
    }
    bar
}

/// Check or uncheck the interface's menu items (`checked`) and retitle
/// them (`titles`), both by action name, through every menu.
pub(crate) fn set_menu_state(mtm: MainThreadMarker, checked: &Value, titles: &Value) {
    fn walk(menu: &NSMenu, checked: &Value, titles: &Value) {
        for item in menu.itemArray().iter() {
            if let Some(submenu) = item.submenu() {
                walk(&submenu, checked, titles);
            }
            if item.action() != Some(sel!(menuAction:)) {
                continue;
            }
            let tag = usize::try_from(item.tag()).unwrap_or(0);
            let Some(name) = MENU_ACTIONS.get(tag) else {
                continue;
            };
            if let Some(on) = checked.get(*name).and_then(Value::as_bool) {
                item.setState(if on {
                    NSControlStateValueOn
                } else {
                    NSControlStateValueOff
                });
            }
            if let Some(title) = titles.get(*name).and_then(Value::as_str) {
                item.setTitle(&NSString::from_str(title));
            }
        }
    }
    if let Some(bar) = NSApplication::sharedApplication(mtm).mainMenu() {
        walk(&bar, checked, titles);
    }
}

/// Menus whose items the interface sets (`menu_items`), by title, and
/// what each of their items does.
#[derive(Default)]
struct Dynamic {
    menus: Vec<(String, Retained<NSMenu>)>,
    /// Each menu's items' {action, id, …} by tag.
    actions: Vec<(String, Vec<Value>)>,
    delegate: Option<Retained<AppDelegate>>,
}

thread_local! {
    static DYNAMIC: RefCell<Dynamic> = RefCell::default();
}

/// Replace the items of menu `title` (Profile or Scene Collection) with
/// `items`: "-" for a separator, or {title, action, id, checked, enabled}.
/// Choosing one sends the interface a `menu` event with its action and id.
/// Nothing happens where there is no menu bar (snapshots).
pub(crate) fn set_menu_items(mtm: MainThreadMarker, title: &str, items: &Value) -> Value {
    DYNAMIC.with(|d| {
        let mut d = d.borrow_mut();
        let Some(menu) = d
            .menus
            .iter()
            .find(|(name, _)| name == title)
            .map(|(_, menu)| menu.clone())
        else {
            return if d.menus.is_empty() {
                json!({"result": null})
            } else {
                let names: Vec<&str> = d.menus.iter().map(|(name, _)| name.as_str()).collect();
                json!({"error": {"code": -32602, "message": format!(
                    "menu: no menu {title:?} takes items; menus: {}", names.join(", ")
                )}})
            };
        };
        let delegate = d.delegate.clone();
        menu.removeAllItems();
        let mut actions = Vec::new();
        for item in items.as_array().into_iter().flatten() {
            if item.as_str() == Some("-") {
                menu.addItem(&NSMenuItem::separatorItem(mtm));
                continue;
            }
            let text = item
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let entry = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &NSString::from_str(text),
                    Some(sel!(dynamicMenuAction:)),
                    ns_string!(""),
                )
            };
            if let Some(delegate) = &delegate {
                unsafe { entry.setTarget(Some(delegate)) };
            }
            entry.setTag(actions.len() as isize);
            entry.setEnabled(item.get("enabled") != Some(&Value::Bool(false)));
            if item.get("checked") == Some(&Value::Bool(true)) {
                entry.setState(NSControlStateValueOn);
            }
            menu.addItem(&entry);
            actions.push(item.clone());
        }
        d.actions.retain(|(name, _)| name != title);
        d.actions.push((title.to_string(), actions));
        json!({"result": null})
    })
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and AppDelegate
    // doesn't implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "IsocastAppDelegate"]
    pub(crate) struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn should_terminate_after_last_window_closed(&self, _app: &NSApplication) -> bool {
            true
        }
    }

    unsafe impl NSWindowDelegate for AppDelegate {
        #[unsafe(method(windowDidEnterFullScreen:))]
        fn window_did_enter_full_screen(&self, _notification: &NSNotification) {
            web::emit(json!({"type": "window", "full_screen": true}));
        }

        #[unsafe(method(windowDidExitFullScreen:))]
        fn window_did_exit_full_screen(&self, _notification: &NSNotification) {
            web::emit(json!({"type": "window", "full_screen": false}));
        }
    }

    impl AppDelegate {
        #[unsafe(method(menuAction:))]
        fn menu_action(&self, sender: &NSMenuItem) {
            let tag = usize::try_from(sender.tag()).unwrap_or(0);
            let action = MENU_ACTIONS.get(tag).copied().unwrap_or_default();
            web::emit(json!({"type": "menu", "action": action}));
        }

        /// An item of a menu the interface filled (`set_menu_items`).
        #[unsafe(method(dynamicMenuAction:))]
        fn dynamic_menu_action(&self, sender: &NSMenuItem) {
            // SAFETY: the item's menu, read on the main thread while the
            // item is being chosen from it.
            let title = unsafe { sender.menu() }.map(|menu| menu.title().to_string());
            let tag = usize::try_from(sender.tag()).unwrap_or(0);
            let item = DYNAMIC.with(|d| {
                let d = d.borrow();
                d.actions
                    .iter()
                    .find(|(name, _)| Some(name) == title.as_ref())
                    .and_then(|(_, items)| items.get(tag).cloned())
            });
            if let Some(item) = item {
                web::emit(json!({"type": "menu", "action": item["action"], "id": item["id"]}));
            }
        }
    }
);

impl AppDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

/// `value` as a JSON string, for the interface.
pub(crate) fn reply_text(value: &Value) -> Retained<NSString> {
    NSString::from_str(&value.to_string())
}

/// A raw object pointer for a block argument.
pub(crate) fn object_ptr<T: objc2::Message>(object: &T) -> *mut AnyObject {
    (object as *const T).cast_mut().cast()
}
