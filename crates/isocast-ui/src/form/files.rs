//! Choosing files with the system's open panel (the web app's
//! `chooseFiles`, which asks the native shell): a source's image, media
//! files, a text file, a stinger's video, through `platform::choose`.
//! Never called offscreen (a panel would appear on the screen).

/// Image files a source can show.
pub const IMAGE_TYPES: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "tga", "svg"];
/// Media files a media source plays.
pub const MEDIA_TYPES: &[&str] = &[
    "mov", "mp4", "m4v", "m4a", "mp3", "wav", "aif", "aiff", "caf", "mkv", "webm",
];

/// The kinds of file a setting takes, by the source kind and field name.
pub fn file_types(kind: &str, name: &str) -> &'static [&'static str] {
    if kind == "image" || kind == "slideshow" || name == "image" || name == "path" {
        return IMAGE_TYPES;
    }
    if kind == "media" || kind == "stinger" {
        return MEDIA_TYPES;
    }
    if name == "file" && kind == "text" {
        return &["txt", "md"];
    }
    if name == "file" && kind == "browser" {
        return &["html", "htm"];
    }
    &[]
}

/// Ask for files (several with `multiple`) with the system's open panel
/// (`platform::choose`); their paths, none if cancelled.
pub fn choose(types: &[&str], multiple: bool, prompt: &str) -> Vec<String> {
    crate::platform::choose(crate::platform::Choose {
        folders: false,
        multiple,
        prompt,
        types,
    })
    .unwrap_or_default()
}
