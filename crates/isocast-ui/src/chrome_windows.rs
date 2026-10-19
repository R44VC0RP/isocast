//! Windows' parts of the app's windows: the title bar, window border and
//! menu bar drawn in the Console theme's colours instead of Windows' white
//! ones, the app icon, and a first window that fits its screen.
//!
//! - The title bar and border: winit's dark theme (asked for when each
//!   window is created, so winit keeps it when Windows' own setting
//!   changes), then DWM's caption, border and text colours from the theme's
//!   tokens (Windows 11; Windows 10 keeps its dark grey).
//! - The menu bar is Windows' own (Slint draws it through muda). Its bar
//!   and titles are drawn here, in a window subclass, with the theme's
//!   colours (the undocumented `WM_UAHDRAWMENU*` messages every dark
//!   Win32 app uses, as muda's own dark menu bar does), and the menus that
//!   drop from it use Windows' dark menus.
//! - Console is the only theme, and it is dark: with a light theme
//!   (`DARK` false) windows keep Windows' own chrome.

use std::sync::OnceLock;

use slint::ComponentHandle;
use slint::winit_030::winit;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{
    DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE,
    DwmSetWindowAttribute,
};
use windows_sys::Win32::Graphics::Gdi::{
    CreateSolidBrush, DT_CENTER, DT_HIDEPREFIX, DT_SINGLELINE, DT_VCENTER, DeleteObject, DrawTextW,
    FillRect, GetMonitorInfoW, GetWindowDC, HDC, HMONITOR, MONITOR_DEFAULTTOPRIMARY, MONITORINFO,
    MapWindowPoints, MonitorFromPoint, OffsetRect, RDW_FRAME, RDW_INVALIDATE, RedrawWindow,
    ReleaseDC, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Threading::{GetCurrentThreadId, GetStartupInfoW, STARTUPINFOW};
use windows_sys::Win32::UI::Controls::{
    DRAWITEMSTRUCT, ODS_DISABLED, ODS_GRAYED, ODS_HOTLIGHT, ODS_NOACCEL, ODS_SELECTED,
};
use windows_sys::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, GetDpiForMonitor, MDT_EFFECTIVE_DPI,
};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DrawMenuBar, EnumThreadWindows, GWL_STYLE, GetClientRect, GetMenuBarInfo, GetMenuItemInfoW,
    GetWindowLongPtrW, GetWindowRect, HMENU, IsWindowVisible, MENUBARINFO, MENUITEMINFOW,
    MIIM_STRING, OBJID_MENU, WM_NCACTIVATE, WM_NCDESTROY, WM_NCPAINT, WM_SETTINGCHANGE,
    WM_THEMECHANGED, WS_CAPTION, WS_OVERLAPPEDWINDOW,
};

use crate::{AppWindow, Theme};

/// Console, the only theme, is dark.
const DARK: bool = true;
/// The app icon's resource ordinal (build.rs `ICON_ORDINAL`).
const ICON: u16 = 1;
/// This module's window subclass.
const SUBCLASS: usize = 0x0150_ca57;

/// `hStdOutput` is the monitor the shell started the app from.
const STARTF_USEMONITOR: u32 = 0x400;

const WM_UAHDRAWMENU: u32 = 0x0091;
const WM_UAHDRAWMENUITEM: u32 = 0x0092;

/// The chrome's colours (COLORREF, 0x00BBGGRR), from the theme's tokens.
#[derive(Clone, Copy)]
struct Colors {
    /// Title bar and menu bar: `surface-chrome`.
    chrome: u32,
    /// The window's 1px edge: `line-strong`.
    border: u32,
    /// Titles: `text`; a disabled menu: `text-disabled`.
    text: u32,
    text_disabled: u32,
    /// A menu title under the pointer or open: `overlay-hover`.
    hot: u32,
    /// Between the menu bar and the window's content: `line`.
    line: u32,
}

static COLORS: OnceLock<Colors> = OnceLock::new();

/// Every window the app makes: winit's dark theme (title bar, scroll bars)
/// and the app icon (title bar, taskbar, Alt-Tab), and test runs never
/// take focus (`background`, see background.rs).
pub fn configure(selector: slint::BackendSelector, background: bool) -> slint::BackendSelector {
    use winit::platform::windows::{IconExtWindows, WindowAttributesExtWindows};
    if DARK {
        dark_menus();
    }
    selector.with_winit_window_attributes_hook(move |mut attributes| {
        if background {
            attributes = attributes.with_active(false);
        }
        if DARK {
            attributes = attributes.with_theme(Some(winit::window::Theme::Dark));
        }
        if let Ok(icon) = winit::window::Icon::from_resource(ICON, None) {
            attributes = attributes
                .with_window_icon(Some(icon.clone()))
                .with_taskbar_icon(Some(icon));
        }
        attributes
    })
}

/// The theme's colours for the chrome (once, from the first window).
pub fn take_colors(app: &AppWindow) {
    let theme = app.global::<Theme>();
    let rgb = |color: slint::Color| {
        u32::from(color.red()) | u32::from(color.green()) << 8 | u32::from(color.blue()) << 16
    };
    let _ = COLORS.set(Colors {
        chrome: rgb(theme.get_surface_chrome()),
        border: rgb(theme.get_line_strong()),
        text: rgb(theme.get_text()),
        text_disabled: rgb(theme.get_text_disabled()),
        hot: rgb(theme.get_overlay_hover()),
        line: rgb(theme.get_line()),
    });
}

/// Colour this thread's windows that have a title bar and aren't yet: call
/// once a window has been shown (it exists from the next turn of the event
/// loop).
pub fn style_windows_soon() {
    if !DARK {
        return;
    }
    // After the window (and its menu bar) exists: a turn or two of the
    // event loop.
    slint::Timer::single_shot(std::time::Duration::from_millis(100), || {
        unsafe extern "system" fn each(hwnd: HWND, _: LPARAM) -> i32 {
            // SAFETY: plain queries of a window of this thread.
            let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32;
            if unsafe { IsWindowVisible(hwnd) } != 0 && style & WS_CAPTION == WS_CAPTION {
                style_window(hwnd);
            }
            1
        }
        // SAFETY: the callback only touches the window it is given.
        unsafe { EnumThreadWindows(GetCurrentThreadId(), Some(each), 0) };
    });
}

fn style_window(hwnd: HWND) {
    let Some(colors) = COLORS.get() else { return };
    apply_dwm(hwnd, colors);
    // SAFETY: a window of this thread; the subclass removes itself when the
    // window is destroyed. Removed and installed again so it is the first
    // to see the menu bar's messages, even after muda made its menu again
    // (the menu bar comes back after full screen).
    unsafe {
        RemoveWindowSubclass(hwnd, Some(subclass), SUBCLASS);
        SetWindowSubclass(hwnd, Some(subclass), SUBCLASS, 0);
        DrawMenuBar(hwnd);
        RedrawWindow(
            hwnd,
            std::ptr::null(),
            std::ptr::null_mut(),
            RDW_FRAME | RDW_INVALIDATE,
        );
    }
}

fn apply_dwm(hwnd: HWND, colors: &Colors) {
    let set = |attribute: i32, value: u32| {
        // SAFETY: a 4-byte value for each of these attributes. Windows 10
        // refuses the colour attributes; its dark title bar stays.
        unsafe {
            DwmSetWindowAttribute(
                hwnd,
                attribute as u32,
                (&raw const value).cast(),
                size_of::<u32>() as u32,
            )
        };
    };
    set(DWMWA_USE_IMMERSIVE_DARK_MODE, 1);
    set(DWMWA_CAPTION_COLOR, colors.chrome);
    set(DWMWA_BORDER_COLOR, colors.border);
    set(DWMWA_TEXT_COLOR, colors.text);
}

/// Windows' dark menus for the menus that drop from the menu bar (uxtheme's
/// undocumented `SetPreferredAppMode(ForceDark)`, ordinal 135, and
/// `FlushMenuThemes`, 136; Windows 10 1809's ordinal 135 takes a BOOL,
/// which 2 also turns on). Nothing happens where they're missing.
fn dark_menus() {
    let name: Vec<u16> = "uxtheme.dll".encode_utf16().chain(Some(0)).collect();
    // SAFETY: a system library, loaded for the process's life; the ordinals'
    // signatures are as above (used this way by many Win32 apps).
    unsafe {
        let module = LoadLibraryW(name.as_ptr());
        if module.is_null() {
            return;
        }
        if let Some(set_mode) = GetProcAddress(module, 135 as *const u8) {
            let set_mode: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(set_mode);
            set_mode(2);
        }
        if let Some(flush) = GetProcAddress(module, 136 as *const u8) {
            let flush: unsafe extern "system" fn() = std::mem::transmute(flush);
            flush();
        }
    }
}

#[repr(C)]
struct UahMenu {
    menu: HMENU,
    hdc: HDC,
    flags: u32,
}

#[repr(C)]
struct UahMenuItem {
    position: u32,
    metrics: [u32; 12],
    popup: [u32; 5],
}

#[repr(C)]
struct UahDrawMenuItem {
    item: DRAWITEMSTRUCT,
    menu: UahMenu,
    entry: UahMenuItem,
}

unsafe extern "system" fn subclass(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _: usize,
    _: usize,
) -> LRESULT {
    let Some(colors) = COLORS.get() else {
        return unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
    };
    match message {
        WM_UAHDRAWMENU => {
            // SAFETY: Windows passes a UAHMENU for this message.
            unsafe { paint_bar(hwnd, &*(lparam as *const UahMenu), colors) };
            0
        }
        WM_UAHDRAWMENUITEM => {
            // SAFETY: Windows passes a UAHDRAWMENUITEM for this message.
            unsafe { paint_title(&mut *(lparam as *mut UahDrawMenuItem), colors) };
            0
        }
        WM_NCPAINT | WM_NCACTIVATE => {
            let result = unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
            paint_edge(hwnd, colors);
            result
        }
        WM_SETTINGCHANGE | WM_THEMECHANGED => {
            let result = unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
            apply_dwm(hwnd, colors);
            result
        }
        WM_NCDESTROY => unsafe {
            RemoveWindowSubclass(hwnd, Some(subclass), SUBCLASS);
            DefSubclassProc(hwnd, message, wparam, lparam)
        },
        _ => unsafe { DefSubclassProc(hwnd, message, wparam, lparam) },
    }
}

fn fill(hdc: HDC, rect: &RECT, color: u32) {
    // SAFETY: a brush made and freed here, on a device context Windows gave.
    unsafe {
        let brush = CreateSolidBrush(color);
        FillRect(hdc, rect, brush);
        DeleteObject(brush);
    }
}

/// The menu bar's rectangle in window coordinates.
fn bar_rect(hwnd: HWND) -> Option<RECT> {
    let mut info = MENUBARINFO {
        cbSize: size_of::<MENUBARINFO>() as u32,
        ..unsafe { std::mem::zeroed() }
    };
    let mut window: RECT = unsafe { std::mem::zeroed() };
    // SAFETY: plain queries into owned structures.
    unsafe {
        if GetMenuBarInfo(hwnd, OBJID_MENU, 0, &mut info) == 0 {
            return None;
        }
        GetWindowRect(hwnd, &mut window);
        OffsetRect(&mut info.rcBar, -window.left, -window.top);
    }
    Some(info.rcBar)
}

fn paint_bar(hwnd: HWND, menu: &UahMenu, colors: &Colors) {
    if let Some(mut rect) = bar_rect(hwnd) {
        rect.top -= 1;
        fill(menu.hdc, &rect, colors.chrome);
    }
}

fn paint_title(item: &mut UahDrawMenuItem, colors: &Colors) {
    let mut label = [0u16; 256];
    let mut info = MENUITEMINFOW {
        cbSize: size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_STRING,
        dwTypeData: label.as_mut_ptr(),
        cch: label.len() as u32 - 1,
        ..unsafe { std::mem::zeroed() }
    };
    // SAFETY: the buffer holds `cch` characters and a NUL.
    if unsafe { GetMenuItemInfoW(item.menu.menu, item.entry.position, 1, &mut info) } == 0 {
        return;
    }
    let state = item.item.itemState;
    let hot = state & (ODS_HOTLIGHT | ODS_SELECTED) != 0;
    let disabled = state & (ODS_GRAYED | ODS_DISABLED) != 0;
    fill(
        item.menu.hdc,
        &item.item.rcItem,
        if hot && !disabled {
            colors.hot
        } else {
            colors.chrome
        },
    );
    let mut flags = DT_CENTER | DT_SINGLELINE | DT_VCENTER;
    if state & ODS_NOACCEL != 0 {
        flags |= DT_HIDEPREFIX;
    }
    // SAFETY: drawing the item's own label into the item's rectangle.
    unsafe {
        SetBkMode(item.menu.hdc, TRANSPARENT as i32);
        SetTextColor(
            item.menu.hdc,
            if disabled {
                colors.text_disabled
            } else {
                colors.text
            },
        );
        DrawTextW(
            item.menu.hdc,
            label.as_ptr(),
            info.cch as i32,
            &mut item.item.rcItem,
            flags,
        );
    }
}

/// The 1px line Windows draws under a menu bar, in the theme's colour.
fn paint_edge(hwnd: HWND, colors: &Colors) {
    if bar_rect(hwnd).is_none() {
        return;
    }
    let mut client: RECT = unsafe { std::mem::zeroed() };
    let mut window: RECT = unsafe { std::mem::zeroed() };
    // SAFETY: plain queries; the window's device context is released.
    unsafe {
        GetClientRect(hwnd, &mut client);
        MapWindowPoints(
            hwnd,
            std::ptr::null_mut(),
            (&raw mut client).cast::<POINT>(),
            2,
        );
        GetWindowRect(hwnd, &mut window);
        OffsetRect(&mut client, -window.left, -window.top);
        let line = RECT {
            bottom: client.top,
            top: client.top - 1,
            ..client
        };
        let hdc = GetWindowDC(hwnd);
        fill(hdc, &line, colors.line);
        ReleaseDC(hwnd, hdc);
    }
}

/// Size and place the first window, before it is shown, so all of it is
/// on its screen: its preferred client size (`preferred`, logical), or less
/// where the screen's work area (without the taskbar) is smaller at its
/// scale, centred in that work area. The screen is the one Windows starts
/// the app on: the shell's (Start menu's) if it says, else the primary.
pub fn fit_first_window(window: &slint::Window, preferred: (f32, f32)) {
    // SAFETY: plain queries into owned structures.
    unsafe {
        let mut startup: STARTUPINFOW = std::mem::zeroed();
        startup.cb = size_of::<STARTUPINFOW>() as u32;
        GetStartupInfoW(&mut startup);
        let monitor: HMONITOR = if startup.dwFlags & STARTF_USEMONITOR != 0 {
            startup.hStdOutput as HMONITOR
        } else {
            MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY)
        };
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..std::mem::zeroed()
        };
        if GetMonitorInfoW(monitor, &mut info) == 0 {
            return;
        }
        let (mut dpi_x, mut dpi_y) = (96, 96);
        GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
        // The title bar, the menu bar and the borders at that scale.
        let mut frame = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        AdjustWindowRectExForDpi(&mut frame, WS_OVERLAPPEDWINDOW, 1, 0, dpi_x);
        let scale = dpi_x as f32 / 96.0;
        let work = info.rcWork;
        let (frame_w, frame_h) = (frame.right - frame.left, frame.bottom - frame.top);
        let (work_w, work_h) = (work.right - work.left, work.bottom - work.top);
        let width = ((preferred.0 * scale).round() as i32).min(work_w - frame_w);
        let height = ((preferred.1 * scale).round() as i32).min(work_h - frame_h);
        if width <= 0 || height <= 0 {
            return;
        }
        window.set_size(slint::PhysicalSize::new(width as u32, height as u32));
        window.set_position(slint::PhysicalPosition::new(
            work.left + (work_w - width - frame_w) / 2,
            work.top + (work_h - height - frame_h) / 2,
        ));
    }
}
