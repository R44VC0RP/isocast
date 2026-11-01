//! Help › Report a Bug… (area D, docs/bug-reports.md): the sheet shows the
//! report as `bug_report` previews it (its ISSUE.md and every file with
//! its size) before anything is written; Create Report writes the zip
//! (`bug_report` without `preview`); Open GitHub Issue opens a new issue
//! on R44VC0RP/isocast with the bug-report template's fields filled in
//! (the redacted description, triage instructions, version and system) and
//! reveals the zip. Send Report instead has the engine file
//! the issue (`bug_report` with `submit` and the preview's id, so exactly
//! the text shown becomes public; the zip goes privately to the
//! maintainers). Nothing is sent before Send Report.

use std::cell::RefCell;
use std::rc::Rc;

use isocast_protocol::api::Method;
use isocast_protocol::diagnostics::{BugReport, BugReportParams};
use serde_json::Value;
use slint::{ComponentHandle, SharedString};

use crate::app::App;
use crate::kit::model;
use crate::{ReportFileRow, SettingsArea};

/// Where issues go when the protocol names no repository yet
/// (`ISSUE_REPOSITORY_URL` is None until the repository is public).
const REPOSITORY: &str = "https://github.com/R44VC0RP/isocast";

/// The new-issue link stays under this many characters: GitHub answers
/// longer ones (Safari cuts the address at 4096) with an error page, so the
/// triage block goes on the clipboard instead (QA-3 #1).
const URL_LIMIT: usize = 2000;

#[derive(Default)]
pub struct Report {
    /// The last report shown (preview or written).
    shown: RefCell<Option<BugReport>>,
    /// The description the shown preview was made with.
    previewed: RefCell<Option<String>>,
}

impl Report {
    pub fn connect(app: &Rc<App>, area: &SettingsArea) {
        let weak = Rc::downgrade(app);
        area.on_report_preview(move || {
            if let Some(app) = weak.upgrade() {
                app.settings.report.preview(&app);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_report_create(move || {
            if let Some(app) = weak.upgrade() {
                app.settings.report.create(&app);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_report_send(move || {
            if let Some(app) = weak.upgrade() {
                app.settings.report.send(&app);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_report_open_issue(move || {
            if let Some(app) = weak.upgrade() {
                app.settings.report.open_issue(&app);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_report_reveal(move || {
            if let Some(app) = weak.upgrade() {
                app.settings.report.reveal(&app);
            }
        });
    }

    /// The sheet opened: start afresh and preview.
    pub fn opened(&self, app: &Rc<App>) {
        *self.shown.borrow_mut() = None;
        if let Some(window) = app.window() {
            let area = window.global::<SettingsArea>();
            area.set_report_describe(SharedString::new());
            area.set_report_archive(SharedString::new());
            area.set_report_copied(false);
            area.set_report_error(SharedString::new());
            area.set_report_sending(false);
            area.set_report_issue_url(SharedString::new());
            area.set_report_issue("Collecting what the report holds…".into());
            area.set_report_issue_lines(1);
            area.set_report_files(model(Vec::<ReportFileRow>::new()));
            area.set_report_files_summary(SharedString::new());
        }
        self.preview(app);
    }

    /// Snapshots: preview again, or write the zip, as the buttons do.
    pub fn refresh(&self, app: &Rc<App>) {
        self.preview(app);
    }

    pub fn write(&self, app: &Rc<App>) {
        self.create(app);
    }

    fn describe(app: &Rc<App>) -> Option<String> {
        let window = app.window()?;
        let text = window.global::<SettingsArea>().get_report_describe();
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_string())
    }

    fn busy(app: &Rc<App>, busy: bool) {
        if let Some(window) = app.window() {
            window.global::<SettingsArea>().set_report_busy(busy);
        }
    }

    fn preview(&self, app: &Rc<App>) {
        *self.previewed.borrow_mut() = None;
        let description = Self::describe(app);
        let params = BugReportParams {
            describe: description.clone(),
            preview: true,
            ..BugReportParams::default()
        };
        Self::busy(app, true);
        app.call_as::<BugReport>(Method::BugReport(params), move |app, result| {
            Self::busy(app, false);
            let report = &app.settings.report;
            match result {
                Ok(preview) => {
                    *report.previewed.borrow_mut() = Some(description.unwrap_or_default());
                    report.show(app, preview, None);
                }
                Err(error) => report.error(app, &error.message),
            }
        });
    }

    fn create(&self, app: &Rc<App>) {
        let params = BugReportParams {
            describe: Self::describe(app),
            preview: false,
            ..BugReportParams::default()
        };
        Self::busy(app, true);
        app.call_as::<BugReport>(Method::BugReport(params), |app, result| {
            Self::busy(app, false);
            let report = &app.settings.report;
            match result {
                Ok(written) => {
                    let archive = written.archive.clone();
                    report.show(app, written, archive.as_deref());
                }
                Err(error) => report.error(app, &error.message),
            }
        });
    }

    /// Send Report: the engine writes the zip from the preview on screen
    /// and posts it. A description typed since that preview is previewed
    /// first, so what is sent is always what was shown.
    pub fn send(&self, app: &Rc<App>) {
        let preview_id = self
            .shown
            .borrow()
            .as_ref()
            .and_then(|r| r.preview_id.clone());
        let current = Self::describe(app).unwrap_or_default();
        let Some(preview_id) =
            preview_id.filter(|_| self.previewed.borrow().as_deref() == Some(current.as_str()))
        else {
            self.preview(app);
            if let Some(window) = app.window() {
                window.global::<SettingsArea>().set_report_error(
                    "The preview now has your description. Check it, then Send Report.".into(),
                );
            }
            return;
        };
        let params = BugReportParams {
            submit: true,
            preview_id: Some(preview_id),
            ..BugReportParams::default()
        };
        Self::busy(app, true);
        if let Some(window) = app.window() {
            window.global::<SettingsArea>().set_report_sending(true);
        }
        app.call_as::<BugReport>(Method::BugReport(params), |app, result| {
            Self::busy(app, false);
            if let Some(window) = app.window() {
                window.global::<SettingsArea>().set_report_sending(false);
            }
            let report = &app.settings.report;
            match result {
                Ok(written) => {
                    let archive = written.archive.clone();
                    let url = written.submitted.as_ref().map(|s| s.issue_url.clone());
                    let failed = written.submit_error.clone();
                    report.show(app, written, archive.as_deref());
                    if let Some(window) = app.window() {
                        let area = window.global::<SettingsArea>();
                        if let Some(url) = url {
                            area.set_report_issue_url(url.into());
                        }
                        // Not sent: the zip is saved, and the manual path
                        // (Open GitHub Issue, Show in Finder) is offered.
                        if let Some(why) = failed {
                            area.set_report_error(
                                format!(
                                    "Couldn’t send the report: {why}. Open GitHub Issue and attach the zip instead."
                                )
                                .into(),
                            );
                        }
                    }
                }
                Err(error) => report.error(app, &error.message),
            }
        });
    }

    fn error(&self, app: &Rc<App>, message: &str) {
        if let Some(window) = app.window() {
            window
                .global::<SettingsArea>()
                .set_report_error(format!("Couldn’t make the report: {message}").into());
        }
    }

    fn show(&self, app: &Rc<App>, report: BugReport, archive: Option<&std::path::Path>) {
        if let Some(window) = app.window() {
            let area = window.global::<SettingsArea>();
            area.set_report_error(SharedString::new());
            area.set_report_issue_text(report.issue_body.as_str().into());
            let (text, lines) = wrap(&report.issue_body, 84);
            area.set_report_issue(text.into());
            area.set_report_issue_lines(lines as i32);
            let total: usize = report.file_sizes.values().sum();
            area.set_report_files_summary(
                format!(
                    "{} file{} · {}",
                    report.files.len(),
                    if report.files.len() == 1 { "" } else { "s" },
                    size(total)
                )
                .into(),
            );
            area.set_report_files(model(report.files.iter().map(|name| {
                ReportFileRow {
                    name: name.as_str().into(),
                    size: report
                        .file_sizes
                        .get(name)
                        .map_or(String::new(), |s| size(*s))
                        .into(),
                }
            })));
            if let Some(archive) = archive {
                area.set_report_archive(
                    crate::platform::abbreviate_home(&archive.to_string_lossy()).into(),
                );
            }
        }
        *self.shown.borrow_mut() = Some(report);
    }

    fn archive(&self) -> Option<String> {
        self.shown
            .borrow()
            .as_ref()
            .and_then(|r| r.archive.as_ref())
            .map(|p| p.to_string_lossy().into_owned())
    }

    fn reveal(&self, _app: &Rc<App>) {
        if let Some(archive) = self.archive() {
            crate::platform::reveal(&archive);
        }
    }

    /// The new-issue page with the template's fields filled in, in the
    /// browser (to post by hand), and the zip in the Finder (to attach).
    fn open_issue(&self, app: &Rc<App>) {
        let Some(report) = self.shown.borrow().clone() else {
            return;
        };
        if let Some(sent) = &report.submitted {
            crate::platform::open_url(&sent.issue_url);
            return;
        }
        // The full triage block, to paste into the issue's Triage field
        // (the zip's ISSUE.md has it too).
        let copied = triage(&report.issue_body).is_some_and(crate::platform::try_copy_text);
        let url = issue_url(
            report.repository_url.as_deref().unwrap_or(REPOSITORY),
            &report.issue_body,
            copied,
            report.archive.as_deref(),
        );
        crate::platform::open_url(&url);
        self.reveal(app);
        // The sheet's line says to paste it (a toast would sit behind the
        // sheet, or be gone by the time the browser is back).
        if let Some(window) = app.window() {
            let area = window.global::<SettingsArea>();
            area.set_report_copied(copied);
            if !copied {
                area.set_report_error(
                    "Clipboard unavailable. Copy the triage block from ISSUE.md in the saved zip."
                        .into(),
                );
            }
        }
    }
}

/// `text` hard-wrapped at `columns` characters (a monospaced well), and
/// how many lines that makes.
fn wrap(text: &str, columns: usize) -> (String, usize) {
    let mut out = Vec::new();
    for line in text.lines() {
        let chars: Vec<char> = line.chars().collect();
        if chars.is_empty() {
            out.push(String::new());
        }
        for chunk in chars.chunks(columns) {
            out.push(chunk.iter().collect());
        }
    }
    let n = out.len().max(1);
    (out.join("\n"), n)
}

/// A file's size: "812 B", "4.1 KB", "1.3 MB".
fn size(bytes: usize) -> String {
    let b = bytes as f64;
    if b < 1000.0 {
        format!("{bytes} B")
    } else if b < 1e6 {
        format!("{:.1} KB", b / 1e3)
    } else {
        format!("{:.1} MB", b / 1e6)
    }
}

/// The triage block: the fenced JSON in ISSUE.md.
fn triage(issue: &str) -> Option<&str> {
    let (_, rest) = issue.split_once("\n## Agent triage\n\n```json\n")?;
    let end = rest.find("\n```")?;
    Some(rest[..end].trim())
}

fn issue_url(
    repository: &str,
    issue: &str,
    copied: bool,
    archive: Option<&std::path::Path>,
) -> String {
    let triage = triage(issue).unwrap_or_default();
    let fields = serde_json::from_str::<Value>(triage).ok();
    let machine = fields
        .as_ref()
        .and_then(|t| t["machine_summary"].as_str())
        .unwrap_or_default();
    let describe = issue
        .split_once("\n## Description\n\n")
        .and_then(|(_, rest)| rest.split_once("\n\n## Agent triage\n"))
        .map(|(s, _)| {
            s.lines()
                .map(|l| l.strip_prefix("> ").unwrap_or(l))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    let version = fields
        .as_ref()
        .and_then(|t| {
            let v = &t["versions"];
            Some(format!(
                "{} ({})",
                v["version"].as_str()?,
                v["commit"].as_str().unwrap_or("?")
            ))
        })
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    let title = describe
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(80)
        .collect::<String>();
    let zip = archive
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let attach = format!(
        "Attach {zip} here (Isocast showed it in the {}).",
        isocast_protocol::platform_text::FILE_MANAGER
    );
    let mut fields = vec![
        ("template", "bug_report.yml".to_string()),
        ("labels", "bug".to_string()),
        ("title", title),
        ("summary", describe.to_string()),
        ("version", version),
        ("mac", machine.to_string()),
        ("attachments", attach),
    ];
    let base = format!("{}/issues/new?", repository.trim_end_matches('/'));
    let query = |fields: &[(&str, String)]| {
        fields
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, v)| format!("{k}={}", encode(v)))
            .collect::<Vec<_>>()
            .join("&")
    };
    // Never the triage block (the clipboard has it): a pointer to paste it.
    fields.push((
        "triage",
        if copied {
            "Paste the triage block from the clipboard."
        } else {
            "Copy the triage block from ISSUE.md in the saved zip."
        }
        .to_string(),
    ));
    let mut url = format!("{base}{}", query(&fields));
    // A very long description is cut to fit (the zip has it whole).
    while url.len() > URL_LIMIT {
        let summary = &mut fields[3].1;
        let keep = summary.chars().count().saturating_sub(200);
        if keep == 0 {
            summary.clear();
            url = format!("{base}{}", query(&fields));
            break;
        }
        *summary = summary.chars().take(keep).collect::<String>() + " …";
        url = format!("{base}{}", query(&fields));
    }
    url
}

/// Percent-encoding for a query value (RFC 3986 unreserved kept).
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn manual_form_uses_reviewed_text_and_actual_clipboard_outcome() {
        let issue = "## Report\n## Description\n\n> Audio & video [redacted]\n\n## Agent triage\n\n```json\n{\"versions\":{\"version\":\"1.2\",\"commit\":\"abc\"},\"machine_summary\":\"Windows 11\"}\n```";
        let url = |copied| super::issue_url("https://example.invalid", issue, copied, None);
        let manual = url(false);
        for field in [
            "summary=Audio%20%26%20video%20%5Bredacted%5D",
            "version=1.2%20%28abc%29",
            "mac=Windows%2011",
            "triage=Copy%20the%20triage%20block%20from%20ISSUE.md",
        ] {
            assert!(manual.contains(field), "missing {field}");
        }
        assert!(!manual.contains("clipboard"));
        assert!(url(true).contains("clipboard"));
    }
}
