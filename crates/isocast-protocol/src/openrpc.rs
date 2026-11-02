//! The method table as an [OpenRPC 1.3.2](https://spec.open-rpc.org/)
//! document (`isocast api spec --json`, `docs/openrpc.json`).
//!
//! Params come from [`Method`]'s own JSON Schema, so they can't drift from
//! what the engine parses. Results come from [`result`], one entry per
//! method naming the type the daemon serializes; a method without one is a
//! test failure, not a silently untyped result. Schemas are JSON Schema
//! draft-07 (what OpenRPC 1.3.2 uses), with every named type under
//! `components.schemas`.
//!
//! Params are generated for what the engine accepts (serde's deserialize
//! contract) and results for what it sends (the serialize contract). A type
//! whose two schemas differ appears twice: its result-side schema gets a
//! `Result` suffix.

use std::collections::{BTreeMap, BTreeSet};

use schemars::generate::{SchemaGenerator, SchemaSettings};
use schemars::{JsonSchema, Schema, json_schema};
use serde_json::{Map, Value, json};

use crate::api::{self, Method, RpcError};

/// The OpenRPC version this document follows.
pub const OPENRPC_VERSION: &str = "1.3.2";

const SCHEMAS: &str = "#/components/schemas/";

/// The OpenRPC document for this build's method table.
pub fn document() -> Value {
    let mut params_gen = generator().for_deserialize().into_generator();
    let mut results_gen = generator().for_serialize().into_generator();

    params_gen.subschema_for::<Method>();
    // Client-sent lines on `shared_frames` and `frames` connections.
    let shared_control = transformed(&mut params_gen, |g| {
        g.subschema_for::<api::shared::Control>()
    })
    .to_value();
    let frame_rate =
        transformed(&mut params_gen, |g| g.subschema_for::<api::FrameRate>()).to_value();
    let mut params_defs = params_gen.take_definitions(true);
    let method = params_defs
        .remove("Method")
        .expect("Method is a named schema");

    // Results, in method-table order.
    let mut methods = Vec::new();
    let mut results = Vec::new();
    for variant in method["oneOf"].as_array().into_iter().flatten() {
        let Some(name) = variant["properties"]["method"]["const"].as_str() else {
            panic!("a Method variant without a const name: {variant}");
        };
        let result = result(name, &mut results_gen)
            .map(|(label, schema)| (label, transform(&mut results_gen, schema)));
        results.push(result);
        methods.push((name.to_string(), variant.clone()));
    }
    let event = transformed(&mut results_gen, |g| g.subschema_for::<api::Event>()).to_value();
    let shared_event = transformed(&mut results_gen, |g| {
        g.subschema_for::<api::shared::Event>()
    })
    .to_value();
    let mut results_defs = results_gen.take_definitions(true);

    // Merge the two sets of definitions; rename result-side ones that
    // differ (or reference one that differs).
    let mut renamed = BTreeSet::new();
    loop {
        let before = renamed.len();
        for (name, schema) in &results_defs {
            let differs = params_defs.get(name).is_some_and(|p| p != schema);
            if differs || refs(schema).iter().any(|r| renamed.contains(r)) {
                renamed.insert(name.clone());
            }
        }
        if renamed.len() == before {
            break;
        }
    }
    let mut taken: BTreeSet<String> = params_defs
        .keys()
        .chain(results_defs.keys())
        .cloned()
        .collect();
    let mut renames = BTreeMap::new();
    for name in &renamed {
        let mut new = format!("{name}Result");
        let mut n = 2;
        while taken.contains(&new) {
            new = format!("{name}Result{n}");
            n += 1;
        }
        taken.insert(new.clone());
        renames.insert(name.clone(), new);
    }
    let rewrite = |value: &mut Value| rewrite_refs(value, &renames);
    let mut schemas = Map::new();
    for (name, schema) in std::mem::take(&mut params_defs) {
        schemas.insert(name, schema);
    }
    for (name, mut schema) in std::mem::take(&mut results_defs) {
        rewrite(&mut schema);
        let name = renames.get(&name).cloned().unwrap_or(name);
        schemas.entry(name).or_insert(schema);
    }

    let mut method_objects = Vec::new();
    for ((name, variant), result) in methods.into_iter().zip(results) {
        let mut object = Map::new();
        object.insert("name".into(), json!(name));
        if let Some(text) = variant.get("description").and_then(Value::as_str) {
            object.insert("summary".into(), json!(summary(text)));
            object.insert("description".into(), json!(text));
        }
        object.insert("paramStructure".into(), json!("by-name"));
        let (descriptors, exact) = params(&variant, &schemas);
        object.insert("params".into(), Value::Array(descriptors));
        if let Some(exact) = exact {
            object.insert("x-isocast-params".into(), exact);
        }
        match result {
            Some((label, schema)) => {
                let mut schema = schema.to_value();
                rewrite(&mut schema);
                object.insert("result".into(), json!({"name": label, "schema": schema}));
            }
            // Unreachable while every method has an entry (see the test).
            None => {
                object.insert("x-isocast-result".into(), json!("untyped"));
            }
        }
        let stream = match name.as_str() {
            "watch" => Some(json!({
                "description": "After the result line, the connection carries JSON-RPC notifications {\"jsonrpc\":\"2.0\",\"method\":\"event\",\"params\":EVENT} until it closes.",
                "notification": "event",
                "server": with_refs(event.clone(), &rewrite),
            })),
            "frames" => Some(json!({
                "description": "After the result line, the connection carries binary frame records (api::frames: a 24-byte header, then H.264 data) until either side closes it; the client may send FrameRate lines.",
                "client": frame_rate.clone(),
            })),
            "shared_frames" => Some(json!({
                "description": "After the result line, the connection carries shared-surface event lines (the ring once, then a slot per frame); the client sends control lines.",
                "server": with_refs(shared_event.clone(), &rewrite),
                "client": shared_control.clone(),
            })),
            "whep_offer" => Some(json!({
                "description": "The SDP answer establishes WebRTC program H.264 and Opus over DTLS-SRTP. This dedicated signaling connection remains open; whep_close on this connection or disconnect terminates media. No HTTP listener, trickle ICE or renegotiation.",
                "close": "whep_close",
            })),
            _ => None,
        };
        if let Some(stream) = stream {
            object.insert("x-isocast-stream".into(), stream);
        }
        method_objects.push(Value::Object(object));
    }

    json!({
        "openrpc": OPENRPC_VERSION,
        "info": {
            "title": "Isocast engine API",
            "version": crate::hello::PROTOCOL.to_string(),
            "description": "The one method table the Isocast app, CLI, MCP and plugins use. JSON-RPC 2.0, one message per line, over the engine's local socket (a Unix socket on macOS, an owner-only named pipe on Windows) or, for paired engines, mutual TLS 1.3 on TCP 47470. Methods only the engine's own machine may call are refused for remote callers with NOT_PERMITTED. info.version and x-isocast-protocol identify hello's protocol version, not the product release.",
            "license": {"name": "See the Isocast repository"},
        },
        "methods": method_objects,
        "x-isocast-protocol": crate::hello::PROTOCOL,
        "components": {
            "schemas": schemas,
            "errors": errors(),
        },
    })
}

fn generator() -> SchemaSettings {
    let mut settings = SchemaSettings::draft07();
    settings.definitions_path = "/components/schemas".into();
    settings.meta_schema = None;
    settings
}

/// A subschema with the generator's transforms applied (draft-07 forms of
/// tuples and the like); named types are transformed as definitions.
fn transformed(
    g: &mut SchemaGenerator,
    make: impl FnOnce(&mut SchemaGenerator) -> Schema,
) -> Schema {
    let schema = make(g);
    transform(g, schema)
}

fn transform(g: &mut SchemaGenerator, mut schema: Schema) -> Schema {
    for t in g.transforms_mut() {
        t.transform(&mut schema);
    }
    schema
}

fn with_refs(mut value: Value, rewrite: &impl Fn(&mut Value)) -> Value {
    rewrite(&mut value);
    value
}

/// The `$ref`s in a schema that point at components.
fn refs(value: &Value) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    fn walk(value: &Value, out: &mut BTreeSet<String>) {
        match value {
            Value::Object(map) => {
                if let Some(Value::String(r)) = map.get("$ref")
                    && let Some(name) = r.strip_prefix(SCHEMAS)
                {
                    out.insert(name.to_string());
                }
                map.values().for_each(|v| walk(v, out));
            }
            Value::Array(list) => list.iter().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    walk(value, &mut out);
    out
}

fn rewrite_refs(value: &mut Value, renames: &BTreeMap<String, String>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(r)) = map.get_mut("$ref")
                && let Some(new) = r.strip_prefix(SCHEMAS).and_then(|name| renames.get(name))
            {
                *r = format!("{SCHEMAS}{new}");
            }
            map.values_mut().for_each(|v| rewrite_refs(v, renames));
        }
        Value::Array(list) => list.iter_mut().for_each(|v| rewrite_refs(v, renames)),
        _ => {}
    }
}

fn summary(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.find(". ") {
        Some(end) => line[..=end].to_string(),
        None => line,
    }
}

/// A method's params as by-name content descriptors, and the exact schema
/// of its `params` member when the descriptors can't say everything (the
/// params as a whole optional, a choice between shapes, or unknown fields
/// refused).
fn params(variant: &Value, schemas: &Map<String, Value>) -> (Vec<Value>, Option<Value>) {
    let Some(schema) = variant["properties"].get("params") else {
        return (Vec::new(), None);
    };
    let resolve = |node: &Value| -> Value {
        let mut node = node.clone();
        while let Some(name) = node
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix(SCHEMAS))
        {
            node = schemas.get(name).cloned().unwrap_or(Value::Null);
        }
        node
    };
    // Option<T>: the params may be left out entirely.
    let mut optional = false;
    let mut body = resolve(schema);
    if let Some(list) = body.get("anyOf").and_then(Value::as_array).cloned() {
        let present: Vec<_> = list
            .iter()
            .filter(|n| n.get("type") != Some(&json!("null")))
            .collect();
        if present.len() + 1 == list.len() && present.len() == 1 {
            optional = true;
            body = resolve(present[0]);
        }
    }
    // A choice of shapes (an externally tagged enum): each key optional,
    // the choice kept in x-isocast-params.
    let mut choice = false;
    let mut shapes = vec![body.clone()];
    for key in ["oneOf", "anyOf"] {
        if let Some(list) = body.get(key).and_then(Value::as_array) {
            choice = true;
            shapes = list.iter().map(&resolve).collect();
        }
    }
    let mut descriptors = Vec::new();
    let mut seen = BTreeSet::new();
    let mut complete = true;
    for shape in &shapes {
        let Some(props) = shape.get("properties").and_then(Value::as_object) else {
            complete = false;
            continue;
        };
        let required: BTreeSet<&str> = shape
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        for (key, node) in props {
            if !seen.insert(key.clone()) {
                continue;
            }
            let mut d = Map::new();
            d.insert("name".into(), json!(key));
            if let Some(text) = node.get("description") {
                d.insert("description".into(), text.clone());
            }
            d.insert("schema".into(), node.clone());
            if !optional && !choice && required.contains(key.as_str()) {
                d.insert("required".into(), json!(true));
            }
            descriptors.push(Value::Object(d));
        }
    }
    let strict = shapes
        .iter()
        .any(|s| s.get("additionalProperties") == Some(&json!(false)));
    let exact = (optional || choice || strict || !complete).then(|| schema.clone());
    (descriptors, exact)
}

fn errors() -> Value {
    let entry = |code: i64, message: &str| json!({"code": code, "message": message});
    json!({
        "ParseError": entry(RpcError::PARSE_ERROR, "The line isn't JSON."),
        "InvalidRequest": entry(RpcError::INVALID_REQUEST, "The request isn't valid JSON-RPC."),
        "MethodNotFound": entry(RpcError::METHOD_NOT_FOUND, "No such method; the message lists the methods."),
        "InvalidParams": entry(RpcError::INVALID_PARAMS, "The params are wrong; the message names the field."),
        "Failed": entry(RpcError::FAILED, "The method failed; the message says why."),
        "WrongState": entry(RpcError::WRONG_STATE, "The method doesn't apply in the engine's current state."),
        "NotPermitted": entry(RpcError::NOT_PERMITTED, "Not permitted for this caller; the message says which."),
        "Unsupported": entry(RpcError::UNSUPPORTED, "This engine can't do it on this platform or build."),
    })
}

fn of<T: JsonSchema>(g: &mut SchemaGenerator) -> Option<(String, Schema)> {
    Some((T::schema_name().into_owned(), g.subschema_for::<T>()))
}

/// What each method returns, as the daemon serializes it. A new method
/// needs an entry here (the tests fail without one).
pub fn result(method: &str, g: &mut SchemaGenerator) -> Option<(String, Schema)> {
    use crate::{
        data, developer, devices, diagnostics, hello, hotkeys, init, library, machine, nle, obs,
        optimize, plugin, project, projector, remote, replay, roots, virtual_camera, websocket,
    };
    match method {
        "status" => of::<api::Status>(g),
        "devices" => of::<devices::Devices>(g),
        "get" | "watch" => of::<api::Live>(g),
        "plan" => of::<api::Plan>(g),
        "apply" => of::<api::Applied>(g),
        "start" | "split" => of::<api::Started>(g),
        "stop" => of::<api::Report>(g),
        "request_stop" => of::<api::StopAccepted>(g),
        "pause" | "resume" => of::<api::RecordingPause>(g),
        "marker" => of::<api::Marked>(g),
        "refresh_browser" => of::<api::Refreshed>(g),
        "switch_scene" | "preview" | "take" | "clear_preview" | "clear_canvas_preview" => {
            of::<api::Switched>(g)
        }
        "doctor" => of::<api::Doctor>(g),
        "tune" => of::<api::Tuning>(g),
        "snapshot" => of::<api::Snapshot>(g),
        "frame" => of::<api::Frame>(g),
        "frames" => of::<api::FrameStream>(g),
        "shared_frames" => of::<api::SharedFrameStream>(g),
        "engine" => of::<Option<api::EngineInfo>>(g),
        "import_obs" => of::<obs::Imported>(g),
        "init_setup" => of::<init::Initialized>(g),
        "permissions" | "request_permissions" => of::<devices::PermissionsReport>(g),
        "open_permission_settings" => of::<api::Opened>(g),
        "stream_start" | "stream_stop" => of::<api::Streaming>(g),
        "stream_key_set" | "stream_key_clear" => of::<api::StreamKey>(g),
        "stream_destination_add" | "stream_destination_remove" | "stream_destination_set" => {
            of::<api::StreamDestinations>(g)
        }
        "canvas_add" | "canvas_set" | "canvas_remove" => of::<api::Canvases>(g),
        "library" => of::<library::Library>(g),
        "library_get" => of::<library::LibraryDocument>(g),
        "library_create" | "library_rename" | "library_delete" | "library_switch"
        | "library_import" | "library_import_obs" => of::<library::LibraryChange>(g),
        "plugin_list" => of::<plugin::Plugins>(g),
        "plugin_add" | "plugin_remove" | "plugin_set_enabled" => of::<plugin::PluginChange>(g),
        "data_get" => of::<data::DataValues>(g),
        "data_set" | "data_delete" => of::<data::DataRevision>(g),
        "bug_report" => of::<diagnostics::BugReport>(g),
        "logs" => of::<diagnostics::Logs>(g),
        "report_file" => of::<diagnostics::ReportFile>(g),
        "hotkeys_get" | "hotkeys_set" => of::<hotkeys::Hotkeys>(g),
        "hotkeys_actions" => of::<hotkeys::Catalogue>(g),
        // Built as JSON in the daemon (hotkeys_input), not a named type.
        "hotkeys_input" => Some((
            "HotkeysInputResult".into(),
            json_schema!({
                "type": "object",
                "properties": {"queued": {"type": "boolean", "description": "Whether the keys matched a binding and were queued."}},
                "required": ["queued"],
            }),
        )),
        "virtual_camera_start" | "virtual_camera_stop" | "virtual_camera_status" => {
            of::<virtual_camera::VirtualCameraStatus>(g)
        }
        "virtual_camera_remove" => of::<virtual_camera::VirtualCameraRemoval>(g),
        // The stopped session's report, or null if nothing was recording.
        "shutdown" => of::<Option<api::Report>>(g),
        "shutdown_if_idle" => of::<()>(g),
        "encoder_options" => of::<Vec<crate::EncoderOption>>(g),
        "debug_status" | "debug_set" => of::<developer::DeveloperStatus>(g),
        "list_projects" => of::<project::Projects>(g),
        "rename_project" => of::<project::ProjectRenamed>(g),
        "estimate_capacity" => of::<api::CapacityEstimate>(g),
        "obs_discover" => of::<obs::Discovery>(g),
        "speed_test" => of::<optimize::SpeedTest>(g),
        "optimize" => of::<optimize::Optimized>(g),
        "replay_start" | "replay_stop" | "replay_status" => of::<replay::ReplayStatus>(g),
        "replay_save" => of::<replay::ReplaySaved>(g),
        "websocket_status" | "websocket_enable" | "websocket_disable" => {
            of::<websocket::WebsocketStatus>(g)
        }
        "remote_status" | "remote_enable" | "remote_disable" => of::<remote::RemoteStatus>(g),
        "remote_pair_begin" => of::<remote::RemotePairWindow>(g),
        // Never answered over JSON-RPC: always NOT_PERMITTED, pointing at
        // the dedicated TLS-bound pairing exchange.
        "remote_pair_finish" => Some((
            "NoResult".into(),
            json_schema!({
                "description": "Never returned: JSON-RPC calls are refused with NOT_PERMITTED; pairing finishes over the dedicated TLS-bound exchange (`isocast remote pair HOST --code-stdin`).",
                "not": {},
            }),
        )),
        "remote_clients" | "remote_revoke" => of::<Vec<remote::RemotePeer>>(g),
        "projector_open" | "projector_place" => of::<projector::Projector>(g),
        "projector_close" | "projector_list" => of::<projector::Projectors>(g),
        "take_export_nle" => of::<nle::NleExported>(g),
        "machine_profile" | "machine_calibrate" | "machine_cancel" => {
            of::<machine::MachineStatus>(g)
        }
        // Windows: the DX12 adapter LUID as [low, high]; null elsewhere.
        "gpu_adapter" => of::<Option<(u32, i32)>>(g),
        "hello" => of::<hello::Hello>(g),
        "folder_roots" | "folder_roots_set" => of::<roots::FolderRoots>(g),
        "files_roots" => of::<roots::FilesRoots>(g),
        "files_list" => of::<roots::FilesList>(g),
        "whep_offer" => of::<api::WhepAnswer>(g),
        // Null, then the connection closes.
        "whep_close" => of::<()>(g),
        "screen_pick" => of::<api::ScreenPicked>(g),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_version_tracks_the_protocol_not_product_releases() {
        let doc = document();
        assert_eq!(doc["info"]["version"], crate::hello::PROTOCOL.to_string());
        assert_eq!(doc["x-isocast-protocol"], crate::hello::PROTOCOL);
    }

    fn resolve<'a>(doc: &'a Value, reference: &str) -> Option<&'a Value> {
        doc.pointer(reference.strip_prefix('#')?)
    }

    #[test]
    fn every_method_has_a_typed_result() {
        let doc = document();
        let methods = doc["methods"].as_array().unwrap();
        let names: Vec<&str> = methods
            .iter()
            .map(|m| m["name"].as_str().unwrap())
            .collect();
        let table: Vec<&str> = api::method_names().iter().map(String::as_str).collect();
        assert_eq!(
            names, table,
            "one OpenRPC method per Method, in table order"
        );
        let untyped: Vec<&str> = methods
            .iter()
            .filter(|m| m.get("result").is_none())
            .map(|m| m["name"].as_str().unwrap())
            .collect();
        assert!(
            untyped.is_empty(),
            "add these methods' result types to openrpc::result: {untyped:?}"
        );
        // No result entry is dead: each name is a method.
        let mut g = generator().into_generator();
        for name in ["status", "folder_roots_set"] {
            assert!(result(name, &mut g).is_some());
        }
        assert!(result("no_such_method", &mut g).is_none());
    }

    #[test]
    fn every_ref_resolves_and_names_are_pointer_safe() {
        let doc = document();
        let schemas = doc["components"]["schemas"].as_object().unwrap();
        for name in schemas.keys() {
            assert!(
                name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "component name {name:?} would need JSON Pointer escaping"
            );
        }
        let mut stack = vec![&doc];
        let mut count = 0;
        while let Some(value) = stack.pop() {
            match value {
                Value::Object(map) => {
                    if let Some(Value::String(r)) = map.get("$ref") {
                        count += 1;
                        assert!(r.starts_with(SCHEMAS), "{r} is outside components.schemas");
                        assert!(resolve(&doc, r).is_some(), "{r} doesn't resolve");
                    }
                    stack.extend(map.values());
                }
                Value::Array(list) => stack.extend(list),
                _ => {}
            }
        }
        assert!(count > 0);
    }

    #[test]
    fn schemas_are_draft_07() {
        let text = document().to_string();
        for word in [
            "$defs",
            "prefixItems",
            "unevaluatedProperties",
            "dependentRequired",
            "$schema",
        ] {
            assert!(
                !text.contains(&format!("\"{word}\"")),
                "{word} isn't draft-07 (or belongs at the root)"
            );
        }
    }

    #[test]
    fn params_describe_what_the_engine_parses() {
        let doc = document();
        let method = |name: &str| {
            doc["methods"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["name"] == name)
                .unwrap()
                .clone()
        };
        let status = method("status");
        assert_eq!(status["params"], json!([]));
        // The serialized Status (`Status`, or `StatusResult` where the
        // engine's output differs from what it would parse).
        let status_ref = status["result"]["schema"]["$ref"].as_str().unwrap();
        assert!(
            status_ref.starts_with(&format!("{SCHEMAS}Status")),
            "{status_ref}"
        );
        assert!(resolve(&doc, status_ref).unwrap()["properties"]["state"].is_object());
        // A struct variant: its field, required.
        let marker = method("marker");
        assert_eq!(marker["params"][0]["name"], "label");
        assert_eq!(marker["params"][0]["required"], true);
        // Optional params: nothing required, the exact schema kept.
        let get = method("get");
        assert!(
            get["params"]
                .as_array()
                .unwrap()
                .iter()
                .all(|d| d.get("required").is_none())
        );
        assert!(get.get("x-isocast-params").is_some());
        // A choice of shapes (Change): each key described.
        let plan = method("plan");
        assert!(!plan["params"].as_array().unwrap().is_empty());
        assert!(plan.get("x-isocast-params").is_some());
        // Streams say what follows the result.
        assert!(method("watch")["x-isocast-stream"]["server"]["$ref"].is_string());
        // Tuples become draft-07 `items` arrays.
        let gpu = method("gpu_adapter")["result"]["schema"].to_string();
        assert!(gpu.contains("\"items\""), "{gpu}");
    }

    #[test]
    fn every_method_with_params_describes_them() {
        let doc = document();
        let table = serde_json::to_value(schemars::schema_for!(Method)).unwrap();
        for (variant, m) in table["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .zip(doc["methods"].as_array().unwrap())
        {
            assert_eq!(variant["properties"]["method"]["const"], m["name"]);
            let takes = variant["properties"].get("params").is_some();
            let described = !m["params"].as_array().unwrap().is_empty();
            let exact = m.get("x-isocast-params").is_some();
            assert_eq!(takes, described || exact, "{}", m["name"]);
        }
    }
}
