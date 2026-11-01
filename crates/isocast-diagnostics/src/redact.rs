//! Redaction before persistence and again with current credentials at export.
use hmac::{Hmac, Mac};
use regex::Regex;
use serde_json::{Value, json};
use sha2::Sha256;
use std::collections::BTreeMap;
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicU64, Ordering},
};

#[derive(Default)]
struct Secrets {
    values: Arc<Vec<String>>,
    pinned: Arc<Vec<String>>,
    owners: BTreeMap<(String, String), Vec<String>>,
}
static SECRETS: OnceLock<Mutex<Secrets>> = OnceLock::new();
static REVISION: AtomicU64 = AtomicU64::new(1);
static EVICTIONS: AtomicU64 = AtomicU64::new(0);
fn registry() -> &'static Mutex<Secrets> {
    SECRETS.get_or_init(Mutex::default)
}

/// Explicit credentials never compete with inferred API values. Keep only
/// the current value and two previous values for each service/account.
/// Call only after a successful credential read/write (or for the env key).
pub fn remember_secret(service: &str, account: &str, secret: &str) {
    if secret.is_empty() || secret == "[redacted]" {
        return;
    }
    let mut state = registry().lock().unwrap_or_else(|p| p.into_inner());
    let values = state
        .owners
        .entry((service.into(), account.into()))
        .or_default();
    if values.last().is_some_and(|s| s == secret) {
        return;
    }
    values.retain(|s| s != secret);
    if values.len() == 3 {
        values.remove(0);
    }
    values.push(secret.into());
    let mut pinned = state.owners.values().flatten().cloned().collect::<Vec<_>>();
    pinned.sort();
    pinned.dedup();
    state.pinned = Arc::new(pinned);
    REVISION.fetch_add(1, Ordering::Release);
    drop(state);
    crate::native::seed(secret, Some((service, account)));
}

/// Only API-learned values are bounded/evictable. Oversized structured values
/// are scrubbed in place without poisoning diagnostics or populating the cache.
pub(crate) fn learn_secret(secret: &str) {
    if !cacheable(secret) {
        return;
    }
    let mut state = registry().lock().unwrap_or_else(|p| p.into_inner());
    if state.pinned.iter().any(|s| s == secret) {
        return;
    }
    if insert(&mut state.values, secret) {
        EVICTIONS.fetch_add(1, Ordering::Relaxed);
    }
    REVISION.fetch_add(1, Ordering::Release);
    drop(state);
    crate::native::seed(secret, None);
}
fn cacheable(secret: &str) -> bool {
    !secret.is_empty() && secret.len() <= 4096 && secret != "[redacted]"
}
fn insert(values: &mut Arc<Vec<String>>, secret: &str) -> bool {
    if !cacheable(secret) {
        return false;
    }
    let values = Arc::make_mut(values);
    let mut evicted = false;
    if let Some(at) = values.iter().position(|s| s == secret) {
        values.remove(at);
    } else if values.len() == 512 {
        values.remove(0);
        evicted = true;
    }
    values.push(secret.into());
    evicted
}
pub fn cache_evictions() -> u64 {
    EVICTIONS.load(Ordering::Relaxed)
}
pub fn pinned_count() -> usize {
    registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .pinned
        .len()
}

#[cfg(unix)]
pub(crate) type KnownSecret = (String, Option<(String, String)>);
#[cfg(unix)]
pub(crate) fn known_secrets() -> Vec<KnownSecret> {
    let state = registry().lock().unwrap_or_else(|p| p.into_inner());
    state
        .values
        .iter()
        .map(|s| (s.clone(), None))
        .chain(
            state.owners.iter().flat_map(|(owner, values)| {
                values.iter().map(|s| (s.clone(), Some(owner.clone())))
            }),
        )
        .collect()
}

pub fn sensitive(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    matches!(
        key.as_str(),
        "key"
            | "sdp"
            | "token"
            | "password"
            | "passwd"
            | "secret"
            | "authorization"
            | "cookie"
            | "credential"
            | "apikey"
            | "accesstoken"
            | "refreshtoken"
            | "clientsecret"
            | "streamkey"
    ) || [
        "_key",
        "_token",
        "_password",
        "_secret",
        "_credential",
        "-key",
        "-token",
        "-password",
        "-secret",
        ".key",
        ".token",
        ".password",
        ".secret",
    ]
    .iter()
    .any(|s| key.ends_with(s))
}

#[cfg(test)]
mod sdp_tests {
    #[test]
    fn sdp_credentials_are_not_retained_in_diagnostics() {
        let input = serde_json::json!({"method":"whep_offer","params":{"sdp":"v=0\r\na=ice-pwd:example-only\r\n"}});
        let redacted = super::Redactor::default().value(&input);
        assert_eq!(redacted["params"]["sdp"], "[redacted]");
        assert_eq!(redacted["method"], "whep_offer");
    }
}

#[derive(Default)]
pub struct Redactor {
    pub hash_paths: bool,
    pub counts: BTreeMap<String, u64>,
    secrets: Arc<Vec<String>>,
    pinned: Arc<Vec<String>>,
    revision: u64,
    /// Report seeds are independent of the live cache and cannot be evicted
    /// by credential-shaped fields in retained logs.
    report_seeds: Vec<String>,
    salt: Option<[u8; 32]>,
    identities: Arc<Vec<String>>,
}
impl Redactor {
    pub fn report(strict: bool, devices: &Value, credentials: Vec<String>) -> Self {
        let state = registry().lock().unwrap_or_else(|p| p.into_inner());
        let mut this = Self {
            hash_paths: strict,
            revision: u64::MAX,
            report_seeds: credentials.into_iter().filter(|s| !s.is_empty()).collect(),
            // A snapshot of this process's recent credentials; learning while
            // building the report updates only this copy.
            secrets: state.values.clone(),
            pinned: state.pinned.clone(),
            ..Default::default()
        };
        drop(state);
        let mut identities = Vec::new();
        for list in [
            "displays",
            "cameras",
            "microphones",
            "audio_outputs",
            "windows",
        ] {
            for device in devices[list].as_array().into_iter().flatten() {
                for field in ["id", "name", "model"] {
                    if let Some(s) = device[field].as_str().filter(|s| !s.is_empty()) {
                        identities.push(s.to_owned());
                    }
                }
            }
        }
        identities.sort_by_key(|s| std::cmp::Reverse(s.len()));
        identities.dedup();
        this.identities = Arc::new(identities);
        this
    }
    fn removed(&mut self, what: &str) {
        *self.counts.entry(what.into()).or_default() += 1;
    }
    fn refresh(&mut self) {
        if self.revision == u64::MAX {
            return;
        }
        let revision = REVISION.load(Ordering::Acquire);
        if self.revision != revision {
            let state = registry().lock().unwrap_or_else(|p| p.into_inner());
            self.secrets = state.values.clone();
            self.pinned = state.pinned.clone();
            self.revision = revision;
        }
    }
    fn fingerprint(&mut self, text: &str) -> String {
        if self.salt.is_none() {
            let mut salt = [0; 32];
            if getrandom::fill(&mut salt).is_err() {
                self.removed("random source unavailable; identifier withheld");
                return "[private]".into();
            }
            self.salt = Some(salt);
        }
        let mut mac = Hmac::<Sha256>::new_from_slice(self.salt.as_ref().unwrap())
            .expect("fixed HMAC key size");
        mac.update(text.as_bytes());
        format!("[hmac:{:x}]", mac.finalize().into_bytes())
    }
    pub fn value(&mut self, value: &Value) -> Value {
        if self.hash_paths {
            self.include_devices(value);
        }
        if self.revision == u64::MAX {
            learn(value, &mut |s| {
                insert(&mut self.secrets, s);
            });
        } else {
            learn(value, &mut learn_secret);
        }
        self.refresh();
        self.scrub(value)
    }

    /// Include identities from retained inventories too, not just devices
    /// currently connected when a report is generated.
    pub fn include_devices(&mut self, value: &Value) {
        fn gather(value: &Value, out: &mut Vec<String>) {
            match value {
                Value::Object(map) => {
                    for (key, value) in map {
                        if matches!(
                            key.as_str(),
                            "displays" | "cameras" | "microphones" | "audio_outputs" | "windows"
                        ) {
                            for device in value.as_array().into_iter().flatten() {
                                for field in ["id", "name", "model"] {
                                    if let Some(s) =
                                        device[field].as_str().filter(|s| !s.is_empty())
                                    {
                                        out.push(s.into());
                                    }
                                }
                            }
                        }
                        gather(value, out);
                    }
                }
                Value::Array(values) => {
                    for value in values {
                        gather(value, out);
                    }
                }
                _ => {}
            }
        }
        let mut found = Vec::new();
        gather(value, &mut found);
        found.retain(|s| !self.identities.contains(s));
        if !found.is_empty() {
            let identities = Arc::make_mut(&mut self.identities);
            identities.extend(found);
            identities.sort_by_key(|s| std::cmp::Reverse(s.len()));
            identities.dedup();
        }
    }
    fn scrub(&mut self, value: &Value) -> Value {
        match value {
            Value::Object(map) => Value::Object(
                map.iter()
                    .filter_map(|(key, value)| {
                        if key.to_ascii_lowercase().contains("serial") {
                            self.removed("serial-number fields");
                            return None;
                        }
                        let value = if sensitive(key) {
                            self.removed("credential fields");
                            json!("[redacted]")
                        } else if key.eq_ignore_ascii_case("title")
                            || key.eq_ignore_ascii_case("window_title")
                        {
                            self.removed("window titles");
                            json!(if self.hash_paths {
                                self.fingerprint(&value.to_string())
                            } else {
                                "[title omitted]".into()
                            })
                        } else {
                            self.scrub(value)
                        };
                        Some((key.clone(), value))
                    })
                    .collect(),
            ),
            Value::Array(values) => Value::Array(values.iter().map(|v| self.scrub(v)).collect()),
            Value::String(text) => json!(self.text(text)),
            other => other.clone(),
        }
    }
    pub fn text(&mut self, text: &str) -> String {
        self.refresh();
        static HOME: OnceLock<Option<String>> = OnceLock::new();
        static URL: OnceLock<Regex> = OnceLock::new();
        static ASSIGN: OnceLock<Regex> = OnceLock::new();
        static TITLES: OnceLock<Regex> = OnceLock::new();
        static PATH: OnceLock<Regex> = OnceLock::new();
        static BLUETOOTH_ID: OnceLock<Regex> = OnceLock::new();
        let mut text = text.to_owned();
        if let Some(at) = text.find("; windows: ") {
            text.truncate(at);
            text.push_str("; windows: [titles omitted]");
            self.removed("window lists");
        }
        if text.contains(" titled \"") {
            text = TITLES
                .get_or_init(|| Regex::new(r#" titled "(?:[^"\\]|\\.)*""#).unwrap())
                .replace_all(&text, |c: &regex::Captures<'_>| {
                    self.removed("window titles");
                    format!(
                        " titled {}",
                        if self.hash_paths {
                            self.fingerprint(c[0].trim_start_matches(" titled "))
                        } else {
                            "[title omitted]".into()
                        }
                    )
                })
                .into_owned();
        }
        let secrets = self.secrets.clone();
        let pinned = self.pinned.clone();
        for secret in self
            .report_seeds
            .clone()
            .iter()
            .chain(pinned.iter())
            .chain(secrets.iter())
        {
            if (secret.len() >= 4 || text == *secret) && text.contains(secret) {
                self.removed("known credentials");
                text = text.replace(secret, "[redacted]");
            }
        }
        if text.contains("://") {
            text = URL
                .get_or_init(|| Regex::new(r#"[a-zA-Z][a-zA-Z0-9+.-]*://[^\s\"'<>]+"#).unwrap())
                .replace_all(&text, |c: &regex::Captures<'_>| {
                    let (scheme, rest) = c[0].split_once("://").unwrap();
                    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
                    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
                    let authority = authority.rsplit('@').next().unwrap_or(authority);
                    let private_path = matches!(
                        scheme.to_ascii_lowercase().as_str(),
                        "http" | "https" | "rtmp" | "rtmps"
                    ) && !path.is_empty();
                    self.removed("URL credentials, query, fragment and capability paths");
                    format!(
                        "{scheme}://{authority}{}{}",
                        if rest.contains('/') { "/" } else { "" },
                        if private_path { "[redacted]" } else { path }
                    )
                })
                .into_owned();
        }
        if text.contains([':', '='])
            || text.to_ascii_lowercase().contains(" is ")
            || text
                .as_bytes()
                .windows(6)
                .any(|w| w.eq_ignore_ascii_case(b"bearer"))
        {
            let assignment=ASSIGN.get_or_init(||Regex::new(r#"(?i)(\b(?:[\w.-]+[_-])?(?:key|token|password|passwd|secret|authorization|cookie|credential)[\"']?\s*(?:[:=]|\bis\b)\s*|\bBearer\s+)(?:(?:Bearer|Basic)\s+)?(?:\"[^\"]*\"|'[^']*'|[^\s,;}]+)"#).unwrap());
            text = assignment
                .replace_all(&text, |c: &regex::Captures<'_>| {
                    self.removed("credentials in text");
                    format!("{}[redacted]", &c[1])
                })
                .into_owned();
        }
        if let Some(home) = HOME.get_or_init(|| {
            isocast_protocol::api::home_folder()
                .into_os_string()
                .into_string()
                .ok()
        }) && !home.is_empty()
            && text.contains(home)
        {
            text = text.replace(home, "~");
            self.removed("home directory");
        }
        #[cfg(windows)]
        if let Some(home) = HOME
            .get()
            .and_then(Option::as_ref)
            .filter(|home| !home.is_empty())
        {
            let forward = home.replace('\\', "/");
            if text.contains(&forward) {
                text = text.replace(&forward, "~");
                self.removed("home directory");
            }
        }
        // Device IDs can contain a USB serial without a serial-number key.
        if text.starts_with("AppleUSBAudioEngine:")
            || text.starts_with("/dev/v4l/by-id/")
            || text
                .get(..8)
                .is_some_and(|s| s.eq_ignore_ascii_case(r"\\?\usb#"))
            || text.starts_with("alsa_input.usb-")
            || text.starts_with("alsa_output.usb-")
        {
            self.removed("serial-bearing device identifiers");
            return self.fingerprint(&text);
        }
        // PipeWire node names and Core Audio UIDs can expose the Bluetooth
        // address even in ordinary diagnostics, including in error messages.
        if text.len() >= 17 && text.contains(['-', '_', ':']) {
            let id = BLUETOOTH_ID.get_or_init(|| Regex::new(r"(?i)\b(?:bluez_[a-z_]+\.)?(?:(?:[0-9a-f]{2}_){5}|(?:[0-9a-f]{2}-){5}|(?:[0-9a-f]{2}:){5})[0-9a-f]{2}(?:[.:](?:input|output|[0-9]+))?\b").unwrap());
            if let std::borrow::Cow::Owned(scrubbed) =
                id.replace_all(&text, |c: &regex::Captures<'_>| {
                    self.removed("Bluetooth device identifiers");
                    self.fingerprint(&c[0])
                })
            {
                text = scrubbed;
            }
        }
        if self.hash_paths {
            for id in self.identities.clone().iter() {
                if text.contains(id) {
                    let alias = self.fingerprint(id);
                    text = text.replace(id, &alias);
                    self.removed("device names and IDs");
                }
            }
            if text.starts_with('/')
                || text.starts_with("~/")
                || (cfg!(windows)
                    && (std::path::Path::new(&text).is_absolute() || text.starts_with(r"~\")))
            {
                self.removed("private paths");
                return self.fingerprint(&text);
            }
            text = PATH
                .get_or_init(|| {
                    let pattern = if cfg!(windows) {
                        r#"(?:[a-zA-Z]:[\\/]|\\\\|~[\\/]|/)[^\s\"'<>]+"#
                    } else {
                        r#"(?:~/|/)[^\s\"'<>]+"#
                    };
                    Regex::new(pattern).unwrap()
                })
                .replace_all(&text, |c: &regex::Captures<'_>| {
                    self.removed("private paths");
                    self.fingerprint(&c[0])
                })
                .into_owned();
        }
        text
    }
}

#[cfg(test)]
mod device_id_tests {
    use super::*;

    #[test]
    fn hardware_identifiers_are_hidden_without_strict_mode() {
        let mut report = Redactor::report(false, &Value::Null, Vec::new());
        for id in [
            "bluez_input.AA_Bb_Cc_00_11_22.0",
            "bluez_output.aa_bb_cc_33_44_55.1",
            "AA-BB-CC-00-11-22:output",
            "aa:bb:cc:33:44:55",
            "/dev/v4l/by-id/usb-Camera-private123-video-index0",
            r"\\?\USB#VID_1234&PID_1111#Private123",
            "alsa_input.usb-Interface_PRIVATE123-00.analog-stereo",
            "alsa_output.usb-Interface_PRIVATE123-00.analog-stereo",
        ] {
            let device = report.value(&json!({"id":id}));
            let alias = device["id"].as_str().unwrap();
            assert!(alias.starts_with("[hmac:") && alias.ends_with(']'), "{id}");
            assert_eq!(report.text(id), alias);
            if id.starts_with("bluez_") || id.starts_with("AA-") {
                assert_eq!(
                    report.text(&format!("device {id} unavailable")),
                    format!("device {alias} unavailable")
                );
            }
        }
        assert_eq!(
            report.text("bluez_output.headphones"),
            "bluez_output.headphones"
        );
    }
}

pub fn panic_value(value: &Value) -> Value {
    if let Ok(state) = registry().try_lock() {
        let mut r = Redactor {
            secrets: state.values.clone(),
            pinned: state.pinned.clone(),
            revision: u64::MAX,
            ..Default::default()
        };
        drop(state);
        r.scrub(value)
    } else {
        let trace = value["backtrace"].as_str().unwrap_or("");
        let trace = isocast_protocol::api::home_folder()
            .into_os_string()
            .into_string()
            .ok()
            .filter(|s| !s.is_empty())
            .map_or_else(|| trace.into(), |home| trace.replace(&home, "~"));
        json!({"panic":"payload withheld: redactor busy","thread":value["thread_id"],"backtrace":trace,"launch":value["launch"],"sequence":value["sequence"],"time":value["time"]})
    }
}
fn learn(value: &Value, remember: &mut impl FnMut(&str)) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if sensitive(key) {
                    if let Some(s) = value.as_str().filter(|s| s.len() >= 8) {
                        remember(s);
                    }
                } else {
                    learn(value, remember);
                }
            }
        }
        Value::Array(values) => {
            for v in values {
                learn(v, remember);
            }
        }
        _ => {}
    }
}
