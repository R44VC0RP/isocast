//! The web interface's files: built into the binary, or read from
//! `$ISOCAST_UI_DIR` while working on them.

use std::borrow::Cow;
use std::path::{Component, Path};

include!(concat!(env!("OUT_DIR"), "/assets.rs"));

/// The file at `path` (relative, like `index.html` or `ui/monitor.js`) and
/// its MIME type.
pub fn get(path: &str) -> Option<(Cow<'static, [u8]>, &'static str)> {
    let path = path.trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    let bytes = match std::env::var_os("ISOCAST_UI_DIR") {
        Some(folder) => {
            let relative = Path::new(path);
            // Only plain relative paths inside the folder.
            if !relative
                .components()
                .all(|c| matches!(c, Component::Normal(_)))
            {
                return None;
            }
            Cow::Owned(std::fs::read(Path::new(&folder).join(relative)).ok()?)
        }
        None => Cow::Borrowed(ASSETS.iter().find(|(name, _)| *name == path)?.1),
    };
    Some((bytes, mime(path)))
}

fn mime(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}
