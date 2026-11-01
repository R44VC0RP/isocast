//! Sending a written report to the report service (services/report-worker):
//! the zip and its ISSUE.md, exactly as previewed, over HTTPS with the
//! system's TLS trust store. Failure never loses the zip.
use isocast_protocol::diagnostics::{BugReport, REPORT_ENDPOINT, SubmittedReport};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::time::Duration;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
use ureq::unversioned::multipart::{Form, Part};

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
pub fn machine_summary(environment: &str) -> String {
    let Ok(env) = serde_json::from_str::<Value>(environment) else {
        return String::new();
    };
    let chip = env["hardware"]["chip"].as_str().unwrap_or_default();
    if let Some(os) = env["os"].as_str() {
        return [chip, os]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", ")
            .chars()
            .take(200)
            .collect();
    }
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

fn report_url(endpoint: &str) -> Result<url::Url, String> {
    let url = url::Url::parse(endpoint).map_err(|e| format!("invalid report endpoint: {e}"))?;
    if url.host().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && (url.scheme() == "https"
            || (url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))))
    {
        Ok(url)
    } else {
        Err(
            "the report endpoint must be HTTPS (or loopback HTTP for tests), without credentials"
                .into(),
        )
    }
}

/// Sends the written report; sets `submitted` or `submit_error`.
pub fn send(report: &mut BugReport, contents: &BTreeMap<String, String>) {
    let Some(archive) = report.archive.clone() else {
        report.submit_error = Some("no zip was written".into());
        return;
    };
    match post(report, contents, &archive, RootCerts::PlatformVerifier) {
        Ok(submitted) => report.submitted = Some(submitted),
        Err(why) => report.submit_error = Some(why),
    }
}

fn post(
    report: &BugReport,
    contents: &BTreeMap<String, String>,
    archive: &Path,
    roots: RootCerts,
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
        // Service schema 1 calls its unlabeled machine-summary footer `mac`.
        "mac": machine_summary(contents.get("environment.json").map_or("", String::as_str)),
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
    let endpoint = report_url(&endpoint)?;
    let mut config = ureq::Agent::config_builder()
        .tls_config(
            TlsConfig::builder()
                .provider(TlsProvider::NativeTls)
                .root_certs(roots)
                .build(),
        )
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_global(Some(Duration::from_secs(300)))
        .max_redirects(0)
        .http_status_as_error(false);
    if endpoint.scheme() == "http" {
        config = config.proxy(None);
    }
    let fields = fields.to_string();
    let form = Form::new()
        .part(
            "fields",
            Part::text(&fields)
                .mime_str("application/json")
                .map_err(|e| e.to_string())?,
        )
        .part(
            "zip",
            Part::file(archive)
                .map_err(|e| e.to_string())?
                .file_name("report.zip"),
        );
    let mut response = config
        .build()
        .new_agent()
        .post(endpoint.as_str())
        .header("Isocast-Install-Id", install_id()?)
        .header("User-Agent", concat!("isocast/", env!("CARGO_PKG_VERSION")))
        .send(form)
        .map_err(|e| format!("couldn't reach the report service (are you offline?): {e}"))?;
    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .with_config()
        .limit(64 << 10)
        .read_to_string()
        .map_err(|e| {
            format!("the report service answered {status}, but its reply could not be read: {e}")
        })?;
    let body = serde_json::from_str::<Value>(&body).unwrap_or_default();
    if status != 201 {
        return Err(format!(
            "the report service answered {}: {}",
            status,
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

#[cfg(test)]
mod tests {
    use super::*;

    // SSH sessions cannot authorize macOS user trust changes. Exercise the
    // same native TLS upload with a root scoped to this one test connection.
    #[test]
    #[ignore = "requires an isolated socket, a loopback HTTPS receiver and its temporary CA"]
    fn https_fixture_with_connection_scoped_root() {
        use std::io::Read;
        let endpoint = std::env::var("ISOCAST_REPORT_ENDPOINT").unwrap();
        let url = report_url(&endpoint).unwrap();
        assert_eq!(url.scheme(), "https");
        assert!(matches!(
            url.host_str(),
            Some("localhost" | "127.0.0.1" | "[::1]")
        ));
        let socket = std::path::PathBuf::from(std::env::var_os("ISOCAST_SOCKET").unwrap());
        assert!(socket.is_absolute());
        let report = crate::bundle::disconnected(&isocast_protocol::diagnostics::BugReportParams {
            describe: Some("Loopback TLS fixture".into()),
            out: Some(socket.parent().unwrap().to_owned()),
            ..Default::default()
        })
        .unwrap();
        let path = report.archive.as_ref().unwrap();
        let mut zip = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
        let mut files = BTreeMap::new();
        for i in 0..zip.len() {
            let mut file = zip.by_index(i).unwrap();
            let mut text = String::new();
            file.read_to_string(&mut text).unwrap();
            files.insert(file.name().to_owned(), text);
        }
        let pem = std::fs::read(std::env::var_os("ISOCAST_TEST_REPORT_CA").unwrap()).unwrap();
        let root = ureq::tls::Certificate::from_pem(&pem).unwrap();
        let sent = post(&report, &files, path, RootCerts::from([root])).unwrap();
        assert_eq!(sent.report_id, "test-only");
    }

    #[test]
    fn report_destinations_are_https_or_loopback() {
        for bad in [
            "http://localhost.example/reports",
            "http://127.0.0.1@example.com/reports",
            "file:///reports",
        ] {
            assert!(report_url(bad).is_err());
        }
        assert!(report_url("http://127.0.0.1:8080/reports").is_ok());
    }

    #[test]
    fn footer_names_the_reporting_machine() {
        assert_eq!(
            machine_summary(r#"{"hardware":{"chip":"Ryzen"},"os":"Windows 11"}"#),
            "Ryzen, Windows 11"
        );
        assert_eq!(machine_summary(r#"{"os":"Linux 6.8"}"#), "Linux 6.8");
        assert_eq!(
            machine_summary(
                r#"{"hardware":{"chip":"M2","model":"Mac14,2"},"macos":{"output":"ProductVersion: 26.1\nBuildVersion: 25B78"}}"#
            ),
            "M2 (Mac14,2), macOS 26.1 (25B78)"
        );
    }
}
