//! What to do about each privacy permission: whom macOS lists it under,
//! the System Settings pane that grants it, and whether isocastd must
//! restart afterwards.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use isocast_protocol::SourceKind;

/// A non-prompting hint before native opening. macOS has no public TCC
/// preflight for files/folders: protected file opens carry a provisional
/// waiting state until access succeeds (or an actual I/O error is returned).
pub(crate) fn source_permission(
    source: &SourceKind,
    permissions: impl FnOnce() -> Option<PermissionsReport>,
) -> Option<String> {
    let device = match source {
        SourceKind::Camera(_) => Some(PermissionKind::Camera),
        SourceKind::Microphone(_) => Some(PermissionKind::Microphone),
        SourceKind::Display(_) | SourceKind::Window(_) | SourceKind::Application(_) => {
            Some(PermissionKind::ScreenRecording)
        }
        // The permission the capture will use (see `tap::backend`).
        SourceKind::SystemAudio(_) | SourceKind::ApplicationAudio(_) => {
            Some(match crate::tap::backend() {
                crate::tap::Backend::ProcessTap => PermissionKind::SystemAudio,
                crate::tap::Backend::ScreenCaptureKit => PermissionKind::ScreenRecording,
            })
        }
        _ => None,
    };
    if let Some(kind) = device {
        return permissions()
            .filter(|p| p.permissions.get(kind) != Permission::Granted)
            .map(|_| pane(kind).to_string());
    }
    let paths: Vec<&str> = match source {
        SourceKind::Media(s) => s.files.iter().map(String::as_str).collect(),
        SourceKind::Image(s) => vec![&s.path],
        SourceKind::Slideshow(s) => s.files.iter().map(String::as_str).collect(),
        SourceKind::Text(s) => s.file.as_deref().into_iter().collect(),
        SourceKind::Browser(s) => s.file.as_deref().into_iter().collect(),
        _ => Vec::new(),
    };
    for path in paths {
        let path = isocast_engine::record::expand_home(path);
        for folder in ["Documents", "Desktop", "Downloads"] {
            if std::env::var_os("HOME")
                .is_some_and(|home| path.starts_with(Path::new(&home).join(folder)))
            {
                return Some(folder.into());
            }
        }
        if path.starts_with("/Volumes") {
            return Some("removable volumes".into());
        }
    }
    None
}
use isocast_protocol::devices::{
    Permission, PermissionAction, PermissionDetail, PermissionKind, Permissions, PermissionsReport,
};

/// The Privacy & Security pane for `permission`.
pub(crate) fn settings_url(permission: PermissionKind) -> &'static str {
    match permission {
        PermissionKind::ScreenRecording => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
        }
        PermissionKind::Camera => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Camera"
        }
        PermissionKind::Microphone => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
        }
        // The same pane as screen recording ("System Audio Recording Only").
        PermissionKind::SystemAudio => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_AudioCapture"
        }
    }
}

/// The pane's name in System Settings → Privacy & Security.
fn pane(permission: PermissionKind) -> &'static str {
    match permission {
        PermissionKind::ScreenRecording => "Screen & System Audio Recording",
        PermissionKind::Camera => "Camera",
        PermissionKind::Microphone => "Microphone",
        PermissionKind::SystemAudio => {
            "Screen & System Audio Recording (System Audio Recording Only)"
        }
    }
}

/// The app bundles this executable is in, innermost first (the engine's
/// own "Isocast Engine.app", then the "Isocast.app" it ships inside).
fn bundles() -> Vec<PathBuf> {
    let exe = std::env::current_exe().unwrap_or_default();
    exe.ancestors()
        .filter(|path| path.extension().is_some_and(|e| e == "app"))
        .map(Path::to_path_buf)
        .collect()
}

/// The bundle macOS lists `permission` under, if this process runs from
/// one: the outermost app (Isocast.app, not the Isocast Engine.app inside
/// it), for all three. tccd says so on a Developer ID build (isocast-m2-a,
/// macOS 27, 2026-10-15): "executable path …/Isocast Engine.app/Contents/
/// MacOS/isocastd resolves to attributed bundle: …/Isocast.app/" (the
/// team ids match), so screen recording, camera and microphone are all
/// checked against dev.isocast.release, whose usage strings and
/// entitlements the prompts use.
pub(crate) fn listed_bundle(_permission: PermissionKind) -> Option<PathBuf> {
    bundles().last().cloned()
}

/// A bundle's name as System Settings shows it.
fn bundle_name(bundle: &Path) -> Option<String> {
    bundle
        .file_stem()
        .map(|name| name.to_string_lossy().into_owned())
}

/// What macOS lists this process's grants under: its outermost app
/// bundle's name ("Isocast"; "Isocast Dev" for the dev app), else the
/// executable's name.
pub(crate) fn app_name() -> String {
    let exe = std::env::current_exe().unwrap_or_default();
    bundles()
        .last()
        .and_then(|bundle| bundle_name(bundle))
        .or_else(|| exe.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "isocastd".into())
}

/// Whether this process runs from an app bundle; otherwise macOS may hold
/// its grants under the app that started it (like a terminal).
fn bundled() -> bool {
    !bundles().is_empty()
}

/// Permissions macOS refused without showing its prompt (by
/// `PermissionKind::ALL` index), as `request_permissions` saw.
static UNASKED: [AtomicBool; KINDS] = [const { AtomicBool::new(false) }; KINDS];

/// How many permissions there are.
const KINDS: usize = PermissionKind::ALL.len();

fn index(kind: PermissionKind) -> usize {
    PermissionKind::ALL
        .iter()
        .position(|k| *k == kind)
        .unwrap_or(0)
}

/// Note how a request for `kind` was answered: refused faster than anyone
/// could read a prompt means macOS never showed one. Only in a session
/// with a screen: an engine started over ssh is refused at once because
/// there is nobody to ask (and its grants read as denied), which an
/// update wouldn't change.
pub(crate) fn answered(kind: PermissionKind, granted: bool, after: Duration) {
    let unasked = !granted && after < Duration::from_millis(600) && graphic_session();
    UNASKED[index(kind)].store(unasked, Ordering::Relaxed);
}

/// Whether this process runs in a login session that can show windows
/// (the app's, or a Terminal on the Mac's screen), not an ssh session's.
fn graphic_session() -> bool {
    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        fn SessionGetInfo(session: u32, id: *mut u32, attributes: *mut u32) -> i32;
    }
    /// `callerSecuritySession` and `sessionHasGraphicAccess` (AuthSession.h).
    const CALLER: u32 = u32::MAX;
    const HAS_GRAPHIC_ACCESS: u32 = 0x0010;
    static GRAPHIC: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *GRAPHIC.get_or_init(|| {
        let (mut id, mut attributes) = (0, 0);
        // SAFETY: plain out-parameters, valid for the call.
        let status = unsafe { SessionGetInfo(CALLER, &mut id, &mut attributes) };
        // Unreadable: assume a screen (the app's case), as before.
        status != 0 || attributes & HAS_GRAPHIC_ACCESS != 0
    })
}

/// The argument isocastd is run with to read the permissions afresh (see
/// [`granted_after_restart`]).
pub const PREFLIGHT_ARG: &str = "--preflight-permissions";

/// The preflight child's exit status: a bit (by `PermissionKind::ALL`
/// index) for each permission that is *not* granted, so 0 is all of them.
pub(crate) fn preflight_status(permissions: &Permissions) -> u8 {
    PermissionKind::ALL
        .into_iter()
        .filter(|kind| permissions.get(*kind) != Permission::Granted)
        .fold(0, |bits, kind| bits | 1 << index(kind))
}

/// Which permissions a fresh isocastd process would see granted (by
/// `PermissionKind::ALL` index), or `None` if it couldn't be asked. macOS
/// applies a grant made in System Settings only once the responsible
/// process starts again ("may not be able to record until it is quit";
/// for the camera and microphone "will not have access … until it is
/// quit"), so a running engine keeps its old answer. The child disclaims
/// isocastd's responsibility, so it is checked as the engine's bundle,
/// afresh (as its own child it inherited the stale answer: measured on
/// isocast-m2-a, macOS 27).
fn granted_after_restart() -> Option<[bool; KINDS]> {
    let exe = std::env::current_exe().ok()?;
    if exe
        .file_name()
        .is_none_or(|name| !isocast_protocol::api::is_engine_executable(name))
    {
        return None;
    }
    match spawn_disclaimed(&exe, PREFLIGHT_ARG) {
        Ok(pid) => {
            let bits = wait_status(pid, Duration::from_secs(3))?;
            (bits < 1 << KINDS).then(|| std::array::from_fn(|i| bits & (1 << i) == 0))
        }
        Err(error) => {
            isocast_diagnostics::diagnostic!("warn";
                "permissions: could not check the permissions afresh: {error}");
            None
        }
    }
}

/// Start `exe arg` as its own responsible process, its output discarded.
fn spawn_disclaimed(exe: &Path, arg: &str) -> std::io::Result<libc::pid_t> {
    use std::ffi::{CString, c_char, c_int};
    use std::os::unix::ffi::OsStrExt;

    unsafe extern "C" {
        // libSystem; not in public headers, but stable since macOS 10.14
        // (the client and plugins use it the same way).
        fn responsibility_spawnattrs_setdisclaim(
            attrs: *mut libc::posix_spawnattr_t,
            disclaim: c_int,
        ) -> c_int;
    }
    let check = |code: c_int| {
        if code == 0 {
            Ok(())
        } else {
            Err(std::io::Error::from_raw_os_error(code))
        }
    };
    let program = CString::new(exe.as_os_str().as_bytes())?;
    let arg = CString::new(arg)?;
    let argv = [program.as_ptr(), arg.as_ptr(), std::ptr::null()];
    let envp: [*const c_char; 1] = [std::ptr::null()];
    let null = c"/dev/null";
    // SAFETY: the attributes and actions are initialised before use and
    // destroyed after; every pointer passed outlives the call.
    unsafe {
        let mut attr: libc::posix_spawnattr_t = std::ptr::null_mut();
        check(libc::posix_spawnattr_init(&mut attr))?;
        let mut actions: libc::posix_spawn_file_actions_t = std::ptr::null_mut();
        if let Err(error) = check(libc::posix_spawn_file_actions_init(&mut actions)) {
            libc::posix_spawnattr_destroy(&mut attr);
            return Err(error);
        }
        let result = (|| {
            check(libc::posix_spawnattr_setflags(
                &mut attr,
                libc::POSIX_SPAWN_CLOEXEC_DEFAULT as _,
            ))?;
            check(responsibility_spawnattrs_setdisclaim(&mut attr, 1))?;
            for fd in 0..3 {
                let flags = if fd == 0 {
                    libc::O_RDONLY
                } else {
                    libc::O_WRONLY
                };
                check(libc::posix_spawn_file_actions_addopen(
                    &mut actions,
                    fd,
                    null.as_ptr(),
                    flags,
                    0,
                ))?;
            }
            let mut pid = 0;
            check(libc::posix_spawn(
                &mut pid,
                program.as_ptr(),
                &actions,
                &attr,
                argv.as_ptr().cast(),
                envp.as_ptr().cast(),
            ))?;
            Ok(pid)
        })();
        libc::posix_spawn_file_actions_destroy(&mut actions);
        libc::posix_spawnattr_destroy(&mut attr);
        result
    }
}

/// Wait up to `limit` for `pid` to exit: its exit status, or `None` if it
/// was killed by a signal or took longer (then it is killed and reaped).
fn wait_status(pid: libc::pid_t, limit: Duration) -> Option<u8> {
    let deadline = std::time::Instant::now() + limit;
    loop {
        let mut status = 0;
        // SAFETY: our own child; WNOHANG never blocks.
        let done = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if done == pid {
            return libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status) as u8);
        }
        if done < 0 || std::time::Instant::now() >= deadline {
            // SAFETY: our own child, still unreaped.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, &mut status, 0);
            }
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Each permission with its next step, as this process sees them now,
/// and which grants wait for a restart (a fresh process is asked only
/// when something isn't granted here, and only from an app bundle). A
/// bare isocastd started from a terminal has the terminal's grants while
/// the disclaimed child is checked as itself, so their answers can differ
/// for good and a restart wouldn't help; one the CLI launched is disclaimed
/// itself, so the check would work there, but a process can't tell which
/// it is, so bare engines skip it.
pub(crate) fn live_report(permissions: Permissions) -> PermissionsReport {
    // Only a decided permission can be turned on in System Settings
    // meanwhile; one never asked for (often system audio) can't be.
    let missing = bundled()
        && PermissionKind::ALL.into_iter().any(|kind| {
            !matches!(
                permissions.get(kind),
                Permission::Granted | Permission::NotDetermined
            )
        });
    let fresh = missing.then(granted_after_restart).flatten();
    let waiting: [bool; KINDS] = std::array::from_fn(|i| {
        fresh.is_some_and(|fresh| fresh[i])
            && permissions.get(PermissionKind::ALL[i]) != Permission::Granted
    });
    report_with(permissions, waiting)
}

/// Each permission with its next step.
pub(crate) fn report(permissions: Permissions) -> PermissionsReport {
    report_with(permissions, [false; KINDS])
}

/// Each permission with its next step; `waiting` (by `PermissionKind::ALL`
/// index): granted, but this process sees it only after a restart.
fn report_with(permissions: Permissions, waiting: [bool; KINDS]) -> PermissionsReport {
    let app = app_name();
    let details = PermissionKind::ALL
        .into_iter()
        .map(|kind| {
            let state = permissions.get(kind);
            let bundle = listed_bundle(kind);
            let listed = bundle
                .as_deref()
                .and_then(bundle_name)
                .unwrap_or_else(|| app.clone());
            let whom = if bundled() {
                format!("“{listed}”")
            } else {
                format!("“{listed}” (or the app that started it, like a terminal)")
            };
            let flag = PermissionKind::NAMES[index(kind)];
            let settings = format!(
                "open System Settings → Privacy & Security → {} (`isocast permissions --open \
                 {flag}`) and turn on {whom}",
                pane(kind)
            );
            let screen = kind == PermissionKind::ScreenRecording;
            let unasked =
                state != Permission::Granted && UNASKED[index(kind)].load(Ordering::Relaxed);
            let (action, next_step) = match state {
                Permission::Granted => (PermissionAction::None, None),
                _ if waiting[index(kind)] => (
                    PermissionAction::Restart,
                    Some(format!(
                        "{} is turned on in System Settings, but isocastd sees it only once it \
                         starts again: restart it (`isocast restart`)",
                        kind.label()
                    )),
                ),
                // macOS doesn't say whether it asked before, so: ask (which
                // adds the app to the list the first time), then settings.
                _ if screen => (
                    PermissionAction::OpenSettings,
                    Some(format!(
                        "{} isn't allowed: {settings} (if it isn't listed, `isocast permissions \
                         --request` adds it), then restart isocastd (`isocast restart`)",
                        kind.label()
                    )),
                ),
                _ if unasked => (
                    PermissionAction::OpenSettings,
                    Some(format!(
                        "macOS refused {} access without asking: this copy of {app} isn't \
                         allowed to ask for it. Install the latest {app} (Check for Updates…), \
                         then ask again; or {settings}",
                        kind.label()
                    )),
                ),
                Permission::NotDetermined if kind == PermissionKind::SystemAudio => (
                    PermissionAction::Request,
                    Some(format!(
                        "macOS hasn't asked yet: `isocast permissions --request {flag}` shows \
                         its prompt (until then, system and app audio are recorded through \
                         screen recording where that is allowed)"
                    )),
                ),
                Permission::NotDetermined => (
                    PermissionAction::Request,
                    Some(format!(
                        "macOS hasn't asked yet: `isocast permissions --request {flag}` shows \
                         its prompt (recording with the {} also asks)",
                        kind.label()
                    )),
                ),
                Permission::Restricted => (
                    PermissionAction::AskAdministrator,
                    Some(format!(
                        "{} access is blocked by a policy (device management or Screen Time); \
                         only this Mac's administrator can allow it",
                        kind.label()
                    )),
                ),
                Permission::Denied | Permission::NotGranted => (
                    PermissionAction::OpenSettings,
                    Some(format!("{} access was refused: {settings}", kind.label())),
                ),
            };
            PermissionDetail {
                permission: kind,
                state,
                needed_for: match kind {
                    PermissionKind::ScreenRecording => {
                        "displays, windows, apps and desktop audio".into()
                    }
                    PermissionKind::Camera => {
                        "cameras, capture cards and iPhone or iPad screens".into()
                    }
                    PermissionKind::Microphone => "microphones and audio interfaces".into(),
                    PermissionKind::SystemAudio => {
                        "system and app audio without screen recording (macOS 14.2+)".into()
                    }
                },
                action,
                settings_url: settings_url(kind).into(),
                next_step,
                restart_after_granting: screen && state != Permission::Granted,
                app: listed,
                app_path: bundle.map(|b| b.to_string_lossy().into_owned()),
                denied_without_prompt: unasked,
            }
        })
        .collect();
    PermissionsReport {
        permissions,
        app,
        details,
    }
}

/// Open the System Settings pane for `permission`.
pub(crate) fn open_settings(permission: PermissionKind) -> Result<String> {
    let url = settings_url(permission);
    let status = std::process::Command::new("/usr/bin/open")
        .arg(url)
        .status()
        .context("could not run /usr/bin/open")?;
    if !status.success() {
        bail!("`open {url}` failed ({status})");
    }
    Ok(url.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_restart_is_offered_for_any_permission_granted_afresh() {
        let denied = Permissions {
            screen_recording: Permission::Granted,
            camera: Permission::Denied,
            microphone: Permission::NotDetermined,
            system_audio: Permission::Granted,
        };
        assert_eq!(preflight_status(&denied), 0b110);
        let report = report_with(denied, [false, true, false, false]);
        assert_eq!(report.details[1].action, PermissionAction::Restart);
        assert_eq!(report.details[2].action, PermissionAction::Request);
    }

    #[test]
    fn each_state_gets_its_next_step() {
        let report = report(Permissions {
            screen_recording: Permission::NotGranted,
            camera: Permission::Denied,
            microphone: Permission::Restricted,
            system_audio: Permission::NotDetermined,
        });
        let [screen, camera, microphone, system_audio] = &report.details[..] else {
            panic!("{report:?}");
        };
        assert_eq!(screen.action, PermissionAction::OpenSettings);
        assert!(screen.restart_after_granting);
        let step = screen.next_step.as_deref().unwrap();
        assert!(step.contains("Screen & System Audio Recording"), "{step}");
        assert!(step.contains("isocast restart"), "{step}");
        assert_eq!(camera.action, PermissionAction::OpenSettings);
        assert!(camera.settings_url.ends_with("Privacy_Camera"));
        assert!(!camera.restart_after_granting);
        assert_eq!(microphone.action, PermissionAction::AskAdministrator);
        assert_eq!(system_audio.action, PermissionAction::Request);
        let step = system_audio.next_step.as_deref().unwrap();
        assert!(step.contains("--request system_audio"), "{step}");
        let granted = super::report(Permissions {
            screen_recording: Permission::Granted,
            camera: Permission::NotDetermined,
            microphone: Permission::Granted,
            system_audio: Permission::Denied,
        });
        assert_eq!(granted.details[0].next_step, None);
        assert_eq!(granted.details[1].action, PermissionAction::Request);
        assert_eq!(granted.details[3].action, PermissionAction::OpenSettings);
        assert!(
            granted.details[3]
                .settings_url
                .ends_with("Privacy_AudioCapture")
        );
    }
}
