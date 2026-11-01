//! Area F, scene collections and profiles (P.2; the web app's
//! ui/ui/library.js): the menu bar's Profile and Scene Collection menus, as
//! OBS has them, and their dialogs (ui/areas/library/). New, Duplicate,
//! Rename and Remove act on the one in use; Import adds one from a file
//! (an exported one, a whole setup, or OBS's); Export writes the one in use
//! to a file; the list below them switches. Every command is a `library_*`
//! call on the method table, and what the engine refuses (a switch while
//! recording or streaming) shows in its own words. The files people pick
//! are read and written with the CLI's code (`isocast_client::library`).
//!
//! The list is read with `library` when the link connects and whenever the
//! engine says the library changed (`LibraryChanged`, from this window or
//! any other client), so both apps and the CLI stay in step.

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;

use anyhow::{Context, anyhow};
use isocast_client::library as files;
use isocast_protocol::api::{Event, Method};
use isocast_protocol::library::{
    Library as Listing, LibraryAction, LibraryChange, LibraryCreateParams, LibraryDocument,
    LibraryEntry, LibraryKind, LibraryRenameParams, LibrarySwitchParams, LibraryTarget,
};
use slint::{ComponentHandle, ModelRc, VecModel};

use super::{Area, Changed};
use crate::app::App;
use crate::daemon::{CallError, Link};
use crate::platform::{self, Choose};
use crate::toasts::Toast;
use crate::{AppWindow, LibraryArea, LibraryRow, ToastKind};

/// Snapshot states (`--state`): the dialogs open with `dialog:library-name:
/// KIND:MODE` and `dialog:library-remove:KIND`, the menus with
/// `menu-bar-open:Profile`.
pub const STATES: &[&str] = &[
    "library:COMMAND (collection_new, profile_remove…)",
    "library-switch:KIND:ID",
    "library-import:KIND:PATH",
    "library-export:KIND:PATH",
];

/// What the name dialog is for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Naming {
    New,
    Duplicate,
    Rename,
}

#[derive(Default)]
pub struct Library {
    /// The library as last read; none until then (or from an engine that
    /// has none).
    listing: RefCell<Option<Listing>>,
    /// The name dialog's kind and purpose while it is open.
    naming: Cell<Option<(LibraryKind, Naming)>>,
    /// The kind Remove's confirmation is for while it is open.
    removing: Cell<Option<LibraryKind>>,
    #[cfg(target_os = "linux")]
    export_target: RefCell<Option<LibraryTarget>>,
    /// The link as last seen, to read the library again when it connects.
    link: Cell<Option<Link>>,
}

fn kind_of(text: &str) -> Option<LibraryKind> {
    match text {
        "collection" => Some(LibraryKind::Collection),
        "profile" => Some(LibraryKind::Profile),
        _ => None,
    }
}

/// "Scene Collection" or "Profile", as the menu bar names them.
fn title(kind: LibraryKind) -> &'static str {
    match kind {
        LibraryKind::Collection => "Scene Collection",
        LibraryKind::Profile => "Profile",
    }
}

fn plural(kind: LibraryKind) -> &'static str {
    match kind {
        LibraryKind::Collection => "scene collections",
        LibraryKind::Profile => "profiles",
    }
}

fn entries(listing: &Listing, kind: LibraryKind) -> &[LibraryEntry] {
    match kind {
        LibraryKind::Collection => &listing.collections,
        LibraryKind::Profile => &listing.profiles,
    }
}

fn current(listing: &Listing, kind: LibraryKind) -> Option<&LibraryEntry> {
    let id = match kind {
        LibraryKind::Collection => listing.collection.as_ref(),
        LibraryKind::Profile => listing.profile.as_ref(),
    }?;
    entries(listing, kind).iter().find(|e| &e.id == id)
}

/// The one that takes the current one's place when it is removed: the
/// first other that can be used.
fn successor(listing: &Listing, kind: LibraryKind) -> Option<&LibraryEntry> {
    let now = current(listing, kind)?;
    entries(listing, kind)
        .iter()
        .find(|e| e.id != now.id && e.problem.is_none())
}

/// What a menu shows for an entry.
fn label(entry: &LibraryEntry) -> String {
    if entry.name.is_empty() {
        entry.id.clone()
    } else {
        entry.name.clone()
    }
}

/// What a menu's list shows: the name, and the id too when another has
/// the same name (the engine allows that; ids tell them apart).
fn menu_label(entry: &LibraryEntry, all: &[LibraryEntry]) -> String {
    let name = label(entry);
    let shared = all
        .iter()
        .any(|other| other.id != entry.id && label(other) == name);
    if shared {
        format!("{name} ({})", entry.id)
    } else {
        name
    }
}

/// `base`, or `base 2`, `base 3`… if it is taken.
fn unique(base: &str, taken: &[String]) -> String {
    if !taken.iter().any(|t| t == base) {
        return base.to_string();
    }
    (2..)
        .map(|n| format!("{base} {n}"))
        .find(|name| !taken.contains(name))
        .expect("a free name")
}

/// Switch params for `kind` to `id`.
fn switch_params(kind: LibraryKind, id: &str) -> LibrarySwitchParams {
    match kind {
        LibraryKind::Collection => LibrarySwitchParams {
            collection: Some(id.to_string()),
            profile: None,
        },
        LibraryKind::Profile => LibrarySwitchParams {
            collection: None,
            profile: Some(id.to_string()),
        },
    }
}

impl Library {
    /// Read the library, then show it.
    pub fn load(&self, app: &Rc<App>) {
        app.call_as::<Listing>(Method::Library, |app, result| {
            let listing = match result {
                Ok(listing) => Some(listing),
                Err(error) => {
                    // An engine from before P.1 has none: the menus offer
                    // nothing.
                    if !error.is_unknown_method() {
                        isocast_diagnostics::diagnostic!("error"; "library: {error}");
                    }
                    None
                }
            };
            *app.library.listing.borrow_mut() = listing;
            app.library.show(app);
        });
    }

    /// Show the library in the menus. The lists are new models each time:
    /// choosing a checkable menu item flips its check by itself, and a new
    /// model makes each entry's check follow `current` again.
    fn show(&self, app: &Rc<App>) {
        let Some(window) = app.window() else { return };
        let area = window.global::<LibraryArea>();
        let listing = self.listing.borrow();
        let rows = |kind| -> ModelRc<LibraryRow> {
            let all = listing
                .as_ref()
                .map(|l| entries(l, kind))
                .unwrap_or_default();
            let rows: Vec<LibraryRow> = all
                .iter()
                .map(|e| LibraryRow {
                    id: e.id.as_str().into(),
                    name: menu_label(e, all).into(),
                    current: e.current,
                    problem: e.problem.is_some(),
                })
                .collect();
            ModelRc::new(VecModel::from(rows))
        };
        area.set_collections(rows(LibraryKind::Collection));
        area.set_profiles(rows(LibraryKind::Profile));
        area.set_available(listing.is_some());
        let has = |kind| listing.as_ref().and_then(|l| current(l, kind)).is_some();
        let can_remove = |kind| listing.as_ref().and_then(|l| successor(l, kind)).is_some();
        area.set_has_collection(has(LibraryKind::Collection));
        area.set_has_profile(has(LibraryKind::Profile));
        area.set_can_remove_collection(can_remove(LibraryKind::Collection));
        area.set_can_remove_profile(can_remove(LibraryKind::Profile));
    }

    /// A menu's entry was chosen: use it.
    fn switch_to(&self, app: &Rc<App>, kind: LibraryKind, id: &str) {
        // The check the menu flipped goes back to what is current until
        // the engine says otherwise.
        self.show(app);
        let in_use = self
            .listing
            .borrow()
            .as_ref()
            .and_then(|l| current(l, kind))
            .is_some_and(|e| e.id == id);
        if in_use || id.is_empty() {
            return;
        }
        app.close_dialog();
        app.call(
            Method::LibrarySwitch(switch_params(kind, id)),
            move |app, result| {
                if let Err(error) = result {
                    app.failed(&format!("Couldn’t switch {}", plural(kind)), &error);
                }
            },
        );
    }

    /// New, Duplicate or Rename: ask for a name.
    fn ask_name(&self, app: &Rc<App>, kind: LibraryKind, naming: Naming) {
        let mode = match naming {
            Naming::New => "new",
            Naming::Duplicate => "duplicate",
            Naming::Rename => "rename",
        };
        app.open_dialog("library-name", &format!("{}:{mode}", kind.command()));
    }

    fn name_opened(&self, app: &Rc<App>, arg: &str) {
        let Some(window) = app.window() else { return };
        let area = window.global::<LibraryArea>();
        let (kind, mode) = arg.split_once(':').unwrap_or((arg, "new"));
        let naming = match mode {
            "duplicate" => Naming::Duplicate,
            "rename" => Naming::Rename,
            _ => Naming::New,
        };
        let Some(kind) = kind_of(kind) else {
            app.close_dialog();
            return;
        };
        let listing = self.listing.borrow();
        let all = listing
            .as_ref()
            .map(|l| entries(l, kind))
            .unwrap_or_default();
        let now = listing.as_ref().and_then(|l| current(l, kind));
        let taken: Vec<String> = listing
            .as_ref()
            .map(|l| entries(l, kind).iter().map(label).collect())
            .unwrap_or_default();
        let (heading, name) = match (naming, now) {
            (Naming::New, _) => (
                format!("Name for the new {}", kind.label()),
                unique("Untitled", &taken),
            ),
            (Naming::Duplicate, Some(now)) => (
                format!("Name for the copy of “{}”", menu_label(now, all)),
                unique(&label(now), &taken),
            ),
            (Naming::Rename, Some(now)) => {
                (format!("Name for “{}”", menu_label(now, all)), label(now))
            }
            // Nothing in use to copy or rename.
            (_, None) => {
                drop(listing);
                app.close_dialog();
                return;
            }
        };
        let verb = match naming {
            Naming::New => "Add",
            Naming::Duplicate => "Duplicate",
            Naming::Rename => "Rename",
        };
        area.set_name_title(format!("{verb} {}", title(kind)).into());
        area.set_name_heading(heading.into());
        area.set_name(name.into());
        self.naming.set(Some((kind, naming)));
    }

    fn name_accept(&self, app: &Rc<App>) {
        let Some(window) = app.window() else { return };
        let name = window.global::<LibraryArea>().get_name().trim().to_string();
        let Some((kind, naming)) = self.naming.take() else {
            return;
        };
        if name.is_empty() {
            self.naming.set(Some((kind, naming)));
            return;
        }
        app.close_dialog();
        let now = self
            .listing
            .borrow()
            .as_ref()
            .and_then(|l| current(l, kind))
            .cloned();
        match (naming, now) {
            (Naming::Rename, Some(now)) => {
                if name == now.name {
                    return;
                }
                let params = LibraryRenameParams {
                    kind,
                    id: now.id,
                    name,
                };
                app.call(
                    Method::LibraryRename(Box::new(params)),
                    move |app, result| {
                        if let Err(error) = result {
                            app.failed(&format!("Couldn’t rename the {}", kind.label()), &error);
                        }
                    },
                );
            }
            (Naming::Rename, None) => {}
            (naming, now) => {
                let copy_of = match naming {
                    Naming::Duplicate => now.map(|e| e.id),
                    _ => None,
                };
                let what = if copy_of.is_some() {
                    "duplicate"
                } else {
                    "add"
                };
                let params = LibraryCreateParams {
                    kind,
                    name,
                    id: None,
                    copy_of,
                    switch: true,
                };
                app.call(
                    Method::LibraryCreate(Box::new(params)),
                    move |app, result| {
                        if let Err(error) = result {
                            app.failed(&format!("Couldn’t {what} the {}", kind.label()), &error);
                        }
                    },
                );
            }
        }
    }

    fn remove_opened(&self, app: &Rc<App>, arg: &str) {
        let Some(window) = app.window() else { return };
        let area = window.global::<LibraryArea>();
        let listing = self.listing.borrow();
        let found = kind_of(arg).and_then(|kind| {
            let listing = listing.as_ref()?;
            Some((kind, current(listing, kind)?, successor(listing, kind)?))
        });
        let Some((kind, now, next)) = found else {
            drop(listing);
            app.close_dialog();
            return;
        };
        let all = listing
            .as_ref()
            .map(|l| entries(l, kind))
            .unwrap_or_default();
        area.set_remove_heading(format!("Remove “{}”?", menu_label(now, all)).into());
        area.set_remove_note(
            format!("Isocast switches to “{}” first.", menu_label(next, all)).into(),
        );
        area.set_remove_detail(
            format!(
                "The {}’s file is kept in the library’s deleted folder.",
                kind.label()
            )
            .into(),
        );
        self.removing.set(Some(kind));
    }

    /// Remove the one in use, as OBS does: switch to the next, then delete
    /// it (the engine keeps its file in the library's deleted folder).
    fn remove_accept(&self, app: &Rc<App>) {
        let Some(kind) = self.removing.take() else {
            return;
        };
        app.close_dialog();
        let pair = self
            .listing
            .borrow()
            .as_ref()
            .and_then(|l| Some((current(l, kind)?.id.clone(), successor(l, kind)?.id.clone())));
        let Some((id, next)) = pair else { return };
        let failed = move |app: &Rc<App>, error: &CallError| {
            app.failed(&format!("Couldn’t remove the {}", kind.label()), error);
        };
        app.call(
            Method::LibrarySwitch(switch_params(kind, &next)),
            move |app, result| {
                if let Err(error) = result {
                    failed(app, &error);
                    return;
                }
                app.call(
                    Method::LibraryDelete(LibraryTarget { kind, id }),
                    move |app, result| {
                        if let Err(error) = result {
                            failed(app, &error);
                        }
                    },
                );
            },
        );
    }

    /// Import…: ask for a file, then add what it holds.
    fn import(&self, app: &Rc<App>, kind: LibraryKind) {
        app.close_dialog();
        let types: &[&str] = match kind {
            LibraryKind::Collection => &["toml", "json"],
            LibraryKind::Profile => &["toml", "json", "ini"],
        };
        let options = Choose {
            types,
            prompt: "Import",
            ..Choose::default()
        };
        // import_path reads the file in the app, not on a remote engine.
        #[cfg(not(target_os = "linux"))]
        use crate::areas::remote::choose as choose_import;
        #[cfg(target_os = "linux")]
        use crate::areas::remote::choose_here as choose_import;
        choose_import(app, options, move |app, chosen| match chosen {
            Some(paths) => {
                if let Some(path) = paths.into_iter().next() {
                    app.library.import_path(app, kind, &path);
                }
            }
            None => no_panel(app),
        });
    }

    /// Add the scene collection or profile at `path` (an exported one, a
    /// whole setup, or OBS's) to the library; a toast offers to switch.
    fn import_path(&self, app: &Rc<App>, kind: LibraryKind, path: &str) {
        let method = match files::import_method(kind, Path::new(path)) {
            Ok(method) => method,
            Err(error) => {
                app.toast(
                    ToastKind::Error,
                    &format!("Couldn’t import the {}", kind.label()),
                    format!("{error:#}"),
                );
                return;
            }
        };
        app.call_as::<LibraryChange>(method, move |app, result| match result {
            Ok(change) => imported(app, &change),
            Err(error) => app.failed(&format!("Couldn’t import the {}", kind.label()), &error),
        });
    }

    /// Export…: ask where, then write the one in use there.
    fn export(&self, app: &Rc<App>, kind: LibraryKind) {
        let Some(now) = self
            .listing
            .borrow()
            .as_ref()
            .and_then(|l| current(l, kind))
            .cloned()
        else {
            return;
        };
        app.close_dialog();
        let name = format!("{}.toml", label(&now).replace(['/', ':'], "-"));
        #[cfg(target_os = "linux")]
        {
            let app = Rc::downgrade(app);
            platform::ask_portal(
                platform::PortalDialog::Save {
                    suggested: isocast_protocol::api::home_folder()
                        .join("Videos")
                        .join(&name),
                },
                "Export",
                &["toml", "json"],
                move |paths| {
                    let Some(app) = app.upgrade() else { return };
                    if let Some(paths) = paths {
                        if let Some(path) = paths.first() {
                            app.library.export_to(&app, kind, &now.id, path);
                        }
                    } else if let Some(window) = app.window() {
                        app.open_dialog("library-export-path", kind.command());
                        let area = window.global::<LibraryArea>();
                        area.set_export_path(format!("~/{name}").into());
                        area.set_replace_path("".into());
                        *app.library.export_target.borrow_mut() =
                            Some(LibraryTarget { kind, id: now.id });
                    }
                },
            );
        }
        #[cfg(not(target_os = "linux"))]
        match platform::save(&name, &["toml", "json"], "Export") {
            Some(Some(path)) => self.export_to(app, kind, &now.id, &path),
            Some(None) => {}
            None => no_panel(app),
        }
    }

    #[cfg(target_os = "linux")]
    fn export_path_accept(&self, app: &Rc<App>) {
        let Some(window) = app.window() else { return };
        let area = window.global::<LibraryArea>();
        let entered = area.get_export_path();
        let expanded = platform::expand_home(&entered);
        let path = Path::new(&expanded);
        if !path.is_absolute() || path.file_name().is_none() || path.is_dir() {
            app.toast(
                ToastKind::Error,
                "Choose a file path",
                "Enter a file name starting with / or ~/.",
            );
            return;
        }
        if path.symlink_metadata().is_ok() && area.get_replace_path() != entered {
            area.set_replace_path(entered);
            return;
        }
        if let Some(target) = self.export_target.take() {
            app.close_dialog();
            self.export_to(app, target.kind, &target.id, &expanded);
        }
    }

    /// Write scene collection or profile `id` to `path` (TOML, or JSON for
    /// .json), from `library_get`.
    fn export_to(&self, app: &Rc<App>, kind: LibraryKind, id: &str, path: &str) {
        let path = path.to_string();
        let target = LibraryTarget {
            kind,
            id: id.to_string(),
        };
        app.call_as::<LibraryDocument>(Method::LibraryGet(target), move |app, result| {
            let written = result
                .map_err(|error| anyhow!("{error}"))
                .and_then(|document| {
                    files::write_export(&document, Path::new(&path))?;
                    Ok(label(&document.entry))
                });
            match written {
                Ok(name) => {
                    // The file was saved on this machine (its save panel),
                    // even in a window on another computer's engine: shown
                    // in this machine's file manager, in its words.
                    let reveal = path.clone();
                    app.show_toast(
                        Toast::new(ToastKind::Success, format!("Exported “{name}”"), path)
                            .action(isocast_protocol::platform_text::SHOW_IN_FILES, move |_| {
                                platform::reveal(&reveal)
                            }),
                    );
                }
                Err(error) => {
                    app.toast(
                        ToastKind::Error,
                        &format!("Couldn’t export the {}", kind.label()),
                        format!("{error:#}"),
                    );
                }
            }
        });
    }

    /// A menu command of this area, by its id (`collection_new`…).
    fn command(&self, app: &Rc<App>, id: &str) -> bool {
        let Some((kind, command)) = id.split_once('_') else {
            return false;
        };
        let Some(kind) = kind_of(kind) else {
            return false;
        };
        match command {
            "new" => self.ask_name(app, kind, Naming::New),
            "duplicate" => self.ask_name(app, kind, Naming::Duplicate),
            "rename" => self.ask_name(app, kind, Naming::Rename),
            "remove" => app.open_dialog("library-remove", kind.command()),
            "import" => self.import(app, kind),
            "export" => self.export(app, kind),
            _ => return false,
        }
        true
    }
}

/// What an import added, and an offer to switch to it.
fn imported(app: &Rc<App>, change: &LibraryChange) {
    let made: Vec<&LibraryEntry> = [&change.collection, &change.profile]
        .into_iter()
        .flatten()
        .collect();
    // A whole setup brings both: say which is which.
    let names = made
        .iter()
        .map(|e| match made.len() {
            1 => format!("“{}”", label(e)),
            _ => format!("{} “{}”", e.kind.label(), label(e)),
        })
        .collect::<Vec<_>>()
        .join(" and ");
    let mut lines = Vec::new();
    if !change.unsupported.is_empty() {
        let n = change.unsupported.len();
        let what: Vec<&str> = change.unsupported.iter().map(|u| u.obs.as_str()).collect();
        lines.push(format!(
            "{n} thing{} from OBS couldn’t come over: {}.",
            if n == 1 { "" } else { "s" },
            what.join(", ")
        ));
    }
    for problem in &change.problems {
        lines.push(format!("{}: {}", problem.path, problem.message));
    }
    let kind = if lines.is_empty() {
        ToastKind::Success
    } else {
        ToastKind::Warning
    };
    let pair = LibrarySwitchParams {
        collection: change.collection.as_ref().map(|e| e.id.clone()),
        profile: change.profile.as_ref().map(|e| e.id.clone()),
    };
    let title = format!("Imported {names}");
    app.show_toast(
        Toast::new(kind, title, lines.join("\n")).action("Switch to it", move |app| {
            let names = names.clone();
            app.call(Method::LibrarySwitch(pair.clone()), move |app, result| {
                if let Err(error) = result {
                    app.failed(&format!("Couldn’t switch to {names}"), &error);
                }
            });
        }),
    );
}

/// Where there is no file panel (not macOS): say so.
fn no_panel(app: &Rc<App>) {
    app.toast(
        ToastKind::Info,
        "No file panel here",
        "Use `isocast collection import FILE` or `isocast profile export` instead.",
    );
}

impl Area for Library {
    fn connect(&self, app: &Rc<App>, window: &AppWindow) {
        let area = window.global::<LibraryArea>();
        #[cfg(target_os = "linux")]
        {
            let weak = Rc::downgrade(app);
            area.on_export_path_accept(move || {
                if let Some(app) = weak.upgrade() {
                    app.library.export_path_accept(&app);
                }
            });
        }
        let weak = Rc::downgrade(app);
        area.on_switch_to(move |kind, id| {
            // On the next turn of the event loop, once the menu is done with
            // the item (its list is replaced).
            let weak = weak.clone();
            slint::Timer::single_shot(std::time::Duration::ZERO, move || {
                if let (Some(app), Some(kind)) = (weak.upgrade(), kind_of(&kind)) {
                    app.library.switch_to(&app, kind, &id);
                }
            });
        });
        let weak = Rc::downgrade(app);
        area.on_name_accept(move || {
            if let Some(app) = weak.upgrade() {
                app.library.name_accept(&app);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_remove_accept(move || {
            if let Some(app) = weak.upgrade() {
                app.library.remove_accept(&app);
            }
        });
    }

    fn update(&self, app: &Rc<App>, _window: &AppWindow, changed: Changed) {
        if !changed.any(Changed::LINK) {
            return;
        }
        // Read again when the link (re)connects: a restarted engine may
        // have another library.
        let link = app.session.borrow().link;
        if self.link.replace(link) != link && link == Some(Link::Connected) {
            self.load(app);
        }
    }

    fn menu(&self, app: &Rc<App>, id: &str) -> bool {
        self.command(app, id)
    }

    fn dialog_opened(&self, app: &Rc<App>, kind: &str, arg: &str) {
        self.naming.set(None);
        self.removing.set(None);
        match kind {
            "library-name" => self.name_opened(app, arg),
            "library-remove" => self.remove_opened(app, arg),
            _ => {}
        }
    }

    fn event(&self, app: &Rc<App>, event: &Event) {
        if let Event::LibraryChanged { action, .. } = event {
            // After a switch the live setup is another scene collection or
            // profile, which an undo must not overwrite with the one before.
            if matches!(action, LibraryAction::Switched | LibraryAction::Migrated) {
                app.session.borrow_mut().forget_undo();
            }
            self.load(app);
        }
    }

    fn state(
        &self,
        app: &Rc<App>,
        _window: &AppWindow,
        name: &str,
        arg: &str,
    ) -> Option<anyhow::Result<()>> {
        let kind_and = |arg: &str| -> anyhow::Result<(LibraryKind, String)> {
            let (kind, rest) = arg.split_once(':').context("KIND:VALUE")?;
            let kind = kind_of(kind).context("KIND is collection or profile")?;
            Ok((kind, rest.to_string()))
        };
        Some(match name {
            "library" => {
                if self.command(app, arg) {
                    Ok(())
                } else {
                    Err(anyhow!(
                        "no command {arg:?}; KIND_COMMAND with KIND collection or profile and \
                         COMMAND new, duplicate, rename, remove, import, export"
                    ))
                }
            }
            "library-switch" => kind_and(arg).map(|(kind, id)| self.switch_to(app, kind, &id)),
            "library-import" => {
                kind_and(arg).map(|(kind, path)| self.import_path(app, kind, &path))
            }
            "library-export" => kind_and(arg).and_then(|(kind, path)| {
                let id = self
                    .listing
                    .borrow()
                    .as_ref()
                    .and_then(|l| current(l, kind))
                    .map(|e| e.id.clone())
                    .context("none in use")?;
                self.export_to(app, kind, &id, &path);
                Ok(())
            }),
            _ => return None,
        })
    }
}
