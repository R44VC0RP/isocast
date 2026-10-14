//! Offline diagnostics: the apps, CLI and MCP share these methods.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Set when the repository is public. Generating a report never uploads it.
pub const ISSUE_REPOSITORY_URL: Option<&str> = None;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BugReportParams {
    /// Absolute or ~/ output directory; defaults to Downloads/Isocast Reports
    /// (isolated engines: beside their logs).
    pub out: Option<PathBuf>,
    pub describe: Option<String>,
    /// Return the file index, ISSUE and redaction report without writing a zip.
    /// Use report_file with the returned preview_id to page through contents.
    #[serde(default)]
    pub preview: bool,
    /// Strict privacy: per-report HMAC pseudonyms for paths, titles and device identities.
    #[serde(default)]
    pub hash_paths: bool,
    /// Last launches of each process to collect (1..10; default 3).
    pub launches: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BugReport {
    pub preview_id: Option<String>,
    pub file_sizes: BTreeMap<String, usize>,
    pub archive: Option<PathBuf>,
    pub issue_body: String,
    pub repository_url: Option<String>,
    pub files: Vec<String>,
    pub redaction: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReportFileParams {
    pub preview_id: String,
    pub file: String,
    #[serde(default)]
    pub offset: usize,
    /// UTF-8 bytes, 1..16384; default 4096. Responses end at a character boundary.
    pub limit: Option<usize>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ReportFile {
    pub file: String,
    pub content: String,
    pub next_offset: Option<usize>,
    pub total_bytes: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LogsParams {
    /// Minimum severity: debug, info, warn, error.
    pub level: Option<String>,
    /// Component substring.
    pub component: Option<String>,
    /// Opaque per-file byte offsets returned by the previous call.
    pub cursor: Option<BTreeMap<String, u64>>,
    /// Maximum returned records (1..1000; default 200).
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Logs {
    pub records: Vec<serde_json::Value>,
    pub cursor: BTreeMap<String, u64>,
}
