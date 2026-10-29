//! What the Scenes and Sources docks and their menus do to the setup (the
//! web app's scenes.js, sources.js, lib/items.js and lib/transform.js):
//! scenes added, duplicated, removed, moved and given a transition;
//! items shown, locked, recorded on their own, moved in the stack,
//! copied and pasted, removed (with their source when nothing else shows
//! it), renamed, and transformed. Every change is an edit of the live
//! setup (or, for structural ones, the whole setup) through `App`.

use std::rc::Rc;

use isocast_protocol::api::{
    CapacityEstimate, CapacityParams, IsoAdmission, Method, SnapshotParams,
};
use isocast_protocol::{CanvasId, SceneItem, Setup};
use serde_json::{Map, Value, json};

use crate::ToastKind;
use crate::app::App;
use crate::kit::MenuBuilder;
use crate::model;
use crate::session::EditOptions;
use crate::toasts::Toast;

// ---------- The setup as JSON ----------

/// The whole live setup (structural changes write it back whole).
fn setup_json(app: &Rc<App>) -> Option<Value> {
    app.session.borrow().setup_json().cloned()
}

/// The setup as the canvas being edited has its items (the vertical
/// ones while editing the vertical canvas, #86): for reading items only,
/// never to write back whole.
fn canvas_json(app: &Rc<App>) -> Option<Value> {
    app.session.borrow().canvas_json().cloned()
}

/// Prune `items` (a JSON list) with `prune`, in a scene's main items and
/// its arrangements for extra canvases.
fn prune_scene(scene: &mut Value, prune: impl Fn(&Value) -> Value) {
    scene["items"] = prune(&scene["items"]);
    if scene["vertical"].is_object() {
        scene["vertical"]["items"] = prune(&scene["vertical"]["items"]);
    }
    if let Some(canvases) = scene.get_mut("canvases").and_then(Value::as_object_mut) {
        for arrangement in canvases.values_mut() {
            if arrangement.is_object() {
                arrangement["items"] = prune(&arrangement["items"]);
            }
        }
    }
}

fn scenes_of(setup: &Value) -> Vec<Value> {
    setup["scenes"].as_array().cloned().unwrap_or_default()
}

fn name_of(value: &Value) -> String {
    value["name"]
        .as_str()
        .or(value["id"].as_str())
        .unwrap_or_default()
        .to_string()
}

/// What patches call an item: its id, else its source or scene.
fn key_of(item: &Value) -> Option<&str> {
    item["id"]
        .as_str()
        .or(item["source"].as_str())
        .or(item["scene"].as_str())
}

/// The fields that identify a JSON item in a patch.
fn identity(item: &Value) -> Value {
    if let Some(id) = item["id"].as_str() {
        json!({ "id": id })
    } else if let Some(source) = item["source"].as_str() {
        json!({ "source": source })
    } else {
        json!({ "scene": item["scene"] })
    }
}

/// A patch list putting identified items in exactly this order and
/// content: every old item removed, then every new one added.
fn replace_list(old: &[Value], new: &[Value]) -> Vec<Value> {
    old.iter()
        .map(|item| {
            let mut id = identity(item);
            id["remove"] = json!(true);
            id
        })
        .chain(new.iter().cloned())
        .collect()
}

/// The JSON items from the top level down to `path` in scene `scene`.
fn item_chain_json(setup: &Value, scene: &str, path: &str) -> Option<Vec<Value>> {
    let scene = scenes_of(setup).into_iter().find(|s| s["id"] == scene)?;
    let mut items = scene["items"].as_array().cloned().unwrap_or_default();
    let mut chain = Vec::new();
    for k in model::path_keys(path) {
        let item = items.iter().find(|i| key_of(i) == Some(k))?.clone();
        items = item["children"].as_array().cloned().unwrap_or_default();
        chain.push(item);
    }
    (!chain.is_empty()).then_some(chain)
}

/// A patch changing `fields` of the item at the end of a JSON chain.
fn item_patch_json(scene: &str, chain: &[Value], fields: Value) -> Value {
    let Some((last, parents)) = chain.split_last() else {
        return json!({});
    };
    let mut node = identity(last);
    if let (Some(node), Value::Object(fields)) = (node.as_object_mut(), fields) {
        node.extend(fields);
    }
    for parent in parents.iter().rev() {
        let mut outer = identity(parent);
        outer["children"] = json!([node]);
        node = outer;
    }
    json!({ "scenes": [{ "id": scene, "items": [node] }] })
}

/// Remove every reference to source `id` from a setup.
fn without_source(setup: &Value, id: &str) -> Value {
    fn prune(items: &Value, id: &str) -> Value {
        Value::Array(
            items
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter(|i| i["source"] != id)
                        .map(|i| {
                            let mut i = i.clone();
                            if i.get("children").is_some() {
                                i["children"] = prune(&i["children"], id);
                            }
                            i
                        })
                        .collect()
                })
                .unwrap_or_default(),
        )
    }
    let drop = |list: &Value| {
        Value::Array(
            list.as_array()
                .map(|l| l.iter().filter(|s| *s != id).cloned().collect())
                .unwrap_or_default(),
        )
    };
    let mut out = setup.clone();
    out["sources"] = Value::Array(
        setup["sources"]
            .as_array()
            .map(|s| s.iter().filter(|s| s["id"] != id).cloned().collect())
            .unwrap_or_default(),
    );
    out["scenes"] = Value::Array(
        scenes_of(setup)
            .into_iter()
            .map(|mut scene| {
                prune_scene(&mut scene, |items| prune(items, id));
                scene
            })
            .collect(),
    );
    if let Some(program) = out["recording"].get_mut("program")
        && program.get("audio").is_some()
    {
        program["audio"] = drop(&program["audio"]);
    }
    if let Some(iso) = out["recording"].get_mut("iso")
        && iso.is_object()
    {
        if iso.get("sources").is_some() {
            iso["sources"] = drop(&iso["sources"]);
        }
        if iso.get("audio").is_some() {
            iso["audio"] = drop(&iso["audio"]);
        }
        if let Some(lists) = iso.get_mut("source_audio").and_then(Value::as_object_mut) {
            lists.remove(id);
            for list in lists.values_mut() {
                *list = drop(list);
            }
        }
    }
    out
}

/// Remove scene `id` and every item showing it.
fn without_scene(setup: &Value, id: &str) -> Value {
    fn prune(items: &Value, id: &str) -> Value {
        Value::Array(
            items
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter(|i| i["scene"] != id)
                        .map(|i| {
                            let mut i = i.clone();
                            if i.get("children").is_some() {
                                i["children"] = prune(&i["children"], id);
                            }
                            i
                        })
                        .collect()
                })
                .unwrap_or_default(),
        )
    }
    let mut out = setup.clone();
    out["scenes"] = Value::Array(
        scenes_of(setup)
            .into_iter()
            .filter(|s| s["id"] != id)
            .map(|mut scene| {
                prune_scene(&mut scene, |items| prune(items, id));
                scene
            })
            .collect(),
    );
    out
}

/// `list` with the entry at `from` moved to land before index `to`.
pub fn moved<T: Clone>(list: &[T], from: usize, to: usize) -> Vec<T> {
    let mut out = list.to_vec();
    if from >= out.len() {
        return out;
    }
    let item = out.remove(from);
    let to = if to > from { to - 1 } else { to };
    out.insert(to.min(out.len()), item);
    out
}

// ---------- Scenes ----------

/// Add Scene: "Scene N", empty, after the others, then chosen (on air,
/// or into preview). Returns its id.
pub fn add_scene(app: &Rc<App>) -> Option<String> {
    let (id, name) = {
        let s = app.session.borrow();
        let setup = s.setup.clone()?;
        let name = model::next_name("Scene", setup.scenes.iter().map(model::scene_name));
        let id = model::new_id(&name, model::taken_ids(&setup));
        (id, name)
    };
    let scene = id.clone();
    app.edit_then(
        json!({ "scenes": [{ "id": id, "name": name, "items": [] }] }),
        EditOptions::default(),
        move |app, applied| {
            if applied {
                app.choose_scene(&scene);
            }
        },
    );
    Some(id)
}

pub fn duplicate_scene(app: &Rc<App>, id: &str) -> Option<String> {
    let setup = setup_json(app)?;
    let mut scenes = scenes_of(&setup);
    let index = scenes.iter().position(|s| s["id"] == id)?;
    let names: Vec<String> = scenes.iter().map(name_of).collect();
    let name = model::next_name(&name_of(&scenes[index]), names.iter().map(String::as_str));
    let typed = app.session.borrow().setup.clone()?;
    let new = model::new_id(&name, model::taken_ids(&typed));
    let mut copy = scenes[index].clone();
    copy["id"] = json!(new);
    copy["name"] = json!(name);
    scenes.insert(index + 1, copy);
    let mut next = setup;
    next["scenes"] = Value::Array(scenes);
    // Switch once the daemon has the duplicate (the web app awaits it).
    let chosen = new.clone();
    app.replace_setup_then(next, true, move |app, applied| {
        if applied {
            app.choose_scene(&chosen);
        }
    });
    Some(new)
}

/// Remove a scene (and every item showing it), with Undo.
pub fn remove_scene(app: &Rc<App>, id: &str) {
    let Some(setup) = setup_json(app) else { return };
    let Some(scene) = scenes_of(&setup).into_iter().find(|s| s["id"] == id) else {
        return;
    };
    app.replace_setup(without_scene(&setup, id), true);
    app.show_toast(
        Toast::new(
            ToastKind::Info,
            format!("Removed “{}”", name_of(&scene)),
            "",
        )
        .action("Undo", move |app| {
            app.replace_setup(setup.clone(), true);
        }),
    );
}

/// Move a scene in the list: "up", "down", "top" or "bottom".
pub fn move_scene(app: &Rc<App>, id: &str, place: &str) {
    let Some(setup) = setup_json(app) else { return };
    let scenes = scenes_of(&setup);
    let Some(from) = scenes.iter().position(|s| s["id"] == id) else {
        return;
    };
    let to = match place {
        "up" if from > 0 => from - 1,
        "down" => from + 2,
        "top" => 0,
        "bottom" => scenes.len(),
        _ => return,
    };
    move_scene_to(app, from, to);
}

/// A scene dragged from `from` to land before `to`.
pub fn move_scene_to(app: &Rc<App>, from: usize, to: usize) {
    let Some(mut setup) = setup_json(app) else {
        return;
    };
    let scenes = scenes_of(&setup);
    if from >= scenes.len() || to > scenes.len() || to == from || to == from + 1 {
        return;
    }
    setup["scenes"] = Value::Array(moved(&scenes, from, to));
    app.replace_setup(setup, true);
}

/// A scene's transition override (None for none).
pub fn set_override(app: &Rc<App>, id: &str, kind: Option<&str>) {
    let Some(setup) = setup_json(app) else { return };
    let Some(scene) = scenes_of(&setup).into_iter().find(|s| s["id"] == id) else {
        return;
    };
    let transition = match kind {
        None => Value::Null,
        Some(kind) => {
            let duration = scene["transition"]["duration_ms"]
                .as_u64()
                .or(setup["transition"]["duration_ms"].as_u64())
                .unwrap_or(300);
            object_patch(
                &scene["transition"],
                &json!({ "kind": kind, "duration_ms": duration }),
            )
        }
    };
    app.edit(
        json!({ "scenes": [{ "id": id, "transition": transition }] }),
        EditOptions::default(),
    );
}

/// A patch turning object `from` into `to` (changed fields set, missing
/// ones nulled; nested objects recursively; lists replaced whole).
pub fn object_patch(from: &Value, to: &Value) -> Value {
    let empty = Map::new();
    let from_map = from.as_object().unwrap_or(&empty);
    let to_map = to.as_object().unwrap_or(&empty);
    let mut out = Map::new();
    for key in from_map.keys() {
        if !to_map.contains_key(key) {
            out.insert(key.clone(), Value::Null);
        }
    }
    for (key, value) in to_map {
        let old = from_map.get(key).unwrap_or(&Value::Null);
        if old.is_object() && value.is_object() {
            let inner = object_patch(old, value);
            if inner.as_object().is_some_and(|o| !o.is_empty()) {
                out.insert(key.clone(), inner);
            }
        } else if old != value {
            let keyed = |l: &Value| {
                l.as_array().is_some_and(|a| {
                    !a.is_empty()
                        && a.iter().all(|i| {
                            i["id"].is_string()
                                || i["source"].is_string()
                                || i["scene"].is_string()
                                || i["kind"].is_string()
                        })
                })
            };
            let merges = old.is_array()
                && value.is_array()
                && keyed(old)
                && (value.as_array().is_some_and(Vec::is_empty) || keyed(value));
            if merges {
                out.insert(
                    key.clone(),
                    Value::Array(replace_list(
                        old.as_array().map(Vec::as_slice).unwrap_or_default(),
                        value.as_array().map(Vec::as_slice).unwrap_or_default(),
                    )),
                );
            } else {
                out.insert(key.clone(), value.clone());
            }
        }
    }
    Value::Object(out)
}

// ---------- Items ----------

/// The selected item's scene id and JSON chain.
pub fn selected_json(app: &Rc<App>) -> Option<(String, Vec<Value>)> {
    let s = app.session.borrow();
    let selection = s.selection.clone()?;
    let setup = s.canvas_json()?;
    let chain = item_chain_json(setup, &selection.scene, &selection.path)?;
    Some((selection.scene, chain))
}

/// The item at `path` in the scene being edited: its scene and chain.
fn chain_at(app: &Rc<App>, path: &str) -> Option<(String, Vec<Value>)> {
    let s = app.session.borrow();
    let scene = s.editing_scene()?;
    let chain = item_chain_json(s.canvas_json()?, &scene, path)?;
    Some((scene, chain))
}

/// Change fields of the selected item (`live`: a gesture's key).
pub fn update_selected(app: &Rc<App>, fields: Value, live: Option<String>) {
    if let Some((scene, chain)) = selected_json(app) {
        let options = match live {
            Some(key) => EditOptions {
                key: Some(key),
                interval: std::time::Duration::from_millis(50),
                ..EditOptions::default()
            },
            None => EditOptions::default(),
        };
        app.edit(item_patch_json(&scene, &chain, fields), options);
    }
}

/// Flip a boolean field (visible, locked) of the item at `path`.
pub fn toggle_item(app: &Rc<App>, path: &str, field: &str, default: bool) {
    if let Some((scene, chain)) = chain_at(app, path) {
        let now = chain[chain.len() - 1][field].as_bool().unwrap_or(default);
        app.edit(
            item_patch_json(&scene, &chain, json!({ field: !now })),
            EditOptions::default(),
        );
    }
}

/// Whether source `id` records to its own file.
pub fn is_iso(setup: &Setup, id: &str) -> bool {
    setup.recording.iso.enabled && setup.recording.iso.sources.iter().any(|s| s == id)
}

/// Record source `id` to its own file, or stop (turning ISO recording on
/// if it was off). Turning one on asks the engine how it fits this Mac's
/// encoders first: at a smaller size or rate, or not at all, and says why
/// (#66). If it can't say, the file goes on as asked.
pub fn set_iso(app: &Rc<App>, id: &str, on: bool) {
    if on {
        let id = id.to_string();
        app.call_as(
            Method::EstimateCapacity(Box::new(CapacityParams {
                setup: None,
                stream: None,
                add_iso: Some(id.clone()),
            })),
            move |app, result: Result<CapacityEstimate, _>| match result
                .ok()
                .and_then(|e| e.admission)
            {
                Some(admission) => admit_iso(app, &id, admission),
                None => set_iso_now(app, &id, true),
            },
        );
        return;
    }
    set_iso_now(app, id, on);
}

/// Turn the ISO file on as the engine said it fits.
fn admit_iso(app: &Rc<App>, id: &str, admission: IsoAdmission) {
    let name = {
        let s = app.session.borrow();
        s.setup
            .as_ref()
            .and_then(|setup| model::find_source(setup, id))
            .map(|source| model::source_name(source).to_string())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| id.to_string())
    };
    let reason = admission
        .reason
        .as_deref()
        .map(crate::app::sentence)
        .unwrap_or_default();
    match (admission.on, admission.change) {
        (true, Some(change)) => {
            let as_is = admission.as_is;
            let format = match (admission.width, admission.height, admission.fps) {
                (Some(w), Some(h), Some(fps)) => format!("at {w}×{h}, {fps} fps"),
                _ => "smaller".into(),
            };
            app.edit_then(change, EditOptions::default(), move |app, applied| {
                if applied && !as_is {
                    app.toast(
                        ToastKind::Warning,
                        &format!("“{name}” captures smaller to fit"),
                        format!(
                            "It now captures {format}, in its ISO file and wherever scenes \
                             show it. {reason}."
                        ),
                    );
                }
            });
        }
        (true, None) => {}
        (false, _) => {
            app.toast(
                ToastKind::Warning,
                &format!("“{name}” isn’t recorded to its own file"),
                format!("{reason}. Turn off another ISO file, or lower the program’s size, first."),
            );
        }
    }
}

fn set_iso_now(app: &Rc<App>, id: &str, on: bool) {
    let patch = {
        let s = app.session.borrow();
        let Some(setup) = s.setup.clone() else { return };
        let iso = &setup.recording.iso;
        let mut sources: Vec<String> = iso.sources.iter().filter(|s| *s != id).cloned().collect();
        if on {
            sources.push(id.to_string());
        }
        let mut patch = json!({ "recording": { "iso": { "sources": sources } } });
        if on && !iso.enabled {
            patch["recording"]["iso"]["enabled"] = json!(true);
        }
        patch
    };
    app.edit(patch, EditOptions::default());
}

/// The "ISO Audio" menu of video source `id`'s ISO file: "Same as Other
/// ISO Files" (the shared choice) and each audio source, checked when the
/// file records it. Ids are `iso_audio:default` and `iso_audio:<audio id>`.
pub fn iso_audio_menu(mut menu: MenuBuilder, setup: &Setup, id: &str) -> MenuBuilder {
    let iso = &setup.recording.iso;
    let Some(source) = setup.source(id) else {
        return menu;
    };
    let tracks = iso.audio_for(source, source.kind.has_audio());
    menu = menu
        .item("iso_audio:default", "Same as Other ISO Files")
        .checked(!iso.source_audio.contains_key(id))
        .separator();
    for audio in setup.sources.iter().filter(|s| s.kind.has_audio()) {
        menu = menu
            .item(
                &format!("iso_audio:{}", audio.id),
                model::source_name(audio),
            )
            .checked(tracks.contains(&audio.id));
    }
    menu
}

/// The `source_audio` entry an `iso_audio:` item of [`iso_audio_menu`]
/// for source `id` makes: null (back to the shared choice) for `default`,
/// else the file's tracks with `audio` added or taken out, starting from
/// what it records now.
pub fn toggled_iso_audio(setup: &Setup, id: &str, audio: &str) -> Value {
    let Some(source) = setup.source(id) else {
        return Value::Null;
    };
    if audio == "default" {
        return Value::Null;
    }
    let mut tracks = setup
        .recording
        .iso
        .audio_for(source, source.kind.has_audio());
    match tracks.iter().position(|t| t == audio) {
        Some(at) => {
            tracks.remove(at);
        }
        None => tracks.push(audio.to_string()),
    }
    json!(tracks)
}

/// Act on an `iso_audio:` item of [`iso_audio_menu`] for source `id`.
pub fn choose_iso_audio(app: &Rc<App>, id: &str, audio: &str) {
    let patch = {
        let s = app.session.borrow();
        let Some(setup) = s.setup.as_ref() else {
            return;
        };
        let Some(source) = setup.source(id) else {
            return;
        };
        let list = toggled_iso_audio(setup, id, audio);
        let mut patch = json!({ "recording": { "iso": { "source_audio": { id: list } } } });
        // The list says whether the file has the source's own sound; an
        // audio.iso opt-in or opt-out would override it.
        if audio == id && source.audio.iso.is_some() {
            patch["sources"] = json!([{ "id": id, "audio": { "iso": null } }]);
        }
        patch
    };
    app.edit(patch, EditOptions::default());
}

/// How many items in every scene show source `id`, on either canvas.
fn uses(setup: &Setup, id: &str) -> usize {
    fn walk(items: &[SceneItem], id: &str) -> usize {
        items
            .iter()
            .map(|i| usize::from(i.source.as_deref() == Some(id)) + walk(&i.children, id))
            .sum()
    }
    setup
        .scenes
        .iter()
        .map(|s| {
            walk(&s.items, id)
                + s.vertical.as_ref().map_or(0, |v| walk(&v.items, id))
                + s.canvases
                    .values()
                    .map(|v| walk(&v.items, id))
                    .sum::<usize>()
        })
        .sum()
}

/// Remove an item from its scene. As in OBS, a video source no other item
/// shows goes with it. Undo brings either back.
pub fn remove_from_scene(app: &Rc<App>, scene: &str, chain: &[Value]) {
    let Some(item) = chain.last() else { return };
    let (setup, typed, canvas) = {
        let s = app.session.borrow();
        (s.setup_json().cloned(), s.setup.clone(), s.edit_canvas())
    };
    let (Some(setup), Some(typed)) = (setup, typed) else {
        return;
    };
    let source = item["source"]
        .as_str()
        .and_then(|id| model::find_source(&typed, id));
    app.select("");
    // On an extra canvas only the item goes: the main canvas (or its
    // layout the extra one falls back to) may still show the source.
    if let Some(source) = source
        && canvas == CanvasId::Main
        && source.kind.has_video()
        && uses(&typed, &source.id) <= 1
    {
        app.replace_setup(without_source(&setup, &source.id), true);
        let before = setup.clone();
        app.show_toast(
            Toast::new(
                ToastKind::Info,
                format!("Removed “{}”", model::source_name(source)),
                "",
            )
            .action("Undo", move |app| {
                app.replace_setup(before.clone(), true);
            }),
        );
        return;
    }
    let label = item_label_json(&typed, item);
    let scene_name = model::find_scene(&typed, scene)
        .map(model::scene_name)
        .unwrap_or(scene)
        .to_string();
    let before = model::canvas_items(&setup, scene, canvas);
    app.edit(
        item_patch_json(scene, chain, json!({ "remove": true })),
        EditOptions::default(),
    );
    let scene = scene.to_string();
    app.show_toast(
        Toast::new(
            ToastKind::Info,
            format!("Removed “{label}” from {scene_name}"),
            "",
        )
        // Back on the canvas it was removed from, whichever is edited now.
        .action("Undo", move |app| {
            let now = setup_json(app)
                .map(|s| model::canvas_items(&s, &scene, canvas))
                .unwrap_or_default();
            app.edit_on(
                json!({ "scenes": [{ "id": scene, "items": replace_list(&now, &before) }] }),
                EditOptions::default(),
                canvas,
            );
        }),
    );
}

fn item_label_json(setup: &Setup, item: &Value) -> String {
    serde_json::from_value::<SceneItem>(item.clone())
        .map(|i| model::item_label(setup, &i))
        .unwrap_or_else(|_| key_of(item).unwrap_or("Item").to_string())
}

/// Remove the selected item (− and ⌫).
pub fn remove_selected(app: &Rc<App>) {
    if let Some((scene, chain)) = selected_json(app) {
        remove_from_scene(app, &scene, &chain);
    }
}

/// Put the items of a level (the scene's top level, or a group's
/// children) in a new order.
fn reorder(app: &Rc<App>, scene: &str, parents: &[Value], items: &[Value]) {
    let patch = if parents.is_empty() {
        let old: Vec<Value> = canvas_json(app)
            .and_then(|s| {
                scenes_of(&s)
                    .into_iter()
                    .find(|x| x["id"] == scene)
                    .and_then(|x| x["items"].as_array().cloned())
            })
            .unwrap_or_default();
        json!({ "scenes": [{ "id": scene, "items": replace_list(&old, items) }] })
    } else {
        let parent = &parents[parents.len() - 1];
        let old = parent["children"].as_array().cloned().unwrap_or_default();
        item_patch_json(
            scene,
            parents,
            json!({ "children": replace_list(&old, items) }),
        )
    };
    app.edit(patch, EditOptions::default());
}

/// Move the selected item: "forward" (up the list), "backward", "front"
/// (top) or "back" (bottom).
pub fn order_selected(app: &Rc<App>, place: &str) {
    let Some((scene, chain)) = selected_json(app) else {
        return;
    };
    let item = chain[chain.len() - 1].clone();
    let parents = &chain[..chain.len() - 1];
    let siblings: Vec<Value> = match parents.last() {
        Some(parent) => parent["children"].as_array().cloned().unwrap_or_default(),
        None => canvas_json(app)
            .and_then(|s| {
                scenes_of(&s)
                    .into_iter()
                    .find(|x| x["id"] == scene.as_str())
                    .and_then(|x| x["items"].as_array().cloned())
            })
            .unwrap_or_default(),
    };
    let k = key_of(&item).map(str::to_string);
    let Some(index) = siblings
        .iter()
        .position(|i| key_of(i).map(str::to_string) == k)
    else {
        return;
    };
    let others: Vec<Value> = siblings
        .iter()
        .filter(|i| key_of(i).map(str::to_string) != k)
        .cloned()
        .collect();
    let list = match place {
        "front" => others.iter().cloned().chain([item]).collect(),
        "back" => [item].into_iter().chain(others.iter().cloned()).collect(),
        _ => {
            let mut list = others.clone();
            let to = if place == "forward" {
                (index + 1).min(others.len())
            } else {
                index.saturating_sub(1)
            };
            list.insert(to, item);
            list
        }
    };
    if list == siblings {
        return;
    }
    reorder(app, &scene, parents, &list);
}

/// A row of the Sources list dragged from `from` to land before row `to`
/// (rows top first: `rows` are their paths and parents' paths). It only
/// moves among its own siblings.
pub fn move_row(app: &Rc<App>, rows: &[(String, String)], from: usize, to: usize) {
    let Some((path, parent)) = rows.get(from).cloned() else {
        return;
    };
    let near = rows.get(to.min(rows.len().saturating_sub(1)));
    let above = to.checked_sub(1).and_then(|i| rows.get(i));
    // Dropped among another group's rows: keep it where it was.
    if near.is_none_or(|n| n.1 != parent)
        && above.is_none_or(|a| a.1 != parent)
        && above.is_none_or(|a| a.0 != parent)
    {
        return;
    }
    let siblings: Vec<&(String, String)> = rows.iter().filter(|r| r.1 == parent).collect();
    let before = rows[..to.min(rows.len())]
        .iter()
        .filter(|r| r.1 == parent)
        .count();
    let Some(from_index) = siblings.iter().position(|r| r.0 == path) else {
        return;
    };
    let display: Vec<String> = moved(&siblings, from_index, before)
        .into_iter()
        .map(|r| r.0.clone())
        .collect();
    let Some((scene, chain)) = chain_at(app, &path) else {
        return;
    };
    let parents = &chain[..chain.len() - 1];
    let level: Vec<Value> = match parents.last() {
        Some(p) => p["children"].as_array().cloned().unwrap_or_default(),
        None => canvas_json(app)
            .and_then(|s| {
                scenes_of(&s)
                    .into_iter()
                    .find(|x| x["id"] == scene.as_str())
                    .and_then(|x| x["items"].as_array().cloned())
            })
            .unwrap_or_default(),
    };
    // Rows are top first; the stack is back to front.
    let items: Vec<Value> = display
        .iter()
        .rev()
        .filter_map(|p| {
            let k = p.rsplit('/').next().unwrap_or(p);
            level.iter().find(|i| key_of(i) == Some(k)).cloned()
        })
        .collect();
    if items.len() == level.len() && items != level {
        reorder(app, &scene, parents, &items);
    }
}

/// Rename what the item at `path` shows: its source, or its scene.
pub fn rename_item(app: &Rc<App>, path: &str, name: &str) {
    let name = name.trim();
    let Some((_, chain)) = chain_at(app, path) else {
        return;
    };
    let item = &chain[chain.len() - 1];
    if name.is_empty() {
        return;
    }
    let patch = if let Some(source) = item["source"].as_str() {
        json!({ "sources": [{ "id": source, "name": name }] })
    } else if let Some(scene) = item["scene"].as_str() {
        json!({ "scenes": [{ "id": scene, "name": name }] })
    } else {
        return;
    };
    app.edit(patch, EditOptions::default());
}

/// Copy the selected item (⌘C).
pub fn copy_selected(app: &Rc<App>) -> Option<Value> {
    selected_json(app).map(|(_, chain)| chain[chain.len() - 1].clone())
}

/// Paste `item` into the scene being edited: showing the same source (a
/// reference), or a copy of the source (`duplicate`).
pub fn paste(app: &Rc<App>, item: &Value, duplicate: bool) {
    let (scene, typed, setup) = {
        let s = app.session.borrow();
        let (Some(scene), Some(typed), Some(setup)) =
            (s.editing_scene(), s.setup.clone(), s.canvas_json().cloned())
        else {
            return;
        };
        (scene, typed, setup)
    };
    let mut item = item.clone();
    let mut patch = json!({});
    if duplicate
        && let Some(id) = item["source"].as_str().map(str::to_string)
        && let Some(source) = setup["sources"]
            .as_array()
            .and_then(|s| s.iter().find(|x| x["id"] == id.as_str()))
    {
        let name = format!("{} 2", name_of(source));
        let new = model::new_id(&name, model::taken_ids(&typed));
        let mut copy = source.clone();
        copy["id"] = json!(new);
        copy["name"] = json!(name);
        patch["sources"] = json!([copy]);
        item["source"] = json!(new);
        if let Some(object) = item.as_object_mut() {
            object.remove("id");
        }
    }
    // Two items showing one source in a scene need ids of their own.
    let keys: Vec<String> = scenes_of(&setup)
        .into_iter()
        .find(|s| s["id"] == scene.as_str())
        .and_then(|s| s["items"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|i| key_of(i).map(str::to_string))
        .collect();
    if let Some(k) = key_of(&item)
        && keys.iter().any(|x| x == k)
    {
        let label = item_label_json(&typed, &item);
        let taken = keys
            .iter()
            .map(String::as_str)
            .chain(model::taken_ids(&typed))
            .collect::<Vec<_>>();
        item["id"] = json!(model::new_id(&label, taken));
    }
    let path = key_of(&item).unwrap_or_default().to_string();
    patch["scenes"] = json!([{ "id": scene, "items": [item] }]);
    app.edit_then(patch, EditOptions::default(), move |app, applied| {
        if applied {
            app.select(&path);
        }
    });
}

// ---------- Transform ----------

/// The native size of a source's picture, if it can be known without
/// asking: from the live engine, which runs every source, or from its
/// settings.
pub fn known_size(app: &Rc<App>, source: &str) -> Option<(f64, f64)> {
    let s = app.session.borrow();
    if let Some(run) = s.running.get(source)
        && let (Some(w), Some(h)) = (run.width, run.height)
        && w > 0
        && h > 0
    {
        return Some((w as f64, h as f64));
    }
    let setup = s.setup_json()?;
    let source = setup["sources"]
        .as_array()?
        .iter()
        .find(|x| x["id"] == source)?;
    let canvas = &setup["canvas"];
    let number = |v: &Value| v.as_f64().filter(|n| *n > 0.0);
    let both = |src: &Value| Some((number(&src["width"])?, number(&src["height"])?));
    match source["kind"].as_str()? {
        "test_pattern" => Some((
            number(&source["width"]).unwrap_or(1920.0),
            number(&source["height"]).unwrap_or(1080.0),
        )),
        "color" => both(source).or_else(|| both(canvas)),
        "browser" => Some((
            number(&source["width"]).unwrap_or(800.0),
            number(&source["height"]).unwrap_or(600.0),
        )),
        "camera" | "image" | "slideshow" | "text" => both(source),
        _ => None,
    }
}

/// The size of a source's picture: known, or measured once from a
/// snapshot; `then` runs with it (None if it can't be known).
pub fn source_size(
    app: &Rc<App>,
    source: Option<String>,
    then: impl FnOnce(&Rc<App>, Option<(f64, f64)>) + 'static,
) {
    let Some(source) = source else {
        then(app, None);
        return;
    };
    if let Some(size) = known_size(app, &source) {
        then(app, Some(size));
        return;
    }
    app.call(
        Method::Snapshot(SnapshotParams {
            source: Some(source),
            scene: None,
            preview: false,
            width: None,
        }),
        move |app, result| {
            let size = result
                .ok()
                .and_then(|v| Some((v["width"].as_f64()?, v["height"].as_f64()?)));
            then(app, size)
        },
    );
}

/// The selected item, unless it is locked.
fn unlocked(app: &Rc<App>) -> Option<(String, Vec<Value>)> {
    selected_json(app).filter(|(_, chain)| chain[chain.len() - 1]["locked"] != true)
}

fn crop_of(item: &Value) -> [f64; 4] {
    let c = &item["crop"];
    let n = |k: &str| c[k].as_f64().unwrap_or(0.0);
    [n("left"), n("top"), n("right"), n("bottom")]
}

/// A patch taking away an item's scale and offset (keeping its rotation),
/// so its rect is where it is drawn.
fn unscaled(item: &Value, fields: &mut Value) {
    if !item["style"]["transform"].is_null() {
        fields["style"] = json!({ "transform": { "scale": null, "translate": null } });
    }
}

/// A whole number for a signed setup field (`rect.x`, `rect.y`): the
/// daemon's `Rect` and `Crop` are integers and refuse `720.0`.
pub fn int(v: f64) -> Value {
    json!(v.round() as i64)
}

/// A whole, non-negative number for an unsigned setup field
/// (`rect.width`, `crop.left`, ...).
pub fn whole(v: f64) -> Value {
    json!(v.round().max(0.0) as u64)
}

/// The size of the canvas being edited (the vertical one's while editing
/// it).
fn canvas_size(app: &Rc<App>) -> (f64, f64) {
    app.session
        .borrow()
        .edited_setup()
        .map(|s| (s.canvas.width as f64, s.canvas.height as f64))
        .unwrap_or((1920.0, 1080.0))
}

/// Fit to Screen (⌘F): as large as fits on the canvas, keeping its shape,
/// centred.
pub fn fit_to_screen(app: &Rc<App>) {
    let Some((_, chain)) = unlocked(app) else {
        return;
    };
    let item = chain[chain.len() - 1].clone();
    let source = item["source"].as_str().map(str::to_string);
    source_size(app, source, move |app, natural| {
        let (cw, ch) = canvas_size(app);
        let crop = crop_of(&item);
        let size = natural
            .map(|(w, h)| {
                (
                    (w - crop[0] - crop[2]).max(1.0),
                    (h - crop[1] - crop[3]).max(1.0),
                )
            })
            .unwrap_or((
                item["rect"]["width"].as_f64().unwrap_or(cw),
                item["rect"]["height"].as_f64().unwrap_or(ch),
            ));
        let scale = (cw / size.0).min(ch / size.1);
        if item["fit"] == "none" && item["layout"].is_null() {
            // Without bounds: its own size, scaled to fit, centred (the
            // scale is about the rect's centre).
            let fields = json!({
                "rect": {
                    "x": int((cw - size.0) / 2.0), "y": int((ch - size.1) / 2.0),
                    "width": whole(size.0), "height": whole(size.1),
                },
                "style": { "transform": {
                    "scale": (scale * 10_000.0).round() / 10_000.0, "translate": null,
                } },
            });
            update_selected(app, fields, None);
            return;
        }
        let width = (size.0 * scale).round().max(1.0);
        let height = (size.1 * scale).round().max(1.0);
        let mut fields = json!({ "rect": {
            "x": int((cw - width) / 2.0), "y": int((ch - height) / 2.0),
            "width": whole(width), "height": whole(height),
        } });
        unscaled(&item, &mut fields);
        update_selected(app, fields, None);
    });
}

/// Stretch to Screen (⌘S): cover the canvas exactly.
pub fn stretch_to_screen(app: &Rc<App>) {
    let Some((_, chain)) = unlocked(app) else {
        return;
    };
    let (cw, ch) = canvas_size(app);
    let item = &chain[chain.len() - 1];
    let mut fields = json!({ "rect": { "x": 0, "y": 0, "width": whole(cw), "height": whole(ch) } });
    // One scale can't stretch: an item without bounds gets them (OBS's
    // Stretch to Screen sets stretch bounds too).
    if item["fit"] == "none" {
        fields["fit"] = json!("stretch");
        fields["style"] = json!({ "transform": { "scale": null, "translate": null } });
    }
    unscaled(item, &mut fields);
    update_selected(app, fields, None);
}

/// Center to Screen (⌘D), or only along one axis ("x": horizontally,
/// "y": vertically).
pub fn center_to_screen(app: &Rc<App>, axis: Option<&str>) {
    let Some((_, chain)) = unlocked(app) else {
        return;
    };
    let item = &chain[chain.len() - 1];
    let (cw, ch) = canvas_size(app);
    let t = &item["style"]["transform"]["translate"];
    let (tx, ty) = (t[0].as_f64().unwrap_or(0.0), t[1].as_f64().unwrap_or(0.0));
    let (w, h) = (
        item["rect"]["width"].as_f64().unwrap_or(0.0),
        item["rect"]["height"].as_f64().unwrap_or(0.0),
    );
    let mut rect = Map::new();
    if axis != Some("y") {
        rect.insert("x".into(), int((cw - w) / 2.0 - tx));
    }
    if axis != Some("x") {
        rect.insert("y".into(), int((ch - h) / 2.0 - ty));
    }
    update_selected(app, json!({ "rect": rect }), None);
}

/// Reset Transform (⌘R): its own size at the top left, no rotation, scale
/// or crop.
pub fn reset_transform(app: &Rc<App>) {
    let Some((_, chain)) = unlocked(app) else {
        return;
    };
    let item = chain[chain.len() - 1].clone();
    let source = item["source"].as_str().map(str::to_string);
    source_size(app, source, move |app, natural| {
        let (w, h) = natural.unwrap_or((
            item["rect"]["width"].as_f64().unwrap_or(1.0),
            item["rect"]["height"].as_f64().unwrap_or(1.0),
        ));
        let mut fields = json!({
            "rect": { "x": 0, "y": 0, "width": whole(w), "height": whole(h) },
            "crop": null, "fit": "none", "align": null,
        });
        if !item["style"]["transform"].is_null() {
            fields["style"] = json!({ "transform": null });
        }
        update_selected(app, fields, None);
    });
}

/// Rotate by `degrees` (clockwise), about the centre.
pub fn rotate_by(app: &Rc<App>, degrees: f64) {
    let Some((_, chain)) = unlocked(app) else {
        return;
    };
    let item = &chain[chain.len() - 1];
    let now = item["style"]["transform"]["rotate"].as_f64().unwrap_or(0.0);
    let mut next = ((now + degrees) % 360.0 + 360.0) % 360.0;
    if next > 180.0 {
        next -= 360.0;
    }
    if next == 0.0 && item["style"]["transform"].is_null() {
        return;
    }
    let rotate = if next == 0.0 {
        Value::Null
    } else {
        json!(next)
    };
    update_selected(
        app,
        json!({ "style": { "transform": { "rotate": rotate } } }),
        None,
    );
}

// ---------- Groups ----------

/// The items of one level of `scene` (its top level, or the children of
/// the last of `parents`).
fn level_of(app: &Rc<App>, scene: &str, parents: &[Value]) -> Vec<Value> {
    match parents.last() {
        Some(parent) => parent["children"].as_array().cloned().unwrap_or_default(),
        None => canvas_json(app)
            .and_then(|s| {
                scenes_of(&s)
                    .into_iter()
                    .find(|x| x["id"] == scene)
                    .and_then(|x| x["items"].as_array().cloned())
            })
            .unwrap_or_default(),
    }
}

/// An id for a new group ("group", "group-2"…) that no item key, source
/// or scene of the setup has.
fn group_id(app: &Rc<App>) -> String {
    let s = app.session.borrow();
    let Some(setup) = s.setup.as_ref() else {
        return "group".into();
    };
    fn keys<'a>(items: &'a [isocast_protocol::SceneItem], out: &mut Vec<&'a str>) {
        for item in items {
            if let Some(k) = item.key() {
                out.push(k);
            }
            keys(&item.children, out);
        }
    }
    let mut taken = model::taken_ids(setup);
    for scene in &setup.scenes {
        keys(&scene.items, &mut taken);
        if let Some(vertical) = &scene.vertical {
            keys(&vertical.items, &mut taken);
        }
        for arrangement in scene.canvases.values() {
            keys(&arrangement.items, &mut taken);
        }
    }
    model::new_id("Group", taken)
}

/// The `[x, y, width, height]` of a JSON item's rect, if it has one.
fn rect_numbers(item: &Value) -> Option<[f64; 4]> {
    let r = item.get("rect")?;
    Some([
        r["x"].as_f64()?,
        r["y"].as_f64()?,
        r["width"].as_f64()?,
        r["height"].as_f64()?,
    ])
}

/// `item` with its rect moved by (dx, dy).
fn shifted(mut item: Value, dx: f64, dy: f64) -> Value {
    if let Some([x, y, _, _]) = rect_numbers(&item) {
        item["rect"]["x"] = int(x + dx);
        item["rect"]["y"] = int(y + dy);
    }
    item
}

/// Group Selected Items: the items at `paths` (siblings) become the
/// children of a new group as large as they cover, where the front one
/// was, with nothing on the canvas moving. One edit; returns the group's
/// path, or why not.
pub fn group_items(app: &Rc<App>, paths: &[String]) -> Result<String, String> {
    let chains: Vec<(String, Vec<Value>)> = paths.iter().filter_map(|p| chain_at(app, p)).collect();
    if chains.len() < 2 || chains.len() != paths.len() {
        return Err("Select two or more items in the Sources list to group.".into());
    }
    let (scene, first) = &chains[0];
    let parents = &first[..first.len() - 1];
    let parent_key = |chain: &[Value]| -> Vec<Option<String>> {
        chain[..chain.len() - 1]
            .iter()
            .map(|i| key_of(i).map(str::to_string))
            .collect()
    };
    if chains
        .iter()
        .any(|(_, c)| parent_key(c) != parent_key(first))
    {
        return Err("Only items side by side in one list can be grouped.".into());
    }
    let level = level_of(app, scene, parents);
    let chosen: Vec<Option<String>> = chains
        .iter()
        .map(|(_, c)| key_of(&c[c.len() - 1]).map(str::to_string))
        .collect();
    let is_chosen = |i: &Value| chosen.contains(&key_of(i).map(str::to_string));
    let members: Vec<Value> = level.iter().filter(|i| is_chosen(i)).cloned().collect();
    let rects: Option<Vec<[f64; 4]>> = members.iter().map(rect_numbers).collect();
    let Some(rects) = rects else {
        return Err("Items placed by a layout can't be grouped yet.".into());
    };
    let left = rects.iter().map(|r| r[0]).fold(f64::INFINITY, f64::min);
    let top = rects.iter().map(|r| r[1]).fold(f64::INFINITY, f64::min);
    let right = rects
        .iter()
        .map(|r| r[0] + r[2])
        .fold(f64::NEG_INFINITY, f64::max);
    let bottom = rects
        .iter()
        .map(|r| r[1] + r[3])
        .fold(f64::NEG_INFINITY, f64::max);
    let id = group_id(app);
    let group = json!({
        "id": id,
        "rect": {
            "x": int(left),
            "y": int(top),
            "width": int((right - left).max(1.0)),
            "height": int((bottom - top).max(1.0)),
        },
        "children": members
            .iter()
            .map(|i| shifted(i.clone(), -left, -top))
            .collect::<Vec<_>>(),
    });
    // Where the front one was, among what's left.
    let front = level.iter().rposition(&is_chosen).unwrap_or(0);
    let at = level[..front].iter().filter(|i| !is_chosen(i)).count();
    let mut items: Vec<Value> = level.iter().filter(|i| !is_chosen(i)).cloned().collect();
    items.insert(at, group);
    reorder(app, scene, parents, &items);
    let mut path: Vec<String> = parents
        .iter()
        .filter_map(|i| key_of(i).map(str::to_string))
        .collect();
    path.push(id);
    Ok(path.join("/"))
}

/// Add → Group: an empty group at the top of the scene being edited
/// (Move to Group puts items in it). Returns its path.
pub fn add_group(app: &Rc<App>) -> Option<String> {
    let scene = app.session.borrow().editing_scene()?;
    let id = group_id(app);
    let group = json!({ "id": id, "rect": { "x": 0, "y": 0, "width": 1, "height": 1 } });
    app.edit(
        json!({ "scenes": [{ "id": scene, "items": [group] }] }),
        EditOptions::default(),
    );
    Some(id)
}

/// Ungroup: a group's children back in its place, where they show now.
pub fn ungroup(app: &Rc<App>, path: &str) {
    let Some((scene, chain)) = chain_at(app, path) else {
        return;
    };
    let group = &chain[chain.len() - 1];
    let parents = &chain[..chain.len() - 1];
    let [gx, gy, _, _] = rect_numbers(group).unwrap_or_default();
    let children: Vec<Value> = group["children"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|c| shifted(c, gx, gy))
        .collect();
    let level = level_of(app, &scene, parents);
    let key = key_of(group).map(str::to_string);
    let mut items = Vec::new();
    for item in level {
        if key_of(&item).map(str::to_string) == key {
            items.extend(children.iter().cloned());
        } else {
            items.push(item);
        }
    }
    reorder(app, &scene, parents, &items);
}

/// The top-level groups of the scene being edited: (key, label).
pub fn groups(app: &Rc<App>) -> Vec<(String, String)> {
    let s = app.session.borrow();
    let (Some(setup), Some(scene)) = (s.edited_setup(), s.editing_scene()) else {
        return Vec::new();
    };
    model::find_scene(setup, &scene)
        .map(|scene| {
            scene
                .items
                .iter()
                .filter(|i| i.source.is_none() && i.scene.is_none() && i.content.is_none())
                .filter_map(|i| Some((i.key()?.to_string(), model::item_label(setup, i))))
                .collect()
        })
        .unwrap_or_default()
}

/// Move the top-level item at `path` into the top-level group `group`,
/// where it shows now (the group grows to cover it). One edit.
pub fn move_to_group(app: &Rc<App>, path: &str, group: &str) {
    let Some((scene, chain)) = chain_at(app, path) else {
        return;
    };
    if chain.len() != 1 {
        return;
    }
    let item = chain[0].clone();
    let level = level_of(app, &scene, &[]);
    let Some(g) = level.iter().find(|i| key_of(i) == Some(group)).cloned() else {
        return;
    };
    let Some(r) = rect_numbers(&item) else {
        return;
    };
    let mut children: Vec<Value> = g["children"].as_array().cloned().unwrap_or_default();
    let [gx, gy, gw, gh] = rect_numbers(&g).unwrap_or([r[0], r[1], 1.0, 1.0]);
    // The group's new box: what it covered (none if empty) and the item.
    let (mut left, mut top, mut right, mut bottom) = (r[0], r[1], r[0] + r[2], r[1] + r[3]);
    if !children.is_empty() {
        left = left.min(gx);
        top = top.min(gy);
        right = right.max(gx + gw);
        bottom = bottom.max(gy + gh);
    }
    children = children
        .into_iter()
        .map(|c| shifted(c, gx - left, gy - top))
        .collect();
    children.push(shifted(item.clone(), -left, -top));
    let mut grown = g.clone();
    grown["rect"] = json!({
        "x": int(left),
        "y": int(top),
        "width": int((right - left).max(1.0)),
        "height": int((bottom - top).max(1.0)),
    });
    grown["children"] = Value::Array(children);
    let key = key_of(&item).map(str::to_string);
    let items: Vec<Value> = level
        .into_iter()
        .filter(|i| key_of(i).map(str::to_string) != key)
        .map(|i| {
            if key_of(&i) == Some(group) {
                grown.clone()
            } else {
                i
            }
        })
        .collect();
    reorder(app, &scene, &[], &items);
}

#[cfg(test)]
mod tests {
    use super::{int, whole, without_scene, without_source};
    use isocast_protocol::Setup;
    use serde_json::json;

    fn setup() -> serde_json::Value {
        json!({
            "version": 1,
            "name": "Talk",
            "canvas": { "width": 1920, "height": 1080, "fps": 30 },
            "recording": {
                "folder": "~/Videos",
                "program": { "enabled": true, "encoder": { "codec": "h264" } },
                "iso": { "enabled": true, "encoder": { "codec": "h264" }, "sources": ["camera", "doc"] }
            },
            "sources": [
                { "id": "camera", "kind": "camera", "device": "cam" },
                { "id": "doc", "kind": "window", "app": "browser" }
            ],
            "scenes": [
                { "id": "camera", "name": "Camera", "items": [{ "id": "cam", "source": "camera" }] },
                { "id": "doc", "name": "Doc", "items": [
                    { "id": "doc", "source": "doc" },
                    { "id": "cam", "source": "camera" }
                ] }
            ]
        })
    }

    #[test]
    fn removing_a_source_or_scene_leaves_a_valid_setup() {
        let out = without_source(&setup(), "doc");
        assert!(out["scenes"][1].get("canvases").is_none());
        let out: Setup = serde_json::from_value(out).unwrap();
        assert!(out.sources.iter().all(|s| s.id != "doc"));
        assert_eq!(out.scenes[1].items.len(), 1);

        let out = without_scene(&setup(), "doc");
        let out: Setup = serde_json::from_value(out).unwrap();
        assert!(out.scenes.iter().all(|s| s.id != "doc"));
    }

    #[test]
    fn transform_numbers_fit_the_typed_rect_and_crop() {
        let rect = json!({ "x": int(-12.6), "y": int(720.0), "width": whole(1919.5), "height": whole(-3.0) });
        let rect: isocast_protocol::Rect = serde_json::from_value(rect).unwrap();
        assert_eq!(
            (rect.x, rect.y, rect.width, rect.height),
            (-13, 720, 1920, 0)
        );
        let crop = json!({ "left": whole(4.4), "top": whole(0.0), "right": whole(8.0), "bottom": whole(2.5) });
        let crop: isocast_protocol::Crop = serde_json::from_value(crop).unwrap();
        assert_eq!((crop.left, crop.right, crop.bottom), (4, 8, 3));
    }
}
