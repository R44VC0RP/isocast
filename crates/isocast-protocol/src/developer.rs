//! Developer mode (Settings › Advanced › Developer mode, `isocast debug`):
//! verbose logs and per-second timing traces for testing a build locally.
//! The setting is the profile's (`[developer]`); off by default.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Hours developer mode stays on unless `auto_off_hours` says otherwise.
pub const DEFAULT_AUTO_OFF_HOURS: u32 = 24;

/// The profile's developer mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeveloperSettings {
    /// Log every API call and debug messages from capture, the compositor,
    /// encoders, writers, streaming and updates, and trace frame timing
    /// each second while recording, into their own log segments (200 MiB
    /// budget, separate from the ordinary logs).
    #[serde(default)]
    pub enabled: bool,
    /// Turn it off by itself this many hours after it was turned on; 0
    /// keeps it on until turned off.
    #[serde(default = "default_auto_off_hours")]
    pub auto_off_hours: u32,
}

fn default_auto_off_hours() -> u32 {
    DEFAULT_AUTO_OFF_HOURS
}

impl Default for DeveloperSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_off_hours: DEFAULT_AUTO_OFF_HOURS,
        }
    }
}

impl DeveloperSettings {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Turn developer mode on or off, or change when it turns itself off.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DebugSetParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Hours until it turns itself off (0: never).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_off_hours: Option<u32>,
}

/// Developer mode as the profile has it and as isocastd runs it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DeveloperStatus {
    /// The profile's setting.
    pub enabled: bool,
    /// Whether isocastd is writing verbose records and traces now (follows
    /// `enabled` within a second).
    pub active: bool,
    pub auto_off_hours: u32,
    /// When it was turned on (RFC 3339), while on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// When it turns itself off (RFC 3339), if it will.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_off_at: Option<String>,
    /// The profile it belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Where the logs are (`<launch>-developer-NNNN.jsonl` are developer
    /// mode's).
    pub logs_folder: PathBuf,
    /// Bytes developer-mode segments use now, and their budget.
    pub log_bytes: u64,
    pub log_budget_bytes: u64,
    /// isocastd's resident memory, in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    /// The latest per-second trace (outputs, compositor, pacing, encoders),
    /// while active and the engine runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<serde_json::Value>,
}
