//! Unpackaged desktop apps have no documented, authoritative query-only
//! equivalent of macOS capture consent. Availability is not authorization:
//! enumerate without prompting, then report the real error on requested use.

use anyhow::{Result, bail};
use isocast_protocol::devices::{
    Permission, PermissionAction, PermissionDetail, PermissionKind, Permissions, PermissionsReport,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, w};

pub(crate) fn states() -> Permissions {
    Permissions {
        screen_recording: Permission::Unknown,
        camera: Permission::Unknown,
        microphone: Permission::Unknown,
        system_audio: Permission::Unknown,
    }
}

fn settings_url(permission: PermissionKind) -> &'static str {
    match permission {
        PermissionKind::Camera => "ms-settings:privacy-webcam",
        PermissionKind::Microphone => "ms-settings:privacy-microphone",
        // Windows 10 has no documented equivalent consent page for desktop
        // WGC/endpoint loopback. Offer the overview, not a nonexistent toggle.
        PermissionKind::ScreenRecording | PermissionKind::SystemAudio => "ms-settings:privacy",
    }
}

pub(crate) fn report() -> PermissionsReport {
    PermissionsReport {
        permissions: states(),
        app: "Isocast (desktop)".into(),
        details: PermissionKind::ALL
            .into_iter()
            .map(|permission| {
                let (needed_for, next_step) = match permission {
                    PermissionKind::ScreenRecording => (
                        "Display and window capture",
                        "Access is unknown until a requested capture starts. Windows capture support is not a permission grant. Settings opens the privacy overview; Windows 10 has no equivalent per-app screen-recording switch.",
                    ),
                    PermissionKind::Camera => (
                        "Camera capture",
                        "Camera access is unknown. Check Windows Camera privacy settings, including desktop-app access. Desktop apps do not use an Isocast-specific permission prompt; capture errors are reported when attempted.",
                    ),
                    PermissionKind::Microphone => (
                        "Microphone capture",
                        "Microphone access is unknown. Check Windows Microphone privacy settings, including desktop-app access. Desktop apps do not use an Isocast-specific permission prompt; capture errors are reported when attempted.",
                    ),
                    PermissionKind::SystemAudio => (
                        "WASAPI endpoint loopback",
                        "Endpoint-loopback access is unknown until requested. Settings opens the privacy overview, not a dedicated system-audio consent switch. Process exclusions are not supported by this backend.",
                    ),
                };
                PermissionDetail {
                    permission,
                    state: Permission::Unknown,
                    needed_for: needed_for.into(),
                    action: PermissionAction::OpenSettings,
                    settings_url: settings_url(permission).into(),
                    next_step: Some(next_step.into()),
                    restart_after_granting: false,
                    app: "Isocast (desktop)".into(),
                    app_path: None,
                    denied_without_prompt: false,
                }
            })
            .collect(),
    }
}

/// Only an explicit open-settings request reaches this function. Merely
/// listing/requesting permission never opens UI or changes system settings.
pub(crate) fn open(permission: PermissionKind) -> Result<String> {
    let url = settings_url(permission);
    let wide: Vec<_> = url.encode_utf16().chain(Some(0)).collect();
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(wide.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        )
    };
    if result.0 as isize <= 32 {
        bail!(
            "permission.settings: Windows could not open {url} (ShellExecute code {})",
            result.0 as isize
        );
    }
    Ok(url.into())
}
