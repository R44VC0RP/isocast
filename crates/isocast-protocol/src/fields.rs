//! Fields a setup gives that its kinds don't have.
//!
//! Sources, filters, audio filters and transitions keep their settings in
//! a kind-tagged enum flattened into them, and serde's `flatten` ignores
//! fields it doesn't know, so a gain filter written `db = -6` (the field is
//! `gain_db`) used to do nothing, silently. These four types keep such
//! fields (as `unknown`, through every TOML and JSON round trip, the API
//! included), and validation reports each one by its path with the fields
//! the kind does have. Each kind's fields come from its JSON Schema, so the
//! list can't drift from the settings types.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde_json::Value;

use crate::Issues;

/// Fields an object had that its kind doesn't, by name.
pub type UnknownFields = BTreeMap<String, Value>;

/// The field names of `T`, as setups write them, from its JSON Schema
/// (flattened fields included).
pub(crate) fn names<T: JsonSchema>() -> Vec<String> {
    let schema = schemars::SchemaGenerator::default().into_root_schema_for::<T>();
    let root = schema.as_value();
    let mut out = Vec::new();
    collect(root, root, &mut out);
    out
}

fn collect(schema: &Value, root: &Value, out: &mut Vec<String>) {
    if let Some(name) = schema
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|r| r.strip_prefix("#/$defs/"))
        && let Some(target) = root.get("$defs").and_then(|defs| defs.get(name))
    {
        collect(target, root, out);
    }
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for name in properties.keys() {
            if !out.contains(name) {
                out.push(name.clone());
            }
        }
    }
    for key in ["allOf", "anyOf", "oneOf"] {
        for item in schema
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            collect(item, root, out);
        }
    }
}

/// Keep only the unknown fields of what a kind's settings were read from:
/// drop the `kind` tag and the kind's own fields.
pub(crate) fn keep_unknown(fields: &mut UnknownFields, known: &[String]) {
    fields.remove("kind");
    for name in known {
        fields.remove(name);
    }
}

/// Report each unknown field of the object at `path`, a `what` (like "a
/// gain audio filter") whose kind has the fields `own`, besides the fields
/// every such object has (`common`).
pub(crate) fn report(
    issues: &mut Issues,
    path: &str,
    unknown: &UnknownFields,
    what: &str,
    own: &[String],
    common: &[&str],
) {
    for field in unknown.keys() {
        let mut message = format!("{field:?} is not a field of {what}");
        let candidates = own.iter().map(String::as_str).chain(common.iter().copied());
        match closest(field, candidates) {
            Some(guess) => message.push_str(&format!("; did you mean {guess:?}? ")),
            // A patch that meant to remove the item.
            None if field.contains("remove") || field.contains("delete") => message.push_str(
                "; to remove an item with a patch, give {\"id\": ..., \"remove\": true}. ",
            ),
            None => message.push_str(". "),
        }
        if own.is_empty() {
            message.push_str(&format!(
                "It has no settings of its own; its fields: {}",
                common.join(", ")
            ));
        } else {
            message.push_str(&format!(
                "Its fields: {} (and {})",
                own.join(", "),
                common.join(", ")
            ));
        }
        issues.add(format!("{path}.{field}"), message);
    }
}

/// The candidate `field` was most likely meant to be: one a typo or two
/// away, or one containing it (`db` for `gain_db`) or contained in it.
fn closest<'a>(field: &str, candidates: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let field = field.to_lowercase();
    candidates
        .filter_map(|candidate| {
            let distance = edit_distance(&field, candidate);
            let near = distance <= field.len().div_ceil(3).max(1);
            let part =
                field.len() >= 2 && (candidate.contains(&field) || field.contains(candidate));
            (near || part).then_some((distance, candidate))
        })
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, candidate)| candidate)
}

/// Levenshtein distance.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (above + 1)
                .min(row[j] + 1)
                .min(diagonal + usize::from(ca != *cb));
            diagonal = above;
        }
    }
    row[b.len()]
}

/// Serde implementations for a type with a kind flattened into it and an
/// `unknown` field catching the rest: derived with `#[serde(remote =
/// "Self")]`, then after reading, `unknown` keeps only what the kind
/// (`$kind`, with a `fields()` method) doesn't have. An error names its
/// field, in the kind's fields too (the kind enum `$kinds` has `NAMES` and
/// `locate`; see [`crate::located`]).
macro_rules! keep_unknown_fields {
    ($type:ident, $kind:ident, $kinds:ident) => {
        impl serde::Serialize for $type {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                $type::serialize(self, serializer)
            }
        }

        impl<'de> serde::Deserialize<'de> for $type {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = <serde_json::Value as serde::Deserialize>::deserialize(deserializer)?;
                let mut read =
                    $crate::located::read(&value, |de| $type::deserialize(de)).map_err(|err| {
                        $crate::located::kinded(err, &value, $kinds::NAMES, $kinds::locate)
                            .into_error()
                    })?;
                $crate::fields::keep_unknown(&mut read.unknown, read.$kind.fields());
                Ok(read)
            }
        }
    };
}

pub(crate) use keep_unknown_fields;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guesses_are_near_misses_or_parts() {
        let fields = ["gain_db", "ratio", "threshold_db", "color"];
        let guess = |field| closest(field, fields.iter().copied());
        assert_eq!(guess("db"), Some("gain_db"));
        assert_eq!(guess("ratoi"), Some("ratio"));
        assert_eq!(guess("colour"), Some("color"));
        assert_eq!(guess("speed"), None);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
    }

    #[test]
    fn unknown_fields_survive_round_trips_and_are_reported_by_path() {
        let setup = crate::Setup::from_toml(
            r##"
            version = 1
            name = "t"
            canvas = { width = 1280, height = 720, fps = 30 }
            transition = { kind = "swipe", direction = "up", directon = "up" }
            [[sources]]
            id = "mic"
            kind = "tone"
            frequency = 440
            audio_filters = [{ kind = "gain", gain_db = 3, db = -6 }]
            [[sources]]
            id = "pattern"
            kind = "test_pattern"
            filters = [{ kind = "chroma_key", gamma = 0.1, spil = 3 }]
            [recording]
            folder = "."
            program = { enabled = false, encoder = { codec = "hevc" } }
            "##,
        )
        .unwrap();
        // Known fields stay out of `unknown`, flattened ones included.
        assert!(setup.sources[0].unknown.is_empty());
        assert_eq!(setup.sources[0].audio_filters[0].unknown.len(), 1);
        assert_eq!(setup.sources[1].filters[0].unknown.len(), 1);
        // As the API sends it: JSON and back, and TOML and back.
        let json = serde_json::to_string(&setup).unwrap();
        assert_eq!(crate::Setup::from_json(&json).unwrap(), setup);
        assert_eq!(
            crate::Setup::from_toml(&setup.to_toml().unwrap()).unwrap(),
            setup
        );
        let paths: Vec<String> = setup.validate().into_iter().map(|i| i.path).collect();
        assert_eq!(
            paths,
            [
                "sources.pattern.filters[0].spil",
                "sources.mic.audio_filters[0].db",
                "transition.directon",
            ]
        );
    }
}
