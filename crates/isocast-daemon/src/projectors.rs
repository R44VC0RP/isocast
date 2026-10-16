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
use isocast_protocol::devices::Display;
use isocast_protocol::projector::{
    CloseParams, MAIN_CANVAS, MultiviewLayout, OpenParams, PlaceParams, Placement, Projector,
    ProjectorView, Projectors,
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
}

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
    /// A full-screen one whose display isn't connected now stays closed
    /// (its placement is remembered), as it would have closed when its
    /// display went.
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
        if saved.open.iter().any(|p| p.placement.fullscreen) {
            let displays = displays();
            let (gone, open): (Vec<Projector>, Vec<Projector>) = std::mem::take(&mut saved.open)
                .into_iter()
                .partition(|p| display_missing(&p.placement, &displays));
            saved.open = open;
            for projector in gone {
                isocast_diagnostics::diagnostic!("info";
                    "isocastd: projector {} stays closed: its display {} isn't connected",
                    projector.id,
                    projector.placement.display.as_deref().unwrap_or_default()
                );
                saved
                    .placements
                    .insert(key(&projector), projector.placement.clone());
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
        Projectors {
            projectors: self.projectors.saved.lock().open.clone(),
            displays: displays(),
        }
    }

    pub(super) fn projector_open(&self, params: OpenParams) -> Result<Projector, RpcError> {
        let canvas = params.canvas.clone().unwrap_or_else(|| MAIN_CANVAS.into());
        if canvas != MAIN_CANVAS {
            return Err(invalid(format!(
                "canvas: {canvas:?} isn't a canvas; canvases: {MAIN_CANVAS}"
            )));
        }
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
        }
        {
            let state = self.state.lock();
            let setup = state.setup.as_ref();
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
        // To check a remembered full-screen display.
        let attached = (params.fullscreen.is_none() && display.is_none()).then(displays);
        let mut projector = Projector {
            id: String::new(),
            view: params.view,
            canvas,
            scene: params.scene.clone(),
            source: params.source.clone(),
            layout: (params.view == ProjectorView::Multiview)
                .then(|| params.layout.unwrap_or(MultiviewLayout::Top8)),
            placement: Placement::default(),
            shown: false,
        };
        let opened = {
            let mut saved = self.projectors.saved.lock();
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
                    if display_missing(&placement, attached.as_deref().unwrap_or_default()) {
                        placement.fullscreen = false;
                        placement.display = None;
                    }
                    placement
                }
            };
            match saved.open.iter_mut().find(|p| p.same_view(&projector)) {
                Some(open) => {
                    open.placement = projector.placement.clone();
                    if projector.layout.is_some() {
                        open.layout = projector.layout;
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
                (None, true) => std::mem::take(&mut saved.open),
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
