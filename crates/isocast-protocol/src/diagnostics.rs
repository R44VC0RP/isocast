//! Offline diagnostics: the apps, CLI and MCP share these methods.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Set when the repository is public. Generating a report never uploads it.
pub const ISSUE_REPOSITORY_URL: Option<&str> = None;

/// The report service (services/report-worker, deployed on Ryan's
/// Cloudflare account) that `bug_report` with `submit` posts to.
/// `ISOCAST_REPORT_ENDPOINT` in the daemon's environment overrides it; the
/// e2e suite points it at an unreachable address so tests never file issues.
pub const REPORT_ENDPOINT: Option<&str> =
    Some("https://isocast-report-worker.exonenterprise.workers.dev/reports");

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
    /// Also send the report: the zip is written, then its ISSUE.md becomes a
    /// public GitHub issue and the zip is kept privately for maintainers.
    /// Never implied; if sending fails the zip is kept and `submit_error`
    /// says why.
    #[serde(default)]
    pub submit: bool,
    /// With `submit`: send exactly this preview (from an earlier `preview`)
    /// instead of collecting again.
    pub preview_id: Option<String>,
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
    /// Set when `submit` filed the report.
    #[serde(default)]
    pub submitted: Option<SubmittedReport>,
    /// Why `submit` didn't send the report. The zip at `archive` is still
    /// written, to attach to a new issue by hand.
    #[serde(default)]
    pub submit_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SubmittedReport {
    /// The new issue, or the comment on an open issue with the same crash.
    pub issue_url: String,
    /// The private zip's id; maintainers fetch it by this.
    pub report_id: String,
    /// True when the report was added to an existing issue.
    pub deduplicated: bool,
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
