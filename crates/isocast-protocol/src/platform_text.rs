//! Host-specific words for user-facing copy, not identifiers or saved values.
//! macOS copy stays unchanged; Windows and Linux share one vocabulary, with
//! Linux's own words where the text names its OS.
//!
//! Words follow the machine they are about (docs/remote-engines.md, decision
//! 22): the engine's words (Keychain or Credential Manager, Finder or
//! Explorer, permissions, recording folders, "this Mac" or "this PC") come
//! from the engine's operating system ([`EngineOs`], from `hello.os`), the
//! app's words (menus, shortcuts) from the app's own. [`select`] and the
//! constants are the app's (this build's) words; [`pick`] and [`Words`] take
//! the operating system the words are about.

use crate::hello::EngineOs;

/// The app's (this build's) words: `mac` on macOS, `windows` elsewhere
/// (Linux uses Windows' conventions and words, as [`pick`] does). Where
/// the Windows words name Windows, use [`select_linux`].
pub const fn select(mac: &'static str, windows: &'static str) -> &'static str {
    if cfg!(target_os = "macos") {
        mac
    } else {
        windows
    }
}

/// [`select`] with Linux's own words, for text that names its OS.
pub const fn select_linux(
    mac: &'static str,
    windows: &'static str,
    linux: &'static str,
) -> &'static str {
    if cfg!(target_os = "macos") {
        mac
    } else if cfg!(windows) {
        windows
    } else {
        linux
    }
}

/// `mac` on macOS, `windows` on Windows: the words about a machine running
/// `os`. Linux (later) and operating systems this build doesn't know get
/// the Windows words, which name no Apple product.
pub const fn pick(os: EngineOs, mac: &'static str, windows: &'static str) -> &'static str {
    match os {
        EngineOs::Macos => mac,
        EngineOs::Windows | EngineOs::Linux | EngineOs::Unknown => windows,
    }
}

/// The nouns for a machine running a given operating system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Words {
    pub machine: &'static str,
    pub os: &'static str,
    pub credential_store: &'static str,
    pub file_manager: &'static str,
    pub show_in_files: &'static str,
    pub settings: &'static str,
    pub engine: &'static str,
}

impl Words {
    pub const fn for_os(os: EngineOs) -> Self {
        match os {
            EngineOs::Macos => Self {
                machine: "Mac",
                os: "macOS",
                credential_store: "Keychain",
                file_manager: "Finder",
                show_in_files: "Show in Finder",
                settings: "System Settings",
                engine: "Isocast Engine",
            },
            EngineOs::Windows => Self {
                machine: "PC",
                os: "Windows",
                credential_store: "Credential Manager",
                file_manager: "Explorer",
                show_in_files: "Show in Explorer",
                settings: "Settings",
                engine: "isocastd.exe",
            },
            EngineOs::Linux => Self {
                machine: "computer",
                os: "Linux",
                credential_store: "keyring",
                file_manager: "file manager",
                show_in_files: "Show in Files",
                settings: "Settings",
                engine: "isocastd",
            },
            EngineOs::Unknown => Self {
                machine: "computer",
                os: "its operating system",
                credential_store: "credential store",
                file_manager: "file manager",
                show_in_files: "Show in Files",
                settings: "Settings",
                engine: "isocastd",
            },
        }
    }
}

// This build's words: its own operating system's (macOS's on others).
const OWN: Words = Words::for_os(match EngineOs::current() {
    EngineOs::Unknown => EngineOs::Macos,
    os => os,
});

pub const MACHINE: &str = OWN.machine;
pub const OS: &str = OWN.os;
pub const CREDENTIAL_STORE: &str = OWN.credential_store;
pub const FILE_MANAGER: &str = OWN.file_manager;
pub const SHOW_IN_FILES: &str = OWN.show_in_files;
pub const SETTINGS: &str = OWN.settings;
pub const ENGINE: &str = OWN.engine;

/// Common terms in statically authored tool descriptions. Do not apply this
/// to user text, names, saved data or OS-specific permission instructions.
pub fn description(mac: &'static str) -> String {
    if cfg!(target_os = "macos") {
        mac.to_string()
    } else {
        mac.replace("this Mac", &format!("this {MACHINE}"))
            .replace("macOS Keychain", CREDENTIAL_STORE)
            .replace("Keychain", CREDENTIAL_STORE)
            .replace("~/Movies/Isocast", "~/Videos/Isocast")
    }
}
