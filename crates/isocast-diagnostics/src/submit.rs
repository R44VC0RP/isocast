//! Sending a written report to the report service (services/report-worker):
//! the zip and its ISSUE.md, exactly as previewed, over HTTPS with the
//! system's curl (no HTTP stack in the engine). Failure never loses the zip.
use isocast_protocol::diagnostics::{BugReport, REPORT_ENDPOINT, SubmittedReport};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const MAX_ZIP: u64 = 50 << 20;

/// `ISOCAST_REPORT_ENDPOINT`, else the built-in production endpoint.
pub fn endpoint() -> Option<String> {
    std::env::var("ISOCAST_REPORT_ENDPOINT")
        .ok()
        .filter(|e| !e.trim().is_empty())
        .or_else(|| REPORT_ENDPOINT.map(String::from))
}

/// A random id kept beside the daemon's socket (in the support folder on
/// Linux, `api::daemon_file`), sent only for rate limiting.
fn install_id() -> Result<String, String> {
    let path = isocast_protocol::api::daemon_file("install-id");
    if let Some(folder) = path.parent() {
        let _ = std::fs::create_dir_all(folder);
    }
    if let Ok(id) = std::fs::read_to_string(&path)
        && id.trim().len() == 32
        && id.trim().bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Ok(id.trim().to_ascii_lowercase());
    }
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|e| e.to_string())?;
    let id = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let _ = std::fs::remove_file(&path);
    let mut file = crate::private_file(&path).map_err(|e| format!("install id: {e}"))?;
    file.write_all(id.as_bytes()).map_err(|e| e.to_string())?;
    Ok(id)
}

/// The issue title: the first line of the (redacted) description.
fn title(issue: &str, version: &str) -> String {
    let line = issue
        .split_once("## Description\n\n")
        .and_then(|(_, rest)| rest.lines().next())
        .map(|l| l.trim_start_matches('>').trim())
        .filter(|l| !l.is_empty() && *l != "No description supplied.");
    match line {
        Some(line) if line.chars().count() > 80 => line.chars().take(79).collect::<String>() + "…",
        Some(line) => line.to_string(),
        None => format!("Bug report from Isocast {version}"),
    }
}

/// "0.0.1 (abc1234)" from build.json.
fn version(build: &str) -> String {
    let build = serde_json::from_str::<Value>(build).unwrap_or_default();
    match (build["version"].as_str(), build["commit"].as_str()) {
        (Some(v), Some(c)) => format!("{v} ({c})"),
        (Some(v), None) => v.to_string(),
        _ => env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// "Apple M5 Max (Mac17,6), macOS 27.2 (26B5101f)" from environment.json.
pub fn mac_summary(environment: &str) -> String {
    let Ok(env) = serde_json::from_str::<Value>(environment) else {
        return String::new();
    };
    let chip = env["hardware"]["chip"].as_str().unwrap_or_default();
    let model = env["hardware"]["model"].as_str().unwrap_or_default();
    let sw = env["macos"]["output"].as_str().unwrap_or_default();
    let field = |name: &str| {
        sw.lines()
            .find_map(|l| l.strip_prefix(name))
            .map(|v| v.trim().to_string())
            .unwrap_or_default()
    };
    let (os, build) = (field("ProductVersion:"), field("BuildVersion:"));
    let mut line = match (chip.is_empty(), model.is_empty()) {
        (false, false) => format!("{chip} ({model})"),
        (false, true) => chip.to_string(),
        _ => model.to_string(),
    };
    if !os.is_empty() {
        line = format!("{line}, macOS {os} ({build})");
    }
    line.chars().take(200).collect()
}

/// A path for curl's `-F name=@"…"` syntax.
fn quoted(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

/// Sends the written report; sets `submitted` or `submit_error`.
pub fn send(report: &mut BugReport, contents: &BTreeMap<String, String>) {
    let Some(archive) = report.archive.clone() else {
        report.submit_error = Some("no zip was written".into());
        return;
    };
    match post(report, contents, &archive) {
        Ok(submitted) => report.submitted = Some(submitted),
        Err(why) => report.submit_error = Some(why),
    }
}

fn post(
    report: &BugReport,
    contents: &BTreeMap<String, String>,
    archive: &PathBuf,
) -> Result<SubmittedReport, String> {
    let size = std::fs::metadata(archive).map_err(|e| e.to_string())?.len();
    if size > MAX_ZIP {
        return Err(format!(
            "the zip is {} MB, over the service's 50 MB limit (try --launches 1)",
            size >> 20
        ));
    }
    let build = contents.get("build.json").map_or("", String::as_str);
    let version = version(build);
    let fields = json!({
        "schema": 1,
        "title": title(&report.issue_body, &version),
        "issue": report.issue_body,
        "version": version,
        "mac": mac_summary(contents.get("environment.json").map_or("", String::as_str)),
    });
    for key in ["title", "issue", "version", "mac"] {
        if let Some(rule) = crate::scan::find(fields[key].as_str().unwrap_or_default()) {
            return Err(format!(
                "fields.{key} contains {rule}; nothing was uploaded"
            ));
        }
    }
    for (name, text) in contents {
        if let Some(rule) = crate::scan::find(text) {
            return Err(format!("{name} contains {rule}; nothing was uploaded"));
        }
    }
    let endpoint = endpoint().ok_or(
        "no report service is configured yet (REPORT_ENDPOINT, or ISOCAST_REPORT_ENDPOINT for the daemon)",
    )?;
    if !(endpoint.starts_with("https://")
        || endpoint.starts_with("http://127.0.0.1")
        || endpoint.starts_with("http://localhost"))
    {
        return Err(format!(
            "the report endpoint must be https:// (or http://127.0.0.1 for tests), not {endpoint}"
        ));
    }
    let install = install_id()?;
    let mut child = Command::new("/usr/bin/curl")
        .args([
            "--silent",
            "--show-error",
            "--proto",
            "=https,http",
            "--connect-timeout",
            "15",
            "--max-time",
            "300",
            "--header",
            &format!("Isocast-Install-Id: {install}"),
            "--user-agent",
            concat!("isocast/", env!("CARGO_PKG_VERSION")),
            "--form",
            "fields=<-;type=application/json",
            "--form",
            &format!(
                "zip=@\"{}\";type=application/zip;filename=report.zip",
                quoted(archive)
            ),
            "--write-out",
            "\n%{http_code}",
            &endpoint,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("couldn't run curl: {e}"))?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(fields.to_string().as_bytes())
        .map_err(|e| format!("couldn't run curl: {e}"))?;
    let output = child
        .wait_with_output()
        .map_err(|e| format!("couldn't run curl: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim().trim_start_matches("curl: ");
        let detail = detail
            .strip_prefix('(')
            .and_then(|d| d.split_once(") "))
            .map_or(detail, |(_, d)| d);
        return Err(format!(
            "couldn't reach the report service (are you offline?): {}",
            detail.trim().trim_end_matches('.')
        ));
    }
    let (body, status) = stdout.rsplit_once('\n').unwrap_or(("", &stdout));
    let body = serde_json::from_str::<Value>(body).unwrap_or_default();
    if status.trim() != "201" {
        return Err(format!(
            "the report service answered {}: {}",
            status.trim(),
            body["error"].as_str().unwrap_or("no reason given")
        ));
    }
    match (body["issue_url"].as_str(), body["report_id"].as_str()) {
        (Some(url), Some(id)) => Ok(SubmittedReport {
            issue_url: url.into(),
            report_id: id.into(),
            deduplicated: body["deduplicated"].as_bool().unwrap_or(false),
        }),
        _ => Err("the report service's answer had no issue URL".into()),
    }
}
