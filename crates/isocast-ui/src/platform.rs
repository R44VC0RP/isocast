//! The system's own settings and panels, put on the window: on macOS,
//! Reduce motion (menus, dialogs and toasts then appear without
//! animating), the open and save panels (choosing files and folders, and
//! where to write one, which Slint has no widget for) and showing a folder
//! in the Finder. These are the web app's native commands `open_file`,
//! `save_file` and `reveal`.

use slint::ComponentHandle;

use crate::{AppWindow, Theme};

pub fn apply_system_settings(window: &AppWindow) {
    window.global::<Theme>().set_reduce_motion(reduce_motion());
    dark_appearance();
}

/// The app's system parts (open and save panels, the menu bar's menus) in
/// the dark appearance, as the window is drawn, whatever the Mac's
/// setting (the web app did the same).
#[cfg(target_os = "macos")]
fn dark_appearance() {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};

    // SAFETY: AppKit's shared objects on the main thread; the appearance
    // name is a constant string NSAppearance knows.
    unsafe {
        let name = ns_string("NSAppearanceNameDarkAqua");
        let appearance: Option<Retained<AnyObject>> =
            msg_send![class!(NSAppearance), appearanceNamed: &*name];
        if let Some(appearance) = appearance {
            let app: Retained<AnyObject> = msg_send![class!(NSApplication), sharedApplication];
            let _: () = msg_send![&*app, setAppearance: &*appearance];
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn dark_appearance() {}

#[cfg(target_os = "macos")]
fn reduce_motion() -> bool {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};

    // SAFETY: NSWorkspace is AppKit's shared object; the property is a
    // plain BOOL read on the main thread.
    unsafe {
        let workspace: Retained<AnyObject> = msg_send![class!(NSWorkspace), sharedWorkspace];
        msg_send![&*workspace, accessibilityDisplayShouldReduceMotion]
    }
}

#[cfg(not(target_os = "macos"))]
fn reduce_motion() -> bool {
    false
}

/// What to ask for in the open panel.
#[derive(Debug, Clone, Copy, Default)]
#[cfg_attr(
    windows,
    expect(dead_code, reason = "Windows has no native file panel yet")
)]
pub struct Choose<'a> {
    /// A folder instead of files.
    pub folders: bool,
    pub multiple: bool,
    /// The panel's button ("Choose", "Open", "Import").
    pub prompt: &'a str,
    /// File extensions to allow (all if empty).
    pub types: &'a [&'a str],
}

/// `path` with a leading `~` made this user's home folder.
#[cfg(not(windows))]
pub fn expand_home(path: &str) -> String {
    match (path.strip_prefix('~'), std::env::var("HOME")) {
        (Some(rest), Ok(home)) if rest.is_empty() || rest.starts_with('/') => {
            format!("{home}{rest}")
        }
        _ => path.to_string(),
    }
}

/// `path` with this user's home folder written `~` (as the setup and the
/// Files list show paths).
pub fn abbreviate_home(path: &str) -> String {
    // The home folder as this OS has it (the user profile on Windows,
    // where HOME is unset), with its own separator after it.
    let home = isocast_protocol::api::home_folder();
    let home = home.to_string_lossy();
    let separator = |rest: &str| rest.starts_with('/') || (cfg!(windows) && rest.starts_with('\\'));
    match path.strip_prefix(home.as_ref()) {
        _ if home.is_empty() || home == "/" => path.to_string(),
        Some("") => "~".into(),
        Some(rest) if separator(rest) => format!("~{rest}"),
        _ => path.to_string(),
    }
}

/// Ask for files (or a folder) with the system's open panel: their paths,
/// or none if cancelled. `None` where there is no open panel.
#[cfg(target_os = "macos")]
pub fn choose(options: Choose) -> Option<Vec<String>> {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};

    // SAFETY: AppKit on the main thread (Slint's event loop runs there);
    // every object is retained for as long as it is used, and strings are
    // copied out before their objects go.
    unsafe {
        let panel: Retained<AnyObject> = msg_send![class!(NSOpenPanel), openPanel];
        let _: () = msg_send![&*panel, setCanChooseDirectories: options.folders];
        let _: () = msg_send![&*panel, setCanChooseFiles: !options.folders];
        let _: () = msg_send![&*panel, setAllowsMultipleSelection: options.multiple];
        if !options.prompt.is_empty() {
            let prompt = ns_string(options.prompt);
            let _: () = msg_send![&*panel, setPrompt: &*prompt];
        }
        if !options.types.is_empty() {
            let types: Retained<AnyObject> = msg_send![class!(NSMutableArray), array];
            for extension in options.types {
                let extension = ns_string(extension);
                let _: () = msg_send![&*types, addObject: &*extension];
            }
            let _: () = msg_send![&*panel, setAllowedFileTypes: &*types];
        }
        // NSModalResponseOK.
        let response: isize = msg_send![&*panel, runModal];
        if response != 1 {
            return Some(Vec::new());
        }
        let urls: Retained<AnyObject> = msg_send![&*panel, URLs];
        let count: usize = msg_send![&*urls, count];
        let mut paths = Vec::with_capacity(count);
        for i in 0..count {
            let url: Retained<AnyObject> = msg_send![&*urls, objectAtIndex: i];
            let path: Option<Retained<AnyObject>> = msg_send![&*url, path];
            if let Some(path) = path {
                paths.push(rust_string(&path));
            }
        }
        Some(paths)
    }
}

#[cfg(not(target_os = "macos"))]
pub fn choose(_options: Choose) -> Option<Vec<String>> {
    None
}

/// Ask for files (or a folder) with the desktop's open panel, through the
/// xdg-desktop-portal FileChooser: their paths (empty if cancelled), or
/// none if there's no portal to ask (no xdg-desktop-portal, or one
/// without a FileChooser). Blocks until the person answers: call it off
/// the window's thread.
#[cfg(target_os = "linux")]
pub fn portal_choose(
    folders: bool,
    multiple: bool,
    title: &str,
    prompt: &str,
    types: &[String],
) -> Option<Vec<String>> {
    use ashpd::desktop::file_chooser::{FileChooserProxy, FileFilter, SelectedFiles};

    // One runtime for every ask: the portal binding keeps its D-Bus
    // connection (and the tasks driving it) from the first, so a runtime
    // per ask left the second waiting for ever.
    static RUNTIME: std::sync::OnceLock<Option<tokio::runtime::Runtime>> =
        std::sync::OnceLock::new();
    let runtime = RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("portal")
            .enable_all()
            .build()
            .inspect_err(|error| {
                isocast_diagnostics::diagnostic!("warn"; "open panel: no runtime for the portal: {error}");
            })
            .ok()
    });
    let runtime = runtime.as_ref()?;
    // A portal that's there but stuck (its backend never starts) would
    // leave the open panel waiting for ever: ask it its version first.
    let probe = runtime.block_on(async {
        tokio::time::timeout(std::time::Duration::from_secs(3), FileChooserProxy::new()).await
    });
    match probe {
        Ok(Ok(proxy)) if proxy.version() > 0 => {}
        Ok(Ok(_)) => {
            isocast_diagnostics::diagnostic!("info"; "open panel: the desktop portal has no file chooser; using the engine's file browser");
            return None;
        }
        Ok(Err(error)) => {
            isocast_diagnostics::diagnostic!("info"; "open panel: no desktop portal ({error}); using the engine's file browser");
            return None;
        }
        Err(_) => {
            isocast_diagnostics::diagnostic!("warn"; "open panel: the desktop portal didn't answer within 3 s; using the engine's file browser");
            return None;
        }
    }
    let result = runtime.block_on(async {
        let mut request = SelectedFiles::open_file()
            .title(title)
            .accept_label(prompt)
            .modal(true)
            .multiple(multiple)
            .directory(folders);
        if !folders && !types.is_empty() {
            // Globs match case as written: both cases of each extension.
            let mut filter = FileFilter::new("Supported files");
            for extension in types {
                filter = filter
                    .glob(&format!("*.{}", extension.to_lowercase()))
                    .glob(&format!("*.{}", extension.to_uppercase()));
            }
            request = request.filter(filter);
        }
        request.send().await?.response()
    });
    match result {
        Ok(files) => Some(
            files
                .uris()
                .iter()
                .filter_map(|uri| file_path(uri.as_str()))
                .collect(),
        ),
        Err(ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)) => Some(Vec::new()),
        Err(error) => {
            isocast_diagnostics::diagnostic!("info"; "open panel: no desktop portal ({error}); using the engine's file browser");
            None
        }
    }
}

/// A `file://` URI's path, percent-decoded (none for another scheme).
#[cfg(any(target_os = "linux", test))]
fn file_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    // An authority (normally empty, or localhost) before the path.
    let path = &rest[rest.find('/')?..];
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match (
            bytes[i],
            bytes.get(i + 1).copied().and_then(hex),
            bytes.get(i + 2).copied().and_then(hex),
        ) {
            (b'%', Some(high), Some(low)) => {
                out.push((high * 16 + low) as u8);
                i += 3;
            }
            (byte, _, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Ask where to save a file with the system's save panel (`name`
/// suggested, `types` its extensions, the first added when none is
/// typed): the path, `Some(None)` if cancelled, `None` where there is no
/// save panel. The panel asks before replacing a file.
#[cfg(target_os = "macos")]
pub fn save(name: &str, types: &[&str], prompt: &str) -> Option<Option<String>> {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};

    // SAFETY: as in `choose`.
    unsafe {
        let panel: Retained<AnyObject> = msg_send![class!(NSSavePanel), savePanel];
        let name = ns_string(name);
        let _: () = msg_send![&*panel, setNameFieldStringValue: &*name];
        if !prompt.is_empty() {
            let prompt = ns_string(prompt);
            let _: () = msg_send![&*panel, setPrompt: &*prompt];
        }
        if !types.is_empty() {
            let list: Retained<AnyObject> = msg_send![class!(NSMutableArray), array];
            for extension in types {
                let extension = ns_string(extension);
                let _: () = msg_send![&*list, addObject: &*extension];
            }
            let _: () = msg_send![&*panel, setAllowedFileTypes: &*list];
        }
        // NSModalResponseOK.
        let response: isize = msg_send![&*panel, runModal];
        if response != 1 {
            return Some(None);
        }
        let url: Option<Retained<AnyObject>> = msg_send![&*panel, URL];
        let Some(url) = url else {
            return Some(None);
        };
        let path: Option<Retained<AnyObject>> = msg_send![&*url, path];
        Some(path.map(|path| rust_string(&path)))
    }
}

#[cfg(not(target_os = "macos"))]
pub fn save(_name: &str, _types: &[&str], _prompt: &str) -> Option<Option<String>> {
    None
}

/// Show `path` (a file or folder, `~` allowed) in the Finder.
#[cfg(target_os = "macos")]
pub fn reveal(path: &str) {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};

    // SAFETY: as in `choose`.
    unsafe {
        let path = ns_string(&expand_home(path));
        let url: Retained<AnyObject> = msg_send![class!(NSURL), fileURLWithPath: &*path];
        let urls: Retained<AnyObject> = msg_send![class!(NSMutableArray), array];
        let _: () = msg_send![&*urls, addObject: &*url];
        let workspace: Retained<AnyObject> = msg_send![class!(NSWorkspace), sharedWorkspace];
        let _: () = msg_send![&*workspace, activateFileViewerSelectingURLs: &*urls];
    }
}

/// Show a file or folder selected in File Explorer.
#[cfg(windows)]
pub fn reveal(path: &str) {
    let path = std::path::PathBuf::from(path.replace('/', "\\"));
    let mut select = std::ffi::OsString::from("/select,");
    select.push(path.as_os_str());
    // Explorer is a GUI program: no console window opens.
    if let Err(error) = std::process::Command::new("explorer.exe")
        .arg(select)
        .spawn()
    {
        isocast_diagnostics::diagnostic!("error"; "couldn’t show {}: {error}", path.display());
    }
}

/// Open the folder holding `path` (or `path`, a folder) in the file
/// manager.
#[cfg(not(any(target_os = "macos", windows)))]
pub fn reveal(path: &str) {
    let path = std::path::PathBuf::from(expand_home(path));
    let folder = match path.is_dir() {
        true => path.clone(),
        false => path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or(path.clone()),
    };
    match std::process::Command::new("xdg-open").arg(&folder).spawn() {
        // Reaped when it exits (it hands the folder to the file manager).
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(error) => {
            isocast_diagnostics::diagnostic!("error"; "couldn’t show {}: {error}", folder.display())
        }
    }
}

/// Open a web page in the browser (a prefilled GitHub issue to post).
pub fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("/usr/bin/open")
        .arg(url)
        .spawn()
        .map(drop);
    #[cfg(windows)]
    let result = shell_open(url);
    #[cfg(not(any(target_os = "macos", windows)))]
    let result = std::process::Command::new("xdg-open")
        .arg(url)
        .spawn()
        .map(drop);
    if let Err(error) = result {
        eprintln!("couldn’t open {url}: {error}");
    }
}

/// Open a URL with its registered handler (the default browser).
#[cfg(windows)]
fn shell_open(url: &str) -> std::io::Result<()> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let wide = |text: &str| text.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (verb, target) = (wide("open"), wide(url));
    // SAFETY: NUL-terminated strings live through the synchronous call.
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // Values above 32 mean success (the documented HINSTANCE convention).
    if result as isize > 32 {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "ShellExecute failed with code {}",
            result as isize
        )))
    }
}

/// Put `text` on the general pasteboard (a command to paste in Terminal).
#[cfg(target_os = "macos")]
pub fn try_copy_text(text: &str) -> bool {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};

    // SAFETY: NSPasteboard's shared object on the main thread; the type is
    // the constant string NSPasteboardTypeString stands for.
    unsafe {
        let board: Retained<AnyObject> = msg_send![class!(NSPasteboard), generalPasteboard];
        let _: isize = msg_send![&*board, clearContents];
        let string = ns_string(text);
        let kind = ns_string("public.utf8-plain-text");
        msg_send![&*board, setString: &*string, forType: &*kind]
    }
}

#[cfg(not(target_os = "macos"))]
pub fn try_copy_text(text: &str) -> bool {
    // X11/Wayland ownership lasts while this process serves the selection.
    thread_local! { static CLIPBOARD: std::cell::RefCell<Option<arboard::Clipboard>> = const { std::cell::RefCell::new(None) }; }
    CLIPBOARD.with_borrow_mut(|clipboard| {
        if clipboard.is_none() {
            *clipboard = arboard::Clipboard::new().ok();
        }
        clipboard
            .as_mut()
            .is_some_and(|clipboard| clipboard.set_text(text).is_ok())
    })
}

pub fn copy_text(text: &str) {
    try_copy_text(text);
}

/// Open `folder` (`~` allowed) in the Finder, as OBS's Show Recordings
/// does; a folder that isn't there yet is made first (recording makes it
/// anyway), and if that fails its nearest existing parent is shown.
#[cfg(target_os = "macos")]
pub fn open_folder(folder: &str) {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};

    let path = expand_home(folder);
    if std::fs::create_dir_all(&path).is_err() {
        let parent = std::path::Path::new(&path)
            .ancestors()
            .find(|p| p.is_dir())
            .map(|p| p.display().to_string());
        if let Some(parent) = parent {
            reveal(&parent);
        }
        return;
    }
    // SAFETY: as in `choose`.
    unsafe {
        let path = ns_string(&path);
        let url: Retained<AnyObject> = msg_send![class!(NSURL), fileURLWithPath: &*path];
        let workspace: Retained<AnyObject> = msg_send![class!(NSWorkspace), sharedWorkspace];
        let _: bool = msg_send![&*workspace, openURL: &*url];
    }
}

#[cfg(not(target_os = "macos"))]
pub fn open_folder(folder: &str) {
    reveal(folder);
}

/// Window → Bring All to Front: the app's windows in front of others.
#[cfg(target_os = "macos")]
pub fn bring_all_to_front() {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};

    // SAFETY: NSApplication's shared object, on the main thread (menu
    // commands run there); `arrangeInFront:` takes any sender.
    unsafe {
        let app: Retained<AnyObject> = msg_send![class!(NSApplication), sharedApplication];
        let none: *const AnyObject = std::ptr::null();
        let _: () = msg_send![&*app, arrangeInFront: none];
    }
}

#[cfg(not(target_os = "macos"))]
pub fn bring_all_to_front() {}

/// An NSString with `text`.
#[cfg(target_os = "macos")]
pub(crate) unsafe fn ns_string(text: &str) -> objc2::rc::Retained<objc2::runtime::AnyObject> {
    use objc2::{class, msg_send};
    let text = std::ffi::CString::new(text.replace('\0', "")).expect("no NULs left");
    // SAFETY: a valid C string, copied by NSString.
    unsafe { msg_send![class!(NSString), stringWithUTF8String: text.as_ptr()] }
}

/// An NSString's text.
#[cfg(target_os = "macos")]
pub(crate) unsafe fn rust_string(string: &objc2::runtime::AnyObject) -> String {
    use objc2::msg_send;
    // SAFETY: UTF8String lives as long as the string, which outlives this.
    unsafe {
        let utf8: *const std::ffi::c_char = msg_send![string, UTF8String];
        if utf8.is_null() {
            return String::new();
        }
        std::ffi::CStr::from_ptr(utf8)
            .to_string_lossy()
            .into_owned()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn portal_uris_become_paths() {
        assert_eq!(
            super::file_path("file:///home/dax/My%20Videos/clip%C3%A9.mp4").as_deref(),
            Some("/home/dax/My Videos/clipé.mp4")
        );
        assert_eq!(
            super::file_path("file://localhost/tmp/a").as_deref(),
            Some("/tmp/a")
        );
        assert_eq!(super::file_path("https://example.com/a"), None);
        // A stray percent stays as it is.
        assert_eq!(
            super::file_path("file:///tmp/100%").as_deref(),
            Some("/tmp/100%")
        );
    }
}
