//! Area A, scenes and sources: the Scenes and Sources docks, the source
//! toolbar, adding sources, Properties, Filters, Edit Transform, Rename,
//! and their menus (ui/areas/sources/). The web app's scenes.js,
//! sources.js, source-toolbar.js, add-source.js, properties.js,
//! filters.js and transform.js are the behaviour matched.
//!
//! - items.rs: what the docks and menus do to the setup;
//! - dialogs.rs: Create/Select Source, Properties, Filters, Edit
//!   Transform, Rename;
//! - form.rs, schema.rs, devices.rs: forms drawn from the setup schema;
//! - preview.rs: a source's live picture in Properties and Filters;
//! - files.rs: the system's open panel.

mod colors;
mod dialogs;
pub(super) mod items;
mod preview;

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use isocast_protocol::SourceKind;
use isocast_protocol::api::Method;
use serde_json::{Value, json};
use slint::{ComponentHandle, SharedString};

use super::{Area, Changed};
use crate::app::App;
use crate::form::{self, FormEvent, devices, files, schema};
use crate::kit::{self, MenuBuilder, model};
use crate::model as setup_model;
use crate::session::EditOptions;
use crate::{AppWindow, ItemRow, Menus, SceneRow, SourcesArea, ToastKind, Ui};
use dialogs::Dialogs;

/// Snapshot states (`--state NAME[:ARG]`).
pub const STATES: &[&str] = &[
    "rename",
    "rename-item",
    "cameras",
    "select:PATH",
    "scene:ID",
    "iso:SOURCE:on|off",
    "filter:LIST:KEY",
    "mark:PATH[:range]",
    "into:GROUP",
    "filter-menu:LIST:KEY",
    "filter-choose:LIST:KEY:ID",
    "anchor:ALIGN",
    "add-menu",
];

#[derive(Default)]
pub struct Sources {
    /// The Scenes and Sources lists, changed row by row (see
    /// `kit::update_rows`).
    scene_rows: Rc<slint::VecModel<SceneRow>>,
    item_rows: Rc<slint::VecModel<ItemRow>>,
    /// Groups folded away in the Sources list, by path.
    collapsed: RefCell<HashSet<String>>,
    /// The Sources list's rows (path, parent's path), top first, for drags.
    rows: RefCell<Vec<(String, String)>>,
    /// The copied item (⌘C).
    clipboard: RefCell<Option<Value>>,
    /// Rows chosen together with ⌘ or Shift (paths, with the selected
    /// one), for Group Selected Items; empty for a single selection.
    marked: RefCell<Vec<String>>,
    pub dialogs: Dialogs,
    /// The colour panel's field and the colour last taken from it.
    color_target: RefCell<Option<(ColorTarget, String)>>,
    color_timer: slint::Timer,
}

/// What the colour panel is choosing for.
#[derive(Clone)]
enum ColorTarget {
    /// A form's field (form, key).
    Form(String, String),
    /// A colour of Edit Transform (border-color…).
    Transform(String),
    /// The source toolbar's colour.
    Quick,
}

impl Sources {
    /// ⌘-click (toggle) or Shift-click (`range`: every row from the
    /// selected one) on a Sources row: several items chosen at once, among
    /// siblings, for Group Selected Items. The clicked one is selected.
    pub fn mark_item(&self, app: &Rc<App>, path: &str, range: bool) {
        let selected = app
            .session
            .borrow()
            .selection
            .as_ref()
            .map(|s| s.path.clone());
        let rows = self.rows.borrow().clone();
        let parent_of = |p: &str| rows.iter().find(|r| r.0 == p).map(|r| r.1.clone());
        {
            let mut marked = self.marked.borrow_mut();
            if marked.is_empty()
                && let Some(selected) = &selected
            {
                marked.push(selected.clone());
            }
            // Only siblings: a row of another level starts again.
            let parent = parent_of(path);
            marked.retain(|m| parent_of(m) == parent);
            match (range, selected.as_deref()) {
                (true, Some(from)) if parent_of(from) == parent => {
                    let at = |p: &str| rows.iter().position(|r| r.0 == p);
                    if let (Some(a), Some(b)) = (at(from), at(path)) {
                        for row in &rows[a.min(b)..=a.max(b)] {
                            if row.1 == parent.clone().unwrap_or_default()
                                && !marked.contains(&row.0)
                            {
                                marked.push(row.0.clone());
                            }
                        }
                    }
                }
                _ => {
                    if let Some(i) = marked.iter().position(|m| m == path) {
                        marked.remove(i);
                    } else {
                        marked.push(path.to_string());
                    }
                }
            }
            if marked.len() < 2 {
                marked.clear();
            }
        }
        let keep =
            self.marked.borrow().iter().any(|m| m == path) || self.marked.borrow().is_empty();
        let next = if keep {
            path.to_string()
        } else {
            self.marked.borrow().last().cloned().unwrap_or_default()
        };
        app.select(&next);
        show_lists(app);
    }

    /// A colour field's swatch: open the system colour panel with its
    /// colour, then take each colour chosen there, as a live gesture,
    /// until the panel closes (or another field asks).
    /// A form's colour swatch: the colour panel, its colours sent to the
    /// form (any form: `form::dispatch` routes them).
    pub fn pick_form_color(&self, app: &Rc<App>, form: &str, key: &str, now: &str) {
        self.pick_color(app, ColorTarget::Form(form.into(), key.into()), now);
    }

    fn pick_color(&self, app: &Rc<App>, target: ColorTarget, now: &str) {
        if app.mode.offscreen {
            return;
        }
        colors::open(now);
        *self.color_target.borrow_mut() = Some((target, now.to_string()));
        let weak = Rc::downgrade(app);
        self.color_timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(100),
            move || {
                let Some(app) = weak.upgrade() else { return };
                let Some(window) = app.window() else { return };
                let Some(hex) = colors::current() else {
                    app.sources.color_timer.stop();
                    *app.sources.color_target.borrow_mut() = None;
                    return;
                };
                let target = {
                    let mut current = app.sources.color_target.borrow_mut();
                    match current.as_mut() {
                        Some((target, last)) if *last != hex => {
                            *last = hex.clone();
                            target.clone()
                        }
                        _ => return,
                    }
                };
                match target {
                    ColorTarget::Form(id, key) => {
                        form::dispatch(&app, &window, &id, &key, FormEvent::Color(hex))
                    }
                    ColorTarget::Transform(field) => app
                        .sources
                        .dialogs
                        .transform_color(&app, &window, &field, &hex, true),
                    ColorTarget::Quick => quick_color(&app, &hex),
                }
            },
        );
    }

    /// The GPU device the window draws with (for the dialogs' pictures).
    pub fn set_device(&self, device: slint::wgpu_30::wgpu::Device) {
        self.dialogs.preview.set_device(device);
    }
}

/// Wire a `SourcesArea` callback to a method taking the app (and window).
macro_rules! wire {
    ($area:ident, $app:ident, $on:ident, |$a:ident, $w:ident $(, $arg:ident)*| $body:expr) => {{
        let weak = Rc::downgrade($app);
        $area.$on(move |$($arg),*| {
            if let Some($a) = weak.upgrade()
                && let Some($w) = $a.window()
            {
                let _ = &$w;
                $body
            }
        });
    }};
}

impl Area for Sources {
    fn connect(&self, app: &Rc<App>, window: &AppWindow) {
        let area = window.global::<SourcesArea>();
        area.set_scenes(slint::ModelRc::from(self.scene_rows.clone()));
        area.set_items(slint::ModelRc::from(self.item_rows.clone()));
        wire!(area, app, on_choose_scene, |app, _w, id| app
            .choose_scene(&id));
        wire!(area, app, on_mark_item, |app, _w, path, range| {
            app.sources.mark_item(&app, &path, range)
        });
        wire!(area, app, on_rename_scene, |app, _w, id, name| {
            let name = name.trim();
            if name.is_empty() {
                app.toast(
                    ToastKind::Error,
                    "A scene needs a name",
                    "It kept its old one.",
                );
                return;
            }
            let now = app.session.borrow().setup.as_ref().and_then(|s| {
                setup_model::find_scene(s, &id).map(|x| setup_model::scene_name(x).to_string())
            });
            if now.as_deref() != Some(name) {
                app.edit(
                    json!({ "scenes": [{ "id": id.as_str(), "name": name }] }),
                    EditOptions::default(),
                );
            }
        });
        wire!(area, app, on_move_scene, |app, _w, from, to| {
            items::move_scene_to(&app, from.max(0) as usize, to.max(0) as usize)
        });
        wire!(area, app, on_move_item, |app, _w, from, to| {
            let rows = app.sources.rows.borrow().clone();
            items::move_row(&app, &rows, from.max(0) as usize, to.max(0) as usize);
        });
        wire!(area, app, on_rename_item, |app, _w, path, name| {
            items::rename_item(&app, &path, &name)
        });
        wire!(area, app, on_toggle_visible, |app, _w, path| {
            items::toggle_item(&app, &path, "visible", true)
        });
        wire!(area, app, on_toggle_locked, |app, _w, path| {
            items::toggle_item(&app, &path, "locked", false)
        });
        wire!(area, app, on_toggle_iso, |app, _w, path| {
            let found = {
                let s = app.session.borrow();
                s.editing_scene().and_then(|scene| {
                    let setup = s.setup.clone()?;
                    let edited = s.edited_setup()?;
                    let sc = setup_model::find_scene(edited, &scene)?;
                    let chain = setup_model::item_chain(sc, &path)?;
                    let source = chain[chain.len() - 1].source.clone()?;
                    Some((items::is_iso(&setup, &source), source))
                })
            };
            if let Some((on, source)) = found {
                items::set_iso(&app, &source, !on);
            }
        });
        wire!(area, app, on_toggle_group, |app, _w, path| {
            {
                let mut collapsed = app.sources.collapsed.borrow_mut();
                if !collapsed.remove(path.as_str()) {
                    collapsed.insert(path.to_string());
                }
            }
            show_lists(&app);
        });
        wire!(area, app, on_quick_choose, |app, _w, value| quick_choose(
            &app, &value
        ));
        wire!(
            area,
            app,
            on_quick_text_changed,
            |app, _w, text| quick_text(&app, &text)
        );
        wire!(area, app, on_quick_pick_color, |app, w| {
            let now = w.global::<SourcesArea>().get_quick_text();
            app.sources.pick_color(&app, ColorTarget::Quick, &now);
        });
        wire!(area, app, on_transform_pick_color, |app, _w, field, now| {
            app.sources
                .pick_color(&app, ColorTarget::Transform(field.into()), &now);
        });
        wire!(area, app, on_quick_button_clicked, |app, _w| quick_button(
            &app
        ));

        // Dialogs.
        wire!(area, app, on_create_accept, |app, w| app
            .sources
            .dialogs
            .create_accept(&app, &w));
        wire!(area, app, on_create_changed, |app, w| app
            .sources
            .dialogs
            .show_create(&app, &w));
        wire!(area, app, on_props_accept, |app, _w| app
            .sources
            .dialogs
            .props_accept(&app));
        wire!(area, app, on_props_cancel, |app, _w| app
            .sources
            .dialogs
            .props_cancel(&app));
        wire!(area, app, on_props_defaults, |app, w| app
            .sources
            .dialogs
            .props_defaults(&app, &w));
        wire!(area, app, on_filter_select, |app, w, list, key| {
            app.sources.dialogs.filter_select(&app, &w, &list, &key)
        });
        wire!(area, app, on_filter_toggle, |app, _w, list, key| {
            app.sources.dialogs.filter_toggle(&app, &list, &key)
        });
        wire!(
            area,
            app,
            on_filter_add,
            |app, _w, list, x, y, width, height| {
                let menu = app.sources.dialogs.filter_menu(&list);
                app.show_menu(&format!("filters:add:{list}"), menu, [x, y, width, height]);
            }
        );
        wire!(area, app, on_filter_remove, |app, w, list| {
            app.sources.dialogs.filter_remove(&app, &w, &list)
        });
        wire!(area, app, on_filter_move, |app, _w, list, by| {
            app.sources.dialogs.filter_move(&app, &list, by)
        });
        wire!(area, app, on_filter_menu, |app, _w, list, key, x, y| {
            let menu = app.sources.dialogs.filter_item_menu(&list);
            app.show_menu(
                &format!("filters:item:{list}:{key}"),
                menu,
                [x, y, 0.0, 0.0],
            );
        });
        wire!(area, app, on_filter_rename, |app, w, list, key, name| {
            app.sources
                .dialogs
                .filter_rename(&app, &w, &list, &key, &name)
        });
        wire!(area, app, on_filter_defaults, |app, _w| app
            .sources
            .dialogs
            .filter_defaults(&app));
        wire!(
            area,
            app,
            on_mask_moved,
            |app, w, x, y, width, height, rotation, done| {
                app.sources
                    .dialogs
                    .mask_moved(&app, &w, [x, y, width, height, rotation], done)
            }
        );
        wire!(area, app, on_transform_number, |app, w, field, v, live| {
            app.sources
                .dialogs
                .transform_number(&app, &w, &field, v, live)
        });
        wire!(area, app, on_transform_choose, |app, w, field, v| {
            app.sources.dialogs.transform_choose(&app, &w, &field, &v)
        });
        wire!(area, app, on_transform_color, |app, w, field, v| {
            app.sources
                .dialogs
                .transform_color(&app, &w, &field, &v, false)
        });
        wire!(area, app, on_transform_action, |app, w, action| {
            app.sources.dialogs.transform_action(&app, &w, &action)
        });
        wire!(area, app, on_rename_accept, |app, w| app
            .sources
            .dialogs
            .rename_accept(&app, &w));
    }

    fn update(&self, app: &Rc<App>, window: &AppWindow, changed: Changed) {
        // A selection made otherwise (a click, the canvas) ends a ⌘/Shift
        // one, and so does another scene.
        let mut lists = changed.any(Changed::SETUP | Changed::SCENES | Changed::RUNNING);
        if changed.any(Changed::SELECTION | Changed::SCENES) {
            let selected = window.global::<Ui>().get_selected_item().to_string();
            let mut marked = self.marked.borrow_mut();
            if !marked.is_empty() && (selected.is_empty() || !marked.contains(&selected)) {
                marked.clear();
                lists = true;
            }
        }
        if lists {
            show_lists(app);
        }
        if changed.any(Changed::SELECTION | Changed::SETUP | Changed::DEVICES) {
            show_quick_setting(app, window);
        }
        if changed.any(
            Changed::SETUP
                | Changed::SELECTION
                | Changed::DEVICES
                | Changed::RUNNING
                | Changed::SCENES,
        ) {
            self.dialogs.update(app, window);
        }
    }

    fn dialog_opened(&self, app: &Rc<App>, kind: &str, arg: &str) {
        if let Some(window) = app.window() {
            self.dialogs.opened(app, &window, kind, arg);
        }
    }

    fn menu(&self, app: &Rc<App>, id: &str) -> bool {
        if let Some(n) = id.strip_prefix("scene:") {
            // 1–9: that scene on air (or into preview).
            let scene = n.parse::<usize>().ok().and_then(|n| {
                let s = app.session.borrow();
                s.setup
                    .as_ref()?
                    .scenes
                    .get(n.checked_sub(1)?)
                    .map(|x| x.id.clone())
            });
            if let Some(scene) = scene {
                app.choose_scene(&scene);
            }
            return true;
        }
        if let Some(by) = id.strip_prefix("nudge:") {
            nudge(app, by);
            return true;
        }
        // Commands that act on the main window wait while a dialog or the
        // first run is showing (the web app's `onWindow`).
        let dialog_open = app.window().is_some_and(|w| {
            let ui = w.global::<Ui>();
            !ui.get_dialog().is_empty() || ui.get_onboarding()
        });
        let current_scene = || app.session.borrow().editing_scene();
        match id {
            // The selected item's source's filters (its menu's Copy
            // Filters and Paste Filters).
            "copy_filters" | "paste_filters" => {
                let found = app
                    .session
                    .borrow()
                    .selected(|_, chain| chain[chain.len() - 1].source.clone());
                if let Some(Some(source)) = found {
                    if id == "copy_filters" {
                        self.dialogs.copy_filters(app, &source);
                    } else {
                        self.dialogs.paste_filters(app, &source, None);
                    }
                }
            }
            // ⇧⌘N only adds (as the web app's); the Scenes dock's + also
            // puts the new name in its field.
            "new_scene" => {
                items::add_scene(app);
            }
            "new_scene_rename" => {
                if let Some(scene) = items::add_scene(app) {
                    self.rename_scene_later(app, scene);
                }
            }
            "add_source" => {
                // Under the Sources dock's +, as the web app's ⇧⌘A.
                if let Some(window) = app.window() {
                    let area = window.global::<SourcesArea>();
                    area.set_add_menu_serial(area.get_add_menu_serial() + 1);
                }
            }
            "remove_scene" => {
                if let Some(scene) = current_scene() {
                    items::remove_scene(app, &scene);
                }
            }
            "scene_up" | "scene_down" => {
                if let Some(scene) = current_scene() {
                    items::move_scene(app, &scene, &id["scene_".len()..]);
                }
            }
            _ if dialog_open => return is_window_command(id),
            "remove" => items::remove_selected(app),
            "properties" | "filters" => {
                let source = app
                    .window()
                    .map(|w| w.global::<Ui>().get_selected_source())
                    .unwrap_or_default();
                if !source.is_empty() {
                    app.open_dialog(id, &source);
                }
            }
            "transform_edit" => app.open_dialog("transform", ""),
            "transform_reset" => items::reset_transform(app),
            "rotate_cw" => items::rotate_by(app, 90.0),
            "rotate_ccw" => items::rotate_by(app, -90.0),
            "rotate_180" => items::rotate_by(app, 180.0),
            "flip_horizontal" | "flip_vertical" => items::flip(app, id),
            "fit_screen" => items::fit_to_screen(app),
            "stretch_screen" => items::stretch_to_screen(app),
            "center_screen" => items::center_to_screen(app, None),
            "center_vertical" => items::center_to_screen(app, Some("y")),
            "center_horizontal" => items::center_to_screen(app, Some("x")),
            "order_up" => items::order_selected(app, "forward"),
            "order_down" => items::order_selected(app, "backward"),
            "order_top" => items::order_selected(app, "front"),
            "order_bottom" => items::order_selected(app, "back"),
            "copy" => {
                if let Some(item) = items::copy_selected(app) {
                    *self.clipboard.borrow_mut() = Some(item);
                }
            }
            "paste" | "paste_duplicate" => {
                let item = self.clipboard.borrow().clone();
                if let Some(item) = item {
                    items::paste(app, &item, id == "paste_duplicate");
                }
            }
            _ => return false,
        }
        true
    }

    fn context_menu(&self, app: &Rc<App>, owner: &str) -> Option<MenuBuilder> {
        if let Some(scene) = owner.strip_prefix("scenes:") {
            return Some(self.scene_menu(app, scene));
        }
        if owner == "sources:add" {
            let has_scene = app
                .session
                .borrow()
                .setup
                .as_ref()
                .is_some_and(|s| !s.scenes.is_empty());
            if !has_scene {
                app.toast(
                    ToastKind::Info,
                    "Add a scene first",
                    "Sources live in scenes: add one with + under Scenes.",
                );
                return None;
            }
            return Some(add_menu(true));
        }
        if let Some(path) = owner.strip_prefix("sources:item:") {
            app.select(path);
            return Some(self.item_menu(app, false, true));
        }
        // The ISO badge's: recording the file, and its audio tracks.
        if let Some(path) = owner.strip_prefix("sources:iso:") {
            app.select(path);
            let s = app.session.borrow();
            let setup = s.setup.clone()?;
            let source = s
                .selected(|_, chain| chain[chain.len() - 1].source.clone())
                .flatten()?;
            drop(s);
            let on = items::is_iso(&setup, &source);
            let menu = MenuBuilder::new()
                .item("iso", "Record ISO File")
                .checked(on);
            if !on {
                return Some(menu);
            }
            return Some(items::iso_audio_menu(
                menu.separator().heading("ISO Audio"),
                &setup,
                &source,
            ));
        }
        if owner == "sources:list" {
            return Some(self.list_menu(app, false));
        }
        // The preview: on an item (the canvas selects it first) the item's
        // menu with the preview's items first and no Rename; elsewhere the
        // preview's own, whatever is selected (the web app's itemMenu and
        // canvasMenu with `onPreview`).
        if let Some(path) = owner.strip_prefix("canvas:item:") {
            app.select(path);
            return Some(self.item_menu(app, true, false));
        }
        if owner == "canvas" {
            return Some(self.list_menu(app, true));
        }
        None
    }

    fn menu_chosen(&self, app: &Rc<App>, owner: &str, id: &str) -> bool {
        if let Some(list) = owner.strip_prefix("filters:add:") {
            if let (Some(kind), Some(window)) = (id.strip_prefix("add:"), app.window()) {
                self.dialogs.filter_add(app, &window, list, kind);
            }
            return true;
        }
        if let Some((list, key)) = owner
            .strip_prefix("filters:item:")
            .and_then(|rest| rest.split_once(':'))
        {
            if let Some(window) = app.window() {
                self.dialogs.filter_item_chosen(app, &window, list, key, id);
            }
            return true;
        }
        if let Some(scene) = owner.strip_prefix("scenes:") {
            self.scene_chosen(app, scene, id);
            return true;
        }
        if !(owner.starts_with("sources:") || owner.starts_with("canvas")) {
            return false;
        }
        if id == "add:group" {
            if let Some(path) = items::add_group(app) {
                app.select(&path);
            }
            return true;
        }
        if let Some(kind) = id.strip_prefix("add:") {
            app.open_dialog("create-source", kind);
            return true;
        }
        let path = owner
            .strip_prefix("sources:item:")
            .or_else(|| owner.strip_prefix("sources:iso:"))
            .or_else(|| owner.strip_prefix("canvas:item:"))
            .map(str::to_string);
        let selected_path = || {
            path.clone().or_else(|| {
                app.session
                    .borrow()
                    .selection
                    .as_ref()
                    .map(|s| s.path.clone())
            })
        };
        if let Some(audio) = id.strip_prefix("iso_audio:") {
            let found = app
                .session
                .borrow()
                .selected(|_, chain| chain[chain.len() - 1].source.clone());
            if let Some(Some(source)) = found {
                items::choose_iso_audio(app, &source, audio);
            }
        } else if let Some(group) = id.strip_prefix("into:") {
            if let Some(path) = selected_path() {
                items::move_to_group(app, &path, group);
            }
        } else if let Some(kind) = id.strip_prefix("show:") {
            item_transition(app, "show_transition", kind);
        } else if let Some(kind) = id.strip_prefix("hide:") {
            item_transition(app, "hide_transition", kind);
        } else if let Some(filter) = id.strip_prefix("scale:") {
            let v = if filter == "bilinear" {
                Value::Null
            } else {
                json!(filter)
            };
            items::update_selected(app, json!({ "scale_filter": v }), None);
        } else if let Some(blend) = id.strip_prefix("blend:") {
            let v = if blend == "normal" {
                Value::Null
            } else {
                json!(blend)
            };
            items::update_selected(app, json!({ "blend": v }), None);
        } else {
            match id {
                "rename" => {
                    if let (Some(path), Some(window)) = (selected_path(), app.window()) {
                        window
                            .global::<SourcesArea>()
                            .set_renaming_item(path.into());
                    }
                }
                "iso" => {
                    let found = app
                        .session
                        .borrow()
                        .selected(|_, chain| chain[chain.len() - 1].source.clone());
                    if let Some(Some(source)) = found {
                        let on = app
                            .session
                            .borrow()
                            .setup
                            .as_ref()
                            .is_some_and(|s| items::is_iso(s, &source));
                        items::set_iso(app, &source, !on);
                    }
                }
                "lock" => {
                    if let Some(path) = selected_path() {
                        items::toggle_item(app, &path, "locked", false);
                    }
                }
                "group_selected" => {
                    let paths = self.marked.borrow().clone();
                    match items::group_items(app, &paths) {
                        Ok(path) => {
                            self.marked.borrow_mut().clear();
                            let weak = Rc::downgrade(app);
                            // Selected once the edit has made it.
                            slint::Timer::single_shot(
                                std::time::Duration::from_millis(50),
                                move || {
                                    if let Some(app) = weak.upgrade() {
                                        app.select(&path);
                                    }
                                },
                            );
                        }
                        Err(why) => {
                            app.toast(ToastKind::Info, "Can’t group these", why);
                        }
                    }
                }
                "ungroup" => {
                    if let Some(path) = selected_path() {
                        items::ungroup(app, &path);
                    }
                }
                "lock_preview" | "preview_scale_window" | "preview_scale_canvas" => app.menu(id),
                other => {
                    self.menu(app, other);
                }
            }
        }
        true
    }

    fn state(
        &self,
        app: &Rc<App>,
        window: &AppWindow,
        name: &str,
        arg: &str,
    ) -> Option<anyhow::Result<()>> {
        let area = window.global::<SourcesArea>();
        match name {
            // The scene being edited in its name field.
            "rename" => {
                let scene = app.session.borrow().editing_scene().unwrap_or_default();
                area.set_renaming_scene(scene.into());
            }
            // The selected item in its name field.
            "rename-item" => {
                area.set_renaming_item(window.global::<Ui>().get_selected_item());
            }
            // The source toolbar's quick setting open.
            "cameras" => {
                area.set_open_quick(area.get_open_quick() + 1);
            }
            // An item of the scene being edited, by its path.
            "select" => app.select(arg),
            // A scene on air (or into preview), by id.
            "scene" => app.choose_scene(arg),
            // A source recorded as its own file, or not.
            "iso" => {
                let (source, on) = arg.split_once(':').unwrap_or((arg, "on"));
                items::set_iso(app, source, on != "off");
            }
            // A filter selected in the open Filters dialog: LIST:KEY.
            "filter" => {
                let (list, key) = arg.split_once(':').unwrap_or(("filters", arg));
                self.dialogs.filter_select(app, window, list, key);
            }
            // ⌘-click (or Shift-click, `:range`) on a Sources row.
            "mark" => {
                let (path, range) = match arg.strip_suffix(":range") {
                    Some(path) => (path, true),
                    None => (arg, false),
                };
                self.mark_item(app, path, range);
            }
            // The selected item's menu's Move to Group → GROUP.
            "into" => {
                self.menu_chosen(app, "sources:list", &format!("into:{arg}"));
            }
            // A filter row's menu, open beside the sidebar (as a
            // right-click opens it), and a choice from it.
            "filter-menu" => {
                let (list, key) = arg.split_once(':').unwrap_or(("filters", arg));
                self.dialogs.filter_select(app, window, list, key);
                let menu = self.dialogs.filter_item_menu(list);
                app.show_menu(
                    &format!("filters:item:{list}:{key}"),
                    menu,
                    [400.0, 120.0, 0.0, 0.0],
                );
            }
            "filter-choose" => {
                let mut parts = arg.splitn(3, ':');
                let (Some(list), Some(key), Some(id)) = (parts.next(), parts.next(), parts.next())
                else {
                    return Some(Err(anyhow::anyhow!(
                        "filter-choose takes LIST:KEY:ID (rename, duplicate, copy, paste, remove)"
                    )));
                };
                self.dialogs.filter_item_chosen(app, window, list, key, id);
            }
            // Edit Transform's positional alignment.
            "anchor" => self.dialogs.transform_choose(app, window, "anchor", arg),
            // The Add menu, hanging from + under Sources (as a click opens it).
            "add-menu" => {
                let ui = window.global::<Ui>();
                let size = window
                    .window()
                    .size()
                    .to_logical(window.window().scale_factor());
                let x = if ui.get_show_scenes() {
                    ui.get_scenes_width() + 1.0
                } else {
                    0.0
                } + 8.0;
                let bottom = size.height - if ui.get_show_status_bar() { 24.0 } else { 0.0 };
                let y = bottom - 32.0 + 1.0 + 4.0;
                window
                    .global::<Menus>()
                    .invoke_request("sources:add".into(), x, y, 24.0, 24.0);
            }
            _ => return None,
        }
        Some(Ok(()))
    }
}

/// Window commands a dialog swallows (they act on the main window).
fn is_window_command(id: &str) -> bool {
    matches!(
        id,
        "remove"
            | "properties"
            | "filters"
            | "transform_edit"
            | "transform_reset"
            | "rotate_cw"
            | "rotate_ccw"
            | "rotate_180"
            | "flip_horizontal"
            | "flip_vertical"
            | "fit_screen"
            | "stretch_screen"
            | "center_screen"
            | "center_vertical"
            | "center_horizontal"
            | "order_up"
            | "order_down"
            | "order_top"
            | "order_bottom"
            | "copy"
            | "paste"
            | "paste_duplicate"
    )
}

// ---------- Menus ----------

/// The kinds of source, in OBS's words, sorted (and Scene), each opening
/// Create/Select Source; with icons for the + menu.
fn add_menu(icons: bool) -> MenuBuilder {
    let mut kinds: Vec<(&str, &str, &str)> = setup_model::KINDS
        .iter()
        .map(|k| (k.kind, k.label, k.icon))
        .collect();
    kinds.push(("scene", "Scene", "scene"));
    kinds.push(("group", "Group", "group"));
    kinds.sort_by_key(|k| k.1.to_lowercase());
    kinds
        .into_iter()
        .fold(MenuBuilder::new(), |menu, (kind, label, icon)| {
            let menu = menu.item(&format!("add:{kind}"), label);
            if icons { menu.icon(icon) } else { menu }
        })
}

const TRANSITIONS: [(&str, &str); 8] = [
    ("cut", "Cut"),
    ("fade", "Fade"),
    ("swipe", "Swipe"),
    ("slide", "Slide"),
    ("fade_to_color", "Fade to Color"),
    ("luma_wipe", "Luma Wipe"),
    ("stinger", "Stinger"),
    ("move", "Move"),
];

const SCALE_FILTERS: [(&str, &str); 5] = [
    ("point", "Point"),
    ("bilinear", "Bilinear"),
    ("bicubic", "Bicubic"),
    ("lanczos", "Lanczos"),
    ("area", "Area"),
];

const BLENDS: [&str; 7] = [
    "normal", "additive", "subtract", "screen", "multiply", "lighten", "darken",
];

fn order_menu() -> MenuBuilder {
    MenuBuilder::new()
        .item("order_up", "Move Up")
        .item("order_down", "Move Down")
        .separator()
        .item("order_top", "Move to Top")
        .item("order_bottom", "Move to Bottom")
}

fn transform_menu() -> MenuBuilder {
    MenuBuilder::new()
        .item("transform_edit", "Edit Transform…")
        .hint("⌘E")
        .item("transform_reset", "Reset Transform")
        .hint("⌘R")
        .separator()
        .item("rotate_cw", "Rotate 90° Clockwise")
        .item("rotate_ccw", "Rotate 90° Counterclockwise")
        .item("rotate_180", "Rotate 180°")
        .separator()
        .item("flip_horizontal", "Flip Horizontal")
        .item("flip_vertical", "Flip Vertical")
        .separator()
        .item("fit_screen", "Fit to Screen")
        .hint("⌘F")
        .item("stretch_screen", "Stretch to Screen")
        .hint("⌘S")
        .item("center_screen", "Center to Screen")
        .hint("⌘D")
        .item("center_vertical", "Center Vertically")
        .item("center_horizontal", "Center Horizontally")
}

impl Sources {
    /// Put a newly added scene's name in its field.
    fn rename_scene_later(&self, app: &Rc<App>, scene: String) {
        let weak = Rc::downgrade(app);
        slint::Timer::single_shot(std::time::Duration::from_millis(50), move || {
            if let Some(app) = weak.upgrade()
                && let Some(window) = app.window()
            {
                window
                    .global::<SourcesArea>()
                    .set_renaming_scene(scene.into());
            }
        });
    }

    /// The preview's own items (Lock Preview, Preview Scaling), at the top
    /// of its menus as in OBS.
    fn preview_items(&self, app: &Rc<App>, menu: MenuBuilder) -> MenuBuilder {
        let (locked, scale) = app
            .window()
            .map(|w| {
                let ui = w.global::<Ui>();
                (ui.get_preview_locked(), ui.get_preview_scale())
            })
            .unwrap_or_default();
        menu.item("lock_preview", "Lock Preview")
            .checked(locked)
            .submenu(
                "Preview Scaling",
                MenuBuilder::new()
                    .item("preview_scale_window", "Scale to Window")
                    .checked(scale != "canvas")
                    .item("preview_scale_canvas", "Canvas (100%)")
                    .checked(scale == "canvas"),
            )
            .separator()
    }

    fn paste_items(&self, menu: MenuBuilder) -> MenuBuilder {
        let has = self.clipboard.borrow().is_some();
        menu.item("paste", "Paste (Reference)")
            .hint("⌘V")
            .disabled(!has)
            .item("paste_duplicate", "Paste (Duplicate)")
            .disabled(!has)
    }

    /// The menu for the Sources list away from any item, or the preview
    /// with nothing selected.
    fn list_menu(&self, app: &Rc<App>, on_preview: bool) -> MenuBuilder {
        let mut menu = MenuBuilder::new();
        if on_preview {
            menu = self.preview_items(app, menu);
        }
        if app.session.borrow().editing_scene().is_some() {
            menu = menu.submenu("Add", add_menu(false));
        }
        self.paste_items(menu)
    }

    /// The menu for the selected item, from its row or the preview.
    fn item_menu(&self, app: &Rc<App>, on_preview: bool, rename: bool) -> MenuBuilder {
        let s = app.session.borrow();
        let found = s.selected(|_, chain| chain[chain.len() - 1].clone());
        let Some(item) = found else {
            drop(s);
            return self.list_menu(app, on_preview);
        };
        let source = item
            .source
            .as_deref()
            .and_then(|id| setup_model::find_source(s.setup.as_ref()?, id).cloned());
        let iso = source.as_ref().map(|src| {
            (
                src.kind.has_video(),
                s.setup.as_ref().is_some_and(|x| items::is_iso(x, &src.id)),
            )
        });
        drop(s);
        let mut menu = MenuBuilder::new();
        if on_preview {
            menu = self.preview_items(app, menu);
        }
        let marked = self.marked.borrow().len();
        if marked >= 2 {
            menu = menu
                .item("group_selected", "Group Selected Items")
                .separator();
        }
        menu = menu
            .submenu("Add", add_menu(false))
            .separator()
            .item("copy", "Copy")
            .hint("⌘C");
        menu = self.paste_items(menu).separator();
        if rename && (item.source.is_some() || item.scene.is_some()) {
            menu = menu.item("rename", "Rename");
        }
        let is_group = item.source.is_none() && item.scene.is_none() && item.content.is_none();
        if is_group {
            menu = menu.item("ungroup", "Ungroup");
        } else if marked < 2 {
            let top_level = app
                .session
                .borrow()
                .selection
                .as_ref()
                .is_some_and(|s| !s.path.contains('/'));
            let groups = items::groups(app);
            if top_level && !groups.is_empty() {
                menu = menu.submenu(
                    "Move to Group",
                    groups.iter().fold(MenuBuilder::new(), |m, (key, label)| {
                        m.item(&format!("into:{key}"), label)
                    }),
                );
            }
        }
        let transition = |current: Option<&isocast_protocol::ItemTransition>, prefix: &str| {
            let kind = current.map(|t| format!("{:?}", t.kind).to_lowercase());
            MenuBuilder::new()
                .item(&format!("{prefix}:none"), "None")
                .checked(kind.is_none())
                .item(&format!("{prefix}:fade"), "Fade")
                .checked(kind.as_deref() == Some("fade"))
                .item(&format!("{prefix}:slide"), "Slide")
                .checked(kind.as_deref() == Some("slide"))
        };
        let scale = serde_json::to_value(item.scale_filter)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or("bilinear".into());
        let blend = serde_json::to_value(item.blend)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or("normal".into());
        menu = menu
            .item("remove", "Remove")
            .hint("⌫")
            .danger()
            .separator()
            .submenu("Order", order_menu())
            .submenu("Transform", transform_menu())
            .separator()
            .submenu(
                "Show Transition",
                transition(item.show_transition.as_ref(), "show"),
            )
            .submenu(
                "Hide Transition",
                transition(item.hide_transition.as_ref(), "hide"),
            )
            .separator()
            .submenu(
                "Scale Filtering",
                SCALE_FILTERS
                    .iter()
                    .fold(MenuBuilder::new(), |m, (v, label)| {
                        m.item(&format!("scale:{v}"), label).checked(scale == *v)
                    }),
            )
            .submenu(
                "Blending Mode",
                BLENDS.iter().fold(MenuBuilder::new(), |m, v| {
                    m.item(&format!("blend:{v}"), &schema::humanize(v))
                        .checked(blend == *v)
                }),
            )
            .separator();
        if let Some((true, on)) = iso {
            menu = menu.item("iso", "Record ISO File").checked(on);
            if let (true, Some(src), Some(setup)) =
                (on, &source, app.session.borrow().setup.as_ref())
            {
                menu = menu.submenu(
                    "ISO Audio",
                    items::iso_audio_menu(MenuBuilder::new(), setup, &src.id),
                );
            }
        }
        menu = menu.item("lock", "Lock").checked(item.locked).separator();
        if source.is_some() {
            menu = menu
                .item("filters", "Filters")
                .item("copy_filters", "Copy Filters")
                .item("paste_filters", "Paste Filters")
                .disabled(!self.dialogs.has_copied_filters())
                .item("properties", "Properties");
        }
        menu
    }

    /// The menu for a scene ("" for the list away from any scene).
    fn scene_menu(&self, app: &Rc<App>, scene: &str) -> MenuBuilder {
        let s = app.session.borrow();
        let setup = s.setup.clone();
        let editing = s.editing_scene();
        drop(s);
        let Some(setup) = setup else {
            return MenuBuilder::new();
        };
        let found = setup.scenes.iter().position(|x| x.id == scene);
        if found.is_some() && editing.as_deref() != Some(scene) {
            app.choose_scene(scene);
        }
        let mut menu = MenuBuilder::new().item("add", "Add");
        let Some(index) = found else {
            return menu;
        };
        let last = setup.scenes.len() - 1;
        let current = setup.scenes[index]
            .transition
            .as_ref()
            .map(|t| t.kind.label());
        let overrides = TRANSITIONS.iter().filter(|(k, _)| *k != "stinger").fold(
            MenuBuilder::new()
                .item("override:", "None")
                .checked(current.is_none())
                .separator(),
            |m, (kind, label)| {
                m.item(&format!("override:{kind}"), label)
                    .checked(current == Some(*kind))
            },
        );
        menu = menu
            .item("duplicate", "Duplicate")
            .item("rename", "Rename")
            // No ⌫: the key doesn't remove a scene (it removes the
            // selected source), so the menu doesn't promise it.
            .item("remove", "Remove")
            .danger()
            .separator()
            .submenu(
                "Order",
                MenuBuilder::new()
                    .item("up", "Move Up")
                    .disabled(index == 0)
                    .item("down", "Move Down")
                    .disabled(index == last)
                    .separator()
                    .item("top", "Move to Top")
                    .disabled(index == 0)
                    .item("bottom", "Move to Bottom")
                    .disabled(index == last),
            )
            .separator()
            .submenu("Transition Override", overrides);
        menu
    }

    fn scene_chosen(&self, app: &Rc<App>, scene: &str, id: &str) {
        if let Some(kind) = id.strip_prefix("override:") {
            items::set_override(app, scene, (!kind.is_empty()).then_some(kind));
            return;
        }
        match id {
            "add" => {
                if let Some(new) = items::add_scene(app) {
                    self.rename_scene_later(app, new);
                }
            }
            "duplicate" => {
                items::duplicate_scene(app, scene);
            }
            "rename" => {
                if let Some(window) = app.window() {
                    window
                        .global::<SourcesArea>()
                        .set_renaming_scene(scene.into());
                }
            }
            "remove" => items::remove_scene(app, scene),
            "up" | "down" | "top" | "bottom" => items::move_scene(app, scene, id),
            _ => {}
        }
    }
}

/// A show or hide transition of the selected item.
fn item_transition(app: &Rc<App>, field: &str, kind: &str) {
    let found = items::selected_json(app);
    let Some((_, chain)) = found else { return };
    let item = &chain[chain.len() - 1];
    let value = if kind == "none" {
        Value::Null
    } else {
        let duration = item[field]["duration_ms"].as_u64().unwrap_or(300);
        json!({ "kind": kind, "duration_ms": duration })
    };
    items::update_selected(app, json!({ field: value }), None);
}

// ---------- The lists ----------

/// The Scenes and Sources lists.
fn show_lists(app: &Rc<App>) {
    let s = app.session.borrow();
    let Some(setup) = s.setup.clone() else {
        app.sources.scene_rows.set_vec(Vec::new());
        app.sources.item_rows.set_vec(Vec::new());
        app.sources.rows.borrow_mut().clear();
        return;
    };
    let scenes = setup.scenes.iter().enumerate().map(|(i, scene)| {
        let name = setup_model::scene_name(scene);
        SceneRow {
            id: scene.id.as_str().into(),
            name: name.into(),
            // On the canvas whose scenes are switched (the vertical one's
            // own, #86).
            on_air: s.on_air() == Some(&scene.id),
            in_preview: s.staged() == Some(&scene.id),
            tooltip: if i < 9 {
                format!("{name} ({})", i + 1)
            } else {
                name.to_string()
            }
            .into(),
        }
    });
    kit::update_rows(&app.sources.scene_rows, scenes.collect());
    let collapsed = app.sources.collapsed.borrow();
    let mut rows = Vec::new();
    // The items of the canvas being edited (the vertical ones while editing
    // it); sources and ISO files are the setup's own.
    let edited = s.edited_setup().cloned().unwrap_or_else(|| setup.clone());
    if let Some(scene) = s
        .editing_scene()
        .and_then(|id| setup_model::find_scene(&edited, &id))
    {
        // Top first, as OBS lists them; children under their group.
        struct Walk<'a> {
            setup: &'a isocast_protocol::Setup,
            running: &'a std::collections::HashMap<String, isocast_protocol::api::SourceStatus>,
            collapsed: &'a HashSet<String>,
            marked: &'a [String],
        }
        fn walk(
            w: &Walk,
            items: &[isocast_protocol::SceneItem],
            prefix: &str,
            depth: i32,
            rows: &mut Vec<ItemRow>,
        ) {
            for item in items.iter().rev() {
                let key = setup_model::key(item);
                let path = if prefix.is_empty() {
                    key.to_string()
                } else {
                    format!("{prefix}/{key}")
                };
                let source = item.source.clone().unwrap_or_default();
                let found = item
                    .source
                    .as_deref()
                    .and_then(|id| setup_model::find_source(w.setup, id));
                let has_video = found.is_some_and(|s| s.kind.has_video());
                let run = w.running.get(&source);
                let state = match run.map(|r| r.state) {
                    Some(isocast_protocol::api::SourceRun::Opening) => "opening",
                    Some(isocast_protocol::api::SourceRun::Failed) => "failed",
                    Some(isocast_protocol::api::SourceRun::WaitingForPermission) => {
                        "waiting for permission"
                    }
                    _ => "",
                };
                let group = !item.children.is_empty();
                let expanded = !w.collapsed.contains(&path);
                let missing = item.source.is_some() && found.is_none();
                let mut name = setup_model::item_label(w.setup, item);
                if missing {
                    name.push_str(" · missing");
                }
                if let Some(run) = run
                    .filter(|r| r.state == isocast_protocol::api::SourceRun::WaitingForPermission)
                {
                    name.push_str(" · ");
                    name.push_str(
                        run.error
                            .as_deref()
                            .unwrap_or("waiting for permission")
                            .split(';')
                            .next()
                            .unwrap_or("waiting for permission"),
                    );
                }
                rows.push(ItemRow {
                    path: path.as_str().into(),
                    source: if found.is_some() {
                        source.as_str().into()
                    } else {
                        SharedString::new()
                    },
                    name: name.into(),
                    icon: if group {
                        if expanded {
                            "chevron-down"
                        } else {
                            "chevron-right"
                        }
                    } else {
                        setup_model::item_icon(w.setup, item)
                    }
                    .into(),
                    visible: item.visible,
                    locked: item.locked,
                    iso: items::is_iso(w.setup, &source),
                    can_iso: has_video,
                    depth,
                    group,
                    expanded,
                    state: state.into(),
                    error: run
                        .and_then(|r| r.error.clone())
                        .unwrap_or_else(|| {
                            if state == "failed" {
                                "This source couldn’t start.".into()
                            } else {
                                String::new()
                            }
                        })
                        .into(),
                    missing,
                    parent: prefix.into(),
                    marked: w.marked.contains(&path),
                });
                if group && expanded {
                    walk(w, &item.children, &path, depth + 1, rows);
                }
            }
        }
        let marked = app.sources.marked.borrow();
        let w = Walk {
            setup: &setup,
            running: &s.running,
            collapsed: &collapsed,
            marked: &marked,
        };
        walk(&w, &scene.items, "", 0, &mut rows);
    }
    *app.sources.rows.borrow_mut() = rows
        .iter()
        .map(|r| (r.path.to_string(), r.parent.to_string()))
        .collect();
    kit::update_rows(&app.sources.item_rows, rows);
}

// ---------- The source toolbar ----------

/// The selected item's source, as the setup has it.
fn selected_source(app: &Rc<App>) -> Option<isocast_protocol::Source> {
    let s = app.session.borrow();
    let id = s
        .selected(|_, chain| chain[chain.len() - 1].source.clone())
        .flatten()?;
    setup_model::find_source(s.setup.as_ref()?, &id).cloned()
}

fn source_value(app: &Rc<App>, id: &str) -> Value {
    app.session
        .borrow()
        .setup_json()
        .and_then(|s| {
            s["sources"]
                .as_array()?
                .iter()
                .find(|x| x["id"] == id)
                .cloned()
        })
        .unwrap_or(Value::Null)
}

/// The source toolbar's quick setting: the one or two settings people
/// change most for the selected source's kind.
fn show_quick_setting(app: &Rc<App>, window: &AppWindow) {
    let area = window.global::<SourcesArea>();
    let clear = |area: &SourcesArea| {
        area.set_quick_label(SharedString::new());
        area.set_quick_kind(SharedString::new());
        area.set_quick_button(SharedString::new());
    };
    let Some(source) = selected_source(app) else {
        clear(&area);
        return;
    };
    let value = source_value(app, &source.id);
    let text = |k: &str| value[k].as_str().unwrap_or_default().to_string();
    let kind = source.kind.label();
    let needs_devices = matches!(
        kind,
        "display" | "camera" | "microphone" | "window" | "application" | "application_audio"
    );
    if needs_devices {
        app.load_devices(false);
    }
    let devices = app.session.borrow().devices.clone();
    let choice = |area: &SourcesArea, label: &str, field: &str, options: Vec<devices::Choice>| {
        let v = text(field);
        let options = if options.is_empty() {
            vec![(v.clone(), v.clone(), String::new())]
        } else {
            options
        };
        let mut list: Vec<_> = options
            .iter()
            .map(|(v, l, d)| kit::option(v, l, d))
            .collect();
        kit::disambiguate(&mut list);
        area.set_quick_label(label.into());
        area.set_quick_kind("choice".into());
        area.set_quick_options(model(list));
        area.set_quick_value(v.into());
        area.set_quick_button(SharedString::new());
    };
    let path = |area: &SourcesArea, label: &str, shown: String, tooltip: String, button: &str| {
        area.set_quick_label(label.into());
        area.set_quick_kind("path".into());
        area.set_quick_text(shown.into());
        area.set_quick_tooltip(tooltip.into());
        area.set_quick_button(button.into());
    };
    let name = |p: &str| p.rsplit('/').next().unwrap_or(p).to_string();
    match &source.kind {
        SourceKind::Display(_) => choice(
            &area,
            "Display",
            "display",
            devices::display_options(devices.as_ref(), &text("display")),
        ),
        SourceKind::Camera(_) => choice(
            &area,
            "Device",
            "device",
            devices::camera_options(devices.as_ref(), &text("device")),
        ),
        SourceKind::Microphone(_) => choice(
            &area,
            "Device",
            "device",
            devices::microphone_options(devices.as_ref(), &text("device")),
        ),
        SourceKind::Window(_) if devices::PICKED_IN_DIALOG => choice(
            &area,
            "Window",
            "app",
            devices::picked_window_options(&text("app")),
        ),
        SourceKind::Window(_) | SourceKind::Application(_) => choice(
            &area,
            "Application",
            "app",
            devices::app_options(devices.as_ref(), &text("app")),
        ),
        SourceKind::ApplicationAudio(_) => choice(
            &area,
            "Application",
            "app",
            devices::audio_app_options(devices.as_ref(), &text("app")),
        ),
        SourceKind::Text(_) => {
            let file = text("file");
            if file.is_empty() {
                area.set_quick_label("Text".into());
                area.set_quick_kind("text".into());
                area.set_quick_text(text("text").into());
                area.set_quick_button(SharedString::new());
            } else {
                path(&area, "From", name(&file), file, "");
            }
        }
        SourceKind::Image(_) => {
            let p = text("path");
            let shown = if p.is_empty() {
                "None".into()
            } else {
                name(&p)
            };
            path(&area, "Image File", shown, p, "Browse");
            area.set_quick_button_tooltip(SharedString::new());
        }
        SourceKind::Color(_) => {
            let color = text("color");
            area.set_quick_label("Color".into());
            area.set_quick_kind("color".into());
            area.set_quick_swatch(form::swatch(&color));
            area.set_quick_text(color.into());
            area.set_quick_button(SharedString::new());
        }
        SourceKind::Media(_) | SourceKind::Slideshow(_) => {
            let files: Vec<String> = value["files"]
                .as_array()
                .map(|f| {
                    f.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let shown = match files.len() {
                0 => "None".to_string(),
                1 => name(&files[0]),
                n => format!("{} and {} more", name(&files[0]), n - 1),
            };
            let label = if kind == "media" { "Media" } else { "Images" };
            path(&area, label, shown, files.join("\n"), "");
        }
        SourceKind::Browser(_) => {
            let url = if value["url"].is_string() {
                text("url")
            } else {
                text("file")
            };
            path(&area, "URL", url.clone(), url, "Refresh");
            area.set_quick_button_tooltip("Reload the page, bypassing the cache".into());
        }
        _ => clear(&area),
    }
}

/// A quick setting's dropdown choice.
fn quick_choose(app: &Rc<App>, value: &str) {
    let Some(source) = selected_source(app) else {
        return;
    };
    if value == devices::PICK {
        dialogs::pick_screen(app, source.id.clone());
        return;
    }
    let now = source_value(app, &source.id);
    let mut patch = json!({ "id": source.id });
    match source.kind.label() {
        // The dialog chooses the window; its name stays.
        "window" if devices::PICKED_IN_DIALOG => return,
        "display" => patch["display"] = json!(value),
        "microphone" => patch["device"] = json!(value),
        "camera" => {
            let devices = app.session.borrow().devices.clone();
            let change = form::camera_change(
                devices.as_ref(),
                now.as_object().unwrap_or(&serde_json::Map::new()),
                value,
            );
            for (k, v) in change {
                patch[k.as_str()] = v;
            }
        }
        kind @ ("window" | "application" | "application_audio") => {
            patch["app"] = json!(value);
            if kind == "window" && now["app"] != value {
                patch["title"] = Value::Null;
            }
        }
        _ => return,
    }
    app.edit(json!({ "sources": [patch] }), EditOptions::default());
}

/// A quick setting typed: a text source's words, a colour.
fn quick_text(app: &Rc<App>, text: &str) {
    let Some(source) = selected_source(app) else {
        return;
    };
    let field = match source.kind.label() {
        "text" => "text",
        "color" => {
            if !form::valid_color(text.trim()) {
                if let Some(window) = app.window() {
                    show_quick_setting(app, &window);
                }
                return;
            }
            "color"
        }
        _ => return,
    };
    let value = if field == "color" { text.trim() } else { text };
    app.edit(
        json!({ "sources": [{ "id": source.id, field: value }] }),
        EditOptions::default(),
    );
}

/// A colour source's colour from the colour panel (a live gesture).
fn quick_color(app: &Rc<App>, hex: &str) {
    let Some(source) = selected_source(app) else {
        return;
    };
    if source.kind.label() == "color" {
        app.edit(
            json!({ "sources": [{ "id": source.id, "color": hex }] }),
            EditOptions::live(format!("quick-color:{}", source.id)),
        );
    }
}

/// The quick setting's button: an image's Browse, a browser's Refresh.
fn quick_button(app: &Rc<App>) {
    let Some(source) = selected_source(app) else {
        return;
    };
    match source.kind.label() {
        "image" => {
            if app.mode.offscreen {
                return;
            }
            let options = crate::platform::Choose {
                types: files::IMAGE_TYPES,
                prompt: "Choose",
                ..crate::platform::Choose::default()
            };
            crate::areas::remote::choose(app, options, move |app, chosen| {
                if let Some(path) = chosen.unwrap_or_default().into_iter().next() {
                    app.edit(
                        json!({ "sources": [{ "id": source.id, "path": path }] }),
                        EditOptions::default(),
                    );
                }
            });
        }
        "browser" => {
            app.call(
                Method::RefreshBrowser {
                    source: source.id.clone(),
                },
                |app, result| match result {
                    Ok(_) => {
                        app.toast(ToastKind::Info, "Reloading the page", "");
                    }
                    Err(error) => app.failed("Couldn’t reload the page", &error),
                },
            );
        }
        _ => {}
    }
}

/// Arrow keys: move the selected item by `dx,dy` canvas pixels (unless it
/// or the preview is locked).
fn nudge(app: &Rc<App>, by: &str) {
    let Some((dx, dy)) = by
        .split_once(',')
        .and_then(|(x, y)| Some((x.parse::<i64>().ok()?, y.parse::<i64>().ok()?)))
    else {
        return;
    };
    let locked = app
        .window()
        .is_some_and(|w| w.global::<Ui>().get_preview_locked());
    // On the canvas being edited (the vertical one's items while editing
    // it, #86).
    let s = app.session.borrow();
    let patch = s.selected(|scene, chain| {
        let item = chain[chain.len() - 1];
        // Items placed by their layout (no rect) aren't moved here.
        let rect = item.rect.filter(|_| !item.locked)?;
        Some(s.canvas_item_patch(
            &scene.id,
            chain,
            json!({ "rect": { "x": rect.x as i64 + dx, "y": rect.y as i64 + dy } }),
        ))
    });
    drop(s);
    if let Some(Some(patch)) = patch
        && !locked
    {
        let key = format!(
            "nudge:{}",
            app.session
                .borrow()
                .selection
                .as_ref()
                .map_or("", |s| s.path.as_str())
        );
        let options = EditOptions {
            key: Some(key),
            interval: std::time::Duration::from_millis(50),
            ..EditOptions::default()
        };
        app.edit(patch, options);
    }
}
