//! Host-specific words for user-facing copy, not identifiers or saved values.
//! Non-Windows copy stays unchanged while the Windows clients share one vocabulary.

pub const fn select(mac: &'static str, windows: &'static str) -> &'static str {
    if cfg!(windows) { windows } else { mac }
}

pub const MACHINE: &str = select("Mac", "PC");
pub const OS: &str = select("macOS", "Windows");
pub const CREDENTIAL_STORE: &str = select("Keychain", "Credential Manager");
pub const FILE_MANAGER: &str = select("Finder", "Explorer");
pub const SHOW_IN_FILES: &str = select("Show in Finder", "Show in Explorer");
pub const SETTINGS: &str = select("System Settings", "Settings");
pub const ENGINE: &str = select("Isocast Engine", "isocastd.exe");

/// Common terms in statically authored tool descriptions. Do not apply this
/// to user text, names, saved data or OS-specific permission instructions.
pub fn description(mac: &'static str) -> String {
    if cfg!(windows) {
        mac.replace("this Mac", "this PC")
            .replace("macOS Keychain", "Credential Manager")
            .replace("Keychain", "Credential Manager")
            .replace("~/Movies/Isocast", "~/Videos/Isocast")
    } else {
        mac.to_string()
    }
}
