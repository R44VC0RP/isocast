//! The suite format: interactions to replay and what counts as smooth.
//! See docs/motion-testing.md for the reference; a suite is TOML:
//!
//! ```toml
//! [defaults]
//! size = "1280x800"
//! variants = ["idle", "recording"]
//! [defaults.thresholds]
//! gap_pt_p95 = 2.0
//!
//! [[scenario]]
//! name = "splitter-scenes"
//! target = "splitter:0"
//! steps = [
//!   { press = "target" },
//!   { move = [160, 0], ms = 800 },
//!   { release = true },
//! ]
//! ```

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Suite {
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default, rename = "scenario")]
    pub scenarios: Vec<Scenario>,
}

/// What every scenario gets unless it says otherwise.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Defaults {
    /// The window, in points: "WIDTHxHEIGHT".
    pub size: String,
    /// Pixels per point.
    pub scale: f32,
    /// Frames a second the virtual display runs at.
    pub fps: u32,
    /// `engine`: the daemon's frames (the real path); `test`: the
    /// offscreen test picture, drawn from the box itself (no engine lag).
    pub picture: String,
    /// `lockstep`: each frame waits for the engine to catch up (bounded),
    /// so results don't depend on the machine's speed (CI); `realtime`:
    /// frames are paced on the wall clock and the engine runs as it does
    /// (lag the user would see on this machine).
    pub clock: String,
    /// `idle`, `recording` (program and ISO files of the synthetic
    /// sources, through the Controls dock).
    pub variants: Vec<String>,
    pub thresholds: Thresholds,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            size: "1280x800".into(),
            scale: 1.0,
            fps: 60,
            picture: "engine".into(),
            clock: "realtime".into(),
            variants: vec!["idle".into(), "recording".into()],
            thresholds: Thresholds::default(),
        }
    }
}

/// Limits; a missing one isn't checked.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Thresholds {
    /// The pointer-to-edge gap on screen (window points), 95th percentile.
    pub gap_pt_p95: Option<f64>,
    /// The largest gap on screen (window points).
    pub gap_pt_max: Option<f64>,
    /// How many frames the edge on screen trails the pointer.
    pub lag_frames: Option<f64>,
    /// How far the gain (edge motion per point of pointer motion, with the
    /// lag taken out) may be from 1.
    pub gain_tolerance: Option<f64>,
    /// Frames where the edge moved against the pointer.
    pub jump_back_frames: Option<u64>,
    /// Frames where the pointer moved and the edge on screen didn't
    /// (beyond the measured lag).
    pub stall_frames: Option<u64>,
    /// Frames whose picture (or window) is blank.
    pub black_frames: Option<u64>,
    /// Places that changed and changed back (A-B-A) while the pointer went
    /// one way.
    pub flicker_frames: Option<u64>,
    /// The longest frame, finished: the interface's work, the GPU's
    /// drawing and the read back (ms). Wall time: CI runs it with
    /// `--no-timing`.
    pub frame_ms_max: Option<f64>,
    /// Frames over the frame budget (1000/fps ms).
    pub late_frames: Option<u64>,
}

impl Thresholds {
    fn or(&self, base: &Thresholds) -> Thresholds {
        Thresholds {
            gap_pt_p95: self.gap_pt_p95.or(base.gap_pt_p95),
            gap_pt_max: self.gap_pt_max.or(base.gap_pt_max),
            lag_frames: self.lag_frames.or(base.lag_frames),
            gain_tolerance: self.gain_tolerance.or(base.gain_tolerance),
            jump_back_frames: self.jump_back_frames.or(base.jump_back_frames),
            stall_frames: self.stall_frames.or(base.stall_frames),
            black_frames: self.black_frames.or(base.black_frames),
            flicker_frames: self.flicker_frames.or(base.flicker_frames),
            frame_ms_max: self.frame_ms_max.or(base.frame_ms_max),
            late_frames: self.late_frames.or(base.late_frames),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// What is dragged and tracked: `splitter:N` (the Nth column
    /// splitter, left to right), `row-splitter` (above the docks),
    /// `canvas-corner` (the selected item's bottom-right handle),
    /// `canvas-move` (the selected item, by its middle), `none`.
    pub target: String,
    /// `--state` steps run before (see `isocast-ui --help`).
    #[serde(default)]
    pub before: Vec<String>,
    pub steps: Vec<Step>,
    pub size: Option<String>,
    pub scale: Option<f32>,
    pub picture: Option<String>,
    pub clock: Option<String>,
    pub variants: Option<Vec<String>>,
    #[serde(default)]
    pub thresholds: Thresholds,
    /// A fault to put in (see runner.rs Inject): `lag:N`, `jump:F`,
    /// `blank:F`, comma-separated.
    pub inject: Option<String>,
    /// For controls: `pass` or `fail`. The run is then judged on whether
    /// its verdict was the expected one.
    pub expect: Option<String>,
    /// For a run expected to fail: exactly the checks it must fail (so a
    /// control fails for the right reason, and a known issue in the
    /// standard suite is caught if it gets worse or is fixed).
    #[serde(default)]
    pub expect_failing: Vec<String>,
}

/// One step. Exactly one of `press`, `move`, `release`, `wait`, `wheel`,
/// `key`, `state`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// Press the left button: at `"target"` (its grab point) or `[x, y]`.
    pub press: Option<toml::Value>,
    /// Move the pointer by `[dx, dy]` points over `ms`.
    #[serde(rename = "move")]
    pub move_by: Option<[f32; 2]>,
    /// Move the pointer to `[x, y]` over `ms`.
    pub to: Option<[f32; 2]>,
    pub release: Option<bool>,
    /// Hold still for `ms`.
    pub wait: Option<bool>,
    /// Scroll by `[dx, dy]` points over `ms`.
    pub wheel: Option<[f32; 2]>,
    pub key: Option<String>,
    pub state: Option<String>,
    /// How long the step takes.
    #[serde(default)]
    pub ms: f64,
    /// Pointer events a second while moving (a mouse reports 60–1000).
    pub hz: Option<f64>,
    /// `linear`, `ease-in-out`, `ease-out`.
    pub easing: Option<String>,
}

/// A scenario with the defaults filled in, for one variant.
#[derive(Debug, Clone)]
pub struct Run {
    pub name: String,
    pub description: String,
    pub variant: String,
    pub target: String,
    pub before: Vec<String>,
    pub steps: Vec<Step>,
    pub size: (u32, u32),
    pub scale: f32,
    pub fps: u32,
    pub engine_picture: bool,
    pub lockstep: bool,
    pub thresholds: Thresholds,
    pub inject: Option<String>,
    pub expect: Option<String>,
    pub expect_failing: Vec<String>,
}

impl Run {
    pub fn id(&self) -> String {
        format!("{}@{}", self.name, self.variant)
    }
}

pub fn load(path: &Path) -> Result<Suite> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("can't read {}", path.display()))?;
    let suite: Suite = toml::from_str(&text)
        .with_context(|| format!("{} isn't a motion suite", path.display()))?;
    for (i, s) in suite.scenarios.iter().enumerate() {
        // The name names the run's folder: plain names only.
        if !safe_name(&s.name) {
            bail!(
                "scenario[{i}].name {:?}: letters, digits, '-' and '_' only (1-64), not starting with '-'",
                s.name
            );
        }
        for state in s
            .before
            .iter()
            .chain(s.steps.iter().filter_map(|st| st.state.as_ref()))
        {
            check_state(state).with_context(|| format!("scenario[{i}] ({})", s.name))?;
        }
        for (j, step) in s.steps.iter().enumerate() {
            let kinds = [
                step.press.is_some(),
                step.move_by.is_some(),
                step.to.is_some(),
                step.release.is_some(),
                step.wait.is_some(),
                step.wheel.is_some(),
                step.key.is_some(),
                step.state.is_some(),
            ];
            if kinds.iter().filter(|k| **k).count() != 1 {
                bail!(
                    "scenario[{i}].steps[{j}] ({}): give exactly one of press, move, to, release, wait, wheel, key, state",
                    s.name
                );
            }
            if let Some(easing) = &step.easing
                && !["linear", "ease-in-out", "ease-out"].contains(&easing.as_str())
            {
                bail!(
                    "scenario[{i}].steps[{j}].easing: {easing:?} isn't one of linear, ease-in-out, ease-out"
                );
            }
        }
    }
    Ok(suite)
}

/// The runs `suite` asks for: every scenario (or the one named) in every
/// variant (or the one named).
pub fn runs(suite: &Suite, only: Option<&str>, variant: Option<&str>) -> Result<Vec<Run>> {
    let d = &suite.defaults;
    let mut out = Vec::new();
    for s in &suite.scenarios {
        if only.is_some_and(|n| n != s.name) {
            continue;
        }
        let size = s.size.as_deref().unwrap_or(&d.size);
        let size = size
            .split_once('x')
            .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
            .with_context(|| {
                format!("scenario {}: size takes WIDTHxHEIGHT, not {size:?}", s.name)
            })?;
        let picture = s.picture.as_deref().unwrap_or(&d.picture);
        let clock = s.clock.as_deref().unwrap_or(&d.clock);
        if !["engine", "test"].contains(&picture) {
            bail!(
                "scenario {}: picture {picture:?} isn't one of engine, test",
                s.name
            );
        }
        if let Some(e) = &s.expect
            && !["pass", "fail"].contains(&e.as_str())
        {
            bail!("scenario {}: expect {e:?} isn't one of pass, fail", s.name);
        }
        if !["lockstep", "realtime"].contains(&clock) {
            bail!(
                "scenario {}: clock {clock:?} isn't one of lockstep, realtime",
                s.name
            );
        }
        for v in s.variants.as_ref().unwrap_or(&d.variants) {
            if !["idle", "recording"].contains(&v.as_str()) {
                bail!(
                    "scenario {}: variant {v:?} isn't one of idle, recording",
                    s.name
                );
            }
            if variant.is_some_and(|only| only != v) {
                continue;
            }
            out.push(Run {
                name: s.name.clone(),
                description: s.description.clone(),
                variant: v.clone(),
                target: s.target.clone(),
                before: s.before.clone(),
                steps: s.steps.clone(),
                size,
                scale: s.scale.unwrap_or(d.scale),
                fps: d.fps.max(1),
                engine_picture: picture == "engine",
                lockstep: clock == "lockstep",
                thresholds: s.thresholds.or(&d.thresholds),
                inject: s.inject.clone(),
                expect: s.expect.clone(),
                expect_failing: s.expect_failing.clone(),
            });
        }
    }
    if out.is_empty() {
        let names: Vec<&str> = suite.scenarios.iter().map(|s| s.name.as_str()).collect();
        bail!(
            "no scenario to run; --scenario takes one of {}",
            names.join(", ")
        );
    }
    Ok(out)
}

/// A scenario name that is safe as a folder name: no separators, dots or
/// leading dash, so a run's folder is always a child of the output folder.
pub fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The `--state` steps a suite may use: the harness's own, and the app's
/// that only change what the window shows or the isolated engine's live
/// setup. Not the ones that read or write files elsewhere (library
/// import/export, opening a setup, OBS import, bug reports, Create
/// exports), go online (streaming) or change settings (a setting could
/// move the recordings folder).
pub const ALLOWED_STATES: &[&str] = &[
    "select-first-item",
    "snap",
    "hide",
    "box",
    "canvas-hover",
    "canvas-drag",
    "canvas-hold",
    "preview-scale",
    "studio",
    "record",
    "stop-record",
    "pause",
    "resume",
    "toasts",
    "toast-drops",
    "transition",
    "details",
    "mixer-menu",
    "mixer-choose",
    "mixer-mute",
    "about",
    "select",
    "scene",
    "filter-menu",
    "add-menu",
    "anchor",
];

pub fn check_state(state: &str) -> Result<()> {
    let name = state.split_once(':').map_or(state, |(n, _)| n);
    if !ALLOWED_STATES.contains(&name) {
        bail!(
            "state {state:?} isn't allowed in a motion suite; one of {}",
            ALLOWED_STATES.join(", ")
        );
    }
    Ok(())
}

/// Where the pointer is `t` (0–1) of the way through a move.
pub fn ease(easing: Option<&str>, t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    match easing.unwrap_or("linear") {
        "ease-in-out" => 0.5 - 0.5 * (std::f64::consts::PI * t).cos(),
        "ease-out" => 1.0 - (1.0 - t) * (1.0 - t),
        _ => t,
    }
}
