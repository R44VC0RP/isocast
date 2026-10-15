//! The per-viewer layout (the web app keeps it in localStorage): dock
//! sizes, which docks and bars show, the locks, preview scaling and Studio
//! Mode; and Settings → General's snapping and preview frame rate. Kept
//! in `$ISOCAST_UI_PREFS`, else `isocast-ui.json` in Isocast's support
//! folder. Snapshots neither read nor write it, so they always show the
//! default layout.

use std::path::PathBuf;

use serde_json::{Value, json};
use slint::{ComponentHandle, Model};

use crate::{AppWindow, MixerArea, Ui};

fn path() -> PathBuf {
    if let Some(path) = std::env::var_os("ISOCAST_UI_PREFS") {
        return PathBuf::from(path);
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let folder = if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Isocast")
    } else {
        home.join(".isocast")
    };
    folder.join("isocast-ui.json")
}

/// Put the saved layout on the window (anything missing keeps its default).
pub fn load(window: &AppWindow) {
    let Ok(text) = std::fs::read_to_string(path()) else {
        return;
    };
    let Ok(saved) = serde_json::from_str::<Value>(&text) else {
        return;
    };
    let ui = window.global::<Ui>();
    let flag = |name: &str, set: &dyn Fn(bool)| {
        if let Some(v) = saved.get(name).and_then(Value::as_bool) {
            set(v);
        }
    };
    let length = |name: &str, set: &dyn Fn(f32)| {
        if let Some(v) = saved.get(name).and_then(Value::as_f64) {
            set(v as f32);
        }
    };
    flag("source_toolbar", &|v| ui.set_show_source_toolbar(v));
    flag("status_bar", &|v| ui.set_show_status_bar(v));
    flag("docks_pane", &|v| ui.set_show_docks(v));
    flag("scenes", &|v| ui.set_show_scenes(v));
    flag("sources", &|v| ui.set_show_sources(v));
    flag("mixer", &|v| ui.set_show_mixer(v));
    flag("transitions", &|v| ui.set_show_transitions(v));
    flag("controls", &|v| ui.set_show_controls(v));
    flag("docks_locked", &|v| ui.set_docks_locked(v));
    flag("preview_locked", &|v| ui.set_preview_locked(v));
    flag("studio", &|v| ui.set_studio(v));
    // The Create view (area G).
    flag("create", &|v| {
        window.global::<crate::CreateArea>().set_active(v)
    });
    length("docks_height", &|v| ui.set_docks_height(v));
    length("scenes_width", &|v| ui.set_scenes_width(v));
    length("sources_width", &|v| ui.set_sources_width(v));
    length("transitions_width", &|v| ui.set_transitions_width(v));
    length("controls_width", &|v| ui.set_controls_width(v));
    if let Some(v) = saved.get("preview_scale").and_then(Value::as_str) {
        ui.set_preview_scale(v.into());
    }
    if let Some(ids) = saved.get("mixer_hidden").and_then(Value::as_array) {
        let ids: Vec<slint::SharedString> = ids
            .iter()
            .filter_map(Value::as_str)
            .map(Into::into)
            .collect();
        window
            .global::<MixerArea>()
            .set_hidden(slint::ModelRc::new(slint::VecModel::from(ids)));
    }
    if let Some(snapping) = saved.get("snapping") {
        if let Some(v) = snapping.get("enabled").and_then(Value::as_bool) {
            ui.set_snap_enabled(v);
        }
        if let Some(v) = snapping.get("distance").and_then(Value::as_f64) {
            ui.set_snap_distance(v as f32);
        }
    }
    if let Some(v) = saved.get("preview_rate").and_then(Value::as_str) {
        ui.set_preview_rate(v.into());
    }
}

/// Keep the window's layout.
pub fn save(window: &AppWindow) {
    let ui = window.global::<Ui>();
    let layout = json!({
        "source_toolbar": ui.get_show_source_toolbar(),
        "status_bar": ui.get_show_status_bar(),
        "docks_pane": ui.get_show_docks(),
        "scenes": ui.get_show_scenes(),
        "sources": ui.get_show_sources(),
        "mixer": ui.get_show_mixer(),
        "transitions": ui.get_show_transitions(),
        "controls": ui.get_show_controls(),
        "docks_locked": ui.get_docks_locked(),
        "preview_locked": ui.get_preview_locked(),
        "studio": ui.get_studio(),
        "create": window.global::<crate::CreateArea>().get_active(),
        "docks_height": ui.get_docks_height(),
        "scenes_width": ui.get_scenes_width(),
        "sources_width": ui.get_sources_width(),
        "transitions_width": ui.get_transitions_width(),
        "controls_width": ui.get_controls_width(),
        "preview_scale": ui.get_preview_scale().as_str(),
        "mixer_hidden": window
            .global::<MixerArea>()
            .get_hidden()
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>(),
        "snapping": { "enabled": ui.get_snap_enabled(), "distance": ui.get_snap_distance() },
        "preview_rate": ui.get_preview_rate().as_str(),
    });
    let path = path();
    if let Some(folder) = path.parent() {
        let _ = std::fs::create_dir_all(folder);
    }
    // Not kept is fine (a read-only home): the layout is a convenience.
    let _ = std::fs::write(&path, format!("{layout:#}\n"));
}

/// View → Reset UI: every dock back, the bars shown, preview scaling to
/// the window. `docks_only` (Docks → Reset Docks) leaves the bars.
pub fn reset(window: &AppWindow, docks_only: bool) {
    let ui = window.global::<Ui>();
    ui.set_docks_height(0.0);
    ui.set_scenes_width(210.0);
    ui.set_sources_width(250.0);
    ui.set_transitions_width(200.0);
    ui.set_controls_width(200.0);
    ui.set_show_scenes(true);
    ui.set_show_sources(true);
    ui.set_show_mixer(true);
    ui.set_show_transitions(true);
    ui.set_show_controls(true);
    ui.set_show_docks(true);
    ui.set_docks_locked(false);
    if !docks_only {
        ui.set_show_source_toolbar(true);
        ui.set_show_status_bar(true);
        ui.set_preview_locked(false);
        ui.set_preview_scale("window".into());
    }
}

/// Notices the viewer chose not to see again (the encoder overload notice
/// for one setup's workload, #66), kept beside the layout in their own
/// file so saving the layout leaves them alone.
fn notices_path() -> PathBuf {
    path().with_file_name("isocast-ui-notices.json")
}

/// Whether notice `key` was hidden for good.
pub fn notice_hidden(key: &str) -> bool {
    std::fs::read_to_string(notices_path())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|saved| saved.get("hidden").cloned())
        .and_then(|hidden| hidden.as_array().cloned())
        .is_some_and(|hidden| hidden.iter().any(|k| k.as_str() == Some(key)))
}

/// Hide notice `key` for good.
pub fn hide_notice(key: &str) {
    let path = notices_path();
    let mut saved = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let hidden = saved["hidden"].as_array().cloned().unwrap_or_default();
    if hidden.iter().any(|k| k.as_str() == Some(key)) {
        return;
    }
    let mut hidden = hidden;
    hidden.push(json!(key));
    saved["hidden"] = Value::Array(hidden);
    if let Some(folder) = path.parent() {
        let _ = std::fs::create_dir_all(folder);
    }
    // Not kept is fine: the notice shows again, once per take.
    let _ = std::fs::write(&path, format!("{saved:#}\n"));
}
