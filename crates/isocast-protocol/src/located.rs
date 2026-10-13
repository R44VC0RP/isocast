//! Errors that say which field they are about.
//!
//! serde_path_to_error names the path of a bad field (`canvas.fps`), but
//! not inside serde's `flatten` or its internally tagged enums, which read
//! their fields into a buffer first: a bad swipe direction came out as
//! `transition: unknown variant ...`, and a bad init option with no path at
//! all. The types that use them read their fields again after an error to
//! find the field, and pass its path out inside the message (``"`direction`:
//! unknown variant ..."``), which the readers here join onto the path
//! further out (`transition.direction: unknown variant ...`).

use std::fmt;

use serde::de::DeserializeOwned;
use serde_json::Value;

/// A problem reading something, and where: the path of the field it is
/// about, like `transition.direction`, or empty for the whole thing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    pub path: String,
    pub message: String,
}

impl fmt::Display for Located {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.path.as_str() {
            "" => f.write_str(&self.message),
            path => write!(f, "{path}: {}", self.message),
        }
    }
}

impl std::error::Error for Located {}

impl Located {
    /// The same problem, inside the field at `outer`.
    pub fn within(mut self, outer: &str) -> Self {
        self.path = join(outer, &self.path);
        self
    }

    /// From the path serde_path_to_error found and the error's message,
    /// which may carry the path of a field further in.
    pub fn new(path: &serde_path_to_error::Path, message: &str) -> Self {
        let outer = path.to_string();
        let outer = if outer == "." { "" } else { &outer };
        let (inner, message) = split(message);
        Self {
            path: join(outer, inner),
            message: message.to_string(),
        }
    }

    /// As any deserializer's error, carrying the path for a reader further
    /// out to join onto its own.
    pub fn into_error<E: serde::de::Error>(self) -> E {
        match self.path.as_str() {
            "" => E::custom(self.message),
            path => E::custom(format_args!("`{path}`: {}", self.message)),
        }
    }
}

/// A message's path, if it carries one (`` `a.b`: message ``), and the
/// message without it.
fn split(message: &str) -> (&str, &str) {
    message
        .strip_prefix('`')
        .and_then(|rest| rest.split_once("`: "))
        .filter(|(path, _)| !path.is_empty() && !path.contains([' ', '`']))
        .unwrap_or(("", message))
}

fn join(outer: &str, inner: &str) -> String {
    match (outer, inner) {
        ("", inner) => inner.to_string(),
        (outer, "") => outer.to_string(),
        (outer, inner) if inner.starts_with('[') => format!("{outer}{inner}"),
        (outer, inner) => format!("{outer}.{inner}"),
    }
}

/// Read a `T` from `value`; an error names the field it is about.
pub fn from_value<T: DeserializeOwned>(value: &Value) -> Result<T, Located> {
    read(value, |de| T::deserialize(de))
}

/// Read `value` with `read` (like a `remote = "Self"` derived
/// `deserialize`); an error names the field it is about.
pub fn read<T>(
    value: &Value,
    read: impl FnOnce(serde_path_to_error::Deserializer<'_, '_, &Value>) -> Result<T, serde_json::Error>,
) -> Result<T, Located> {
    let mut track = serde_path_to_error::Track::new();
    let result = read(serde_path_to_error::Deserializer::new(value, &mut track));
    result.map_err(|err| Located::new(&track.path(), &err.to_string()))
}

/// Names as a list for a message: `` `a`, `b`, `c` ``.
pub fn one_of<S: AsRef<str>>(names: &[S]) -> String {
    let names: Vec<String> = names.iter().map(|n| format!("`{}`", n.as_ref())).collect();
    names.join(", ")
}

/// Where an object with a `kind` (one of `kinds`) flattened into it went
/// wrong. serde names an error in a field of its own; one in its kind's
/// fields comes out without a path, so `locate` reads those again on their
/// own to find it.
pub(crate) fn kinded(
    err: Located,
    value: &Value,
    kinds: &[&str],
    locate: fn(&str, &Value) -> Option<Located>,
) -> Located {
    if !err.path.is_empty() {
        return err;
    }
    let Some(object) = value.as_object() else {
        return Located {
            path: String::new(),
            message: format!(
                "{}; give an object with a `kind`, one of {}",
                err.message,
                one_of(kinds)
            ),
        };
    };
    match object.get("kind") {
        None => Located {
            path: String::new(),
            message: format!("missing field `kind`, expected one of {}", one_of(kinds)),
        },
        Some(Value::String(kind)) if kinds.contains(&kind.as_str()) => {
            locate(kind, value).unwrap_or(err)
        }
        Some(_) => err.within("kind"),
    }
}

/// Read a struct with another flattened into it, with `read`: refuse a
/// field it doesn't have, naming those it does (`fields`, which serde's
/// `deny_unknown_fields` can't list through `flatten`), and find the field
/// of an error in the flattened part, which serde can't either, with
/// `locate`.
pub(crate) fn flattened<T>(
    value: &Value,
    fields: &[String],
    read: impl FnOnce(serde_path_to_error::Deserializer<'_, '_, &Value>) -> Result<T, serde_json::Error>,
    locate: impl FnOnce(&Value) -> Option<Located>,
) -> Result<T, Located> {
    if let Some(unknown) = value
        .as_object()
        .and_then(|object| object.keys().find(|key| !fields.contains(key)))
    {
        return Err(Located {
            path: unknown.clone(),
            message: format!(
                "unknown field `{unknown}`, expected one of {}",
                one_of(fields)
            ),
        });
    }
    self::read(value, read).map_err(|err| match err.path.as_str() {
        "" if value.is_object() => locate(value).unwrap_or(err),
        _ => err,
    })
}

/// `value` without the fields `names`, to read a flattened part that
/// refuses fields it doesn't have.
pub(crate) fn without(value: &Value, names: &[&str]) -> Value {
    let mut value = value.clone();
    if let Some(object) = value.as_object_mut() {
        object.retain(|key, _| !names.contains(&key.as_str()));
    }
    value
}

/// Serde implementations for a struct with another flattened into it,
/// derived with `#[serde(remote = "Self")]`, whose errors name their field
/// (see [`flattened`]); `$locate` finds an error's field in the flattened
/// part.
macro_rules! flattened_fields {
    ($type:ident, $locate:expr) => {
        impl serde::Serialize for $type {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                $type::serialize(self, serializer)
            }
        }

        impl<'de> serde::Deserialize<'de> for $type {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                static FIELDS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
                let fields = FIELDS.get_or_init($crate::fields::names::<$type>);
                let value = <serde_json::Value as serde::Deserialize>::deserialize(deserializer)?;
                $crate::located::flattened(&value, fields, |de| $type::deserialize(de), $locate)
                    .map_err($crate::located::Located::into_error)
            }
        }
    };
}

pub(crate) use flattened_fields;
