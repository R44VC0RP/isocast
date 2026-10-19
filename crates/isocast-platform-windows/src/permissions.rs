//! Windows asks no permission for screen, window or output-audio capture,
//! and desktop apps get no per-app prompt for cameras and microphones.
//! What can stop them is a privacy switch in Settings: Camera or
//! Microphone access for this device, for this user, or for desktop apps.
//! Those switches are read here, from the consent store Settings writes
//! (never written, and nothing is opened or prompted by reading them).

use anyhow::{Result, bail};
use isocast_protocol::devices::{
    Permission, PermissionAction, PermissionDetail, PermissionKind, Permissions, PermissionsReport,
};
use windows::Win32::System::Registry::{HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, PCWSTR, w};

use crate::system::registry_string_in;

/// Where Settings › Privacy keeps a capability's switches.
const CONSENT_STORE: &str =
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore";

/// Which privacy switch turns a capability off for Isocast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Switch {
    /// "Camera access" for every user of this PC (an administrator's).
    Device,
    /// "Camera access" / "Let apps access your camera" for this user.
    User,
    /// "Let desktop apps access your camera".
    DesktopApps,
}

/// The switch that blocks `capability` (`webcam`, `microphone`), if any.
/// A missing value is Windows' default: allowed.
fn blocked(capability: &str) -> Option<Switch> {
    let denied = |root: HKEY, subkey: String| {
        let subkey = HSTRING::from(subkey);
        registry_string_in(root, PCWSTR(subkey.as_ptr()), w!("Value"))
            .is_some_and(|value| value.eq_ignore_ascii_case("Deny"))
    };
    let key = format!(r"{CONSENT_STORE}\{capability}");
    if denied(HKEY_LOCAL_MACHINE, key.clone()) {
        Some(Switch::Device)
    } else if denied(HKEY_CURRENT_USER, key.clone()) {
        Some(Switch::User)
    } else if denied(HKEY_CURRENT_USER, format!(r"{key}\NonPackaged")) {
        Some(Switch::DesktopApps)
    } else {
        None
    }
}

fn capability(permission: PermissionKind) -> Option<&'static str> {
    match permission {
        PermissionKind::Camera => Some("webcam"),
        PermissionKind::Microphone => Some("microphone"),
        PermissionKind::ScreenRecording | PermissionKind::SystemAudio => None,
    }
}

fn state(permission: PermissionKind) -> (Permission, Option<Switch>) {
    match capability(permission).and_then(blocked) {
        Some(switch) => (Permission::Denied, Some(switch)),
        None => (Permission::Granted, None),
    }
}

pub(crate) fn states() -> Permissions {
    Permissions {
        screen_recording: state(PermissionKind::ScreenRecording).0,
        camera: state(PermissionKind::Camera).0,
        microphone: state(PermissionKind::Microphone).0,
        system_audio: state(PermissionKind::SystemAudio).0,
    }
}

fn settings_url(permission: PermissionKind) -> &'static str {
    match permission {
        PermissionKind::Camera => "ms-settings:privacy-webcam",
        PermissionKind::Microphone => "ms-settings:privacy-microphone",
        // Nothing to switch on: the privacy overview, if asked for at all.
        PermissionKind::ScreenRecording | PermissionKind::SystemAudio => "ms-settings:privacy",
    }
}

/// What to do about a switch that's off, in Settings' words.
fn next_step(permission: PermissionKind, switch: Switch) -> String {
    let (device, page) = match permission {
        PermissionKind::Microphone => ("microphone", "Microphone"),
        _ => ("camera", "Camera"),
    };
    let which = match switch {
        Switch::Device => format!(
            "{page} access is off for this PC. Turn on {page} access (an administrator may \
             need to)"
        ),
        Switch::User => format!("{page} access is off. Turn on {page} access"),
        Switch::DesktopApps => format!(
            "Desktop apps can't use the {device}. Turn on Let desktop apps access your {device}"
        ),
    };
    format!(
        "{which} in Settings › Privacy & security › {page}, then come back: this updates by \
         itself."
    )
}

pub(crate) fn report() -> PermissionsReport {
    let details = PermissionKind::ALL
        .into_iter()
        .map(|permission| {
            let (state, switch) = state(permission);
            let needed_for = match permission {
                PermissionKind::ScreenRecording => "Display and window capture",
                PermissionKind::Camera => "Cameras and capture cards",
                PermissionKind::Microphone => "Microphones and audio interfaces",
                PermissionKind::SystemAudio => "What this PC plays",
            };
            PermissionDetail {
                permission,
                state,
                needed_for: needed_for.into(),
                action: if switch.is_some() {
                    PermissionAction::OpenSettings
                } else {
                    PermissionAction::None
                },
                settings_url: settings_url(permission).into(),
                next_step: switch.map(|switch| next_step(permission, switch)),
                restart_after_granting: false,
                app: "Isocast (desktop)".into(),
                app_path: None,
                denied_without_prompt: false,
            }
        })
        .collect();
    PermissionsReport {
        permissions: states(),
        app: "Isocast (desktop)".into(),
        details,
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
