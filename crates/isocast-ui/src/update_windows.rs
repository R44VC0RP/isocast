//! WinSparkle 0.9.4 verifies Ed25519 before handing us an installer. We
//! deliberately replace its ShellExecute handler: feed-supplied arguments
//! (not covered by the enclosure signature) must never become installer flags.
//! The root of trust and feed are compiled in, never read from user settings.

use std::ffi::{CString, OsString};
use std::fs::{File, OpenOptions};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use isocast_client::Client;
use windows_sys::Win32::Foundation::FreeLibrary;
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

use crate::app::App;
use crate::{ToastKind, daemon};

const FEED: &str =
    "https://github.com/R44VC0RP/isocast/releases/latest/download/appcast-windows.xml";
const RELEASE_KEY: &str = include_str!("../../../scripts/sparkle/public-ed-key.txt");
static API: OnceLock<Api> = OnceLock::new();
static INITIALIZED: AtomicBool = AtomicBool::new(false);
static INSTALLING: AtomicBool = AtomicBool::new(false);

struct Configuration {
    key: String,
    feed: String,
    build: String,
    test_only: bool,
}

fn configuration() -> Result<Configuration> {
    let test_only = option_env!("ISOCAST_WINDOWS_UPDATE_TEST") == Some("1");
    let key_override = option_env!("ISOCAST_WINDOWS_UPDATE_KEY");
    let feed_override = option_env!("ISOCAST_WINDOWS_APPCAST_URL");
    if !test_only && (key_override.is_some() || feed_override.is_some()) {
        bail!("update key/feed overrides require an explicit test-only build");
    }
    let key = key_override
        .or_else(|| {
            RELEASE_KEY
                .lines()
                .find(|line| !line.starts_with('#') && !line.trim().is_empty())
        })
        .context("no embedded update public key")?
        .trim()
        .to_string();
    let feed = feed_override.unwrap_or(FEED).to_string();
    if !feed.starts_with("https://") && !(test_only && feed.starts_with("http://127.0.0.1:")) {
        bail!("update feed must use HTTPS (test-only builds also allow loopback HTTP)");
    }
    let build = option_env!("ISOCAST_WINDOWS_BUILD_VERSION")
        .unwrap_or(env!("CARGO_PKG_VERSION"))
        .to_string();
    let parts: Vec<_> = build.split('.').collect();
    if !(3..=4).contains(&parts.len()) || parts.iter().any(|part| part.parse::<u16>().is_err()) {
        bail!("Windows build version must have three or four numeric components (0–65535)");
    }
    Ok(Configuration {
        key,
        feed,
        build,
        test_only,
    })
}

type Callback = Option<extern "C" fn()>;
type CanShutdown = Option<extern "C" fn() -> i32>;
type RunInstaller = Option<extern "C" fn(*const u16) -> i32>;

struct Api {
    // Retain the module until process exit, including after cleanup, so an
    // in-flight callback can never return through an unloaded DLL.
    _module: usize,
    init: unsafe extern "C" fn(),
    cleanup: unsafe extern "C" fn(),
    set_appcast_url: unsafe extern "C" fn(*const u8),
    set_eddsa_public_key: unsafe extern "C" fn(*const u8) -> i32,
    set_app_details: unsafe extern "C" fn(*const u16, *const u16, *const u16),
    set_app_build_version: unsafe extern "C" fn(*const u16),
    set_registry_path: unsafe extern "C" fn(*const u8),
    set_automatic_check_for_updates: unsafe extern "C" fn(i32),
    check_update_with_ui: unsafe extern "C" fn(),
    set_can_shutdown_callback: unsafe extern "C" fn(CanShutdown),
    set_shutdown_request_callback: unsafe extern "C" fn(Callback),
    set_error_callback: unsafe extern "C" fn(Callback),
    set_user_run_installer_callback: unsafe extern "C" fn(RunInstaller),
}

impl Api {
    fn load() -> Result<Self> {
        let path = std::env::current_exe()?.with_file_name("WinSparkle.dll");
        let path: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: absolute, NUL-terminated path; dependencies may come only
        // from the DLL's own directory or System32, not CWD or PATH.
        let module = unsafe {
            LoadLibraryExW(
                path.as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        };
        if module.is_null() {
            bail!(
                "WinSparkle.dll could not load: {}",
                std::io::Error::last_os_error()
            );
        }
        macro_rules! load {
            ($name:ident, $kind:ty) => {{
                // SAFETY: these exact C declarations are from the pinned
                // WinSparkle 0.9.4 header; the module remains loaded.
                let symbol = unsafe {
                    GetProcAddress(
                        module,
                        concat!("win_sparkle_", stringify!($name), "\0").as_ptr(),
                    )
                };
                let Some(symbol) = symbol else {
                    unsafe {
                        FreeLibrary(module);
                    }
                    bail!("WinSparkle.dll is missing {}", stringify!($name));
                };
                unsafe {
                    std::mem::transmute::<unsafe extern "system" fn() -> isize, $kind>(symbol)
                }
            }};
        }
        let confined = load!(isocast_confined_downloads, unsafe extern "C" fn() -> i32);
        // SAFETY: Isocast's source-built SDK exports this exact marker. Reject
        // the upstream 0.9.4 binary before configuring a feed or initializing
        // any downloader: its post-download signature check is too late to
        // prevent attacker-selected response filenames from escaping staging.
        if unsafe { confined() } != 1 {
            unsafe {
                FreeLibrary(module);
            }
            bail!("WinSparkle lacks Isocast's confined download boundary");
        }
        Ok(Self {
            _module: module as usize,
            init: load!(init, unsafe extern "C" fn()),
            cleanup: load!(cleanup, unsafe extern "C" fn()),
            set_appcast_url: load!(set_appcast_url, unsafe extern "C" fn(*const u8)),
            set_eddsa_public_key: load!(
                set_eddsa_public_key,
                unsafe extern "C" fn(*const u8) -> i32
            ),
            set_app_details: load!(
                set_app_details,
                unsafe extern "C" fn(*const u16, *const u16, *const u16)
            ),
            set_app_build_version: load!(set_app_build_version, unsafe extern "C" fn(*const u16)),
            set_registry_path: load!(set_registry_path, unsafe extern "C" fn(*const u8)),
            set_automatic_check_for_updates: load!(
                set_automatic_check_for_updates,
                unsafe extern "C" fn(i32)
            ),
            check_update_with_ui: load!(check_update_with_ui, unsafe extern "C" fn()),
            set_can_shutdown_callback: load!(
                set_can_shutdown_callback,
                unsafe extern "C" fn(CanShutdown)
            ),
            set_shutdown_request_callback: load!(
                set_shutdown_request_callback,
                unsafe extern "C" fn(Callback)
            ),
            set_error_callback: load!(set_error_callback, unsafe extern "C" fn(Callback)),
            set_user_run_installer_callback: load!(
                set_user_run_installer_callback,
                unsafe extern "C" fn(RunInstaller)
            ),
        })
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn api() -> Result<&'static Api> {
    if let Some(api) = API.get() {
        return Ok(api);
    }
    let config = configuration()?;
    let key = CString::new(config.key)?;
    let api = Api::load()?;
    // SAFETY: valid public key string, copied by WinSparkle; no check starts
    // until init. Never fall back to unsigned updates if the key is invalid.
    if unsafe { (api.set_eddsa_public_key)(key.as_ptr().cast()) } != 1 {
        // No callbacks or worker threads have been registered yet.
        unsafe {
            FreeLibrary(api._module as _);
        }
        bail!("WinSparkle rejected the embedded Ed25519 public key");
    }
    let _ = API.set(api);
    Ok(API.get().expect("the updater API was installed"))
}

pub fn start(app: &Rc<App>) {
    if app.mode.offscreen {
        return;
    }
    let result = (|| -> Result<()> {
        let config = configuration()?;
        let api = api()?;
        let feed = CString::new(config.feed)?;
        let name = wide("Isocast");
        let version = wide(env!("CARGO_PKG_VERSION"));
        let build = wide(&config.build);
        // SAFETY: all strings are copied, callbacks have the declared ABI
        // and are thread-safe. The API/module outlive every callback.
        unsafe {
            (api.set_app_details)(name.as_ptr(), name.as_ptr(), version.as_ptr());
            (api.set_app_build_version)(build.as_ptr());
            (api.set_registry_path)(c"Software\\Isocast\\Updates".as_ptr().cast());
            (api.set_appcast_url)(feed.as_ptr().cast());
            // Disable the SDK's startup prompt, timer and unsolicited UI,
            // including a preference retained from an older build. Its
            // install callback cannot gate a notification over a live capture.
            (api.set_automatic_check_for_updates)(0);
            (api.set_can_shutdown_callback)(Some(can_shutdown));
            (api.set_shutdown_request_callback)(Some(shutdown));
            (api.set_error_callback)(Some(aborted));
            (api.set_user_run_installer_callback)(Some(run_installer));
            if !INITIALIZED.swap(true, Ordering::SeqCst) {
                (api.init)();
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        isocast_diagnostics::diagnostic!("info"; "updates: off ({error:#})");
    }
}

pub fn cleanup() {
    if INITIALIZED.swap(false, Ordering::SeqCst)
        && let Some(api) = API.get()
    {
        // SAFETY: main-thread shutdown after the app event loop ends.
        unsafe {
            (api.cleanup)();
        }
    }
}

pub fn available() -> bool {
    INITIALIZED.load(Ordering::SeqCst)
}
pub fn automatic() -> bool {
    false
}
pub fn set_automatic(_on: bool) {
    if available()
        && let Some(api) = API.get()
    {
        unsafe {
            (api.set_automatic_check_for_updates)(0);
        }
    }
}
pub fn check_now() {
    if available()
        && let Some(api) = API.get()
    {
        unsafe {
            (api.check_update_with_ui)();
        }
    }
}
pub fn staged() -> bool {
    INSTALLING.load(Ordering::SeqCst)
}
pub fn stopping_engine() -> bool {
    daemon::NO_LAUNCH.load(Ordering::SeqCst)
}
pub fn feed_url() -> Option<String> {
    configuration().ok().map(|config| config.feed)
}

fn explain(error: String) {
    isocast_diagnostics::diagnostic!("warn"; "updates: {error}");
    let _ = slint::invoke_from_event_loop(move || {
        crate::window::with_app(|app| {
            app.toast(ToastKind::Warning, "Update waiting", error);
        });
    });
}

extern "C" fn aborted() {
    INSTALLING.store(false, Ordering::SeqCst);
    daemon::NO_LAUNCH.store(false, Ordering::SeqCst);
}

extern "C" fn can_shutdown() -> i32 {
    std::panic::catch_unwind(|| {
        if daemon::NO_LAUNCH.swap(true, Ordering::SeqCst) {
            return 0;
        }
        match Client::shutdown_if_idle() {
            Ok(()) => {
                INSTALLING.store(true, Ordering::SeqCst);
                1
            }
            Err(error) => {
                aborted();
                explain(format!(
                    "The engine must be idle before updating: {error:#}"
                ));
                0
            }
        }
    })
    .unwrap_or_else(|_| {
        aborted();
        0
    })
}

extern "C" fn shutdown() {
    let _ = slint::invoke_from_event_loop(|| {
        let _ = slint::quit_event_loop();
    });
}

fn locked_installer(path: &Path) -> Result<File> {
    if !path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
    {
        bail!("update file must be a Windows .exe installer");
    }
    // FILE_SHARE_READ permits verification/loader reads, but not replacement
    // or modification between verification and process creation.
    let file = OpenOptions::new().read(true).share_mode(1).open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > 1 << 30 {
        bail!("update file must be a nonempty installer no larger than 1 GiB");
    }
    Ok(file)
}

fn launch(path: &Path, rollback: bool) -> Result<u32> {
    let mut command = Command::new(path);
    command.args([
        "/SP-",
        "/NORESTART",
        "/NOCLOSEAPPLICATIONS",
        "/NOFORCECLOSEAPPLICATIONS",
    ]);
    if rollback {
        command.arg("/ALLOWDOWNGRADE=1");
    }
    Ok(command
        .spawn()
        .context("could not launch the verified installer")?
        .id())
}

extern "C" fn run_installer(path: *const u16) -> i32 {
    std::panic::catch_unwind(|| {
        let result = (|| -> Result<()> {
            if path.is_null() || !staged() {
                bail!("update installation was not authorized");
            }
            // SAFETY: WinSparkle supplies a NUL-terminated UTF-16 filename
            // for this callback's duration, after successful signature verification.
            let path = unsafe {
                let mut len = 0;
                while len < 32768 && *path.add(len) != 0 {
                    len += 1;
                }
                if len == 32768 {
                    bail!("installer filename is too long");
                }
                PathBuf::from(OsString::from_wide(std::slice::from_raw_parts(path, len)))
            };
            let _lease = locked_installer(&path)?;
            launch(&path, false)?;
            Ok(())
        })();
        match result {
            Ok(()) => 1, // Handled: WinSparkle must NOT use feed-supplied arguments.
            Err(error) => {
                aborted();
                explain(format!("{error:#}"));
                -1
            }
        }
    })
    .unwrap_or_else(|_| {
        aborted();
        -1
    })
}

fn verify_rollback(path: &Path, signature: &str, config: &Configuration) -> Result<()> {
    let verifier = std::env::current_exe()?.with_file_name("winsparkle-tool.exe");
    let mut child = Command::new(verifier)
        .args([
            "verify",
            "--public-key",
            &config.key,
            "--signature",
            signature,
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .context("could not start the bundled signature verifier")?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                bail!("rollback installer has no valid signature from this app's update key");
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("rollback signature verification timed out; nothing was installed");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Packaging/rollback operations, not engine capabilities. Never opens a
/// window or contacts the update feed; an invalid signature never stops work.
pub fn command_line(args: &[String]) -> Option<Result<()>> {
    match args.first().map(String::as_str) {
        Some("--update-info") => Some((|| {
            if args.len() != 1 {
                bail!("--update-info takes no arguments");
            }
            let config = configuration()?;
            api()?; // Validate the embedded key using the packaged verifier, without init/UI.
            println!(
                "{}",
                serde_json::json!({"version":env!("CARGO_PKG_VERSION"),
                "build_version":config.build, "public_key":config.key,
                "feed_url":config.feed, "test_only":config.test_only})
            );
            Ok(())
        })()),
        Some("--rollback-update") => Some((|| {
            if args.len() != 3 {
                bail!("usage: isocast-ui --rollback-update INSTALLER.exe ED25519_SIGNATURE");
            }
            let config = configuration()?;
            let path = Path::new(&args[1]).canonicalize()?;
            let _lease = locked_installer(&path)?;
            verify_rollback(&path, &args[2], &config)?;
            Client::shutdown_if_idle()?;
            let pid = launch(&path, true)?;
            println!(
                "{}",
                serde_json::json!({"installer_pid":pid,"rollback":true})
            );
            Ok(())
        })()),
        _ => None,
    }
}
