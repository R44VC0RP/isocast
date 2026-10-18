//! Projectors (G.4, #19): the program, studio mode's preview, a scene, a
//! source or a multiview grid shown full screen on a display or in a
//! window of their own, as OBS has them. The daemon keeps the list (and
//! remembers where each was last placed); the app shows each in a window
//! and reports where it is. `projector_open`, `projector_close`,
//! `projector_list` and `projector_place` are on the one method table, so
//! the CLI and agents open them as the app does.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::devices::Display;

/// The main canvas (`canvas`, the default).
pub const MAIN_CANVAS: &str = "main";
/// The setup's vertical canvas (#86, #96): its program, its own preview
/// and its arrangement of a scene. Any other extra canvas (`[[canvases]]`,
/// #120) is named by its id the same way.
pub const VERTICAL_CANVAS: &str = "vertical";

/// What a projector shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProjectorView {
    /// What is on air.
    #[default]
    Program,
    /// Studio mode's preview scene (empty while nothing is staged).
    Preview,
    /// One scene (`scene`), whether or not it is on air.
    Scene,
    /// One video source (`source`), as captured.
    Source,
    /// A grid of the preview, the program and the scenes, with labels and
    /// tally (`layout`).
    Multiview,
}

impl ProjectorView {
    pub const NAMES: [&str; 5] = ["program", "preview", "scene", "source", "multiview"];
}

/// How a multiview grid is laid out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum MultiviewLayout {
    /// The preview and the program side by side across the top half, and
    /// the first eight scenes in two rows of four below (OBS's default).
    #[default]
    #[serde(rename = "top-8")]
    Top8,
    /// Three by three: the preview, the program and the first seven
    /// scenes.
    #[serde(rename = "3x3")]
    Grid3,
    /// Two by two: the preview, the program and the first two scenes.
    #[serde(rename = "2x2")]
    Grid2,
}

impl MultiviewLayout {
    pub const NAMES: [&str; 3] = ["top-8", "3x3", "2x2"];

    pub fn name(self) -> &'static str {
        match self {
            Self::Top8 => "top-8",
            Self::Grid3 => "3x3",
            Self::Grid2 => "2x2",
        }
    }

    /// How many scenes it shows besides the preview and the program.
    pub fn scenes(self) -> usize {
        match self {
            Self::Top8 => 8,
            Self::Grid3 => 7,
            Self::Grid2 => 2,
        }
    }

    /// The most extra canvases' programs it shows in place of scenes
    /// (#120): half its scene tiles, at least one.
    pub fn canvas_tiles(self) -> usize {
        (self.scenes() / 2).max(1)
    }

    /// Where each tile goes, as fractions of the grid (x, y, width,
    /// height): the preview, the program, then the scenes in order.
    pub fn tiles(self) -> Vec<[f32; 4]> {
        let grid = |columns: usize, rows: usize, top: f32| {
            let height = (1.0 - top) / rows as f32;
            let width = 1.0 / columns as f32;
            (0..rows * columns)
                .map(move |i| {
                    let (column, row) = (i % columns, i / columns);
                    [
                        column as f32 * width,
                        top + row as f32 * height,
                        width,
                        height,
                    ]
                })
                .collect::<Vec<_>>()
        };
        match self {
            Self::Top8 => {
                let mut tiles = vec![[0.0, 0.0, 0.5, 0.5], [0.5, 0.0, 0.5, 0.5]];
                tiles.extend(grid(4, 2, 0.5));
                tiles
            }
            Self::Grid3 => grid(3, 3, 0.0),
            Self::Grid2 => grid(2, 2, 0.0),
        }
    }
}

/// A window's place on the desktop, in points from the top left of the
/// main display (macOS's global coordinates, y down).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Frame {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Where a projector is shown.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Placement {
    /// Full screen on `display` (else in a window).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fullscreen: bool,
    /// The display's id (`isocast devices`): the one it fills when full
    /// screen, else the one its window was last on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    /// The window's place, when not full screen (the app picks one if
    /// absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<Frame>,
}

/// An open projector.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Projector {
    /// Stable while it is open (`projector-1`, …).
    pub id: String,
    pub view: ProjectorView,
    /// The canvas it shows: `main`, `vertical` (the setup's vertical
    /// canvas) or a `[[canvases]]` id (#120): its program, preview or
    /// scene.
    pub canvas: String,
    /// The scene, for `view = scene`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scene: Option<String>,
    /// The video source, for `view = source`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The grid, for `view = multiview`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<MultiviewLayout>,
    /// For `view = multiview`: its last tile shows the vertical canvas's
    /// program instead of a scene.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub vertical: bool,
    /// For `view = multiview`: extra canvases (ids) whose programs fill
    /// its last tiles instead of scenes, in order (#120); with
    /// `vertical`, the vertical canvas's comes first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub canvases: Vec<String>,
    #[serde(default)]
    pub placement: Placement,
    /// An app window shows it now (as the app last said). False while no
    /// app runs: the app opens it when it starts.
    #[serde(default)]
    pub shown: bool,
}

impl Projector {
    /// The `shared_frames` target its picture comes from (none for a
    /// multiview, whose tiles each have their own).
    pub fn target(&self) -> Option<String> {
        let canvas = self.canvas_id();
        match self.view {
            ProjectorView::Program => Some(canvas.program_target()),
            ProjectorView::Preview => Some(canvas.preview_target()),
            ProjectorView::Scene => self
                .scene
                .as_ref()
                .map(|id| format!("{}{id}", canvas.scene_prefix())),
            ProjectorView::Source => self.source.clone(),
            ProjectorView::Multiview => None,
        }
    }

    /// The canvas it shows (main if its `canvas` isn't a canvas id).
    pub fn canvas_id(&self) -> crate::CanvasId {
        crate::CanvasId::parse(&self.canvas).unwrap_or_default()
    }

    /// Whether it shows the vertical canvas.
    pub fn is_vertical(&self) -> bool {
        self.canvas == VERTICAL_CANVAS
    }

    /// For a multiview: the extra canvases whose programs fill its last
    /// tiles, in order (the vertical one first with `vertical`), each once.
    pub fn tile_canvases(&self) -> Vec<crate::CanvasId> {
        let mut out: Vec<crate::CanvasId> = Vec::new();
        let ids = self
            .vertical
            .then_some(VERTICAL_CANVAS)
            .into_iter()
            .chain(self.canvases.iter().map(String::as_str));
        for id in ids {
            if let Ok(canvas) = crate::CanvasId::parse(id)
                && !canvas.is_main()
                && !out.contains(&canvas)
            {
                out.push(canvas);
            }
        }
        out
    }

    /// Whether `other` shows the same picture (the same view of the same
    /// scene or source on the same canvas): opening it again moves this
    /// one rather than adding another.
    pub fn same_view(&self, other: &Projector) -> bool {
        self.view == other.view
            && self.canvas == other.canvas
            && self.scene == other.scene
            && self.source == other.source
    }
}

/// The `shared_frames` (and `frame`, `frames`) target of scene `id`.
pub fn scene_target(id: &str) -> String {
    format!("scene:{id}")
}

/// The `shared_frames` target of the vertical canvas's own preview.
pub const VERTICAL_PREVIEW: &str = "vertical-preview";

/// The prefix of a vertical scene target (see [`vertical_scene_target`]).
pub const VERTICAL_SCENE_PREFIX: &str = "vertical-scene:";

/// The `shared_frames` target of scene `id` as the vertical canvas
/// arranges it (its `vertical` layout, else its items placed again).
pub fn vertical_scene_target(id: &str) -> String {
    format!("{VERTICAL_SCENE_PREFIX}{id}")
}

/// `projector_open`: what to show, and where.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OpenParams {
    pub view: ProjectorView,
    /// The scene's id, for `view = scene`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scene: Option<String>,
    /// The video source's id, for `view = source`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The canvas: `main` (the default), `vertical` (the live setup's
    /// vertical canvas) or one of its `[[canvases]]` ids (#120); views
    /// program, preview and scene.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canvas: Option<String>,
    /// The grid, for `view = multiview`: `top-8` (the default), `3x3` or
    /// `2x2`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<MultiviewLayout>,
    /// For `view = multiview`: show the vertical canvas's program in the
    /// grid's last tile, in place of a scene (the live setup needs a
    /// vertical canvas). Absent: as the open multiview has it, else no.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vertical: Option<bool>,
    /// For `view = multiview`: extra canvases (`vertical` or
    /// `[[canvases]]` ids) whose programs fill the grid's last tiles in
    /// place of scenes, in order (#120); at most half the grid's scene
    /// tiles (one in `2x2`). Absent: as the open multiview has it, else
    /// none; empty: none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canvases: Option<Vec<String>>,
    /// `true`: full screen on `display` (the main display if absent);
    /// `false`: in a window. Absent: where the same view was last (a
    /// window the first time), or full screen if `display` is given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fullscreen: Option<bool>,
    /// `main`, a display's id or its name (`isocast devices`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    /// The window's place (not full screen). Absent: where the same view
    /// was last, else the app picks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<Frame>,
}

/// `projector_close`: one projector, or all.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CloseParams {
    /// The projector's id (`projector_list`); absent with `all`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Close every open projector.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub all: bool,
    /// Why, for the `projectors` event (the app says "the display was
    /// disconnected").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `projector_place`: what the app showing a projector reports (where its
/// window is, and whether it shows it), so it opens there next time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlaceParams {
    pub id: String,
    pub placement: Placement,
    #[serde(default = "yes")]
    pub shown: bool,
}

fn yes() -> bool {
    true
}

/// `projector_list` (and what `projector_close` returns).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Projectors {
    pub projectors: Vec<Projector>,
    /// The displays a projector can fill.
    #[serde(default)]
    pub displays: Vec<Display>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_tile_the_grid_without_overlap() {
        for layout in [
            MultiviewLayout::Top8,
            MultiviewLayout::Grid3,
            MultiviewLayout::Grid2,
        ] {
            let tiles = layout.tiles();
            assert_eq!(tiles.len(), layout.scenes() + 2, "{}", layout.name());
            let area: f32 = tiles.iter().map(|t| t[2] * t[3]).sum();
            assert!((area - 1.0).abs() < 1e-4, "{}: {area}", layout.name());
        }
        let names: Vec<String> = MultiviewLayout::NAMES
            .iter()
            .map(|n| serde_json::from_value::<MultiviewLayout>(serde_json::json!(n)).unwrap())
            .map(|l| l.name().to_string())
            .collect();
        assert_eq!(names, MultiviewLayout::NAMES);
    }
}
