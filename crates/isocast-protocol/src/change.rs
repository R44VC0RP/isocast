//! Changes to the live setup: patches, and diffs that describe them.
//!
//! Lists of items with ids (sources and scenes by `id`, scene items by
//! `source`) are matched by id, not by position, both when patching and in
//! diff paths such as `sources.facecam.device`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A proposed change to the live setup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    /// Replace the whole setup.
    Setup(Box<crate::Setup>),
    /// A JSON merge patch (RFC 7386) on the setup, except that lists of
    /// items with ids merge by id (scene items without an id by their
    /// source): a patch item updates the item with the same id or is added,
    /// and `{"id": "x", "remove": true}` removes one.
    /// Other lists, like `recording.iso.sources`, are replaced whole.
    Patch(Value),
}

/// One difference between two setups.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Difference {
    /// Dotted path, using ids for items in id lists, like
    /// `scenes.desktop.items.facecam.visible`.
    pub path: String,
    pub op: DifferenceOp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<Value>,
}

impl Change {
    /// Why params that give a change (in `setup` or `patch`) don't: they
    /// give neither or both. serde only says "expected map with a single
    /// key", or for `apply`, where the change is flattened, no more than
    /// that no variant was found.
    pub(crate) fn neither_or_both(params: &Value) -> Option<&'static str> {
        let given = ["setup", "patch"]
            .into_iter()
            .filter(|key| params.get(key).is_some())
            .count();
        match given {
            0 => Some("missing field `setup` (a whole setup) or `patch` (a merge patch)"),
            2 => Some("give `setup` or `patch`, not both"),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DifferenceOp {
    Add,
    Remove,
    Change,
}

/// Apply a patch to a document (see [`Change::Patch`]).
pub fn merge_patch(document: &Value, patch: &Value) -> Value {
    match (document, patch) {
        (Value::Object(doc), Value::Object(patch)) => {
            let mut out = doc.clone();
            for (key, value) in patch {
                if value.is_null() {
                    out.remove(key);
                } else {
                    let merged = match out.get(key) {
                        Some(existing) => merge_patch(existing, value),
                        None => strip_nulls(value),
                    };
                    out.insert(key.clone(), merged);
                }
            }
            Value::Object(out)
        }
        // Lists of identified objects merge item by item; an empty list can
        // take any patch list.
        (Value::Array(doc), Value::Array(patch))
            if keyed(patch) && (doc.is_empty() || keyed(doc)) =>
        {
            Value::Array(merge_by_id(doc, patch))
        }
        (Value::Array(_), Value::Array(patch)) => strip_nulls(&Value::Array(patch.clone())),
        (_, patch) => strip_nulls(patch),
    }
}

fn merge_by_id(doc: &[Value], patch: &[Value]) -> Vec<Value> {
    let mut out = doc.to_vec();
    for item in patch {
        let id = item_key(item);
        let remove = item.get("remove") == Some(&Value::Bool(true));
        let position = out.iter().position(|existing| item_key(existing) == id);
        match (position, remove) {
            (Some(i), true) => {
                out.remove(i);
            }
            (None, true) => {}
            (Some(i), false) => out[i] = merge_patch(&out[i], item),
            (None, false) => out.push(strip_nulls(item)),
        }
    }
    out
}

/// The items a patch removes (`{"id": "x", "remove": true}`) that aren't
/// in `document`, as problems naming the list and the ids it has: such a
/// removal would otherwise do nothing, silently (a typo in the id).
pub fn missing_removals(document: &Value, patch: &Value) -> Vec<crate::Issue> {
    let mut out = Vec::new();
    missing_removals_into(document, patch, "", &mut out);
    out
}

fn missing_removals_into(document: &Value, patch: &Value, path: &str, out: &mut Vec<crate::Issue>) {
    let join = |key: &str| {
        if path.is_empty() {
            key.to_string()
        } else {
            format!("{path}.{key}")
        }
    };
    match (document, patch) {
        (Value::Object(doc), Value::Object(patch)) => {
            for (key, value) in patch {
                if let Some(existing) = doc.get(key) {
                    missing_removals_into(existing, value, &join(key), out);
                }
            }
        }
        (Value::Array(doc), Value::Array(patch))
            if keyed(patch) && (doc.is_empty() || keyed(doc)) =>
        {
            for item in patch {
                let id = item_key(item).unwrap_or_default();
                let remove = item.get("remove") == Some(&Value::Bool(true));
                match doc.iter().find(|existing| item_key(existing) == Some(id)) {
                    Some(existing) if !remove => {
                        missing_removals_into(existing, item, &join(id), out)
                    }
                    None if remove => {
                        let ids: Vec<&str> = doc.iter().filter_map(item_key).collect();
                        let list = path.rsplit('.').next().unwrap_or(path);
                        out.push(crate::Issue {
                            path: path.to_string(),
                            message: format!(
                                "there is no {id:?} to remove; {list}: {}",
                                if ids.is_empty() {
                                    "none".to_string()
                                } else {
                                    ids.join(", ")
                                }
                            ),
                        });
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// What identifies an item of a list: its `id`, or for scene items without
/// one, its `source`.
fn item_key(item: &Value) -> Option<&str> {
    item.get("id")
        .and_then(Value::as_str)
        .or_else(|| item.get("source").and_then(Value::as_str))
        .or_else(|| item.get("scene").and_then(Value::as_str))
        // Filters without ids go by kind.
        .or_else(|| item.get("kind").and_then(Value::as_str))
}

/// A non-empty list whose items are all identified.
fn keyed(items: &[Value]) -> bool {
    !items.is_empty() && items.iter().all(|item| item_key(item).is_some())
}

fn strip_nulls(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k.clone(), strip_nulls(v)))
                .collect::<Map<_, _>>(),
        ),
        other => other.clone(),
    }
}

/// Every difference between two documents, in document order.
pub fn diff(old: &Value, new: &Value) -> Vec<Difference> {
    let mut out = Vec::new();
    diff_into(old, new, String::new(), &mut out);
    out
}

fn diff_into(old: &Value, new: &Value, path: String, out: &mut Vec<Difference>) {
    let join = |key: &str| {
        if path.is_empty() {
            key.to_string()
        } else {
            format!("{path}.{key}")
        }
    };
    match (old, new) {
        (Value::Object(a), Value::Object(b)) => {
            for (key, value) in a {
                match b.get(key) {
                    Some(other) => diff_into(value, other, join(key), out),
                    None => out.push(removed(join(key), value)),
                }
            }
            for (key, value) in b {
                if !a.contains_key(key) {
                    out.push(added(join(key), value));
                }
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            let identified =
                (keyed(a) || keyed(b)) && (a.is_empty() || keyed(a)) && (b.is_empty() || keyed(b));
            if identified {
                let id = |item: &Value| item_key(item).unwrap_or_default().to_string();
                for item in a {
                    match b.iter().find(|other| item_key(other) == item_key(item)) {
                        Some(other) => diff_into(item, other, join(&id(item)), out),
                        None => out.push(removed(join(&id(item)), item)),
                    }
                }
                for item in b {
                    if !a.iter().any(|other| item_key(other) == item_key(item)) {
                        out.push(added(join(&id(item)), item));
                    }
                }
                // Order matters too (which scene comes first, which item is
                // on top, which filter runs first): when the items both
                // lists share come in a different order, that's a change of
                // the list itself, from the old ids to the new ones.
                let order = |x: &[Value], y: &[Value]| -> Vec<Value> {
                    x.iter()
                        .filter(|item| y.iter().any(|other| item_key(other) == item_key(item)))
                        .map(|item| Value::String(id(item)))
                        .collect()
                };
                let (before, after) = (order(a, b), order(b, a));
                if before != after {
                    out.push(changed(path, &Value::Array(before), &Value::Array(after)));
                }
            } else if a != b {
                out.push(changed(path, old, new));
            }
        }
        _ if old != new => out.push(changed(path, old, new)),
        _ => {}
    }
}

fn added(path: String, value: &Value) -> Difference {
    Difference {
        path,
        op: DifferenceOp::Add,
        from: None,
        to: Some(value.clone()),
    }
}

fn removed(path: String, value: &Value) -> Difference {
    Difference {
        path,
        op: DifferenceOp::Remove,
        from: Some(value.clone()),
        to: None,
    }
}

fn changed(path: String, old: &Value, new: &Value) -> Difference {
    Difference {
        path,
        op: DifferenceOp::Change,
        from: Some(old.clone()),
        to: Some(new.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lists_with_ids_merge_by_id() {
        let doc = json!({"sources": [{"id": "a", "device": "x"}, {"id": "b", "device": "y"}]});
        let patch = json!({"sources": [
            {"id": "b", "device": "z"},
            {"id": "c", "device": "w"},
            {"id": "a", "remove": true},
        ]});
        assert_eq!(
            merge_patch(&doc, &patch),
            json!({"sources": [{"id": "b", "device": "z"}, {"id": "c", "device": "w"}]})
        );
    }

    #[test]
    fn scene_items_merge_by_id_or_else_source() {
        let doc = json!({"items": [
            {"source": "screen", "visible": true},
            {"id": "bg", "style": {"opacity": 1.0, "background": "#000"}},
        ]});
        let patch = json!({"items": [
            {"source": "screen", "visible": false},
            {"id": "bg", "style": {"opacity": 0.5}},
        ]});
        assert_eq!(
            merge_patch(&doc, &patch),
            json!({"items": [
                {"source": "screen", "visible": false},
                {"id": "bg", "style": {"opacity": 0.5, "background": "#000"}},
            ]})
        );
    }

    #[test]
    fn plain_lists_are_replaced_and_nulls_remove_fields() {
        let doc = json!({"iso": {"sources": ["a", "b"], "audio": ["m"]}, "name": "x"});
        let patch = json!({"iso": {"sources": ["c"], "audio": null}});
        assert_eq!(
            merge_patch(&doc, &patch),
            json!({"iso": {"sources": ["c"]}, "name": "x"})
        );
    }

    #[test]
    fn diffs_name_list_items_by_id() {
        let old = json!({"scenes": [{"id": "desktop", "items": [
            {"source": "facecam", "visible": true},
            {"id": "title", "source": "facecam"},
        ]}]});
        let new = json!({"scenes": [{"id": "desktop", "items": [
            {"source": "facecam", "visible": false},
        ]}]});
        let paths: Vec<(String, DifferenceOp)> = diff(&old, &new)
            .into_iter()
            .map(|d| (d.path, d.op))
            .collect();
        assert_eq!(
            paths,
            [
                (
                    "scenes.desktop.items.facecam.visible".to_string(),
                    DifferenceOp::Change
                ),
                (
                    "scenes.desktop.items.title".to_string(),
                    DifferenceOp::Remove
                ),
            ]
        );
    }

    #[test]
    fn diffs_a_reorder_as_a_change_of_the_list() {
        let old = json!({"scenes": [
            {"id": "a", "items": [{"source": "cam"}, {"source": "bars"}]},
            {"id": "b", "items": []},
        ]});
        let new = json!({"scenes": [
            {"id": "b", "items": []},
            {"id": "a", "items": [{"source": "bars"}, {"source": "cam"}]},
        ]});
        let changes = diff(&old, &new);
        let paths: Vec<&str> = changes.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, ["scenes.a.items", "scenes"]);
        assert_eq!(changes[1].from, Some(json!(["a", "b"])));
        assert_eq!(changes[1].to, Some(json!(["b", "a"])));
        // Adding or removing alone isn't a reorder.
        let added = json!({"scenes": [{"id": "a", "items": []}, {"id": "c", "items": []}, {"id": "b", "items": []}]});
        let only_added: Vec<String> = diff(
            &json!({"scenes": [{"id": "a", "items": []}, {"id": "b", "items": []}]}),
            &added,
        )
        .into_iter()
        .map(|d| d.path)
        .collect();
        assert_eq!(only_added, ["scenes.c"]);
    }
}
