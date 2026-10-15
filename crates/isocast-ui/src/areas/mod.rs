//! The window's areas, each built in its own files so they can be worked on
//! side by side:
//!
//! | Area | Rust | Slint (ui/areas/…) | Global |
//! |---|---|---|---|
//! | A scenes and sources | `sources.rs` | `sources/` | `SourcesArea` |
//! | B audio | `mixer.rs` | `mixer/` | `MixerArea` |
//! | C controls and status | `controls.rs` | `controls/` | `ControlsArea` |
//! | D settings and first run | `settings.rs` | `settings/` | `SettingsArea` |
//! | E the canvas | `canvas.rs` (+ preview.rs, layers.rs, frames.rs, picture.rs) | `canvas/` | `CanvasArea` |
//! | F scene collections and profiles | `library.rs` | `library/` | `LibraryArea` |
//! | G the Create view (clip bar) | `create.rs` (+ thumbs.rs) | `create/` | `CreateArea` |
//! | H the permissions step | `permissions.rs` (+ helper.rs) | `permissions/` | `PermissionsArea` |
//! | I the setup wizard (OBS, optimize) | `wizard.rs` | `wizard/` | `WizardArea` |
//!
//! What is shared lives outside the areas and changes rarely: the widget
//! kit (ui/kit.slint, ui/widgets/), the tokens (ui/theme.slint), icons
//! (ui/icons.slint), the window (ui/app.slint), the `Ui` global
//! (ui/state.slint), and in Rust `App` (app.rs: calls, edits, selection,
//! dialogs, menus, toasts), `Session` (session.rs), `model` and `kit`.
//!
//! # How an area works
//!
//! - **Data in**: the area's global (`ui/areas/<area>/state.slint`) holds
//!   its models and state; `update` fills them from `app.session` when the
//!   session changed in a way the area shows ([`Changed`]).
//! - **Actions out**: the global's callbacks, wired in `connect`, call the
//!   daemon through `App` (`app.edit(patch, …)` for setup changes,
//!   `app.call(Method::…, reply)` for everything else): no area holds
//!   state the daemon owns, and nothing is done that the API can't do.
//! - **Dialogs**: `Ui.dialog` names the open one; an area shows its own
//!   with `if Ui.dialog == "…"` in its `dialogs.slint` and opens them with
//!   `app.open_dialog(kind, arg)` (or `Ui.open-dialog` from Slint).
//! - **Menus**: Slint asks `Menus.request(owner, x, y, w, h)`; the area
//!   whose owner prefix it is builds the menu in `context_menu` (a
//!   `MenuBuilder`) and handles the choice in `menu_chosen`. Menu bar and
//!   keyboard commands arrive in `menu` by id (ui/app.slint lists them).
//! - **Toasts**: `app.toast(kind, title, message)` or `app.show_toast` for
//!   actions and rows; `app.failed(title, &error)` for a refused call.
//!
//! # Snapshot states
//!
//! `scripts/snapshot.sh --snapshot OUT.png --state NAME[:ARG]…` (a throwaway
//! engine; isocast-ui refuses offscreen runs on the user's own) runs each state in
//! order, offscreen, against the daemon (see offscreen.rs for the
//! window-wide ones). To add one for your area, answer it in `state`:
//! return `Some(Ok(()))` when you handled `name`, `Some(Err(…))` when it
//! failed, `None` when it isn't yours; and list it in `STATES` so `--help`
//! and the error for an unknown state name it. Prefer states that drive
//! the real interface (open the dialog through `app.open_dialog`, select
//! through `app.select`) over ones that set properties directly.
//!
//! # The layout loop
//!
//! `cargo build --profile ui-dev -p isocast-ui --features live-slint`
//! builds an app that reads ui/*.slint at run time (Slint's interpreter;
//! never shipped). `isocast-ui --snapshot-watch DIR [--state …] [--crop
//! controls]` then redraws DIR/snapshot.png about a second after each
//! .slint save. What the Rust side set carries over (dialogs, menus,
//! record, stream, hide:) with the daemon's state; what lives in the
//! interface (hover, typed text, focus, dragged splitters) does not.
//! `--snapshot` may be repeated: one picture after each one's states.
//!
//! # Testing
//!
//! Build with your own `CARGO_TARGET_DIR`; snapshot offscreen against an
//! isolated engine (ISOCAST_SOCKET, ISOCAST_SETUP, ISOCAST_DEV_APP); real
//! windows only on a test Mac (scripts/testmac.sh). `cargo fmt`, `cargo
//! clippy --all-targets -- -D warnings` and `cargo test` clean before each
//! commit.

pub mod canvas;
pub mod controls;
pub mod create;
pub mod developer;
pub mod hotkeys;
pub mod library;
pub mod mixer;
pub mod permissions;
pub mod report;
pub mod settings;
pub mod sources;
pub mod wizard;

use std::rc::Rc;

use isocast_protocol::api::Event;

use crate::AppWindow;
use crate::app::App;
use crate::kit::MenuBuilder;
pub use crate::session::Changed;

/// Every area's snapshot states, for `--help` and errors.
pub const STATES: &[&[&str]] = &[
    sources::STATES,
    mixer::STATES,
    controls::STATES,
    settings::STATES,
    canvas::STATES,
    library::STATES,
    create::STATES,
    developer::STATES,
    permissions::STATES,
    wizard::STATES,
];

/// What `App` asks of an area. Everything but `connect` and `update` has
/// a default that does nothing.
pub trait Area {
    /// Once, when the window is made: wire the area's global's callbacks.
    fn connect(&self, app: &Rc<App>, window: &AppWindow);

    /// The session changed in the `changed` ways: show it.
    fn update(&self, app: &Rc<App>, window: &AppWindow, changed: Changed);

    /// A menu bar or keyboard command (ids in ui/app.slint). True if it
    /// was this area's.
    fn menu(&self, _app: &Rc<App>, _id: &str) -> bool {
        false
    }

    /// The menu `owner` asked for (`Menus.request`), if the owner is this
    /// area's (by prefix: "scenes:", "sources:", "mixer:"…).
    fn context_menu(&self, _app: &Rc<App>, _owner: &str) -> Option<MenuBuilder> {
        None
    }

    /// An item of a menu this area built was chosen. True if it was this
    /// area's.
    fn menu_chosen(&self, _app: &Rc<App>, _owner: &str, _id: &str) -> bool {
        false
    }

    /// A dialog is opening (`kind`, `arg`): load what it shows.
    fn dialog_opened(&self, _app: &Rc<App>, _kind: &str, _arg: &str) {}

    /// An event from the daemon, after the session took it.
    fn event(&self, _app: &Rc<App>, _event: &Event) {}

    /// A warning from the daemon: true if this area shows it (instead of
    /// the plain warning toast).
    fn warning(&self, _app: &Rc<App>, _message: &str) -> bool {
        false
    }

    /// A snapshot `--state NAME[:ARG]` (see the module's notes).
    fn state(
        &self,
        _app: &Rc<App>,
        _window: &AppWindow,
        _name: &str,
        _arg: &str,
    ) -> Option<anyhow::Result<()>> {
        None
    }
}
