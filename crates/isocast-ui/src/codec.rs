//! A Linux engine or this app without a usable H.264 codec (OpenH264,
//! crates/isocast-openh264): the fix offered with a refused recording or
//! stream, and the status bar's notice while it lasts (until dismissed
//! for this run). An engine on another computer is told about in words;
//! only this computer's own installer is ever run.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use slint::ComponentHandle;

use crate::app::App;
use crate::toasts::Toast;
use crate::{AppWindow, ControlsArea, ToastKind};

/// The installer the Debian and RPM packages put on the PATH: the only
/// command the app runs itself (never one an engine names).
const INSTALLER: &str = "/usr/bin/isocast-openh264-install";

/// Why H.264 can't be used, and what fixes it, in isocast-openh264's
/// words (its `Unavailable`).
#[derive(Clone, Debug, PartialEq)]
pub struct Missing {
    reason: String,
    fix: String,
    /// On this computer: this app's own decoder, or a local engine.
    local: bool,
}

impl Missing {
    /// An engine's refusal (`…; fix: …`) when it is about OpenH264.
    pub fn from_refusal(message: &str) -> Option<Self> {
        let (reason, fix) = message.rsplit_once("; fix: ")?;
        reason.contains("OpenH264").then(|| Self {
            reason: reason.trim().to_owned(),
            fix: fix.trim().to_owned(),
            local: crate::engine::is_local(),
        })
    }

    /// The engine's doctor check `codec: OpenH264`.
    pub fn from_check(detail: &str, fix: &str) -> Self {
        Self {
            reason: detail.to_owned(),
            fix: fix.to_owned(),
            local: crate::engine::is_local(),
        }
    }

    /// No library at all (rather than one that was refused): Cisco's or
    /// the system's.
    fn absent(&self) -> bool {
        self.reason.contains("No OpenH264 found") || self.reason.contains("is missing from")
    }

    /// The fix as a command to copy, when it is one (not prose such as
    /// "install your distro's OpenH264"). An engine's messages can name
    /// the home folder as `~`.
    fn command(&self) -> Option<&str> {
        let path = self.fix.starts_with('/') || self.fix.starts_with("~/");
        (path && !self.fix.contains(" to ")).then_some(self.fix.as_str())
    }

    /// The installer can run here, as the user: the packaged one, for
    /// this computer's own codec, unless a library was chosen by hand.
    fn downloadable(&self) -> bool {
        #[cfg(target_os = "linux")]
        return self.local
            && self.fix == INSTALLER
            && std::env::var_os(isocast_openh264::ENV).is_none();
        #[cfg(not(target_os = "linux"))]
        {
            let _ = INSTALLER;
            false
        }
    }

    /// Where the command runs: here, or on the engine's computer.
    fn where_to_run(&self) -> String {
        if self.local {
            String::new()
        } else {
            match crate::engine::name() {
                Some(name) => format!(" on {name}"),
                None => " on the engine’s computer".into(),
            }
        }
    }

    /// What's wrong and what to do, in plain words.
    pub fn message(&self) -> String {
        let at = self.where_to_run();
        if !self.absent() {
            let reason = self
                .reason
                .split_once('\n')
                .map_or(&*self.reason, |(s, _)| s);
            return format!(
                "Isocast’s H.264 codec (OpenH264) can’t be used: {reason}. To fix it{at}: {}.",
                self.fix
            );
        }
        let need = "Isocast needs an H.264 codec to record and stream.";
        if self.downloadable() {
            format!(
                "{need} Download Cisco’s OpenH264 for your user (no sudo), or install your distro’s OpenH264 2.6 or later."
            )
        } else if self.command().is_some() {
            format!(
                "{need} Copy the command that downloads Cisco’s OpenH264 and run it in a terminal{at}, or install your distro’s OpenH264 2.6 or later."
            )
        } else {
            format!("{need} Install your distro’s OpenH264 2.6 or later{at}.")
        }
    }

    /// The first run's line, which has no button: the command itself.
    pub fn hint(&self) -> String {
        match self.command() {
            Some(command) if !self.downloadable() && self.absent() => {
                format!("{} The command: {command}", self.message())
            }
            _ => self.message(),
        }
    }

    /// The action's label, if there is one.
    fn action(&self) -> Option<&'static str> {
        if self.downloadable() {
            Some("Download OpenH264")
        } else {
            self.command().map(|_| "Copy Command")
        }
    }
}

/// What's missing, from the engine and from this app's own decoder.
#[derive(Default)]
pub struct Codec {
    engine: RefCell<Option<Missing>>,
    decoder: RefCell<Option<Missing>>,
    /// The notice was closed: not again until the app starts again.
    dismissed: Cell<bool>,
}

impl Codec {
    pub fn connect(app: &Rc<App>, window: &AppWindow) {
        let area = window.global::<ControlsArea>();
        let weak = Rc::downgrade(app);
        area.on_codec_act(move || {
            if let Some(app) = weak.upgrade()
                && let Some(missing) = app.codec.shown()
            {
                act(&app, &missing);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_codec_dismiss(move || {
            if let Some(app) = weak.upgrade() {
                app.codec.dismissed.set(true);
                app.codec.show(&app);
            }
        });
    }

    /// The engine's word: missing, or usable (`None`).
    pub fn set_engine(&self, app: &Rc<App>, missing: Option<Missing>) {
        *self.engine.borrow_mut() = missing;
        self.show(app);
    }

    /// This app can't decode the engine's video (previews show stills).
    #[cfg(target_os = "linux")]
    pub fn set_decoder(&self, app: &Rc<App>, missing: Option<Missing>) {
        *self.decoder.borrow_mut() = missing;
        self.show(app);
    }

    fn shown(&self) -> Option<Missing> {
        self.engine
            .borrow()
            .clone()
            .or_else(|| self.decoder.borrow().clone())
    }

    fn show(&self, app: &Rc<App>) {
        let Some(window) = app.window() else { return };
        let area = window.global::<ControlsArea>();
        let missing = self.shown().filter(|_| !self.dismissed.get());
        let (tooltip, action) = match &missing {
            Some(missing) => (missing.message(), missing.action().unwrap_or_default()),
            None => (String::new(), ""),
        };
        area.set_codec_off(missing.is_some());
        area.set_codec_tooltip(tooltip.into());
        area.set_codec_action(action.into());
    }

    /// A recording or stream the engine refused: the plain reason and the
    /// fix, when the codec is why. Returns whether it was.
    pub fn refused(&self, app: &Rc<App>, title: &str, message: &str, key: String) -> bool {
        let Some(missing) = Missing::from_refusal(message) else {
            return false;
        };
        isocast_diagnostics::diagnostic!("warn"; "codec: {message}");
        let mut toast = Toast::new(ToastKind::Error, title, missing.message()).key(key);
        if let Some(label) = missing.action() {
            let shown = missing.clone();
            toast = toast.action(label, move |app| act(app, &shown));
        }
        app.show_toast(toast);
        // The notice comes back with a new refusal, even if closed.
        self.dismissed.set(false);
        self.set_engine(app, Some(missing));
        true
    }
}

/// Download Cisco's codec, or copy the command that does.
fn act(app: &Rc<App>, missing: &Missing) {
    if missing.downloadable() {
        #[cfg(target_os = "linux")]
        download(app);
        return;
    }
    if let Some(command) = missing.command() {
        let at = missing.where_to_run();
        app.copy(
            command,
            "Copied the command",
            format!("Run it in a terminal{at}, then try again."),
            format!("Run this in a terminal{at}: {command}"),
        );
    }
}

/// The app's own decoder found no codec: say so in the status bar.
#[cfg(target_os = "linux")]
pub fn decoder_missing() {
    let Err(error) = isocast_openh264::check() else {
        return;
    };
    let missing = Missing {
        reason: error.reason,
        fix: error.fix,
        local: true,
    };
    let _ = slint::invoke_from_event_loop(move || {
        crate::window::with_app(|app| app.codec.set_decoder(app, Some(missing)));
    });
}

/// The first run's offer (Linux, a local engine that the packaged
/// installer can fix). "Not now" leaves the status bar's notice.
#[cfg(target_os = "linux")]
pub fn offer(app: &Rc<App>) {
    app.show_toast(Toast::new(ToastKind::Info, "Enable software H.264 and live previews?",
        "OpenH264 Video Codec provided by Cisco Systems, Inc. Download from Cisco for your user; no sudo. Remove later with isocast-openh264-install --remove.")
        .action2("Not now", |_| {})
        .action("Download", download));
}

/// Whether the first run can offer the download for this check's fix.
#[cfg(target_os = "linux")]
pub fn can_offer(fix: &str) -> bool {
    Missing::from_check("", fix).downloadable()
}

#[cfg(target_os = "linux")]
fn download(app: &Rc<App>) {
    const KEY: &str = "openh264-download";
    let mut progress = Toast::new(
        ToastKind::Info,
        "Downloading OpenH264",
        "Fetching and verifying Cisco’s codec…",
    )
    .key(KEY);
    progress.persistent = true;
    app.show_toast(progress);
    std::thread::spawn(|| {
        // Never execute a PATH lookup or a command supplied by an engine.
        let result = std::process::Command::new(INSTALLER).output();
        let _ = slint::invoke_from_event_loop(move || {
            crate::window::with_app(|app| {
                let notice = match result {
                    Ok(o) if o.status.success() => {
                        app.codec.set_decoder(app, None);
                        app.codec.set_engine(app, None);
                        // The engine's own check, again (it may differ).
                        app.wizard.linux_first_run(app, false);
                        Toast::new(
                            ToastKind::Success,
                            "OpenH264 installed",
                            "Software H.264 and live previews are available.",
                        )
                    }
                    Ok(o) => {
                        let stderr = String::from_utf8_lossy(&o.stderr);
                        let reason = stderr
                            .lines()
                            .rev()
                            .map(str::trim)
                            .find(|s| !s.is_empty())
                            .map(str::to_owned)
                            .unwrap_or_else(|| format!("Installer exited with {}.", o.status));
                        Toast::new(ToastKind::Error, "OpenH264 wasn’t installed", reason)
                    }
                    Err(error) => Toast::new(
                        ToastKind::Error,
                        "Couldn’t start isocast-openh264-install",
                        error.to_string(),
                    ),
                };
                app.show_toast(notice.key(KEY));
            })
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn missing(reason: &str, fix: &str) -> Missing {
        Missing {
            reason: reason.into(),
            fix: fix.into(),
            local: true,
        }
    }

    #[test]
    fn a_refusal_about_openh264_says_what_to_do_in_plain_words() {
        let refusal = "program: No OpenH264 found.\ncould not load libopenh264.so.8: cannot open \
                       shared object file (system libopenh264.so.8; no per-user Cisco download); \
                       fix: /home/me/.local/lib/isocast/install.sh --openh264";
        let found = Missing::from_refusal(refusal).unwrap();
        assert_eq!(
            found.fix,
            "/home/me/.local/lib/isocast/install.sh --openh264"
        );
        assert_eq!(found.action(), Some("Copy Command"));
        let message = found.message();
        assert!(
            message.starts_with("Isocast needs an H.264 codec"),
            "{message}"
        );
        assert!(!message.contains("libopenh264.so.8"), "{message}");
        let tilde = Missing::from_refusal(
            "No OpenH264 found.\ncould not load libopenh264.so.8; fix: ~/.local/lib/isocast/install.sh --openh264",
        )
        .unwrap();
        assert_eq!(tilde.action(), Some("Copy Command"));
        // Other refusals are left alone.
        assert!(Missing::from_refusal("disk full; fix: free space").is_none());
        assert!(Missing::from_refusal("No OpenH264 found.").is_none());
    }

    #[test]
    fn prose_fixes_and_rejected_libraries_offer_no_command() {
        let prose = missing(
            "No OpenH264 found.\ncould not load libopenh264.so.8",
            "install your distro's OpenH264 2.x (2.6 or later, libopenh264.so.8)",
        );
        assert_eq!(prose.action(), None);
        assert!(prose.message().contains("Install your distro’s OpenH264"));
        let rejected = missing(
            "OpenH264 2.4.1 is incompatible; need version 2.6 or later in major 2",
            "/i/install.sh --openh264 to replace it, or /i/install.sh --remove-openh264 to use the system library",
        );
        assert_eq!(rejected.action(), None);
        assert!(
            rejected
                .message()
                .contains("can’t be used: OpenH264 2.4.1 is incompatible"),
            "{}",
            rejected.message()
        );
    }
}
