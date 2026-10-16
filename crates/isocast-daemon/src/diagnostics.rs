//! Control/event-rate hooks; no per-frame or per-audio-buffer log writes.
use crate::Daemon;
use isocast_diagnostics::{emit, emit_developer, redact::Redactor};
use isocast_protocol::api::{Event, Response, RpcError};
use isocast_protocol::diagnostics::{BugReport, BugReportParams};
use serde_json::{Value, json};
use std::sync::{Arc, Weak};

use std::time::{Duration, Instant};
pub(crate) static CONNECTION_PANIC: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub struct ApiCall {
    started: Instant,
    request: Value,
    client: Value,
    done: bool,
}
impl ApiCall {
    pub fn new(line: &str, client: &Value) -> Self {
        // Malformed JSON may contain credentials: don't retain its raw bytes.
        let request = serde_json::from_str::<Value>(line)
            .unwrap_or_else(|_| json!({"invalid_json":true,"bytes":line.len()}));
        let request = Redactor::default().value(&request);
        Self {
            started: Instant::now(),
            request,
            client: client.clone(),
            done: false,
        }
    }
    pub fn response(&mut self, response: &Response) {
        self.response_bytes(response, 0);
    }
    pub fn response_bytes(&mut self, response: &Response, bytes: usize) {
        let method = self.request["method"].as_str().unwrap_or("invalid");
        if response.error.is_none() && isocast_diagnostics::poll::is_poll(method) {
            let ms = self.started.elapsed().as_secs_f64() * 1000.0;
            isocast_diagnostics::poll::record(method, &self.client, ms, bytes);
            // Developer mode: every call on its own, without the (large,
            // frequent) payloads the summary leaves out too.
            if isocast_diagnostics::developer() {
                emit_developer(
                    "debug",
                    "api",
                    "request completed",
                    json!({"request":self.request,"response_bytes":bytes,"duration_ms":ms,"client":self.client}),
                );
            }
            self.done = true;
        } else if response.error.is_none() && matches!(method, "bug_report" | "snapshot") {
            self.value(json!({"success":true,"response_bytes":bytes}));
        } else {
            self.value(serde_json::to_value(response).unwrap_or_default());
        }
    }
    pub fn value(&mut self, value: Value) {
        let error = value.get("error").is_some_and(|e| !e.is_null());
        let value = match self.request["method"].as_str() {
            // The bundle and log reader already contain diagnostics; logging
            // their payload would recursively copy all previous logs.
            Some("bug_report" | "logs") if !error => {
                json!({"summary":"diagnostic payload not duplicated","success":true})
            }
            Some("frame" | "snapshot") if !error => {
                json!({"summary":"image payload omitted","success":true})
            }
            _ => value,
        };
        if !error && !isocast_diagnostics::poll::permit(&self.client) {
            // Over the ordinary log's per-peer rate: developer mode keeps it.
            if isocast_diagnostics::developer() {
                emit_developer(
                    "debug",
                    "api",
                    "request completed",
                    json!({"request":self.request,"response":value,"duration_ms":self.started.elapsed().as_secs_f64()*1000.0,"client":self.client,"over_rate":true}),
                );
            }
            self.done = true;
            return;
        }
        emit(
            if error { "error" } else { "info" },
            "api",
            "request completed",
            json!({"request":self.request,"response":value,"duration_ms":self.started.elapsed().as_secs_f64()*1000.0,"client":self.client}),
        );
        self.done = true;
    }
}
impl Drop for ApiCall {
    fn drop(&mut self) {
        if !self.done {
            emit(
                "warn",
                "api",
                "connection ended before response",
                json!({"request":self.request,"client":self.client,"duration_ms":self.started.elapsed().as_secs_f64()*1000.0}),
            );
        }
    }
}

/// Flushes the peer's omitted-record count when its connection ends.
pub struct PeerClosed(pub Value);
impl Drop for PeerClosed {
    fn drop(&mut self) {
        isocast_diagnostics::poll::peer_closed(&self.0);
    }
}

pub fn event(event: &Event) {
    if matches!(event, Event::Levels { .. }) {
        return;
    }
    let fields = serde_json::to_value(event).unwrap_or_default();
    let failed = fields["error"].is_string()
        || fields["status"]["error"].is_string()
        || fields["stream"]["state"] == "failed"
        || fields["report"]["errors"]
            .as_array()
            .is_some_and(|e| !e.is_empty());
    let level = if failed {
        "error"
    } else if matches!(event, Event::Warning { .. }) {
        "warn"
    } else {
        "info"
    };
    emit(level, "event", "engine event", fields);
}

pub fn start(daemon: &Arc<Daemon>) {
    crate::debug::start(daemon);
    let weak = Arc::downgrade(daemon);
    isocast_diagnostics::native::on_warning(move |message| {
        if let Some(daemon) = weak.upgrade() {
            daemon.publish(Event::Warning { message });
        }
    });
    let weak = Arc::downgrade(daemon);
    isocast_diagnostics::on_worker_warning(move |message| {
        if let Some(daemon) = weak.upgrade() {
            daemon.worker_failed(message);
        }
    });
    let health = isocast_diagnostics::health();
    if let Some(error) = health["init_error"].as_str() {
        daemon.publish(Event::Warning {
            message: format!("diagnostics unavailable; using stderr: {error}"),
        });
    }
    for previous in isocast_diagnostics::previous_abnormal() {
        daemon.publish(Event::Warning {
            message: format!(
                "previous isocastd launch ended abnormally: {}; use isocast report",
                previous["launch"].as_str().unwrap_or("unknown")
            ),
        });
    }
    let weak = Arc::downgrade(daemon);
    let _ = std::thread::Builder::new()
        .name("diagnostics".into())
        .spawn(move || {
            if let Some(d) = weak.upgrade() {
                d.contain("diagnostics sampler", || timeline(Arc::downgrade(&d)));
            }
        });
    // Explicit isolated, debug-only fault injection, never a public method.
    if (cfg!(debug_assertions) || std::env::var_os("ISOCAST_SOCKET").is_some())
        && let Ok(seconds) = std::env::var("ISOCAST_DEBUG_PANIC_AFTER")
            .unwrap_or_default()
            .parse::<u64>()
    {
        let daemon = daemon.clone();
        let _ = std::thread::Builder::new()
            .name("debug fault".into())
            .spawn(move || {
                std::thread::sleep(Duration::from_secs(seconds));
                match std::env::var("ISOCAST_DEBUG_PANIC_MODE").as_deref() {
                    Ok("native_exit") => {
                        use std::io::Write;
                        let text = std::env::var("ISOCAST_DEBUG_NATIVE_LINE")
                            .unwrap_or_else(|_| "native fatal marker\n".into());
                        let _ = std::io::stderr().write_all(text.as_bytes());
                        // Simulate an uncatchable native termination without
                        // running Rust destructors; never used for panics.
                        unsafe { libc::_exit(71) }
                    }
                    Ok("connection") => {
                        CONNECTION_PANIC.store(true, std::sync::atomic::Ordering::SeqCst)
                    }
                    // A panic mid-change to the control state: it may be
                    // half-updated, so recordings end by emergency stop.
                    Ok("state") => daemon.contain("debug broken control state", || {
                        let _change = daemon.state.lock();
                        panic!("requested shared-state break");
                    }),
                    // A panic while only looking at it (housekeeping, like
                    // the sampler): recordings and the stream keep running.
                    Ok("state_read") => daemon.contain("debug control-state reader", || {
                        let _look = daemon.state.read();
                        panic!("requested panic while reading shared state");
                    }),
                    _ => daemon.contain("debug worker", || {
                        panic!("ISOCAST_DEBUG_PANIC_AFTER: requested diagnostic worker panic")
                    }),
                }
            });
    }
}

fn timeline(weak: Weak<Daemon>) {
    #[cfg(target_os = "macos")]
    let mut sampler = isocast_platform_macos::diagnostics::Sampler::default();
    #[cfg(target_os = "macos")]
    let mut device_changes = isocast_platform_macos::diagnostics::DeviceChanges::default();
    let mut inventory = Value::Null;
    let mut permissions = Value::Null;
    let mut sampler_cpu_us = 0.0;
    let mut count = 0u64;
    let mut next = Instant::now() + Duration::from_secs(1);
    loop {
        std::thread::sleep(next.saturating_duration_since(Instant::now()));
        next += Duration::from_secs(1);
        let Some(daemon) = weak.upgrade() else { return };
        let started = Instant::now();
        let cpu = thread_cpu();
        let status = daemon.status();
        crate::debug::tick(&daemon, &status);
        let folder = {
            let state = daemon.state.read();
            status
                .files
                .first()
                .and_then(|f| f.parent())
                .map(std::path::Path::to_owned)
                .or_else(|| {
                    state
                        .setup
                        .as_ref()
                        .map(|s| crate::expand_home(&s.recording.folder))
                })
                .unwrap_or_else(isocast_diagnostics::logs_folder)
        };
        #[cfg(target_os = "macos")]
        {
            let system = sampler.sample(&folder);
            if status.engine.is_some() || count.is_multiple_of(5) {
                let health = isocast_diagnostics::health();
                emit(
                    "info",
                    "metrics",
                    "engine metrics",
                    json!({"system":system,"engine":status.engine,"recording":status.live,"stream":status.stream,
                    "sampler_previous_cpu_us":sampler_cpu_us,"sampler_collection_wall_us":started.elapsed().as_micros(),
                    "encoders":isocast_platform_macos::diagnostics::encoder_metrics(),
                    "log_health":{"write_errors":health["write_errors"],"logging_cpu_ns":health["logging_cpu_ns"],"panics":health["panics"],"mode":health["mode"]}}),
                );
            }
            let now = isocast_platform_macos::diagnostics::permissions();
            if now != permissions {
                emit(
                    "info",
                    "permissions",
                    "permission state changed",
                    json!({"previous":permissions,"current":now}),
                );
                permissions = now;
            }
            let changed = device_changes.poll();
            if changed.is_some() || count.is_multiple_of(60) {
                if let Some(changed) = changed {
                    emit("info", "devices", "OS device change notifications", changed);
                }
                let now = isocast_platform_macos::diagnostics::inventory();
                if now != inventory {
                    emit(
                        "info",
                        "devices",
                        "device inventory or formats changed",
                        json!({"previous":inventory,"current":now}),
                    );
                    // Devices that came or went since the last inventory
                    // (none for the first one), for watchers.
                    if !inventory.is_null() {
                        for event in devices_came_or_went(&inventory, &now) {
                            daemon.publish(event);
                        }
                    }
                    inventory = now;
                }
            }
        }
        sampler_cpu_us = (thread_cpu() - cpu) * 1e6;
        count += 1;
        if Instant::now() > next {
            next = Instant::now() + Duration::from_secs(1);
        }
    }
}

/// `device_changed` events for the cameras, microphones, displays and
/// audio outputs that appear in only one of two inventories (by id). An
/// inventory that failed (no lists) compares as unchanged.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn devices_came_or_went(before: &Value, after: &Value) -> Vec<isocast_protocol::api::Event> {
    use isocast_protocol::api::{DeviceAction, DeviceKind, Event};
    let mut events = Vec::new();
    for (key, kind) in [
        ("cameras", DeviceKind::Camera),
        ("microphones", DeviceKind::Microphone),
        ("displays", DeviceKind::Display),
        ("audio_outputs", DeviceKind::AudioOutput),
    ] {
        let (Some(old), Some(new)) = (before[key].as_array(), after[key].as_array()) else {
            continue;
        };
        let id = |device: &Value| device["id"].to_string().trim_matches('"').to_string();
        let name = |device: &Value| device["name"].as_str().unwrap_or_default().to_string();
        for (from, to, action) in [
            (new, old, DeviceAction::Added),
            (old, new, DeviceAction::Removed),
        ] {
            for device in from {
                if !to.iter().any(|other| id(other) == id(device)) {
                    events.push(Event::DeviceChanged {
                        action,
                        kind,
                        id: id(device),
                        name: name(device),
                    });
                }
            }
        }
    }
    events
}

fn thread_cpu() -> f64 {
    let mut time = std::mem::MaybeUninit::<libc::timespec>::zeroed();
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, time.as_mut_ptr()) } != 0 {
        return 0.0;
    }
    let time = unsafe { time.assume_init() };
    time.tv_sec as f64 + time.tv_nsec as f64 / 1e9
}

pub fn bug_report(daemon: &Daemon, params: &BugReportParams) -> Result<BugReport, RpcError> {
    if params.preview_id.is_some() {
        return isocast_diagnostics::bundle::from_preview(params).map_err(error);
    }
    let status = daemon.status();
    let live = daemon.get(Default::default());
    let library = daemon.library();
    let mut credentials = std::env::var("ISOCAST_STREAM_KEY")
        .ok()
        .into_iter()
        .collect::<Vec<_>>();
    // Old CLI/app/native/plugin logs may predate this daemon's memory. Seed
    // the export redactor from every configured profile, without returning
    // those credentials on the API or writing a secret inventory to disk.
    for entry in &library.profiles {
        if let Ok(document) = daemon.library_get(isocast_protocol::library::LibraryTarget {
            kind: isocast_protocol::library::LibraryKind::Profile,
            id: entry.id.clone(),
        }) && let Some(stream) = document.profile.and_then(|p| p.stream)
        {
            // This build's key and an old one it may still read (#63).
            for service in [
                crate::key_service(),
                isocast_engine::stream::LEGACY_KEY_SERVICE,
            ] {
                if let Ok(Some(key)) = crate::secret(service, &stream.key_account()) {
                    credentials.push(key);
                }
            }
        }
    }
    let profile = library.profile.as_ref().and_then(|id| {
        daemon
            .library_get(isocast_protocol::library::LibraryTarget {
                kind: isocast_protocol::library::LibraryKind::Profile,
                id: id.clone(),
            })
            .ok()
    });
    let mut folders = Vec::new();
    if let Some(setup) = &live.setup {
        // The recordings folder and its newest project folders.
        folders.extend(isocast_protocol::project::recording_folders(
            &crate::expand_home(&setup.recording.folder),
            10,
        ));
    }
    if let Some(last) = &status.last {
        folders.push(last.folder.clone());
    }
    for file in &status.files {
        if let Some(parent) = file.parent() {
            folders.push(parent.to_owned());
        }
    }
    folders.sort();
    folders.dedup();
    let folder = folders
        .first()
        .cloned()
        .unwrap_or_else(isocast_diagnostics::logs_folder);
    #[cfg(target_os = "macos")]
    let platform = json!({"environment":isocast_platform_macos::diagnostics::environment(&folder),
        "permissions":isocast_platform_macos::diagnostics::permissions(),"devices":isocast_platform_macos::diagnostics::inventory()});
    #[cfg(not(target_os = "macos"))]
    let platform =
        json!({"environment":{"os":std::env::consts::OS},"permissions":null,"devices":null});
    let snapshot = json!({"environment":platform["environment"],"permissions":platform["permissions"],"devices":platform["devices"],
        "status":status,"setup":live,"library":library,"profile":profile,"plugins":daemon.plugins.list(),
        "diagnostics":isocast_diagnostics::health(),"build":{"version":env!("CARGO_PKG_VERSION"),"commit":env!("ISOCAST_BUILD_COMMIT"),"profile":env!("ISOCAST_BUILD_PROFILE"),"arch":std::env::consts::ARCH}});
    isocast_diagnostics::bundle::generate(params, snapshot, &folders, credentials).map_err(error)
}

pub fn error(message: String) -> RpcError {
    RpcError {
        code: if message.starts_with("params.") {
            RpcError::INVALID_PARAMS
        } else {
            RpcError::FAILED
        },
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use isocast_protocol::api::{DeviceAction, DeviceKind, Event};

    #[test]
    fn devices_that_came_or_went_become_events() {
        let before = json!({"cameras":[{"id":"a","name":"Cam A"}],"microphones":[{"id":"m","name":"Mic"}],
            "displays":[{"id":1,"name":"Built-in"}],"audio_outputs":[]});
        let after = json!({"cameras":[{"id":"b","name":"Cam B"}],"microphones":[{"id":"m","name":"Mic"}],
            "displays":[{"id":1,"name":"Built-in"}],"audio_outputs":[{"id":"o","name":"Out"}]});
        let events = devices_came_or_went(&before, &after);
        let found: Vec<_> = events
            .iter()
            .map(|e| match e {
                Event::DeviceChanged {
                    action, kind, id, ..
                } => (*action, *kind, id.as_str()),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            found,
            [
                (DeviceAction::Added, DeviceKind::Camera, "b"),
                (DeviceAction::Removed, DeviceKind::Camera, "a"),
                (DeviceAction::Added, DeviceKind::AudioOutput, "o"),
            ]
        );
        // A failed inventory (no lists) is no change.
        assert!(devices_came_or_went(&before, &json!({"error":"x"})).is_empty());
    }
}
