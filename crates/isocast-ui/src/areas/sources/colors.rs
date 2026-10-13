//! Choosing a colour with the system's colour panel, as the web app's
//! colour fields do (their swatch opens it): macOS's shared NSColorPanel
//! through the Objective-C runtime (no new dependency). The panel keeps no
//! target here; the caller reads its colour while it is open (`current`).
//! Other platforms have none yet. Never called offscreen.

/// Show the panel with `hex` (#rgb, #rrggbb, #rrggbbaa) in it.
#[cfg(target_os = "macos")]
pub fn open(hex: &str) {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject};

    let [r, g, b, a] = rgba(hex);
    // SAFETY: AppKit calls on the main thread (Slint callbacks run there)
    // on the shared panel; nil is checked before use.
    unsafe {
        let (Some(panel_class), Some(color_class)) =
            (AnyClass::get(c"NSColorPanel"), AnyClass::get(c"NSColor"))
        else {
            return;
        };
        let panel: *mut AnyObject = msg_send![panel_class, sharedColorPanel];
        if panel.is_null() {
            return;
        }
        let color: *mut AnyObject = msg_send![
            color_class,
            colorWithSRGBRed: r,
            green: g,
            blue: b,
            alpha: a
        ];
        let _: () = msg_send![panel, setShowsAlpha: hex.trim_start_matches('#').len() == 8];
        if !color.is_null() {
            let _: () = msg_send![panel, setColor: color];
        }
        let nil: *mut AnyObject = std::ptr::null_mut();
        let _: () = msg_send![panel, orderFront: nil];
    }
}

/// The panel's colour as #rrggbb (#rrggbbaa when it shows alpha), while it
/// is open.
#[cfg(target_os = "macos")]
pub fn current() -> Option<String> {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject};

    // SAFETY: as in `open`; the colour is converted to sRGB before its
    // components are read (they are only defined for RGB colours).
    unsafe {
        let panel_class = AnyClass::get(c"NSColorPanel")?;
        let space_class = AnyClass::get(c"NSColorSpace")?;
        let shared: bool = msg_send![panel_class, sharedColorPanelExists];
        if !shared {
            return None;
        }
        let panel: *mut AnyObject = msg_send![panel_class, sharedColorPanel];
        let visible: bool = msg_send![panel, isVisible];
        if !visible {
            return None;
        }
        let color: *mut AnyObject = msg_send![panel, color];
        let srgb: *mut AnyObject = msg_send![space_class, sRGBColorSpace];
        if color.is_null() || srgb.is_null() {
            return None;
        }
        let rgb: *mut AnyObject = msg_send![color, colorUsingColorSpace: srgb];
        if rgb.is_null() {
            return None;
        }
        let r: f64 = msg_send![rgb, redComponent];
        let g: f64 = msg_send![rgb, greenComponent];
        let b: f64 = msg_send![rgb, blueComponent];
        let a: f64 = msg_send![rgb, alphaComponent];
        let alpha: bool = msg_send![panel, showsAlpha];
        let byte = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        Some(if alpha {
            format!(
                "#{:02x}{:02x}{:02x}{:02x}",
                byte(r),
                byte(g),
                byte(b),
                byte(a)
            )
        } else {
            format!("#{:02x}{:02x}{:02x}", byte(r), byte(g), byte(b))
        })
    }
}

#[cfg(not(target_os = "macos"))]
pub fn open(_hex: &str) {}

#[cfg(not(target_os = "macos"))]
pub fn current() -> Option<String> {
    None
}

/// A hex colour's components, 0 to 1 (black if it isn't one).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn rgba(hex: &str) -> [f64; 4] {
    let c = super::form::swatch(hex);
    [
        c.red() as f64 / 255.0,
        c.green() as f64 / 255.0,
        c.blue() as f64 / 255.0,
        if super::form::valid_color(hex) {
            c.alpha() as f64 / 255.0
        } else {
            1.0
        },
    ]
}
