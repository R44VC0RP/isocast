//! Rust helpers for the widget kit (ui/kit.slint): building menus and
//! dropdown options, and the one thing the kit can't do in Slint (find the
//! next menu item, which needs a loop).

use std::collections::HashMap;

use slint::{Model, ModelRc, SharedString, VecModel};

use crate::{DropdownOption, MenuEntry, MenuEntryKind, MenuList};

/// `Menus.step`: the next entry after `from` going `by` (1 or -1) that can
/// be chosen (an enabled item), wrapping; -1 if none.
pub fn step(entries: ModelRc<MenuEntry>, from: i32, by: i32) -> i32 {
    let count = entries.row_count() as i32;
    if count == 0 {
        return -1;
    }
    let by = if by < 0 { -1 } else { 1 };
    let mut at = if from < 0 {
        if by > 0 { -1 } else { count }
    } else {
        from
    };
    for _ in 0..count {
        at = (at + by).rem_euclid(count);
        if let Some(entry) = entries.row_data(at as usize)
            && entry.kind == MenuEntryKind::Item
            && !entry.disabled
        {
            return at;
        }
    }
    -1
}

/// A menu being built: entries, separators, headings and one level of
/// submenus, as `Menus.show` takes them.
#[derive(Default)]
pub struct MenuBuilder {
    entries: Vec<MenuEntry>,
    submenus: Vec<Vec<MenuEntry>>,
}

impl MenuBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// An item: what `Menus.activated` reports as `id`, and its label.
    pub fn item(mut self, id: &str, label: &str) -> Self {
        self.entries.push(entry(id, label));
        self
    }

    /// Change the last entry (its hint, icon, check, state).
    pub fn with(mut self, change: impl FnOnce(&mut MenuEntry)) -> Self {
        if let Some(last) = self.entries.last_mut() {
            change(last);
        }
        self
    }

    /// The last item's shortcut, shown on the right ("⌘C").
    pub fn hint(self, hint: &str) -> Self {
        self.with(|e| e.hint = hint.into())
    }

    /// The last item's icon, by name ("camera": `Icons.named` in
    /// ui/icons.slint).
    pub fn icon(self, icon: &str) -> Self {
        self.with(|e| e.icon = icon.into())
    }

    pub fn checked(self, checked: bool) -> Self {
        self.with(|e| e.checked = checked)
    }

    pub fn disabled(self, disabled: bool) -> Self {
        self.with(|e| e.disabled = disabled)
    }

    pub fn danger(self) -> Self {
        self.with(|e| e.danger = true)
    }

    pub fn separator(mut self) -> Self {
        // No separator first, last or twice in a row.
        if self
            .entries
            .last()
            .is_some_and(|e| e.kind != MenuEntryKind::Separator)
        {
            self.entries.push(MenuEntry {
                kind: MenuEntryKind::Separator,
                ..Default::default()
            });
        }
        self
    }

    pub fn heading(mut self, label: &str) -> Self {
        self.entries.push(MenuEntry {
            kind: MenuEntryKind::Heading,
            label: label.into(),
            ..Default::default()
        });
        self
    }

    /// An item opening a submenu of `items` (only one level deep).
    pub fn submenu(mut self, label: &str, items: MenuBuilder) -> Self {
        let mut item = entry("", label);
        item.has_submenu = true;
        item.submenu = self.submenus.len() as i32;
        item.disabled = items.entries.is_empty();
        self.entries.push(item);
        self.submenus.push(items.entries);
        self
    }

    /// The menu and its submenus, for `Menus.show`.
    pub fn build(mut self) -> (MenuList, ModelRc<MenuList>) {
        if self
            .entries
            .last()
            .is_some_and(|e| e.kind == MenuEntryKind::Separator)
        {
            self.entries.pop();
        }
        // Every menu keeps the 16px lane for checks and icons, empty or
        // not, so labels always start 36px in (Paper's Components "Menu").
        let submenus: Vec<MenuList> = self
            .submenus
            .into_iter()
            .map(|entries| MenuList {
                lane: true,
                entries: ModelRc::new(VecModel::from(entries)),
            })
            .collect();
        (
            MenuList {
                lane: true,
                entries: ModelRc::new(VecModel::from(self.entries)),
            },
            ModelRc::new(VecModel::from(submenus)),
        )
    }
}

/// Every kind of entry, for the widget gallery (`--state kit-menu`).
pub fn sample_menu() -> MenuBuilder {
    MenuBuilder::new()
        .submenu(
            "Add",
            MenuBuilder::new()
                .item("camera", "Video Capture Device")
                .icon("camera")
                .item("display", "Display Capture")
                .icon("display")
                .item("media", "Media Source")
                .icon("media"),
        )
        .separator()
        .item("copy", "Copy")
        .hint("⌘C")
        .item("paste", "Paste (Reference)")
        .hint("⌘V")
        .disabled(true)
        .separator()
        .heading("Scene")
        .item("visible", "Visible")
        .checked(true)
        .item("lock", "Lock")
        .icon("lock")
        .separator()
        .item("remove", "Remove")
        .hint("⌫")
        .danger()
}

fn entry(id: &str, label: &str) -> MenuEntry {
    MenuEntry {
        id: id.into(),
        label: label.into(),
        kind: MenuEntryKind::Item,
        ..Default::default()
    }
}

/// A dropdown option.
pub fn option(value: &str, label: &str, detail: &str) -> DropdownOption {
    DropdownOption {
        value: value.into(),
        label: label.into(),
        detail: detail.into(),
        disabled: false,
    }
}

/// Tell apart options with the same label: where two or more share one,
/// each keeps its own detail if the details differ, or else gets "1 of 2",
/// "2 of 2"… (two capture cards of the same model, say).
pub fn disambiguate(options: &mut [DropdownOption]) {
    let mut by_label: HashMap<SharedString, Vec<usize>> = HashMap::new();
    for (i, option) in options.iter().enumerate() {
        by_label.entry(option.label.clone()).or_default().push(i);
    }
    for indices in by_label.values().filter(|i| i.len() > 1) {
        let details: Vec<&SharedString> = indices.iter().map(|&i| &options[i].detail).collect();
        let distinct = details.iter().all(|d| !d.is_empty())
            && details
                .iter()
                .enumerate()
                .all(|(a, d)| details.iter().skip(a + 1).all(|e| e != d));
        if distinct {
            continue;
        }
        let total = indices.len();
        for (n, &i) in indices.iter().enumerate() {
            let counter = format!("{} of {total}", n + 1);
            options[i].detail = if options[i].detail.is_empty() {
                counter.into()
            } else {
                format!("{} · {counter}", options[i].detail).into()
            };
        }
    }
}

/// Show `rows` in `model`: row by row when the count is the same (only
/// the rows that differ change), else all anew. Replacing a list's model
/// rebuilds its rows, which ends a drag, an inline rename or a
/// double-click in progress.
pub fn update_rows<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    if model.row_count() != rows.len() {
        model.set_vec(rows);
        return;
    }
    for (i, row) in rows.into_iter().enumerate() {
        if model.row_data(i).as_ref() != Some(&row) {
            model.set_row_data(i, row);
        }
    }
}

/// A model from a list.
pub fn model<T: Clone + 'static>(items: impl IntoIterator<Item = T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(items.into_iter().collect::<Vec<_>>()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_names_are_told_apart() {
        let mut options = vec![
            option("a", "USB Capture HDMI", ""),
            option("b", "FaceTime HD Camera", ""),
            option("c", "USB Capture HDMI", ""),
            option("d", "Studio", "NDI"),
            option("e", "Studio", "USB"),
        ];
        disambiguate(&mut options);
        let details: Vec<&str> = options.iter().map(|o| o.detail.as_str()).collect();
        assert_eq!(details, ["1 of 2", "", "2 of 2", "NDI", "USB"]);
    }

    #[test]
    fn steps_skip_what_cant_be_chosen() {
        let (menu, _) = MenuBuilder::new()
            .item("a", "A")
            .separator()
            .item("b", "B")
            .disabled(true)
            .heading("H")
            .item("c", "C")
            .build();
        let entries = menu.entries;
        assert_eq!(step(entries.clone(), -1, 1), 0);
        assert_eq!(step(entries.clone(), 0, 1), 4);
        assert_eq!(step(entries.clone(), 4, 1), 0);
        assert_eq!(step(entries.clone(), -1, -1), 4);
        assert_eq!(step(entries, 0, -1), 4);
    }
}
