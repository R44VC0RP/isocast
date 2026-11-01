//! Area H: the permissions step. Screen Recording, Camera and Microphone,
//! each with what it is for, its live status and one action, read from the
//! engine's `permissions` every second while shown (so rows flip to
//! Allowed on their own). Shown before Welcome on the first launch and at
//! launch whenever the live setup needs a permission it lacks; opened from
//! Settings › General and from a source's permission warning
//! (`Ui.menu("permissions")`).
//!
//! The actions are the engine's, as for the CLI and MCP:
//! `request_permissions` with one permission (macOS's own prompt, on a
//! connection of its own so the window's calls don't wait for the answer),
//! `open_permission_settings` (then the floating helper, src/helper.rs,
//! beside System Settings with the app to drag into its list), and
//! `shutdown_if_idle` for a screen recording grant that waits for the
//! engine to start again (the next call launches it afresh). The app itself
//! needn't relaunch: isocastd is its own responsible process, so only it
//! holds the stale answer.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use isocast_protocol::SourceKind;
use isocast_protocol::api::{Method, RequestPermissionsParams};
use isocast_protocol::devices::{
    Permission, PermissionAction, PermissionDetail, PermissionKind, Permissions, PermissionsReport,
};
use slint::ComponentHandle;

use super::{Area, Changed};
use crate::app::App;
use crate::daemon::{self, Link};
use crate::helper::Helper;
use crate::kit::model;
use crate::{AppWindow, PermissionItem, PermissionsArea, SettingsArea, ToastKind, Ui};

/// This area's snapshot states (see src/areas/mod.rs).
pub const STATES: &[&str] = &[
    "permissions",
    "permissions-sample:first-launch|asking|helper|relaunch|denied|silent|allowed|blocked",
    "permissions-helper:OUT.png[:APP]",
];

/// A request answered on its own connection: what it returned.
type Answer = Arc<Mutex<Option<Result<PermissionsReport, String>>>>;

#[derive(Default)]
pub struct PermissionsStep {
    report: RefCell<Option<PermissionsReport>>,
    /// Offered at launch already (once per run).
    offered: Cell<bool>,
    /// Shown before Welcome (Continue leads on) rather than opened later.
    first_launch: Cell<bool>,
    /// A `permissions` call is on its way.
    checking: Cell<bool>,
    /// Reads the permissions every second while they show.
    timer: slint::Timer,
    /// What is under way, and for which permission.
    busy: Cell<Option<(PermissionKind, &'static str)>>,
    /// The request on its own connection, until it is answered.
    asking: RefCell<Option<Answer>>,
    /// The floating helper beside System Settings, for this permission.
    helper: RefCell<Option<(PermissionKind, Helper)>>,
    /// A snapshot's sample: shown as given, not read from the engine.
    sample: Cell<bool>,
    /// The dialog the step replaced (Settings, from its Review…), opened
    /// again when the step closes.
    back_to: RefCell<Option<(String, String)>>,
    /// Until when Relaunch's stop is expected to break the link.
    relaunch_until: Cell<Option<std::time::Instant>>,
}

impl Area for PermissionsStep {
    fn connect(&self, app: &Rc<App>, window: &AppWindow) {
        let area = window.global::<PermissionsArea>();
        let weak = Rc::downgrade(app);
        area.on_act(move |id, action| {
            if let Some(app) = weak.upgrade() {
                let kind = kind(&id);
                app.permissions.act(&app, kind, &action);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_finish(move || {
            if let Some(app) = weak.upgrade() {
                app.permissions.close(&app);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_skip(move || {
            if let Some(app) = weak.upgrade() {
                app.permissions.close(&app);
            }
        });
        if !app.mode.offscreen {
            let weak = Rc::downgrade(app);
            self.timer.start(
                slint::TimerMode::Repeated,
                Duration::from_secs(1),
                move || {
                    if let Some(app) = weak.upgrade() {
                        app.permissions.tick(&app);
                    }
                },
            );
        }
    }

    fn update(&self, app: &Rc<App>, _window: &AppWindow, changed: Changed) {
        if app.mode.offscreen || self.offered.get() || !changed.any(Changed::SETUP | Changed::LINK)
        {
            return;
        }
        let (ready, first) = {
            let s = app.session.borrow();
            (
                s.loaded && s.link == Some(Link::Connected),
                s.setup.is_none(),
            )
        };
        if !ready {
            return;
        }
        // At launch: before Welcome on the first one, else only if the live
        // setup needs a permission it lacks.
        self.offered.set(true);
        app.call_as::<PermissionsReport>(Method::Permissions, move |app, result| {
            let Ok(report) = result else { return };
            let needed = needed(app, Some(&report));
            let missing = report
                .details
                .iter()
                .any(|d| needed[index(d.permission)] && !settled(d.state));
            app.permissions.take(app, report);
            // Windows' first run shows a blocked camera or microphone as
            // its own first step (nothing else there needs allowing), so
            // no sheet before Welcome.
            if missing && !(crate::engine::windows() && first) {
                app.permissions.open(app, first);
            }
        });
    }

    fn menu(&self, app: &Rc<App>, id: &str) -> bool {
        if id != "permissions" {
            return false;
        }
        self.open(app, false);
        true
    }

    fn state(
        &self,
        app: &Rc<App>,
        window: &AppWindow,
        name: &str,
        arg: &str,
    ) -> Option<anyhow::Result<()>> {
        match name {
            "permissions" => {
                self.open(app, !app.session.borrow().setup.is_some());
                Some(Ok(()))
            }
            "permissions-sample" => Some(self.sample(app, arg)),
            "permissions-helper" => {
                let (out, bundle) = arg.split_once(':').unwrap_or((arg, ""));
                let bundle = if bundle.is_empty() {
                    "/System/Applications/System Settings.app"
                } else {
                    bundle
                };
                let name = std::path::Path::new(bundle)
                    .file_stem()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                Some(crate::helper::render_png(
                    std::path::Path::new(bundle),
                    &helper_line(&name, true),
                    std::path::Path::new(out),
                    window.window().scale_factor(),
                ))
            }
            _ => None,
        }
    }
}

impl PermissionsStep {
    /// Whether the first run starts with its permissions step: always on
    /// macOS (its prompts are asked there); on Windows only when a privacy
    /// switch blocks the camera or microphone (screen and output audio need
    /// no permission there).
    pub fn access_needed(&self) -> bool {
        !crate::engine::windows()
            || self.report.borrow().as_ref().is_some_and(|report| {
                report.details.iter().any(|d| {
                    matches!(
                        d.permission,
                        PermissionKind::Camera | PermissionKind::Microphone
                    ) && !settled(d.state)
                })
            })
    }

    /// Relaunch stopped the engine a moment ago: calls failing on the lost
    /// link are expected, not errors to show.
    pub fn relaunching(&self) -> bool {
        self.relaunch_until
            .get()
            .is_some_and(|until| std::time::Instant::now() < until)
    }

    /// Show the step (`first_launch`: before Welcome) and read the
    /// permissions now.
    pub fn open(&self, app: &Rc<App>, first_launch: bool) {
        self.first_launch.set(first_launch);
        self.sample.set(false);
        if let Some(window) = app.window() {
            // A dialog is a popup, drawn over everything in the window, so
            // the step would open unseen under it (Settings › General ›
            // Review…): it takes the dialog's place, and the dialog comes
            // back when the step closes, Settings with its unsaved edits.
            let ui = window.global::<Ui>();
            let dialog = ui.get_dialog().to_string();
            if !dialog.is_empty() {
                let arg = if dialog == "settings" {
                    app.settings.park();
                    window.global::<SettingsArea>().get_tab().to_string()
                } else {
                    ui.get_dialog_arg().to_string()
                };
                *self.back_to.borrow_mut() = Some((dialog, arg));
                app.open_dialog("", "");
            }
            window.global::<PermissionsArea>().set_shown(true);
        }
        self.show(app);
        self.check(app);
    }

    fn close(&self, app: &Rc<App>) {
        self.helper.borrow_mut().take();
        if let Some(window) = app.window() {
            window.global::<PermissionsArea>().set_shown(false);
        }
        self.show(app);
        let back_to = self.back_to.borrow_mut().take();
        if let Some((kind, arg)) = back_to {
            app.open_dialog(&kind, &arg);
        }
    }

    /// Whether the rows show: the step itself, or the first run's
    /// Permissions step.
    fn visible(app: &Rc<App>) -> bool {
        app.window().is_some_and(|w| {
            w.global::<PermissionsArea>().get_shown()
                || (w.global::<Ui>().get_onboarding() && w.global::<SettingsArea>().get_step() == 0)
        })
    }

    /// Every second: take a request's answer, and read the permissions
    /// while they show.
    fn tick(&self, app: &Rc<App>) {
        let answer = self
            .asking
            .borrow()
            .as_ref()
            .and_then(|slot| slot.lock().ok()?.take());
        if let Some(answer) = answer {
            self.asking.borrow_mut().take();
            self.busy.set(None);
            match answer {
                Ok(report) => self.take(app, report),
                Err(error) => {
                    app.toast(
                        ToastKind::Error,
                        crate::engine::pick("Couldn’t ask macOS", "Couldn’t check Windows access"),
                        error,
                    );
                }
            }
            self.show(app);
        }
        if Self::visible(app) || self.helper.borrow().is_some() {
            self.check(app);
        }
    }

    /// Read the permissions now (once at a time).
    pub fn check(&self, app: &Rc<App>) {
        if self.checking.replace(true) || self.sample.get() {
            if self.sample.get() {
                self.checking.set(false);
            }
            return;
        }
        app.call_as::<PermissionsReport>(Method::Permissions, |app, result| {
            app.permissions.checking.set(false);
            match result {
                Ok(report) => app.permissions.take(app, report),
                // The engine restarting (Relaunch): the next tick asks again.
                Err(error) if error.code.is_none() => {}
                Err(error) => app.failed("Couldn’t check permissions", &error),
            }
        });
    }

    /// A fresh report: show it, close the helper once its permission is
    /// granted (or waits only for a relaunch), and list devices again when
    /// more is allowed.
    fn take(&self, app: &Rc<App>, report: PermissionsReport) {
        if self.sample.get() {
            return;
        }
        let granted = |r: &PermissionsReport| {
            r.details
                .iter()
                .filter(|d| d.state == Permission::Granted)
                .count()
        };
        let more = self
            .report
            .borrow()
            .as_ref()
            .is_some_and(|before| granted(&report) > granted(before));
        let done = self.helper.borrow().as_ref().is_some_and(|(kind, _)| {
            report
                .details
                .iter()
                .find(|d| d.permission == *kind)
                .is_some_and(|d| {
                    d.state == Permission::Granted || d.action == PermissionAction::Restart
                })
        });
        if done {
            self.helper.borrow_mut().take();
        }
        *self.report.borrow_mut() = Some(report);
        if more {
            app.load_devices(true);
        }
        self.show(app);
        app.settings.permissions_changed(app);
    }

    fn act(&self, app: &Rc<App>, kind: PermissionKind, action: &str) {
        if self.busy.get().is_some() || self.sample.get() {
            return;
        }
        // Another computer's permissions are granted on it, by someone
        // there: no prompts or settings open on this one (R2).
        if !crate::engine::is_local() {
            let engine = crate::engine::name().unwrap_or_default();
            let words = crate::engine::words();
            app.toast(
                ToastKind::Info,
                &format!("Grant on {engine}"),
                format!(
                    "Permissions belong to the computer that records. On {engine}, open \
                     Isocast and allow it there ({} on that {}); this window updates by \
                     itself.",
                    words.settings, words.machine
                ),
            );
            return;
        }
        match action {
            "ask" => self.ask(app, kind),
            "open" => self.open_settings(app, kind),
            "relaunch" => self.relaunch(app, kind),
            // Refused without a prompt: only a newer app can ask.
            "update" => app.menu("check_updates"),
            _ => {}
        }
    }

    /// macOS's prompt for `kind`, asked on a connection of its own: the
    /// engine waits up to a minute for the answer, and the window's calls
    /// mustn't wait with it.
    fn ask(&self, app: &Rc<App>, kind: PermissionKind) {
        self.busy.set(Some((kind, "ask")));
        let slot: Answer = Arc::default();
        *self.asking.borrow_mut() = Some(slot.clone());
        self.show(app);
        std::thread::Builder::new()
            .name("permission request".into())
            .spawn(move || {
                let result = daemon::connect()
                    .and_then(|mut client| {
                        client.call::<PermissionsReport>(Method::RequestPermissions(Some(
                            RequestPermissionsParams {
                                permission: Some(kind),
                            },
                        )))
                    })
                    .map_err(|error| format!("{error:#}"));
                if let Ok(mut slot) = slot.lock() {
                    *slot = Some(result);
                }
            })
            .expect("spawn a thread");
    }

    /// The exact System Settings pane, then the helper beside it with the
    /// app macOS lists the permission under.
    fn open_settings(&self, app: &Rc<App>, kind: PermissionKind) {
        self.busy.set(Some((kind, "open")));
        self.show(app);
        app.call(
            Method::OpenPermissionSettings { permission: kind },
            move |app, result| {
                let step = &app.permissions;
                step.busy.set(None);
                if let Err(error) = result {
                    app.failed(
                        crate::engine::pick(
                            "Couldn’t open System Settings",
                            "Couldn’t open Windows Settings",
                        ),
                        &error,
                    );
                    step.show(app);
                    return;
                }
                let detail = step
                    .report
                    .borrow()
                    .as_ref()
                    .and_then(|r| r.details.iter().find(|d| d.permission == kind).cloned());
                if !crate::engine::windows()
                    && let Some(path) = detail.as_ref().and_then(|d| d.app_path.clone())
                {
                    let detail = detail.expect("checked above");
                    // A screen recording grant (or a denied one) is turned on
                    // by hand; drag it in if it isn't listed.
                    let listed = kind != PermissionKind::ScreenRecording
                        && detail.state == Permission::Denied;
                    let weak = Rc::downgrade(app);
                    let shown = Helper::show(
                        std::path::Path::new(&path),
                        &helper_line(shown_name(&detail), !listed),
                        move || {
                            if let Some(app) = weak.upgrade() {
                                // Back in Isocast (or System Settings quit).
                                app.permissions.helper.borrow_mut().take();
                                app.permissions.show(&app);
                                app.permissions.check(&app);
                            }
                        },
                    );
                    match shown {
                        Ok(helper) => *step.helper.borrow_mut() = Some((kind, helper)),
                        Err(error) => isocast_diagnostics::diagnostic!("warn";
                            "permissions: no helper: {error:#}"),
                    }
                }
                step.show(app);
            },
        );
    }

    /// Start the engine again so it sees a screen recording grant: refused
    /// (atomically, by the engine) while it records, streams or exports.
    fn relaunch(&self, app: &Rc<App>, kind: PermissionKind) {
        self.busy.set(Some((kind, "relaunch")));
        self.show(app);
        self.relaunch_until
            .set(Some(std::time::Instant::now() + Duration::from_secs(6)));
        app.call(Method::ShutdownIfIdle, |app, result| {
            if let Err(error) = result {
                app.permissions.relaunch_until.set(None);
                app.permissions.busy.set(None);
                app.failed("Couldn’t relaunch the engine", &error);
                app.permissions.show(app);
                return;
            }
            // The next call launches it afresh; give it a moment to stop.
            let weak = Rc::downgrade(app);
            slint::Timer::single_shot(Duration::from_millis(1500), move || {
                if let Some(app) = weak.upgrade() {
                    app.permissions.busy.set(None);
                    app.reload();
                    app.permissions.check(&app);
                    app.permissions.show(&app);
                }
            });
        });
    }

    /// A snapshot's sample state, as the engine would report it.
    fn sample(&self, app: &Rc<App>, which: &str) -> anyhow::Result<()> {
        use Permission::{Denied, Granted, NotDetermined, NotGranted, Restricted};
        let states = match which {
            "first-launch" | "asking" => [NotGranted, NotDetermined, NotDetermined, NotDetermined],
            "helper" | "relaunch" => [NotGranted, Granted, Granted, NotDetermined],
            "denied" => [Granted, Denied, Granted, Denied],
            "silent" => [Granted, Granted, Denied, Granted],
            "allowed" => [Granted, Granted, Granted, Granted],
            "blocked" => [Granted, Restricted, Granted, Granted],
            other => anyhow::bail!(
                "no sample {other:?}; one of first-launch, asking, helper, relaunch, denied, silent, allowed, blocked"
            ),
        };
        let busy = (which == "asking").then_some((PermissionKind::Camera, "ask"));
        let helper = which == "helper";
        let (unasked, restart) = (which == "silent", which == "relaunch");
        self.first_launch
            .set(which == "first-launch" || which == "asking" || which == "helper");
        self.sample.set(true);
        self.busy.set(busy);
        *self.report.borrow_mut() = Some(sample_report(states, unasked, restart));
        if let Some(window) = app.window() {
            let area = window.global::<PermissionsArea>();
            area.set_shown(true);
            area.set_helper_shown(helper);
        }
        self.show_rows(app, helper);
        Ok(())
    }

    fn show(&self, app: &Rc<App>) {
        let helper = self.helper.borrow().is_some();
        if self.sample.get() {
            return;
        }
        self.show_rows(app, helper);
    }

    fn show_rows(&self, app: &Rc<App>, helper: bool) {
        let Some(window) = app.window() else { return };
        let area = window.global::<PermissionsArea>();
        let report = self.report.borrow();
        // The first launch's sample has no setup yet: it needs the three
        // that aren't optional.
        let needed = if self.sample.get() && self.first_launch.get() {
            [true, true, true, false]
        } else {
            needed(app, report.as_ref())
        };
        area.set_checked(report.is_some());
        area.set_unknown_access(report.as_ref().is_some_and(|report| {
            report
                .details
                .iter()
                .any(|d| d.state == Permission::Unknown)
        }));
        area.set_first_launch(self.first_launch.get());
        area.set_helper_shown(helper);
        let Some(report) = report.as_ref() else {
            area.set_ready(false);
            return;
        };
        area.set_app_name(
            if crate::engine::windows() || report.app == "isocastd" {
                "Isocast"
            } else {
                report.app.as_str()
            }
            .into(),
        );
        area.set_ready(
            report
                .details
                .iter()
                .all(|d| !needed[index(d.permission)] || settled(d.state)),
        );
        let busy = self.busy.get();
        // Windows can only block the camera and microphone: the rows are
        // those two.
        let shown = |d: &&PermissionDetail| {
            !crate::engine::windows()
                || matches!(
                    d.permission,
                    PermissionKind::Camera | PermissionKind::Microphone
                )
        };
        area.set_items(model(report.details.iter().filter(shown).map(|d| {
            row(
                d,
                needed[index(d.permission)],
                busy.filter(|(k, _)| *k == d.permission).map(|(_, a)| a),
            )
        })));
    }
}

/// One permission's row, in the engine machine's words (decision 22). In
/// a window on another computer's engine its action is "Grant on …": the
/// grant happens there, never through a prompt on this computer.
fn row(d: &PermissionDetail, needed: bool, busy: Option<&str>) -> PermissionItem {
    let mut item = if crate::engine::windows() {
        row_windows(d, needed, busy)
    } else {
        row_mac(d, needed, busy)
    };
    if !crate::engine::is_local() && !item.action.is_empty() {
        item.action = "remote".into();
        item.action_label =
            format!("Grant on {}", crate::engine::name().unwrap_or_default()).into();
        item.alt_action = "".into();
        item.alt_label = "".into();
    }
    item
}

/// One permission's row on macOS: its status in words, what happened in
/// plain words, and the one action.
fn row_mac(d: &PermissionDetail, needed: bool, busy: Option<&str>) -> PermissionItem {
    let (icon, title, purpose, pane) = match d.permission {
        PermissionKind::ScreenRecording => (
            "display",
            "Screen Recording",
            "Displays, windows and apps, and what this Mac plays.",
            "Screen & System Audio Recording",
        ),
        PermissionKind::Camera => (
            "camera",
            "Camera",
            "Cameras, capture cards and iPhone or iPad screens.",
            "Camera",
        ),
        PermissionKind::Microphone => (
            "mic",
            "Microphone",
            "Microphones and audio interfaces.",
            "Microphone",
        ),
        PermissionKind::SystemAudio => (
            "speaker",
            "System Audio",
            "Optional: what this Mac and its apps play, recorded without Screen Recording.",
            "Screen & System Audio Recording",
        ),
    };
    let app = shown_name(d);
    let mut alt = ("", "");
    let (status, label, detail, action, action_label) = if d.state == Permission::Granted {
        ("allowed", "Allowed", String::new(), "", "")
    } else if d.action == PermissionAction::Restart {
        (
            "relaunch",
            "Needs relaunch",
            "It’s on in System Settings. Isocast’s engine uses it once it starts again; nothing \
             you’ve set up is lost."
                .to_string(),
            "relaunch",
            "Relaunch",
        )
    } else if d.denied_without_prompt {
        // Or it was turned off before (another copy, a reset): System
        // Settings may still turn it on.
        alt = ("open", "Open Settings");
        (
            "silent",
            "Refused without asking",
            format!(
                "macOS said no without showing you its question, because this copy of {app} \
                 isn’t allowed to ask for it, so asking again won’t help. Install the latest \
                 {app}, then allow it here, or switch it on in System Settings › {pane}."
            ),
            "update",
            "Check for Updates…",
        )
    } else {
        match d.state {
            Permission::NotDetermined => {
                ("not-asked", "Not asked yet", String::new(), "ask", "Allow…")
            }
            // Neutral: neither allowed nor refused, and nothing to ask.
            Permission::Unknown => {
                let (action, label) = if d.action == PermissionAction::OpenSettings {
                    ("open", "Open Settings")
                } else {
                    ("", "")
                };
                (
                    "not-asked",
                    "Unknown",
                    d.next_step.clone().unwrap_or_else(|| {
                        "It can’t be checked without using it, so recording tries it.".into()
                    }),
                    action,
                    label,
                )
            }
            Permission::Restricted => (
                "blocked",
                "Blocked",
                "A policy on this Mac (device management or Screen Time) blocks it; only its \
                 administrator can allow it."
                    .to_string(),
                "",
                "",
            ),
            Permission::Denied => (
                "denied",
                "Turned off",
                format!("It was turned down. Switch on {app} in System Settings › {pane}."),
                "open",
                "Open Settings",
            ),
            // Screen recording: macOS doesn't say whether it asked.
            _ => (
                "not-asked",
                "Not allowed yet",
                String::new(),
                "open",
                "Open Settings",
            ),
        }
    };
    // System audio is never required (screen recording records it too).
    let purpose = if d.state == Permission::Unknown {
        d.needed_for.clone()
    } else if needed || d.permission == PermissionKind::SystemAudio {
        purpose.to_string()
    } else {
        format!("{purpose} This setup doesn’t use it.")
    };
    PermissionItem {
        id: PermissionKind::NAMES[index(d.permission)].into(),
        icon: icon.into(),
        title: title.into(),
        purpose: purpose.into(),
        status: status.into(),
        status_label: label.into(),
        detail: detail.into(),
        action: action.into(),
        action_label: action_label.into(),
        alt_action: alt.0.into(),
        alt_label: alt.1.into(),
        busy: busy.is_some(),
        needed,
    }
}

/// One permission's row on Windows.
fn row_windows(d: &PermissionDetail, needed: bool, busy: Option<&str>) -> PermissionItem {
    let (icon, title, purpose) = match d.permission {
        PermissionKind::ScreenRecording => (
            "display",
            "Screen Recording",
            "Displays and windows on this PC.",
        ),
        PermissionKind::Camera => ("camera", "Camera", "Cameras and capture cards."),
        PermissionKind::Microphone => ("mic", "Microphone", "Microphones and audio interfaces."),
        PermissionKind::SystemAudio => (
            "speaker",
            "System Audio",
            "Optional: what this PC and its apps play.",
        ),
    };
    // A privacy switch in Settings is the only thing Windows can turn off;
    // its exact page opens, and the row flips to Allowed by itself.
    let (status, label, action, action_label) = if d.state == Permission::Granted {
        ("allowed", "Allowed", "", "")
    } else if d.state == Permission::Restricted || d.action == PermissionAction::AskAdministrator {
        ("blocked", "Blocked", "", "")
    } else if d.action == PermissionAction::OpenSettings {
        // The row's own page (Camera or Microphone) opens.
        ("denied", "Turned off", "open", "Open Settings")
    } else {
        ("not-asked", "Unknown", "", "")
    };
    let detail = if d.state == Permission::Granted {
        String::new()
    } else if status == "blocked" {
        d.next_step.clone().unwrap_or_else(|| {
            format!(
                "A policy on this {} blocks it; only its administrator can allow it.",
                crate::engine::words().machine
            )
        })
    } else {
        d.next_step.clone().unwrap_or_default()
    };
    PermissionItem {
        id: PermissionKind::NAMES[index(d.permission)].into(),
        icon: icon.into(),
        title: title.into(),
        purpose: if needed || d.permission == PermissionKind::SystemAudio {
            purpose.into()
        } else {
            format!("{purpose} This setup doesn’t use it.").into()
        },
        status: status.into(),
        status_label: label.into(),
        detail: detail.into(),
        action: action.into(),
        action_label: action_label.into(),
        alt_action: "".into(),
        alt_label: "".into(),
        busy: busy.is_some(),
        needed,
    }
}

/// The helper's one line.
fn helper_line(app: &str, drag: bool) -> String {
    if drag {
        format!("Drag {app} into the list, then switch it on")
    } else {
        format!("Switch on {app} in the list (or drag it in)")
    }
}

/// The name to show for the app: the one macOS lists, or "Isocast" for a
/// bare isocastd (a development build), whose name means nothing to a
/// person.
fn shown_name(d: &PermissionDetail) -> &str {
    if crate::engine::windows() {
        crate::engine::words().engine
    } else if d.app_path.is_none() && d.app == "isocastd" {
        "Isocast"
    } else {
        &d.app
    }
}

/// Nothing left to do before recording: granted, or unknown (it can only
/// be found out by recording, which tries it).
fn settled(state: Permission) -> bool {
    matches!(state, Permission::Granted | Permission::Unknown)
}

fn index(kind: PermissionKind) -> usize {
    PermissionKind::ALL
        .iter()
        .position(|k| *k == kind)
        .unwrap_or(0)
}

fn kind(id: &str) -> PermissionKind {
    PermissionKind::ALL[PermissionKind::NAMES
        .iter()
        .position(|n| *n == id)
        .unwrap_or(0)]
}

/// Which permissions are needed: screen recording, camera and microphone
/// before there is a setup (its first setup picks a screen, camera and
/// microphone), else those its sources use. System audio recording is
/// never needed: screen recording records system and app audio too, and
/// where system audio recording is allowed, system and app audio don't
/// need screen recording.
fn needed(app: &Rc<App>, report: Option<&PermissionsReport>) -> [bool; 4] {
    let played_by_tap = report.is_some_and(|r| r.permissions.system_audio == Permission::Granted);
    let s = app.session.borrow();
    let Some(setup) = s.setup.as_ref() else {
        return [true, true, true, false];
    };
    let mut needed = [false; 4];
    for source in &setup.sources {
        let kind = match source.kind {
            SourceKind::SystemAudio(_) | SourceKind::ApplicationAudio(_) if played_by_tap => {
                continue;
            }
            SourceKind::Display(_)
            | SourceKind::Window(_)
            | SourceKind::Application(_)
            | SourceKind::SystemAudio(_)
            | SourceKind::ApplicationAudio(_) => PermissionKind::ScreenRecording,
            SourceKind::Camera(_) => PermissionKind::Camera,
            SourceKind::Microphone(_) => PermissionKind::Microphone,
            _ => continue,
        };
        needed[index(kind)] = true;
    }
    needed
}

/// A report as the engine gives it, for snapshots: the release app's names.
fn sample_report(states: [Permission; 4], unasked: bool, restart: bool) -> PermissionsReport {
    let details = PermissionKind::ALL
        .into_iter()
        .zip(states)
        .map(|(kind, state)| {
            let screen = kind == PermissionKind::ScreenRecording;
            let action = match state {
                Permission::Granted => PermissionAction::None,
                _ if screen && restart => PermissionAction::Restart,
                Permission::NotDetermined if !screen => PermissionAction::Request,
                Permission::Restricted => PermissionAction::AskAdministrator,
                _ => PermissionAction::OpenSettings,
            };
            PermissionDetail {
                permission: kind,
                state,
                needed_for: String::new(),
                action,
                settings_url: String::new(),
                next_step: None,
                restart_after_granting: screen && state != Permission::Granted,
                app: "Isocast".into(),
                app_path: None,
                denied_without_prompt: unasked && kind == PermissionKind::Microphone,
            }
        })
        .collect();
    PermissionsReport {
        permissions: Permissions {
            screen_recording: states[0],
            camera: states[1],
            microphone: states[2],
            system_audio: states[3],
        },
        app: "Isocast".into(),
        details,
    }
}
