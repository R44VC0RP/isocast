//! Virtual camera settings. The selection belongs to the scene collection,
//! because scene/source ids have meaning only in that collection.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Issue, Setup};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VirtualCameraSettings {
    /// Program, studio preview, the vertical program, or an explicit
    /// scene/source id.
    #[serde(default)]
    pub target: Target,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scene: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The extra canvas (a `[[canvases]]` id, or `vertical`), for target
    /// = canvas (#120).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canvas: Option<String>,
    /// Output dimensions; both absent uses the canvas (the vertical
    /// canvas's for target = vertical, the extra canvas's for target =
    /// canvas). Other aspect ratios
    /// are letterboxed. These are the camera extension's advertised modes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    #[default]
    Program,
    Preview,
    Scene,
    Source,
    /// The vertical canvas's program (`[vertical]` must be on).
    Vertical,
    /// An extra canvas's program (`canvas`: a `[[canvases]]` id, #120).
    Canvas,
}

/// Keep in the same order as camera-extension/Camera.swift.
pub const SIZES: [(u32, u32); 6] = [
    (1920, 1080),
    (1280, 720),
    (2560, 1440),
    (3840, 2160),
    (1080, 1920),
    (720, 1280),
];

impl VirtualCameraSettings {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }

    /// The extra canvas whose program the camera shows: the vertical one
    /// for target = vertical, `canvas` for target = canvas; None for the
    /// others (and an invalid `canvas`, which `issues` reports).
    pub fn target_canvas(&self) -> Option<crate::CanvasId> {
        match self.target {
            Target::Vertical => Some(crate::CanvasId::Vertical),
            Target::Canvas => self
                .canvas
                .as_deref()
                .and_then(|id| crate::CanvasId::parse(id).ok())
                .filter(|c| !c.is_main()),
            _ => None,
        }
    }

    /// The canvas the camera's picture comes from: the extra one for
    /// target = vertical or canvas (the main one while the setup lacks
    /// it, which `issues` reports), else the main one.
    fn canvas(&self, setup: &Setup) -> crate::Canvas {
        match self.target_canvas().and_then(|c| setup.extra_canvas(c)) {
            Some(extra) => extra.canvas(&setup.canvas),
            None => setup.canvas.clone(),
        }
    }

    pub fn size(&self, setup: &Setup) -> (u32, u32) {
        let canvas = self.canvas(setup);
        (
            self.width.unwrap_or(canvas.width),
            self.height.unwrap_or(canvas.height),
        )
    }

    /// The output's frame rate: its canvas's.
    pub fn fps(&self, setup: &Setup) -> u32 {
        self.canvas(setup).fps
    }

    pub fn issues(&self, setup: &Setup) -> Vec<Issue> {
        let mut issues = Vec::new();
        let mut add = |path: &str, message: String| {
            issues.push(Issue {
                path: format!("virtual_camera.{path}"),
                message,
            })
        };
        if self.target == Target::Scene
            && !setup
                .scenes
                .iter()
                .any(|s| Some(&s.id) == self.scene.as_ref())
        {
            add(
                "scene",
                format!(
                    "give a scene id: {}",
                    setup
                        .scenes
                        .iter()
                        .map(|s| s.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
        }
        if self.target == Target::Source
            && !setup
                .sources
                .iter()
                .any(|s| Some(&s.id) == self.source.as_ref() && s.kind.has_video())
        {
            add(
                "source",
                format!(
                    "give a video source id: {}",
                    setup
                        .sources
                        .iter()
                        .filter(|s| s.kind.has_video())
                        .map(|s| s.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
        }
        if self.target == Target::Vertical && setup.vertical.is_none() {
            add(
                "target",
                "vertical needs the vertical canvas; turn it on ([vertical]) or give program, \
                 preview, scene or source"
                    .into(),
            );
        }
        if self.target == Target::Canvas {
            let extras: Vec<String> = setup
                .extra_canvas_ids()
                .iter()
                .map(|c| c.to_string())
                .collect();
            let listed = if extras.is_empty() {
                "(none; add one in Settings › Video › Canvases)".to_string()
            } else {
                extras.join(", ")
            };
            match self.target_canvas() {
                Some(canvas) if setup.has_canvas(canvas) => {}
                Some(canvas) => add(
                    "canvas",
                    format!("{canvas:?} isn't an extra canvas of the setup; give one of: {listed}"),
                ),
                None => add(
                    "canvas",
                    format!("give an extra canvas's id for target = canvas: {listed}"),
                ),
            }
        }
        if self.target != Target::Canvas && self.canvas.is_some() {
            add(
                "canvas",
                "only valid with target = canvas; otherwise omit canvas".into(),
            );
        }
        if self.target != Target::Scene && self.scene.is_some() {
            add(
                "scene",
                "only valid with target = scene; otherwise omit scene".into(),
            );
        }
        if self.target != Target::Source && self.source.is_some() {
            add(
                "source",
                "only valid with target = source; otherwise omit source".into(),
            );
        }
        if self.width.is_some() != self.height.is_some() {
            add(
                if self.width.is_none() {
                    "width"
                } else {
                    "height"
                },
                "give width and height together, or omit both for the canvas size (the extra \
                 canvas's for target = vertical or canvas)"
                    .into(),
            );
        } else if self.width.is_some() && !SIZES.contains(&self.size(setup)) {
            add("width", size_error());
        }
        issues
    }

    /// The default remains valid in any setup, but a camera can only start
    /// in a mode it advertises to applications.
    pub fn check_start(&self, setup: &Setup, flexible_canvas: bool) -> Result<(), String> {
        let issues = self.issues(setup);
        if !issues.is_empty() {
            return Err(issues
                .iter()
                .map(|i| format!("{}: {}", i.path, i.message))
                .collect::<Vec<_>>()
                .join("; "));
        }
        if !flexible_canvas && !SIZES.contains(&self.size(setup)) {
            return Err(format!(
                "virtual_camera.width: {}; set virtual_camera.width and height",
                size_error()
            ));
        }
        Ok(())
    }
}

fn size_error() -> String {
    format!(
        "give an advertised size: {}",
        SIZES
            .iter()
            .map(|(w, h)| format!("{w}x{h}"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VirtualCameraState {
    #[default]
    Stopped,
    Starting,
    Running,
    Stopping,
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct VirtualCameraStatus {
    pub state: VirtualCameraState,
    pub settings: VirtualCameraSettings,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// cmio-iosurface, or diagnostic-iosurface (explicit development probe).
    pub transport: String,
    /// Name and node of the opened camera (Linux), not a guessed device name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
    pub frames_sent: u64,
    /// Accepted re-sends of the last converted picture (included in frames_sent).
    #[serde(default)]
    pub frames_repeated: u64,
    pub frames_dropped: u64,
    pub last_error: Option<String>,
    /// Why this engine cannot start the camera, including any installation step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
    /// Live governor limit below the requested rate. Zero means hold the last delivered picture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub governor_fps: Option<u32>,
    /// Host-clock picture timestamp to sink enqueue, not camera-client latency.
    pub enqueue_latency_us: crate::api::Percentiles,
    /// This engine carries the camera extension (the release app), so
    /// `virtual_camera_remove` can take it out of macOS.
    #[serde(default)]
    pub removable: bool,
}

/// What `virtual_camera_remove` did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VirtualCameraRemoval {
    /// macOS finishes removing it when the Mac restarts.
    pub restart_needed: bool,
    /// What happened, in words for the user.
    pub message: String,
}
