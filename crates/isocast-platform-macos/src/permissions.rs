//! What to do about each privacy permission: whom macOS lists it under,
//! the System Settings pane that grants it, and whether isocastd must
//! restart afterwards.

use std::path::Path;

use anyhow::{Context, Result, bail};
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
    }
}

/// The pane's name in System Settings → Privacy & Security.
fn pane(permission: PermissionKind) -> &'static str {
    match permission {
        PermissionKind::ScreenRecording => "Screen & System Audio Recording",
        PermissionKind::Camera => "Camera",
        PermissionKind::Microphone => "Microphone",
    }
}

/// What macOS lists this process's grants under: its app bundle's name
/// ("Isocast Dev" for the dev app), else the executable's name.
pub(crate) fn app_name() -> String {
    let exe = std::env::current_exe().unwrap_or_default();
    exe.ancestors()
        .find(|path| path.extension().is_some_and(|e| e == "app"))
        .and_then(Path::file_stem)
        .or_else(|| exe.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "isocastd".into())
}

/// Whether this process runs from an app bundle; otherwise macOS may hold
/// its grants under the app that started it (like a terminal).
fn bundled() -> bool {
    std::env::current_exe()
        .unwrap_or_default()
        .ancestors()
        .any(|path| path.extension().is_some_and(|e| e == "app"))
}

/// Each permission with its next step.
pub(crate) fn report(permissions: Permissions) -> PermissionsReport {
    let app = app_name();
    let whom = if bundled() {
        format!("“{app}”")
    } else {
        format!("“{app}” (or the app that started it, like a terminal)")
    };
    let details = PermissionKind::ALL
        .into_iter()
        .map(|kind| {
            let state = permissions.get(kind);
            let flag = match kind {
                PermissionKind::ScreenRecording => "screen_recording",
                PermissionKind::Camera => "camera",
                PermissionKind::Microphone => "microphone",
            };
            let settings = format!(
                "open System Settings → Privacy & Security → {} (`isocast permissions --open \
                 {flag}`) and turn on {whom}",
                pane(kind)
            );
            let screen = kind == PermissionKind::ScreenRecording;
            let (action, next_step) = match state {
                Permission::Granted => (PermissionAction::None, None),
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
                Permission::NotDetermined => (
                    PermissionAction::Request,
                    Some(format!(
                        "macOS hasn't asked yet: `isocast permissions --request` shows its \
                         prompt (recording with the {} also asks)",
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
                },
                action,
                settings_url: settings_url(kind).into(),
                next_step,
                restart_after_granting: screen && state != Permission::Granted,
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
    fn each_state_gets_its_next_step() {
        let report = report(Permissions {
            screen_recording: Permission::NotGranted,
            camera: Permission::Denied,
            microphone: Permission::Restricted,
        });
        let [screen, camera, microphone] = &report.details[..] else {
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
        let granted = super::report(Permissions {
            screen_recording: Permission::Granted,
            camera: Permission::NotDetermined,
            microphone: Permission::Granted,
        });
        assert_eq!(granted.details[0].next_step, None);
        assert_eq!(granted.details[1].action, PermissionAction::Request);
    }
}
