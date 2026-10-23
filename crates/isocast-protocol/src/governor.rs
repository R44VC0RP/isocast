//! Intended automatic-quality policy, and its separate runtime overrides.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GovernorSettings {
    #[serde(default = "enabled")]
    pub enabled: bool,
    /// The user's explicit choice to let the governor skip ISO pictures
    /// under pressure (off by default). Skipped pictures are lost from the
    /// file for good. Off, every ISO counts as pinned; the native encoder
    /// safety controller still sheds ISO frames on real overload.
    #[serde(default)]
    pub allow_iso_thinning: bool,
    /// Settings the governor must not override. Native overload protection
    /// still applies; a pin cannot guarantee that hardware delivers frames.
    #[serde(default)]
    pub pins: Vec<String>,
}

fn enabled() -> bool {
    true
}

impl Default for GovernorSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_iso_thinning: false,
            pins: Vec::new(),
        }
    }
}

impl GovernorSettings {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
    pub fn pinned(&self, field: &str) -> bool {
        self.pins.iter().any(|p| p == field)
    }
    /// Whether the governor must leave this ISO's admission alone: always,
    /// unless the user allowed ISO frame thinning and didn't pin it.
    pub fn iso_pinned(&self, source: &str) -> bool {
        !self.allow_iso_thinning
            || self.pinned("recording.iso.fps")
            || self.pinned(&format!("recording.iso.sources.{source}.fps"))
    }
    /// The legacy vertical pin protects every secondary canvas. A named
    /// canvas pin protects just that canvas; main is never governor-thinned.
    pub fn secondary_pinned(&self, canvas: crate::CanvasId) -> bool {
        canvas.is_main()
            || self.pinned("vertical.fps")
            || self.pinned(&format!("canvases.{canvas}.fps"))
    }
    pub fn valid_pin(pin: &str) -> bool {
        matches!(
            pin,
            "preview.fps" | "recording.iso.fps" | "vertical.fps" | "stream.video_bitrate_kbps"
        ) || pin
            .strip_prefix("canvases.")
            .and_then(|p| p.strip_suffix(".fps"))
            .is_some_and(|id| {
                crate::CanvasId::parse(id)
                    .is_ok_and(|canvas| !canvas.is_main() && canvas.as_str() == id)
            })
            || pin.len() <= 256
                && pin
                    .strip_prefix("recording.iso.sources.")
                    .and_then(|p| p.strip_suffix(".fps"))
                    .is_some_and(|id| !id.is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct GovernorStatus {
    pub enabled: bool,
    pub bottleneck: Option<String>,
    pub reason: String,
    /// Actual policy changes, not polls or ordinary native ISO shedding.
    pub steps: u64,
    pub preview_fps: Option<u32>,
    /// Governor ceilings. Pinned sources retain 100%; native pressure may
    /// admit fewer pictures independently. Audio and file clocks do not change.
    pub iso_percent: u32,
    /// Ceiling for unpinned secondary canvases. Pinned canvases retain 100%.
    pub vertical_percent: u32,
    /// Lowest acknowledged percentage among active stream hubs, not a
    /// canvas-wide last-writer value; each destination has its own target.
    pub stream_bitrate_percent: u32,
    pub native_iso_percent: u32,
    pub recovery_seconds: Option<f64>,
    pub limited_by_pins: bool,
    /// A failed optional retune, including a failed restoration while off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Default for GovernorStatus {
    fn default() -> Self {
        Self {
            enabled: true,
            bottleneck: None,
            reason: "At requested quality".into(),
            steps: 0,
            preview_fps: None,
            iso_percent: 100,
            vertical_percent: 100,
            stream_bitrate_percent: 100,
            native_iso_percent: 100,
            recovery_seconds: None,
            limited_by_pins: false,
            error: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CanvasId;

    #[test]
    fn named_secondary_pin_does_not_pin_its_sibling() {
        let shorts = CanvasId::parse("shorts").unwrap();
        let square = CanvasId::parse("square").unwrap();
        let mut settings = GovernorSettings {
            pins: vec!["canvases.shorts.fps".into()],
            ..Default::default()
        };
        assert!(settings.secondary_pinned(shorts));
        assert!(!settings.secondary_pinned(square));
        assert!(settings.secondary_pinned(CanvasId::Main));
        settings.pins = vec!["vertical.fps".into()];
        assert!(settings.secondary_pinned(shorts));
        assert!(settings.secondary_pinned(square));
        for pin in ["canvases.main.fps", "canvases..fps", "canvases. shorts.fps"] {
            assert!(!GovernorSettings::valid_pin(pin), "{pin}");
        }
        assert!(GovernorSettings::valid_pin("canvases.shorts.fps"));
    }
}
