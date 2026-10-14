//! Bounded summaries for successful read polling. No request/response payloads.
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

struct Count {
    client: Value,
    method: String,
    at: Instant,
    calls: u64,
    bytes: u64,
    ms: f64,
    max_ms: f64,
}
static POLLS: OnceLock<Mutex<HashMap<(u64, String), Count>>> = OnceLock::new();

pub fn is_poll(method: &str) -> bool {
    matches!(
        method,
        "status"
            | "get"
            | "data_get"
            | "logs"
            | "frame"
            | "devices"
            | "permissions"
            | "plugin_list"
            | "library"
            | "library_get"
            | "report_file"
    )
}
pub fn record(method: &str, client: &Value, ms: f64, bytes: usize) {
    let map = POLLS.get_or_init(Mutex::default);
    let mut map = map.lock().unwrap_or_else(|p| p.into_inner());
    let key = (client["pid"].as_u64().unwrap_or(0), method.to_owned());
    if map.len() >= 512 && !map.contains_key(&key) {
        drop(map);
        flush(true);
        map = POLLS
            .get()
            .unwrap()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
    }
    let count = map.entry(key).or_insert_with(|| Count {
        client: client.clone(),
        method: method.into(),
        at: Instant::now(),
        calls: 0,
        bytes: 0,
        ms: 0.0,
        max_ms: 0.0,
    });
    count.calls += 1;
    count.bytes += bytes as u64;
    count.ms += ms;
    count.max_ms = count.max_ms.max(ms);
}
pub fn flush(all: bool) {
    let Some(map) = POLLS.get() else { return };
    let ready = {
        let mut map = map.lock().unwrap_or_else(|p| p.into_inner());
        let keys = map
            .iter()
            .filter(|(_, c)| all || c.at.elapsed() >= Duration::from_secs(60))
            .map(|(k, _)| k.clone())
            .collect::<Vec<_>>();
        keys.into_iter()
            .filter_map(|k| map.remove(&k))
            .collect::<Vec<_>>()
    };
    for c in ready {
        crate::emit(
            "info",
            "api",
            "poll summary",
            json!({"method":c.method,"client":c.client,"calls":c.calls,"response_bytes":c.bytes,"mean_ms":c.ms/c.calls.max(1) as f64,"max_ms":c.max_ms,"window_seconds":c.at.elapsed().as_secs_f64()}),
        );
    }
}
struct Budget {
    at: Instant,
    used: u32,
    omitted: u64,
}
static LIMITS: OnceLock<Mutex<HashMap<u64, Budget>>> = OnceLock::new();

/// A peer's connection ended: write its pending omission count now rather
/// than on a later call that may never come.
pub fn peer_closed(client: &Value) {
    let pid = client["pid"].as_u64().unwrap_or(0);
    let omitted = LIMITS.get().and_then(|m| {
        let mut map = m.lock().unwrap_or_else(|p| p.into_inner());
        let b = map.get_mut(&pid)?;
        Some(std::mem::take(&mut b.omitted))
    });
    if let Some(omitted) = omitted.filter(|n| *n > 0) {
        crate::emit(
            "warn",
            "api",
            "peer log rate limited",
            json!({"pid":pid,"omitted":omitted,"at":"disconnect"}),
        );
    }
}

/// Limit diagnostic writes, not engine operations. Errors are never limited.
pub fn permit(client: &Value) -> bool {
    let mut map = LIMITS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let pid = client["pid"].as_u64().unwrap_or(0);
    if map.len() > 512 {
        map.retain(|_, b| b.at.elapsed() < Duration::from_secs(60));
    }
    let key = if map.len() > 512 { 0 } else { pid };
    let b = map.entry(key).or_insert(Budget {
        at: Instant::now(),
        used: 0,
        omitted: 0,
    });
    if b.at.elapsed() >= Duration::from_secs(1) {
        if b.omitted > 0 {
            crate::emit(
                "warn",
                "api",
                "peer log rate limited",
                json!({"pid":pid,"omitted":b.omitted}),
            );
        }
        b.at = Instant::now();
        b.used = 0;
        b.omitted = 0;
    }
    b.used += 1;
    if b.used > 20 {
        b.omitted += 1;
        false
    } else {
        true
    }
}
