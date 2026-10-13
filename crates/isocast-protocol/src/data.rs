//! The data store (scenes as code, 5.5): runtime values that scenes show,
//! kept by the engine apart from the setup and addressed by key. Text can
//! show a key's value (`data = "score.home"`), so `isocast data set` (or
//! an agent's `set_data`) changes what the program shows without a setup
//! change. Values set with `persist` are kept with the scene collection
//! and come back after a restart; the rest last as long as isocastd.

use std::collections::BTreeMap;
use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::Percentiles;

/// The longest a key may be, in characters.
pub const MAX_KEY_LEN: usize = 128;
/// The most one value may take, as JSON (64 KB).
pub const MAX_VALUE_BYTES: usize = 64 * 1024;
/// The most all keys and values may take together, as JSON (16 MB).
pub const MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;

/// What a key must look like, for messages.
pub const KEY_RULE: &str = "use lowercase letters, digits, dashes and underscores, in parts \
                            joined by dots (like score.home), at most 128 characters";

/// Why `key` isn't a data key, if it isn't: dotted ids (`score.home`,
/// `twitch.chat`) of lowercase letters, digits, dashes and underscores, at
/// most [`MAX_KEY_LEN`] characters.
pub fn key_problem(key: &str) -> Option<String> {
    let part = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    };
    if key.chars().count() > MAX_KEY_LEN {
        return Some(format!(
            "{:?} is {} characters long; data keys are at most {MAX_KEY_LEN}",
            truncated(key),
            key.chars().count()
        ));
    }
    (!key.split('.').all(part)).then(|| format!("{key:?} is not a data key: {KEY_RULE}"))
}

/// The first characters of a long key, for messages.
fn truncated(key: &str) -> String {
    let mut short: String = key.chars().take(40).collect();
    short.push('…');
    short
}

/// Whether `key` is `prefix` or under it: `score` covers `score` and
/// `score.home` but not `scoreboard`; a prefix ending in a dot (`score.`)
/// covers the keys that start with it.
pub fn under(key: &str, prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    if prefix.ends_with('.') {
        return key.starts_with(prefix);
    }
    key.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
}

/// The text a value shows as bound text: strings as they are, numbers
/// formatted, nothing for a key that isn't set (or is null), anything else
/// as JSON.
pub fn text_of(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => number.to_string(),
        Some(other) => other.to_string(),
    }
}

/// The size a value takes, as compact JSON: what the limits count.
pub fn value_bytes(value: &Value) -> usize {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    let _ = serde_json::to_writer(&mut count, value);
    count.0
}

/// Parameters of `data_get`: every key, or those named, or those under a
/// prefix.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DataGetParams {
    /// The keys to read; every key if neither this nor `prefix` is given.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
    /// Or the keys under this prefix: `score` reads `score` and
    /// `score.home`, not `scoreboard`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
}

/// Parameters of `data_set`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DataSetParams {
    /// Keys and their new values (any JSON), set together: all of them or,
    /// if one is refused, none.
    pub values: BTreeMap<String, Value>,
    /// Keep these keys with the scene collection, so they come back after
    /// a restart (`true`), or stop keeping them (`false`). If absent, a
    /// key keeps what it had; a new key isn't kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persist: Option<bool>,
}

/// Parameters of `data_delete`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DataDeleteParams {
    /// The keys to delete (kept ones from the collection's file too).
    pub keys: Vec<String>,
}

/// Values from the data store.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DataValues {
    /// The store's revision: it counts changes, so a client can tell
    /// whether it has seen the newest values (it starts higher than any
    /// before at each start of isocastd, so it never repeats).
    pub revision: u64,
    pub values: BTreeMap<String, Value>,
    /// Which of them are kept with the scene collection.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub persisted: Vec<String>,
    /// Keys asked for that aren't set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<String>,
}

/// What a `data_set` or `data_delete` did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DataRevision {
    /// The store's revision after it (the same as before if nothing
    /// changed).
    pub revision: u64,
    /// The keys whose value changed, or that started or stopped being
    /// kept; setting a key to the value it has changes nothing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed: Vec<String>,
}

/// The data store, for `status`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DataStatus {
    pub revision: u64,
    pub keys: u64,
    /// What its keys and values take, as JSON (at most 16 MB).
    pub bytes: u64,
    /// Keys kept with the scene collection.
    pub persisted: u64,
    /// `data_set` and `data_delete` calls that changed something.
    pub writes: u64,
    /// `data_changed` events sent (at most 20 a second).
    pub events: u64,
    /// How long recent `data_set` calls took in the store (parsing the
    /// request not included), in microseconds, over the last 1,000.
    #[serde(default)]
    pub set_us: Percentiles,
    /// Where kept keys are saved: beside the scene collection's file. None
    /// before there is a collection (they are saved once there is).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<PathBuf>,
    /// Why kept keys couldn't be saved, if the last try failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save_error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_dotted_ids() {
        for good in ["title", "score.home", "twitch.chat", "a-b_c.d9", "x.y.z"] {
            assert_eq!(key_problem(good), None, "{good}");
        }
        for bad in ["", "Score", "score.", ".score", "a..b", "a b", "a/b", "é"] {
            let problem = key_problem(bad).unwrap();
            assert!(problem.contains("lowercase"), "{bad}: {problem}");
        }
        let long = "a".repeat(129);
        assert!(key_problem(&long).unwrap().contains("at most 128"));
        assert_eq!(key_problem(&"a".repeat(128)), None);
    }

    #[test]
    fn prefixes_cover_whole_parts() {
        assert!(under("score", "score"));
        assert!(under("score.home", "score"));
        assert!(!under("scoreboard", "score"));
        assert!(under("score.home", "score."));
        assert!(!under("score", "score."));
        assert!(under("anything", ""));
    }

    #[test]
    fn values_show_as_text() {
        assert_eq!(text_of(None), "");
        assert_eq!(text_of(Some(&Value::Null)), "");
        assert_eq!(text_of(Some(&"Hello".into())), "Hello");
        assert_eq!(text_of(Some(&3.into())), "3");
        assert_eq!(text_of(Some(&serde_json::json!(2.5))), "2.5");
        assert_eq!(text_of(Some(&true.into())), "true");
        assert_eq!(
            text_of(Some(&serde_json::json!({"a": [1]}))),
            r#"{"a":[1]}"#
        );
        assert_eq!(value_bytes(&serde_json::json!({"a": [1]})), 9);
    }

    #[test]
    fn get_params_are_optional_and_strict() {
        let params: DataGetParams = serde_json::from_str("{}").unwrap();
        assert_eq!(params, DataGetParams::default());
        assert!(serde_json::from_str::<DataGetParams>(r#"{"key":"a"}"#).is_err());
        let set: DataSetParams =
            serde_json::from_str(r#"{"values":{"title":"Hi","n":3},"persist":true}"#).unwrap();
        assert_eq!(set.values.len(), 2);
        assert_eq!(set.persist, Some(true));
    }
}
