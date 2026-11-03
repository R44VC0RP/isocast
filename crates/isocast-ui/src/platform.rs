//! The system's own settings and panels, put on the window: Reduce
//! motion (macOS, and Linux through the desktop's settings portal; menus,
//! dialogs and toasts then appear without animating), the open and save
//! panels (choosing files and folders, and where to write one, which Slint has no widget for) and showing a folder
//! in the Finder. These are the web app's native commands `open_file`,
//! `save_file` and `reveal`.

use slint::ComponentHandle;

use crate::{AppWindow, Theme};

pub fn apply_system_settings(window: &AppWindow) {
    #[cfg(windows)]
    apply_windows_motion(window, true);
    #[cfg(not(windows))]
    window.global::<Theme>().set_reduce_motion(reduce_motion());
    #[cfg(target_os = "linux")]
    watch_reduce_motion(window.as_weak());
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

#[cfg(not(any(target_os = "macos", windows)))]
fn reduce_motion() -> bool {
    false
}

#[cfg(any(windows, test))]
fn reduce_from_client_area_animation(enabled: i32) -> bool {
    enabled == 0 // Win32 BOOL: any nonzero value enables animations.
}

#[cfg(windows)]
pub fn apply_windows_motion(window: &AppWindow, startup: bool) {
    use windows::Win32::UI::WindowsAndMessaging::{
        SPI_GETCLIENTAREAANIMATION, SystemParametersInfoW,
    };

    let mut enabled = 0i32;
    // SAFETY: GETCLIENTAREAANIMATION writes one BOOL into this live i32.
    let result = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some((&raw mut enabled).cast()),
            Default::default(),
        )
    };
    if let Err(error) = result {
        isocast_diagnostics::diagnostic!("warn"; "could not read Windows animation setting: {error}");
        return; // Keep the current preference (animations on by default).
    }
    let reduce = reduce_from_client_area_animation(enabled);
    let theme = window.global::<Theme>();
    if startup || theme.get_reduce_motion() != reduce {
        theme.set_reduce_motion(reduce);
        isocast_diagnostics::diagnostic!("info"; "reduce motion from Windows settings: {}", theme.get_reduce_motion());
    }
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

/// Drive all desktop requests off the window thread on one runtime.
#[cfg(target_os = "linux")]
fn portal_runtime() -> Option<&'static tokio::runtime::Runtime> {
    static RUNTIME: std::sync::OnceLock<Option<tokio::runtime::Runtime>> =
        std::sync::OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("portal")
                .enable_all()
                .build()
                .inspect_err(|error| {
                    isocast_diagnostics::diagnostic!("warn"; "no runtime for the desktop portal: {error}");
                })
                .ok()
        })
        .as_ref()
}

/// A desktop setting's value, as the settings portal sends it.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, PartialEq)]
enum SettingValue {
    Bool(bool),
    Unsigned(u32),
    Number(f64),
    Text(String),
    Other,
}

/// Each desktop's own way of saying "reduce motion", as the settings
/// portal exposes it: none known means animations stay on.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct MotionSettings {
    /// `org.freedesktop.appearance` `reduced-motion` (1 reduce, 0 no preference).
    appearance: Option<bool>,
    /// GNOME's `org.gnome.desktop.interface` `enable-animations`, inverted.
    gnome: Option<bool>,
    /// KDE's animation speed `AnimationDurationFactor`, 0 meaning none.
    kde: Option<bool>,
}

#[cfg(any(target_os = "linux", test))]
impl MotionSettings {
    #[cfg(target_os = "linux")]
    const NAMESPACES: [&'static str; 3] = [
        "org.freedesktop.appearance",
        "org.gnome.desktop.interface",
        "org.kde.kdeglobals.KDE",
    ];

    /// Take one setting in; false if it isn't one of the motion keys.
    fn set(&mut self, namespace: &str, key: &str, value: &SettingValue) -> bool {
        let (slot, reduce) = match (namespace, key) {
            ("org.freedesktop.appearance", "reduced-motion") => (
                &mut self.appearance,
                match value {
                    SettingValue::Unsigned(1) => Some(true),
                    SettingValue::Unsigned(0) => Some(false),
                    _ => None,
                },
            ),
            ("org.gnome.desktop.interface", "enable-animations") => (
                &mut self.gnome,
                match value {
                    SettingValue::Bool(on) => Some(!on),
                    _ => None,
                },
            ),
            ("org.kde.kdeglobals.KDE", "AnimationDurationFactor") => (
                &mut self.kde,
                match value {
                    SettingValue::Number(factor) => Some(*factor <= 0.0),
                    // KDE's portal sends kdeglobals entries as text.
                    SettingValue::Text(text) => {
                        text.trim().parse::<f64>().ok().map(|factor| factor <= 0.0)
                    }
                    _ => None,
                },
            ),
            _ => return false,
        };
        *slot = reduce;
        true
    }

    /// Reduce motion if any desktop setting asks to; none if none is known.
    fn reduce(&self) -> Option<bool> {
        let known = [self.appearance, self.gnome, self.kde];
        known
            .iter()
            .any(Option::is_some)
            .then(|| known.contains(&Some(true)))
    }
}

#[cfg(target_os = "linux")]
fn setting_value(value: &ashpd::zbus::zvariant::Value) -> SettingValue {
    use ashpd::zbus::zvariant::Value;
    match value {
        Value::Value(inner) => setting_value(inner),
        Value::Bool(b) => SettingValue::Bool(*b),
        Value::U32(n) => SettingValue::Unsigned(*n),
        Value::F64(n) => SettingValue::Number(*n),
        Value::Str(text) => SettingValue::Text(text.to_string()),
        _ => SettingValue::Other,
    }
}

/// Follow the desktop's reduce-motion setting through the settings
/// portal, live, into the window's Theme. Without the portal or its keys
/// the window keeps animating.
#[cfg(target_os = "linux")]
fn watch_reduce_motion(window: slint::Weak<AppWindow>) {
    use ashpd::zbus::zvariant::OwnedValue;
    use ashpd::zbus::{Connection, MessageStream, export::futures_core::Stream};
    use std::collections::HashMap;

    let Some(runtime) = portal_runtime() else {
        return;
    };
    let publish = move |reduce: bool| {
        let window = window.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(window) = window.upgrade() {
                let theme = window.global::<Theme>();
                theme.set_reduce_motion(reduce);
                isocast_diagnostics::diagnostic!("info"; "reduce motion from the desktop's settings: {}", theme.get_reduce_motion());
            }
        });
    };
    runtime.spawn(async move {
        let watch = async {
            let bus = Connection::session().await?;
            // Listen before reading, so a change in between isn't missed.
            let mut changes = MessageStream::for_match_rule(
                "type='signal',interface='org.freedesktop.portal.Settings',member='SettingChanged',path='/org/freedesktop/portal/desktop'",
                &bus,
                Some(16),
            )
            .await?;
            let reply = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                bus.call_method(
                    Some("org.freedesktop.portal.Desktop"),
                    "/org/freedesktop/portal/desktop",
                    Some("org.freedesktop.portal.Settings"),
                    "ReadAll",
                    &(MotionSettings::NAMESPACES.as_slice(),),
                ),
            )
            .await
            .map_err(|_| anyhow::anyhow!("the settings portal did not answer"))??;
            let all: HashMap<String, HashMap<String, OwnedValue>> = reply.body().deserialize()?;
            let mut motion = MotionSettings::default();
            for (namespace, keys) in &all {
                for (key, value) in keys {
                    motion.set(namespace, key, &setting_value(value));
                }
            }
            let mut shown = motion.reduce();
            match shown {
                Some(reduce) => publish(reduce),
                None => isocast_diagnostics::diagnostic!("info"; "the settings portal has no reduce-motion setting"),
            }
            while let Some(message) =
                std::future::poll_fn(|cx| std::pin::Pin::new(&mut changes).poll_next(cx)).await
            {
                let Ok((namespace, key, value)) =
                    message?.body().deserialize::<(String, String, OwnedValue)>()
                else {
                    continue;
                };
                if motion.set(&namespace, &key, &setting_value(&value)) {
                    let reduce = motion.reduce();
                    if reduce != shown {
                        shown = reduce;
                        publish(reduce.unwrap_or(false));
                    }
                }
            }
            Ok::<_, anyhow::Error>(())
        };
        if let Err(error) = watch.await {
            isocast_diagnostics::diagnostic!("info"; "desktop reduce-motion setting unavailable: {error}");
        }
    });
}

#[cfg(target_os = "linux")]
pub enum PortalDialog {
    Open { folders: bool, multiple: bool },
    Save { suggested: std::path::PathBuf },
}

/// FileChooser's `current_name` is a basename, not a path. An absent
/// or unusable parent lets the desktop pick its normal starting folder.
#[cfg(target_os = "linux")]
fn save_location(suggested: &std::path::Path) -> (Option<&str>, Option<&std::path::Path>) {
    (
        suggested.file_name().and_then(|name| name.to_str()),
        suggested
            .parent()
            .filter(|dir| dir.is_absolute() && dir.is_dir()),
    )
}

/// Ask the local desktop, never the engine. None means no chooser;
/// cancellation is a successful, empty selection and must not open a fallback.
#[cfg(target_os = "linux")]
pub fn ask_portal(
    dialog: PortalDialog,
    prompt: &str,
    types: &[&str],
    done: impl FnOnce(Option<Vec<String>>) + 'static,
) {
    use ashpd::desktop::file_chooser::{FileFilter, SelectedFiles};
    use ashpd::zbus::Connection;

    let Some(runtime) = portal_runtime() else {
        return done(None);
    };
    let filters = (!types.is_empty()).then(|| {
        let label = match dialog {
            PortalDialog::Open { .. } => "Supported files".to_string(),
            PortalDialog::Save { .. } => types.join(", ").to_uppercase(),
        };
        types.iter().fold(FileFilter::new(&label), |filter, ext| {
            let filter = filter.glob(&format!("*.{ext}"));
            match dialog {
                PortalDialog::Open { .. } => filter.glob(&format!("*.{}", ext.to_uppercase())),
                PortalDialog::Save { .. } => filter,
            }
        })
    });
    let prompt = if prompt.is_empty() { "Choose" } else { prompt }.to_owned();
    let request = runtime.spawn(async move {
        // Ask for FileChooser itself. ashpd's proxy.version() defaults to 1
        // on some D-Bus errors. Bound only this probe (including activation
        // of a stuck service), never the time a person takes in the dialog.
        let probe = async {
            let bus = Connection::session().await?;
            bus.call_method(
                Some("org.freedesktop.portal.Desktop"),
                "/org/freedesktop/portal/desktop",
                Some("org.freedesktop.DBus.Properties"),
                "Get",
                &("org.freedesktop.portal.FileChooser", "version"),
            )
            .await?;
            Ok::<_, ashpd::zbus::Error>(bus)
        };
        let bus = tokio::time::timeout(std::time::Duration::from_millis(250), probe)
            .await
            .map_err(|_| anyhow::anyhow!("FileChooser did not answer"))??;
        let request = match dialog {
            PortalDialog::Open { folders, multiple } => {
                let title = if folders {
                    "Choose a Folder"
                } else {
                    "Choose a File"
                };
                SelectedFiles::open_file()
                    .connection(Some(bus))
                    .title(title)
                    .accept_label(prompt.as_str())
                    .modal(true)
                    .directory(folders)
                    .multiple(multiple)
                    .filters(filters)
                    .send()
                    .await
            }
            PortalDialog::Save { suggested } => {
                let (name, folder) = save_location(&suggested);
                // ashpd encodes the folder's native bytes as NUL-terminated `ay`.
                SelectedFiles::save_file()
                    .connection(Some(bus))
                    .title(prompt.as_str())
                    .accept_label(prompt.as_str())
                    .modal(true)
                    .current_name(name)
                    .current_folder::<&std::path::Path>(folder)?
                    .filters(filters)
                    .send()
                    .await
            }
        };
        match request.and_then(|r| r.response()) {
            Ok(files) => files
                .uris()
                .iter()
                .map(|uri| {
                    file_path(uri.as_str())
                        .ok_or_else(|| anyhow::anyhow!("FileChooser did not return a local file"))
                })
                .collect(),
            // GTK's Cancel returns 1; closing the same panel with Escape
            // can return 2 (interaction ended otherwise). Neither means
            // the chooser is missing or warrants opening a second dialog.
            Err(ashpd::Error::Response(_)) => Ok(Vec::new()),
            Err(error) => Err(error.into()),
        }
    });
    slint::spawn_local(async move {
        let result = request.await.map_err(anyhow::Error::from).and_then(|r| r);
        done(
            result
                .inspect_err(|error| {
                    isocast_diagnostics::diagnostic!("info"; "file panel unavailable: {error}");
                })
                .ok(),
        );
    })
    .expect("file panels are requested on the window thread");
}

/// A `file://` URI's path, percent-decoded (none for another scheme).
#[cfg(any(target_os = "linux", test))]
fn file_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let (authority, path) = rest.split_at(rest.find('/')?);
    if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
        return None;
    }
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

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
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
/// False without one (no desktop session, as over SSH).
#[cfg(target_os = "macos")]
pub fn try_copy_text(text: &str) -> bool {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};

    // SAFETY: NSPasteboard's shared object on the main thread; the type is
    // the constant string NSPasteboardTypeString stands for.
    unsafe {
        let board: Option<Retained<AnyObject>> = msg_send![class!(NSPasteboard), generalPasteboard];
        let Some(board) = board else {
            return false;
        };
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
    fn windows_animation_bool_maps_to_reduce_motion() {
        use super::reduce_from_client_area_animation;
        assert!(reduce_from_client_area_animation(0));
        assert!(!reduce_from_client_area_animation(1));
        assert!(!reduce_from_client_area_animation(-1));
    }

    /// Copy buttons' one path, read back from the system clipboard. Run by
    /// hand in a desktop session (`-- --ignored clipboard`); CI has none.
    #[test]
    #[ignore = "needs a desktop session's clipboard"]
    fn clipboard_holds_copied_text() {
        let text = format!("isocast clipboard check {}", std::process::id());
        assert!(super::try_copy_text(&text));
        #[cfg(not(target_os = "macos"))]
        assert_eq!(arboard::Clipboard::new().unwrap().get_text().unwrap(), text);
        println!("copied: {text}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn save_options_separate_basename_from_existing_directory() {
        use std::path::Path;
        let dir = std::env::temp_dir();
        let name = "Studio export.toml";
        assert_eq!(
            super::save_location(&dir.join(name)),
            (Some(name), Some(dir.as_path()))
        );
        assert_eq!(super::save_location(Path::new(name)), (Some(name), None));
        // A regular file or a nonexistent directory cannot be opened as the starting folder.
        let file = std::env::current_exe().unwrap();
        assert_eq!(super::save_location(&file.join(name)), (Some(name), None));
        assert_eq!(
            super::save_location(&file.join("missing").join(name)),
            (Some(name), None)
        );
    }

    #[test]
    fn portal_motion_settings_read() {
        use super::{MotionSettings, SettingValue};
        let mut motion = MotionSettings::default();
        assert_eq!(motion.reduce(), None);
        assert!(!motion.set(
            "org.gnome.desktop.interface",
            "color-scheme",
            &SettingValue::Text("prefer-dark".into())
        ));
        assert_eq!(motion.reduce(), None);
        // GNOME: animations off means reduce.
        let gnome = ("org.gnome.desktop.interface", "enable-animations");
        assert!(motion.set(gnome.0, gnome.1, &SettingValue::Bool(true)));
        assert_eq!(motion.reduce(), Some(false));
        motion.set(gnome.0, gnome.1, &SettingValue::Bool(false));
        assert_eq!(motion.reduce(), Some(true));
        // The freedesktop key's "no preference" doesn't undo GNOME's.
        let appearance = ("org.freedesktop.appearance", "reduced-motion");
        motion.set(appearance.0, appearance.1, &SettingValue::Unsigned(0));
        assert_eq!(motion.reduce(), Some(true));
        motion.set(gnome.0, gnome.1, &SettingValue::Bool(true));
        assert_eq!(motion.reduce(), Some(false));
        motion.set(appearance.0, appearance.1, &SettingValue::Unsigned(1));
        assert_eq!(motion.reduce(), Some(true));
        // An unexpected type forgets that key.
        motion.set(appearance.0, appearance.1, &SettingValue::Other);
        assert_eq!(motion.reduce(), Some(false));
        // KDE: an animation speed of 0, as text or a number.
        let kde = ("org.kde.kdeglobals.KDE", "AnimationDurationFactor");
        let mut motion = MotionSettings::default();
        motion.set(kde.0, kde.1, &SettingValue::Text("0".into()));
        assert_eq!(motion.reduce(), Some(true));
        motion.set(kde.0, kde.1, &SettingValue::Text("0.5".into()));
        assert_eq!(motion.reduce(), Some(false));
        motion.set(kde.0, kde.1, &SettingValue::Number(0.0));
        assert_eq!(motion.reduce(), Some(true));
        motion.set(kde.0, kde.1, &SettingValue::Text("fast".into()));
        assert_eq!(motion.reduce(), None);
    }

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
        assert_eq!(super::file_path("file://another-computer/tmp/a"), None);
        // A stray percent stays as it is.
        assert_eq!(
            super::file_path("file:///tmp/100%").as_deref(),
            Some("/tmp/100%")
        );
    }
}
