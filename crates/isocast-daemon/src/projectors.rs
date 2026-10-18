//! Projectors (G.4, #19): `projector_open`, `projector_close`,
//! `projector_list` and `projector_place`. The daemon keeps the open
//! projectors and where each view was last shown, in
//! `<setup>.projectors.json` beside the setup (so a throwaway engine on
//! its own socket keeps its own); the app shows them in windows and
//! reports where they are. A full-screen projector whose display goes away
//! is closed, with the reason in the `projectors` event.

use std::collections::BTreeMap;
use std::fs;

use serde::{Deserialize, Serialize};

use super::*;
use isocast_protocol::CanvasId;
use isocast_protocol::devices::Display;
use isocast_protocol::projector::{
    CloseParams, MAIN_CANVAS, MultiviewLayout, OpenParams, PlaceParams, Placement, Projector,
    ProjectorView, Projectors, VERTICAL_CANVAS,
};

/// What is kept on disk.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    open: Vec<Projector>,
    /// Where each view was last shown (by [`key`]), for the next time it
    /// opens.
    #[serde(default)]
    placements: BTreeMap<String, Placement>,
    /// The number the next projector's id gets.
    #[serde(default)]
    next: u32,
    /// Full-screen projectors that were open when the engine stopped
    /// but whose display wasn't connected when it started again (an
    /// external display that attaches late): they open again when it
    /// comes (#95), unless the same view was opened or everything closed
    /// in between.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    waiting: Vec<Projector>,
}

/// The most projectors kept waiting for their displays (#95).
const MAX_WAITING: usize = 16;

#[derive(Default)]
pub(super) struct ProjectorState {
    saved: Mutex<Saved>,
}

fn path() -> PathBuf {
    isocast_protocol::api::setup_path().with_extension("projectors.json")
}

fn invalid(message: impl Into<String>) -> RpcError {
    RpcError {
        code: RpcError::INVALID_PARAMS,
        message: message.into(),
    }
}

/// The view a projector shows, as placements remember it.
fn key(projector: &Projector) -> String {
    let what = match projector.view {
        ProjectorView::Scene => format!("scene:{}", projector.scene.as_deref().unwrap_or("")),
        ProjectorView::Source => format!("source:{}", projector.source.as_deref().unwrap_or("")),
        view => serde_json::to_value(view)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default(),
    };
    format!("{}/{what}", projector.canvas)
}

/// The displays a projector can fill (none where they can't be listed).
fn displays() -> Vec<Display> {
    super::devices().map(|d| d.displays).unwrap_or_default()
}

/// A full-screen placement on a display that isn't connected (when the
/// displays could be listed): it must not cover another one instead.
fn display_missing(placement: &Placement, displays: &[Display]) -> bool {
    placement.fullscreen
        && !displays.is_empty()
        && placement
            .display
            .as_ref()
            .is_some_and(|id| !displays.iter().any(|d| &d.id == id))
}

/// A window placement on a display that isn't connected (when the
/// displays could be listed): its frame is somewhere no screen shows.
fn window_display_missing(placement: &Placement, displays: &[Display]) -> bool {
    !placement.fullscreen
        && !displays.is_empty()
        && placement
            .display
            .as_ref()
            .is_some_and(|id| !displays.iter().any(|d| &d.id == id))
}

/// The display `query` names: `main`, an id or a name (exact, then
/// ignoring case). An error lists them.
fn find_display<'a>(displays: &'a [Display], query: &str) -> Result<&'a Display, RpcError> {
    let found = match query {
        "main" => displays.iter().find(|d| d.main),
        _ => displays
            .iter()
            .find(|d| d.id == query || d.name == query)
            .or_else(|| displays.iter().find(|d| d.name.eq_ignore_ascii_case(query))),
    };
    found.ok_or_else(|| {
        let names: Vec<String> = displays
            .iter()
            .map(|d| format!("{} ({})", d.id, d.name))
            .collect();
        invalid(format!(
            "display: {query:?} isn't a display; give \"main\" or one of: {}",
            if names.is_empty() {
                "(none found)".to_string()
            } else {
                names.join(", ")
            }
        ))
    })
}

impl ProjectorState {
    /// Read the projectors kept on disk (none shown until an app says so).
    /// A full-screen one whose display isn't connected now waits for it
    /// (never covering another display): it opens again when the display
    /// comes (#95).
    fn load(&self) {
        let path = path();
        let saved = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Saved>(&bytes) {
                Ok(saved) => saved,
                Err(error) => {
                    isocast_diagnostics::diagnostic!("warn";
                        "isocastd: ignoring {}: {error}", path.display()
                    );
                    Saved::default()
                }
            },
            Err(_) => Saved::default(),
        };
        let mut saved = saved;
        if !saved.open.is_empty() {
            let displays = displays();
            for projector in &mut saved.open {
                if window_display_missing(&projector.placement, &displays) {
                    projector.placement.display = None;
                    projector.placement.frame = None;
                }
            }
            let (gone, open): (Vec<Projector>, Vec<Projector>) = std::mem::take(&mut saved.open)
                .into_iter()
                .partition(|p| display_missing(&p.placement, &displays));
            saved.open = open;
            for projector in gone {
                isocast_diagnostics::diagnostic!("info";
                    "isocastd: projector {} waits for its display {}, which isn't connected",
                    projector.id,
                    projector.placement.display.as_deref().unwrap_or_default()
                );
                saved
                    .placements
                    .insert(key(&projector), projector.placement.clone());
                saved.waiting.retain(|p| !p.same_view(&projector));
                saved.waiting.push(projector);
                // Bounded (#95): the oldest give way.
                let over = saved.waiting.len().saturating_sub(MAX_WAITING);
                saved.waiting.drain(..over);
            }
        }
        let mut kept = self.saved.lock();
        *kept = saved;
        for projector in &mut kept.open {
            projector.shown = false;
        }
    }

    fn save(saved: &Saved) {
        let path = path();
        let result = serde_json::to_vec_pretty(saved)
            .map_err(anyhow::Error::from)
            .and_then(|text| store::write_atomically(&path, &text));
        if let Err(error) = result {
            isocast_diagnostics::diagnostic!("warn";
                "isocastd: could not save {}: {error:#}", path.display()
            );
        }
    }
}

impl Daemon {
    pub(super) fn load_projectors(&self) {
        self.projectors.load();
    }

    pub(super) fn projector_list(&self) -> Projectors {
        let displays = displays();
        // A display that attached after the engine started (before the
        // first device inventory, which sends no `device_changed`).
        self.projectors_waiting(&displays);
        Projectors {
            projectors: self.projectors.saved.lock().open.clone(),
            displays,
        }
    }

    /// Open again the projectors waiting for a display that is connected
    /// now (see [`Saved::waiting`]).
    fn projectors_waiting(&self, displays: &[Display]) {
        let back: Vec<String> = {
            let mut saved = self.projectors.saved.lock();
            if saved.waiting.is_empty() || displays.is_empty() {
                return;
            }
            let (back, waiting): (Vec<Projector>, Vec<Projector>) =
                std::mem::take(&mut saved.waiting)
                    .into_iter()
                    .partition(|p| !display_missing(&p.placement, displays));
            saved.waiting = waiting;
            let mut names = Vec::new();
            for mut projector in back {
                if saved.open.iter().any(|p| p.same_view(&projector)) {
                    continue;
                }
                if saved.open.iter().any(|p| p.id == projector.id) {
                    saved.next = saved.next.max(1);
                    projector.id = format!("projector-{}", saved.next);
                    saved.next += 1;
                }
                projector.shown = false;
                names.push(
                    projector
                        .placement
                        .display
                        .as_deref()
                        .and_then(|id| displays.iter().find(|d| d.id == id))
                        .map(|d| d.name.clone())
                        .unwrap_or_default(),
                );
                saved.open.push(projector);
            }
            names
        };
        if !back.is_empty() {
            let mut names = back;
            names.dedup();
            self.projectors_changed(Some(format!(
                "the display {} was connected",
                names.join(", ")
            )));
        }
    }

    pub(super) fn projector_open(&self, params: OpenParams) -> Result<Projector, RpcError> {
        let canvas = params.canvas.clone().unwrap_or_else(|| MAIN_CANVAS.into());
        // Kept as parsed (trimmed), so the same canvas written two ways is
        // one view (`same_view`, placements).
        let canvas = match CanvasId::parse(&canvas) {
            Ok(id) => id.as_str().to_string(),
            Err(_) => canvas,
        };
        // The live setup's canvases (#120), for the checks below.
        let canvas_ids: Vec<CanvasId> = {
            let state = self.state.lock();
            state
                .setup
                .as_ref()
                .map(|s| s.canvas_ids())
                .unwrap_or_else(|| vec![CanvasId::Main])
        };
        let listed = || {
            canvas_ids
                .iter()
                .map(|c| c.as_str().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let extra = match CanvasId::parse(&canvas) {
            Ok(CanvasId::Main) => false,
            Ok(id) if canvas_ids.contains(&id) => true,
            // Said apart below, with how to add it.
            Ok(CanvasId::Vertical) => true,
            _ => {
                return Err(invalid(format!(
                    "canvas: {canvas:?} isn't a canvas of the live setup; canvases: {}",
                    listed()
                )));
            }
        };
        if extra
            && matches!(
                params.view,
                ProjectorView::Source | ProjectorView::Multiview
            )
        {
            return Err(invalid(format!(
                "canvas: {canvas:?} shows views program, preview and scene; a {} is the \
                 same on every canvas, so leave canvas out (a multiview shows extra \
                 canvases' programs with canvases = [...])",
                if params.view == ProjectorView::Source {
                    "source"
                } else {
                    "multiview"
                }
            )));
        }
        let vertical = canvas == VERTICAL_CANVAS;
        let only = |field: &str, given: bool, view: &str| {
            if given {
                Err(invalid(format!(
                    "{field}: only for view = {view}; leave it out for view = {}",
                    serde_json::to_value(params.view)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_string))
                        .unwrap_or_default()
                )))
            } else {
                Ok(())
            }
        };
        if params.view != ProjectorView::Scene {
            only("scene", params.scene.is_some(), "scene")?;
        }
        if params.view != ProjectorView::Source {
            only("source", params.source.is_some(), "source")?;
        }
        if params.view != ProjectorView::Multiview {
            only("layout", params.layout.is_some(), "multiview")?;
            only("vertical", params.vertical.is_some(), "multiview")?;
            only("canvases", params.canvases.is_some(), "multiview")?;
        }
        // A multiview's canvas tiles: extra canvases of the live setup, each
        // once, at most half its scene tiles.
        let tile_canvases: Option<Vec<String>> = match &params.canvases {
            None => None,
            Some(list) => {
                let mut kept: Vec<String> = Vec::new();
                for (i, id) in list.iter().enumerate() {
                    match CanvasId::parse(id) {
                        Ok(c) if !c.is_main() && canvas_ids.contains(&c) => {
                            if !kept.iter().any(|k| k == c.as_str()) {
                                kept.push(c.as_str().to_string());
                            }
                        }
                        _ => {
                            let extras: Vec<&str> = canvas_ids
                                .iter()
                                .filter(|c| !c.is_main())
                                .map(|c| c.as_str())
                                .collect();
                            return Err(invalid(format!(
                                "canvases[{i}]: {id:?} isn't an extra canvas of the live setup; \
                                 extra canvases: {}",
                                if extras.is_empty() {
                                    "(none)".to_string()
                                } else {
                                    extras.join(", ")
                                }
                            )));
                        }
                    }
                }
                Some(kept)
            }
        };
        {
            let state = self.state.lock();
            let setup = state.setup.as_ref();
            let has_vertical = setup.is_some_and(|s| s.vertical.is_some());
            if (vertical || params.vertical == Some(true)) && !has_vertical {
                // (A `[[canvases]]` id the setup lacks was refused above.)
                let field = if vertical { "canvas" } else { "vertical" };
                return Err(invalid(format!(
                    "{field}: the live setup has no vertical canvas; add one first (Settings › \
                     Video › Vertical Canvas, or apply {{\"vertical\": {{\"width\": 1080, \
                     \"height\": 1920, \"follow_main\": true}}}})"
                )));
            }
            match params.view {
                ProjectorView::Scene => {
                    let ids: Vec<&str> = setup
                        .map(|s| s.scenes.iter().map(|s| s.id.as_str()).collect())
                        .unwrap_or_default();
                    match params.scene.as_deref() {
                        Some(id) if ids.contains(&id) => {}
                        given => {
                            return Err(invalid(format!(
                                "scene: {} the live setup's scenes: {}",
                                given.map_or("give one of".to_string(), |id| format!(
                                    "{id:?} isn't a scene; give one of"
                                )),
                                if ids.is_empty() {
                                    "(none)".into()
                                } else {
                                    ids.join(", ")
                                }
                            )));
                        }
                    }
                }
                ProjectorView::Source => {
                    let ids: Vec<&str> = setup
                        .map(|s| {
                            s.sources
                                .iter()
                                .filter(|s| s.kind.has_video())
                                .map(|s| s.id.as_str())
                                .collect()
                        })
                        .unwrap_or_default();
                    match params.source.as_deref() {
                        Some(id) if ids.contains(&id) => {}
                        given => {
                            return Err(invalid(format!(
                                "source: {} the live setup's video sources: {}",
                                given.map_or("give one of".to_string(), |id| format!(
                                    "{id:?} isn't a video source; give one of"
                                )),
                                if ids.is_empty() {
                                    "(none)".into()
                                } else {
                                    ids.join(", ")
                                }
                            )));
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(frame) = params.frame
            && !(frame.width > 0.0 && frame.height > 0.0)
        {
            return Err(invalid(
                "frame: width and height must be more than 0 (points)",
            ));
        }
        // Resolved before taking the lock: listing displays asks macOS.
        let display = match params.display.as_deref() {
            Some(query) => Some(find_display(&displays(), query)?.id.clone()),
            None if params.fullscreen == Some(true) => {
                displays().iter().find(|d| d.main).map(|d| d.id.clone())
            }
            None => None,
        };
        // To check a remembered display.
        let attached = displays();
        let mut projector = Projector {
            id: String::new(),
            view: params.view,
            canvas,
            scene: params.scene.clone(),
            source: params.source.clone(),
            layout: (params.view == ProjectorView::Multiview)
                .then(|| params.layout.unwrap_or(MultiviewLayout::Top8)),
            vertical: params.vertical == Some(true),
            canvases: tile_canvases.clone().unwrap_or_default(),
            placement: Placement::default(),
            shown: false,
        };
        let opened = {
            let mut saved = self.projectors.saved.lock();
            // Opened again by hand: no longer waiting for its display.
            saved.waiting.retain(|p| !p.same_view(&projector));
            let remembered = saved.placements.get(&key(&projector)).cloned();
            projector.placement = match (params.fullscreen, display) {
                (Some(true), display) => Placement {
                    fullscreen: true,
                    display,
                    frame: remembered.and_then(|p| p.frame),
                },
                (None, Some(display)) => Placement {
                    fullscreen: true,
                    display: Some(display),
                    frame: remembered.and_then(|p| p.frame),
                },
                (Some(false), display) => Placement {
                    fullscreen: false,
                    display: display
                        .or_else(|| remembered.as_ref().and_then(|p| p.display.clone())),
                    frame: params
                        .frame
                        .or_else(|| remembered.as_ref().and_then(|p| p.frame)),
                },
                (None, None) => {
                    let mut placement = remembered.unwrap_or_default();
                    if params.frame.is_some() {
                        placement.fullscreen = false;
                        placement.frame = params.frame;
                    }
                    // Its display isn't connected: a window, rather than
                    // covering another display.
                    if display_missing(&placement, &attached) {
                        placement.fullscreen = false;
                        placement.display = None;
                    }
                    placement
                }
            };
            // A window last on a display that isn't connected would open
            // off screen: the app places it afresh (#95).
            if params.frame.is_none() && window_display_missing(&projector.placement, &attached) {
                projector.placement.display = None;
                projector.placement.frame = None;
            }
            // A multiview's canvas tiles fit its grid (the one it will
            // have: given, else the open one's).
            if projector.view == ProjectorView::Multiview {
                let open = saved.open.iter().find(|p| p.same_view(&projector));
                let layout = params
                    .layout
                    .or_else(|| open.and_then(|p| p.layout))
                    .unwrap_or(MultiviewLayout::Top8);
                let mut will = projector.clone();
                if let Some(open) = open {
                    if params.vertical.is_none() {
                        will.vertical = open.vertical;
                    }
                    if tile_canvases.is_none() {
                        will.canvases = open.canvases.clone();
                    }
                }
                let most = layout.canvas_tiles();
                let count = will.tile_canvases().len();
                if count > most
                    && (params.layout.is_some()
                        || tile_canvases.is_some()
                        || params.vertical.is_some())
                {
                    return Err(invalid(format!(
                        "canvases: a {} multiview shows at most {most} canvas tile{} (the rest \
                         are scenes), not {count}; give fewer, or layout top-8 for up to {}",
                        layout.name(),
                        if most == 1 { "" } else { "s" },
                        MultiviewLayout::Top8.canvas_tiles()
                    )));
                }
            }
            match saved.open.iter_mut().find(|p| p.same_view(&projector)) {
                Some(open) => {
                    open.placement = projector.placement.clone();
                    if projector.layout.is_some() {
                        open.layout = projector.layout;
                    }
                    if params.vertical.is_some() {
                        open.vertical = projector.vertical;
                    }
                    if tile_canvases.is_some() {
                        open.canvases = projector.canvases.clone();
                    }
                    open.clone()
                }
                None => {
                    saved.next = saved.next.max(1);
                    projector.id = format!("projector-{}", saved.next);
                    saved.next += 1;
                    saved.open.push(projector.clone());
                    projector
                }
            }
        };
        self.projectors_changed(None);
        Ok(opened)
    }

    pub(super) fn projector_close(&self, params: CloseParams) -> Result<Projectors, RpcError> {
        {
            let mut saved = self.projectors.saved.lock();
            let closing: Vec<Projector> = match (&params.id, params.all) {
                (Some(_), true) => {
                    return Err(invalid(
                        "id: leave it out with all = true, or give all = false",
                    ));
                }
                (None, true) => {
                    saved.waiting.clear();
                    std::mem::take(&mut saved.open)
                }
                (Some(id), false) => match saved.open.iter().position(|p| &p.id == id) {
                    Some(index) => vec![saved.open.remove(index)],
                    None => {
                        let ids: Vec<&str> = saved.open.iter().map(|p| p.id.as_str()).collect();
                        return Err(invalid(format!(
                            "id: {id:?} isn't an open projector; open: {}",
                            if ids.is_empty() {
                                "(none)".into()
                            } else {
                                ids.join(", ")
                            }
                        )));
                    }
                },
                (None, false) => {
                    return Err(invalid(
                        "id: give a projector's id (projector_list), or all = true",
                    ));
                }
            };
            for projector in closing {
                saved
                    .placements
                    .insert(key(&projector), projector.placement.clone());
            }
        }
        self.projectors_changed(params.reason);
        Ok(self.projector_list())
    }

    pub(super) fn projector_place(&self, params: PlaceParams) -> Result<Projector, RpcError> {
        if let Some(frame) = params.placement.frame
            && !(frame.width > 0.0 && frame.height > 0.0)
        {
            return Err(invalid(
                "placement.frame: width and height must be more than 0 (points)",
            ));
        }
        let (projector, changed) = {
            let mut saved = self.projectors.saved.lock();
            let ids: Vec<String> = saved.open.iter().map(|p| p.id.clone()).collect();
            let Some(open) = saved.open.iter_mut().find(|p| p.id == params.id) else {
                return Err(invalid(format!(
                    "id: {:?} isn't an open projector; open: {}",
                    params.id,
                    if ids.is_empty() {
                        "(none)".into()
                    } else {
                        ids.join(", ")
                    }
                )));
            };
            let changed = open.placement != params.placement || open.shown != params.shown;
            open.placement = params.placement;
            open.shown = params.shown;
            let open = open.clone();
            saved.placements.insert(key(&open), open.placement.clone());
            (open, changed)
        };
        if changed {
            self.projectors_changed(None);
        }
        Ok(projector)
    }

    /// A display came: projectors waiting for it open again.
    pub(super) fn projectors_display_came(&self) {
        self.projectors_waiting(&displays());
    }

    /// A display went away: full-screen projectors on it close.
    pub(super) fn projectors_display_gone(&self, id: &str, name: &str) {
        let closing: Vec<String> = self
            .projectors
            .saved
            .lock()
            .open
            .iter()
            .filter(|p| p.placement.fullscreen && p.placement.display.as_deref() == Some(id))
            .map(|p| p.id.clone())
            .collect();
        for projector in closing {
            let _ = self.projector_close(CloseParams {
                id: Some(projector),
                all: false,
                reason: Some(format!("the display {name} was disconnected")),
            });
        }
    }

    /// Save the projectors and tell watchers.
    fn projectors_changed(&self, reason: Option<String>) {
        let projectors = {
            let saved = self.projectors.saved.lock();
            ProjectorState::save(&saved);
            saved.open.clone()
        };
        self.publish(Event::Projectors { projectors, reason });
    }
}
