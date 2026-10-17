//! The pre-upload secret check, the same rules as the report service's
//! (services/report-worker/src/scan.ts): scripts/publish-snapshot.sh's secret
//! patterns, plus anything [`crate::redact::Redactor`] would have replaced
//! but did not. Findings name the rule, never the text.
use regex::Regex;
use std::sync::OnceLock;

struct Rules {
    fixed: Vec<(&'static str, Regex)>,
    field: Regex,
    assign: Regex,
    value: Regex,
    url: Regex,
}

fn rules() -> &'static Rules {
    static RULES: OnceLock<Rules> = OnceLock::new();
    RULES.get_or_init(|| {
        let word = "(?:key|token|password|passwd|secret|authorization|cookie|credential)";
        Rules {
            fixed: [
                (
                    "an Apple app-specific password",
                    r"(?:^|[^a-z-])[a-z]{4}-[a-z]{4}-[a-z]{4}-[a-z]{4}(?:[^a-z-]|$)",
                ),
                ("an AWS access key", r"AKIA[0-9A-Z]{16}"),
                ("a GitHub token", r"gh[pousr]_[A-Za-z0-9]{30,}"),
                ("a GitHub token", r"github_pat_[A-Za-z0-9_]{30,}"),
                ("an API key", r"sk-[A-Za-z0-9_-]{30,}"),
                ("a private key", r"BEGIN [A-Z ]*PRIVATE KEY"),
                (
                    "a stream URL with a key",
                    r#"rtmps?://[^ "]*/(?:live|x|app)/[A-Za-z0-9_-]{16,}"#,
                ),
            ]
            .into_iter()
            .map(|(rule, re)| (rule, Regex::new(&format!("(?m){re}")).unwrap()))
            .collect(),
            field: Regex::new(&format!(
                r#"(?i)"((?:[A-Za-z0-9]+[_.-])*{word}|apikey|accesstoken|refreshtoken|clientsecret|streamkey)"\s*:\s*"#
            ))
            .unwrap(),
            assign: Regex::new(&format!(
                r#"(?i)(?:\b(?:[\w.-]+[_-])?{word}["']?\s*(?::|=|\bis\b)\s*|\bBearer\s+)(?:(?:Bearer|Basic)\s+)?"#
            ))
            .unwrap(),
            value: Regex::new(r#"^(?:"[^"]*"|'[^']*'|[^\s,;}]+)"#).unwrap(),
            url: Regex::new(r#"(?i)(?:https?|rtmps?)://[^\s"'<>\\]+"#).unwrap(),
        }
    })
}

fn leaks(rest: &str, field: bool) -> bool {
    let rest = rest.trim_start_matches(['"', '\'']);
    if rest.starts_with("[redacted]") {
        return false;
    }
    field || rules().value.is_match(rest)
}

/// Why `text` must not be uploaded, or None.
pub fn find(text: &str) -> Option<&'static str> {
    let r = rules();
    for (rule, re) in &r.fixed {
        if re.is_match(text) {
            return Some(rule);
        }
    }
    for (re, field) in [(&r.field, true), (&r.assign, false)] {
        for m in re.find_iter(text) {
            let mut end = (m.end() + 256).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            if leaks(&text[m.end()..end], field) {
                return Some(if field {
                    "a credential field that is not redacted"
                } else {
                    "a credential in text"
                });
            }
        }
    }
    for m in r.url.find_iter(text) {
        // A scheme that is the tail of a longer one (git+https://) isn't one.
        if text[..m.start()]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))
        {
            continue;
        }
        let rest = &m.as_str()[m.as_str().find("://").unwrap() + 3..];
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        if authority.contains(['@', '?', '#']) {
            return Some("a URL with a user name, query or fragment");
        }
        if !path.is_empty() && !path.starts_with("[redacted]") {
            return Some("a URL with a path (capability URLs must be redacted)");
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::find;

    #[test]
    fn redacted_text_passes_and_planted_secrets_do_not() {
        let clean = "{\n  \"stream_key\": \"[redacted]\",\n  \"url\": \"rtmps://live.example.com/[redacted]\",\n  \"hotkey\": \"cmd+k\"\n}\ntoken: [redacted] Bearer [redacted] see https://example.com/ and git+https://host/repo";
        assert_eq!(find(clean), None);
        for (text, rule) in [
            (
                "{\"stream_key\": \"abc\"}",
                "a credential field that is not redacted",
            ),
            ("password is hunter2", "a credential in text"),
            (
                "GET https://hooks.example.com/services/T0",
                "a URL with a path (capability URLs must be redacted)",
            ),
            (
                "https://user:pw@host",
                "a URL with a user name, query or fragment",
            ),
            (&format!("ghp_{}", "a".repeat(36)), "a GitHub token"),
        ] {
            assert_eq!(find(text), Some(rule), "{text}");
        }
        #[cfg(windows)]
        {
            let mut redactor =
                crate::redact::Redactor::report(true, &serde_json::Value::Null, vec![]);
            for path in [
                r"C:\Users\runner\Private Talk\take.mov",
                "C:/Users/runner/take.mov",
                r"\\server\share\Private Talk\take.mov",
                r"~\Private Talk\take.mov",
            ] {
                let redacted = redactor.text(path);
                assert!(!redacted.contains("take.mov"), "{redacted}");
                assert!(!redacted.contains("Private Talk"), "{redacted}");
            }
            let text = redactor.text(r"failed to open C:\recordings\take.mov");
            assert!(!text.contains("take.mov"), "{text}");
            let home = isocast_protocol::api::home_folder()
                .to_string_lossy()
                .into_owned();
            if !home.is_empty() {
                let mut redactor = crate::redact::Redactor::default();
                assert_eq!(redactor.text(&home), "~");
                assert_eq!(redactor.text(&home.replace('\\', "/")), "~");
            }
        }
    }
}
