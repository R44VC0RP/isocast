//! `isocast-browser`: renders one web page for an Isocast browser source on
//! Linux, in a process of its own that isocastd starts and watches
//! (crates/isocast-platform-linux/src/browser.rs).
//!
//! The page lives in a WebKitGTK 6 view, in a window on a private GTK
//! Broadway display (`gtk4-broadwayd`, started here): nothing shows on the
//! user's screen, and it needs no display server or GPU. At the source's
//! frame rate the view is snapshotted with a transparent background, and
//! each picture goes to stdout as straight-alpha BGRA. With `--audio`, a
//! script routes the page's sound into one capture context whose samples
//! come out the same way (the macOS browser source's script).
//!
//! Protocol (isocastd's `browser::protocol` reads it): records of a 4-byte
//! tag, a little-endian u32 payload length and the payload:
//! - `REDY` (empty): the page is loading;
//! - `FRAM`: u32 width, u32 height, then width × height × 4 bytes BGRA;
//! - `AUDI`: u32 sample rate, then interleaved stereo f32 samples;
//! - `FAIL`: a UTF-8 reason, before exiting.
//!
//! Lines on stdin: `reload`, `unload`, `load`. stdin closing ends it.

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
fn main() {
    std::process::exit(linux::main());
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("isocast-browser: browser sources use this helper on Linux only");
    std::process::exit(2);
}
