//! The setup's JSON Schema (the one `isocast setup schema` prints, made by
//! the protocol crate itself) read into simple field lists, so source and
//! filter settings get forms without hand-written panels (the web app's
//! lib/schema.js).

use serde_json::{Map, Value, json};

thread_local! {
    static SCHEMA: Value = serde_json::to_value(isocast_protocol::Setup::json_schema())
        .unwrap_or(Value::Null);
}

/// What kind of control a field takes.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldType {
    Color,
    /// One of these values.
    Enum(Vec<Value>),
    Boolean,
    Number {
        integer: bool,
        min: f64,
        max: f64,
    },
    String,
    /// A list of strings (files).
    Strings,
    /// Settings of their own, switched on or off as a whole.
    Object(Vec<Field>),
    /// Anything else, shown as JSON.
    Json,
}

/// One field of a kind's settings.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    /// As setups write it ("line_height").
    pub name: String,
    /// As the interface says it ("Line height").
    pub label: String,
    /// Shown after the value ("ms"), from the name's suffix.
    pub unit: &'static str,
    pub description: String,
    /// It may be left out (the engine's default then applies).
    pub optional: bool,
    pub default: Option<Value>,
    pub ty: FieldType,
}

/// Words in setup names that read better otherwise in the interface.
fn word(w: &str) -> &str {
    match w {
        "fps" => "frame rate",
        "url" => "address",
        "css" => "CSS",
        "id" => "ID",
        "lut" => "LUT",
        "rgb" => "RGB",
        "srgb" => "sRGB",
        other => other,
    }
}

/// "line_height" → "Line height", "fps" → "Frame rate".
pub fn humanize(name: &str) -> String {
    let text = name.split('_').map(word).collect::<Vec<_>>().join(" ");
    let mut chars = text.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// Units at the end of setup names ("attack_ms"), shown beside the value.
const UNITS: &[(&str, &str)] = &[
    ("_ms", "ms"),
    ("_db", "dB"),
    ("_seconds", "s"),
    ("_kbps", "kb/s"),
    ("_gb", "GB"),
];

fn split_unit(name: &str) -> (&str, &'static str) {
    UNITS
        .iter()
        .find(|(suffix, _)| name.ends_with(suffix) && name.len() > suffix.len())
        .map(|(suffix, unit)| (&name[..name.len() - suffix.len()], *unit))
        .unwrap_or((name, ""))
}

/// A range like "−1 to 1" or "1 to 1000" in a description: a number at
/// the start or after a space, "(" or ":", then "to" or "–", then another.
pub fn range_of(description: &str) -> Option<(f64, f64)> {
    let chars: Vec<char> = description.chars().collect();
    let number = |at: usize| -> Option<(f64, usize)> {
        let mut i = at;
        let mut text = String::new();
        if i < chars.len() && (chars[i] == '-' || chars[i] == '\u{2212}') {
            text.push('-');
            i += 1;
        }
        let digits = i;
        while i < chars.len() && chars[i].is_ascii_digit() {
            text.push(chars[i]);
            i += 1;
        }
        if i == digits {
            return None;
        }
        if i + 1 < chars.len() && chars[i] == '.' && chars[i + 1].is_ascii_digit() {
            text.push('.');
            i += 1;
            while i < chars.len() && chars[i].is_ascii_digit() {
                text.push(chars[i]);
                i += 1;
            }
        }
        Some((text.parse().ok()?, i))
    };
    let spaces = |mut i: usize| {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        i
    };
    for start in 0..chars.len() {
        if start > 0 && !(chars[start - 1].is_whitespace() || matches!(chars[start - 1], '(' | ':'))
        {
            continue;
        }
        let Some((min, end)) = number(start) else {
            continue;
        };
        let i = spaces(end);
        let after = if chars[i..].starts_with(&['t', 'o']) {
            i + 2
        } else if chars.get(i) == Some(&'\u{2013}') {
            i + 1
        } else {
            continue;
        };
        if let Some((max, _)) = number(spaces(after)) {
            return Some((min, max));
        }
    }
    None
}

/// `node` with its `$ref`s followed (their properties and requirements
/// merged in) and the name of the last definition it came from.
fn resolve(schema: &Value, node: &Value) -> (Value, String) {
    let mut current = node.clone();
    let mut name = String::new();
    for _ in 0..8 {
        let Some(reference) = current.get("$ref").and_then(Value::as_str) else {
            break;
        };
        name = reference.trim_start_matches("#/$defs/").to_string();
        let def = schema["$defs"].get(&name).cloned().unwrap_or(json!({}));
        let mut rest = current.as_object().cloned().unwrap_or_default();
        rest.remove("$ref");
        let mut merged: Map<String, Value> = def.as_object().cloned().unwrap_or_default();
        let mut properties = def
            .get("properties")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if let Some(own) = rest.get("properties").and_then(Value::as_object) {
            properties.extend(own.clone());
        }
        let mut required: Vec<Value> = def
            .get("required")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        required.extend(
            rest.get("required")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        );
        merged.extend(rest);
        if !properties.is_empty() {
            merged.insert("properties".into(), Value::Object(properties));
        }
        merged.insert("required".into(), Value::Array(required));
        current = Value::Object(merged);
    }
    (current, name)
}

/// The values an enum takes: `enum`, or variants as consts (or groups of
/// them in an `enum`). Of a choice between forms (`anyOf`, like an easing:
/// a name, or a cubic Bézier or steps), the names it may take.
fn enum_options(schema: &Value, node: &Value) -> Option<Vec<Value>> {
    if let Some(values) = node.get("enum").and_then(Value::as_array) {
        return Some(values.clone());
    }
    if let Some(forms) = node.get("anyOf").and_then(Value::as_array) {
        let forms: Vec<Value> = forms.iter().map(|form| resolve(schema, form).0).collect();
        let mut names = forms.iter().filter_map(|form| enum_options(schema, form));
        let objects = forms
            .iter()
            .filter(|form| form.get("type") == Some(&json!("object")))
            .count();
        return match (names.next(), names.next()) {
            (Some(names), None) if objects + 1 == forms.len() => Some(names),
            _ => None,
        };
    }
    let variants = node.get("oneOf")?.as_array()?;
    if variants
        .iter()
        .all(|o| o.get("const").is_some() || o.get("enum").is_some_and(Value::is_array))
    {
        return Some(
            variants
                .iter()
                .flat_map(|o| match o.get("const") {
                    Some(c) => vec![c.clone()],
                    None => o["enum"].as_array().cloned().unwrap_or_default(),
                })
                .collect(),
        );
    }
    None
}

/// One field of a kind's settings.
fn field(schema: &Value, name: &str, raw: &Value, required: bool) -> Field {
    let mut node = raw.clone();
    let mut optional = !required;
    // `Option<T>`: anyOf [T, null], or type [T, "null"].
    if let Some(any) = node.get("anyOf").and_then(Value::as_array).cloned()
        && any.len() == 2
        && any.iter().any(|o| o.get("type") == Some(&json!("null")))
    {
        let mut inner = any
            .iter()
            .find(|o| o.get("type") != Some(&json!("null")))
            .cloned()
            .unwrap_or(json!({}));
        if let Some(d) = raw.get("description") {
            inner["description"] = d.clone();
        }
        node = inner;
        optional = true;
    }
    if let Some(types) = node.get("type").and_then(Value::as_array).cloned()
        && types.contains(&json!("null"))
    {
        let other = types
            .into_iter()
            .find(|t| t != &json!("null"))
            .unwrap_or(Value::Null);
        node["type"] = other;
        optional = true;
    }
    let description = node
        .get("description")
        .or_else(|| raw.get("description"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let (resolved, ref_name) = resolve(schema, &node);
    let (bare, unit) = split_unit(name);
    let default = raw
        .get("default")
        .or_else(|| node.get("default"))
        .filter(|d| !d.is_null())
        .cloned();
    let ty_of = |t: &str| resolved.get("type").and_then(Value::as_str) == Some(t);
    let ty = if ref_name == "Color" {
        FieldType::Color
    } else if let Some(options) = enum_options(schema, &resolved) {
        FieldType::Enum(options)
    } else if ty_of("boolean") {
        FieldType::Boolean
    } else if ty_of("integer") || ty_of("number") {
        let range = range_of(&description);
        FieldType::Number {
            integer: ty_of("integer"),
            min: range
                .map(|r| r.0)
                .or_else(|| resolved.get("minimum").and_then(Value::as_f64))
                .unwrap_or(f64::NEG_INFINITY),
            max: range.map(|r| r.1).unwrap_or(f64::INFINITY),
        }
    } else if ty_of("string") {
        FieldType::String
    } else if ty_of("array")
        && resolve(schema, resolved.get("items").unwrap_or(&json!({})))
            .0
            .get("type")
            == Some(&json!("string"))
    {
        FieldType::Strings
    } else if ty_of("object") && resolved.get("properties").is_some() {
        FieldType::Object(fields_of(schema, &resolved))
    } else {
        FieldType::Json
    };
    Field {
        name: name.to_string(),
        label: humanize(bare),
        unit,
        description,
        optional,
        default,
        ty,
    }
}

/// Fields people look for first come first; the schema lists them
/// alphabetically.
const ORDER: &[&str] = &[
    "display",
    "device",
    "app",
    "title",
    "url",
    "text",
    "file",
    "path",
    "files",
    "font",
    "size",
    "weight",
    "italic",
    "color",
    "key_color_type",
    "key_color",
    "background",
    "align",
    "width",
    "height",
    "fps",
    "cursor",
];

fn fields_of(schema: &Value, node: &Value) -> Vec<Field> {
    let required: Vec<&str> = node
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let rank = |name: &str| ORDER.iter().position(|o| *o == name).unwrap_or(ORDER.len());
    let mut entries: Vec<(&String, &Value)> = node
        .get("properties")
        .and_then(Value::as_object)
        .map(|p| p.iter().filter(|(name, _)| *name != "kind").collect())
        .unwrap_or_default();
    entries.sort_by(|(a, _), (b, _)| rank(a).cmp(&rank(b)).then_with(|| a.cmp(b)));
    entries
        .into_iter()
        .map(|(name, raw)| field(schema, name, raw, required.contains(&name.as_str())))
        .collect()
}

/// The variant of a kind-tagged definition (`Source`, `Filter`,
/// `AudioFilter`, `Transition`) for `kind`, resolved.
fn variant(schema: &Value, def: &str, kind: &str) -> Option<Value> {
    let option = schema["$defs"][def]["oneOf"]
        .as_array()?
        .iter()
        .find(|o| o["properties"]["kind"]["const"] == kind)?
        .clone();
    Some(resolve(schema, &option).0)
}

/// The settings fields of a source kind (`def` "Source"), filter kind
/// ("Filter") or audio filter kind ("AudioFilter").
pub fn kind_fields(def: &str, kind: &str) -> Vec<Field> {
    SCHEMA.with(|schema| {
        variant(schema, def, kind)
            .map(|node| fields_of(schema, &node))
            .unwrap_or_default()
    })
}

/// Every kind of a kind-tagged definition.
pub fn kinds_of(def: &str) -> Vec<String> {
    SCHEMA.with(|schema| {
        schema["$defs"][def]["oneOf"]
            .as_array()
            .map(|options| {
                options
                    .iter()
                    .filter_map(|o| o["properties"]["kind"]["const"].as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    })
}

/// A starting value for a field that must be set.
pub fn start_value(f: &Field) -> Value {
    if let Some(default) = &f.default {
        return default.clone();
    }
    match &f.ty {
        FieldType::Color => json!("#000000"),
        FieldType::Boolean => json!(false),
        FieldType::Number { integer, min, .. } => {
            if min.is_finite() && *min > 0.0 {
                json!(min)
            } else if *integer {
                json!(0)
            } else {
                json!(1)
            }
        }
        FieldType::Enum(options) => options.first().cloned().unwrap_or(Value::Null),
        FieldType::Strings => json!([]),
        FieldType::Object(fields) => Value::Object(
            fields
                .iter()
                .filter(|x| !x.optional)
                .map(|x| (x.name.clone(), start_value(x)))
                .collect(),
        ),
        _ => json!(""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_ranges_read_as_the_web_app_reads_them() {
        assert_eq!(humanize("line_height"), "Line height");
        assert_eq!(humanize("fps"), "Frame rate");
        assert_eq!(humanize("color"), "Color");
        assert_eq!(split_unit("attack_ms"), ("attack", "ms"));
        assert_eq!(range_of("Balance, −1 to 1."), Some((-1.0, 1.0)));
        assert_eq!(range_of("Speed (0.25–4)"), Some((0.25, 4.0)));
        assert_eq!(range_of("A 2x scale"), None);
    }

    #[test]
    fn kinds_have_their_fields() {
        let text: Vec<String> = kind_fields("Source", "text")
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(text.first().map(String::as_str), Some("text"));
        assert!(text.contains(&"font".to_string()));
        let gain = kind_fields("AudioFilter", "gain");
        assert_eq!(gain[0].unit, "dB");
        assert!(kinds_of("Filter").contains(&"color_correction".to_string()));
    }

    #[test]
    fn transition_kinds_have_their_fields() {
        let names = |kind: &str| -> Vec<String> {
            kind_fields("Transition", kind)
                .into_iter()
                .map(|f| f.name)
                .filter(|n| n != "duration_ms")
                .collect()
        };
        assert!(names("cut").is_empty());
        assert!(names("fade").is_empty());
        assert_eq!(names("stinger"), ["file", "cut_point_ms", "matte"]);
        let stinger = kind_fields("Transition", "stinger");
        let cut = stinger.iter().find(|f| f.name == "cut_point_ms").unwrap();
        assert_eq!((cut.label.as_str(), cut.unit), ("Cut point", "ms"));
        assert!(names("fade_to_color").contains(&"color".to_string()));
        // An easing is a name or a cubic Bézier or steps; the form offers
        // the names.
        let swipe = kind_fields("Transition", "swipe");
        let easing = swipe.iter().find(|f| f.name == "easing").unwrap();
        let FieldType::Enum(options) = &easing.ty else {
            panic!("{:?}", easing.ty);
        };
        assert_eq!(
            options,
            &["linear", "ease_in", "ease_out", "ease_in_out"].map(|name| json!(name))
        );
    }
}
