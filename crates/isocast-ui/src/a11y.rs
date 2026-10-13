//! `--a11y`: what the window tells screen readers, as JSON lines in tree
//! order: each element with an accessible role, its name and state. This is
//! the tree Slint hands to AccessKit (and so to VoiceOver on macOS and UI
//! Automation on Windows).

use anyhow::Result;
use i_slint_backend_testing::{AccessibleRole, ElementQuery};
use serde_json::{Map, Value, json};

use crate::AppWindow;

pub fn print(app: &AppWindow) -> Result<()> {
    for element in ElementQuery::from_root(app).find_all() {
        let Some(role) = element.accessible_role() else {
            continue;
        };
        if role == AccessibleRole::None {
            continue;
        }
        let mut entry = Map::new();
        entry.insert("role".into(), json!(format!("{role:?}")));
        let mut put = |key: &str, value: Option<Value>| {
            if let Some(value) = value {
                entry.insert(key.into(), value);
            }
        };
        put("name", text(element.accessible_label()));
        put("value", text(element.accessible_value()));
        put("description", text(element.accessible_description()));
        if element.accessible_item_selectable() == Some(true) {
            put(
                "selected",
                element.accessible_item_selected().map(Value::from),
            );
        }
        if role == AccessibleRole::ListItem {
            put("index", element.accessible_item_index().map(Value::from));
            put("count", element.accessible_item_count().map(Value::from));
        }
        if element.accessible_expandable() == Some(true) {
            put("expanded", element.accessible_expanded().map(Value::from));
        }
        if element.accessible_enabled() == Some(false) {
            put("enabled", Some(json!(false)));
        }
        let size = element.size();
        let at = element.absolute_position();
        put(
            "frame",
            Some(json!([
                at.x.round(),
                at.y.round(),
                size.width.round(),
                size.height.round()
            ])),
        );
        println!("{}", Value::Object(entry));
    }
    Ok(())
}

fn text(value: Option<slint::SharedString>) -> Option<Value> {
    value.filter(|v| !v.is_empty()).map(|v| json!(v.as_str()))
}
