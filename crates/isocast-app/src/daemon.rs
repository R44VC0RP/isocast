//! The app's side of isocastd: calls relayed from the interface, the event
//! stream, and monitors' video (an H.264 stream per monitor, or JPEG
//! frames from daemons without one). Plain threads and blocking sockets;
//! the window code hands results back to the main thread.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use isocast_ipc::Stream as UnixStream;
use std::io::BufReader;

use isocast_client::{Client, DaemonError};
use isocast_protocol::api::{FrameStream, FramesParams, RpcError};
use serde_json::{Value, json};

/// The error code the interface gets when the daemon can't be reached
/// (JSON-RPC's implementation-defined server error range).
pub const UNREACHABLE: i64 = -32099;

/// How long a `frame` call waits for a newer frame.
const FRAME_TIMEOUT_MS: u64 = 1000;

/// JPEG quality for monitor frames (0 to 1).
const FRAME_QUALITY: f64 = 0.8;

/// Without `frame`, snapshots stand in for preview frames, at most this
/// often per monitor.
const SNAPSHOT_INTERVAL: Duration = Duration::from_millis(250);

/// The longest wait between attempts to reach the daemon.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Idle connections kept for later calls.
const POOL_SIZE: usize = 6;

#[derive(Default)]
pub struct Daemon {
    /// Connections not in use.
    idle: Mutex<Vec<Client>>,
    /// Whether the daemon has `frame`: unknown, yes, or no (snapshots).
    frames: AtomicU8,
    /// The latest connection event, for an interface that has just loaded.
    connection: Mutex<Value>,
    /// Set to try reaching the daemon again at once.
    retry: (Mutex<bool>, Condvar),
    /// When each monitor last got a snapshot standing in for a frame.
    snapshots: Mutex<HashMap<String, Instant>>,
    snapshot_seq: AtomicU64,
}

const FRAMES_UNKNOWN: u8 = 0;
const FRAMES_YES: u8 = 1;
const FRAMES_NO: u8 = 2;

/// A preview frame for a monitor.
pub struct Frame {
    pub seq: u64,
    pub width: u32,
    pub height: u32,
    pub mime: &'static str,
    pub bytes: Vec<u8>,
    /// The setup revision a program or preview frame was drawn from.
    pub revision: Option<u64>,
}

impl Daemon {
    /// Call `method` and return the reply the interface expects:
    /// `{"result": …}` or `{"error": {"code", "message"}}`.
    pub fn call(&self, method: &str, params: Option<Value>) -> Value {
        match self.call_value(method, params) {
            Ok(result) => json!({ "result": result }),
            Err((code, message)) => json!({ "error": { "code": code, "message": message } }),
        }
    }

    fn call_value(&self, method: &str, params: Option<Value>) -> Result<Value, (i64, String)> {
        let pooled = self.idle.lock().unwrap().pop();
        let mut client = match pooled {
            Some(client) => client,
            None => Client::connect_or_launch().map_err(|err| (UNREACHABLE, format!("{err:#}")))?,
        };
        match client.call_value(method, params) {
            Ok(result) => {
                self.give_back(client);
                Ok(result)
            }
            Err(err) => match err.downcast_ref::<DaemonError>() {
                Some(error) => {
                    let reply = (error.code(), error.to_string());
                    self.give_back(client);
                    Err(reply)
                }
                // The connection broke; it isn't reused.
                None => Err((UNREACHABLE, format!("{err:#}"))),
            },
        }
    }

    fn give_back(&self, client: Client) {
        let mut idle = self.idle.lock().unwrap();
        if idle.len() < POOL_SIZE {
            idle.push(client);
        }
    }

    /// The latest connection event.
    pub fn connection(&self) -> Value {
        self.connection.lock().unwrap().clone()
    }

    /// Try to reach the daemon again now, rather than after the back-off.
    pub fn reconnect(&self) {
        let (flag, wake) = &self.retry;
        *flag.lock().unwrap() = true;
        wake.notify_all();
    }

    /// Watch the daemon's events on a thread of its own, launching the
    /// daemon if it isn't running and reconnecting if it goes away. `emit`
    /// gets connection changes (`{"type": "connection", "state": …}`), the
    /// live setup on each connection (`{"type": "watching", "live": …}`)
    /// and every event as the daemon sends it.
    pub fn watch(self: &Arc<Self>, emit: impl Fn(Value) + Send + 'static) {
        let daemon = self.clone();
        thread::spawn(move || {
            let mut backoff = Duration::from_secs(1);
            loop {
                daemon.set_connection(&emit, json!({"type": "connection", "state": "connecting"}));
                match Client::connect_or_launch().and_then(Client::watch_values) {
                    Ok((live, events)) => {
                        backoff = Duration::from_secs(1);
                        daemon.frames.store(FRAMES_UNKNOWN, Ordering::Relaxed);
                        daemon.set_connection(
                            &emit,
                            json!({"type": "connection", "state": "connected"}),
                        );
                        emit(json!({"type": "watching", "live": live}));
                        for event in events {
                            match event {
                                Ok(event) => emit(event),
                                Err(_) => break,
                            }
                        }
                        // Pooled connections went with the daemon.
                        daemon.idle.lock().unwrap().clear();
                        daemon.set_connection(
                            &emit,
                            json!({"type": "connection", "state": "disconnected"}),
                        );
                    }
                    Err(err) => daemon.set_connection(
                        &emit,
                        json!({
                            "type": "connection",
                            "state": "failed",
                            "error": format!("{err:#}"),
                            "retry_seconds": backoff.as_secs(),
                        }),
                    ),
                }
                let (flag, wake) = &daemon.retry;
                let woken = wake
                    .wait_timeout_while(flag.lock().unwrap(), backoff, |retry| !*retry)
                    .unwrap();
                let mut retry = woken.0;
                if *retry {
                    backoff = Duration::from_secs(1);
                } else {
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
                *retry = false;
            }
        });
    }

    fn set_connection(&self, emit: &impl Fn(Value), event: Value) {
        *self.connection.lock().unwrap() = event.clone();
        emit(event);
    }

    /// Start a monitor's H.264 stream of `target` (`frames`): the reply and
    /// the connection carrying its records. `Err(None)` when the daemon
    /// has no `frames` (an older build), so the monitor asks for frames
    /// instead.
    pub fn frames(
        &self,
        target: &str,
        max_width: u32,
        fps: u32,
    ) -> Result<(FrameStream, BufReader<UnixStream>), Option<String>> {
        let client = Client::connect_or_launch().map_err(|err| Some(format!("{err:#}")))?;
        let params = FramesParams {
            target: target.to_string(),
            max_width: Some(max_width),
            fps: Some(fps),
        };
        client
            .frames(params)
            .map_err(|err| match err.downcast_ref::<DaemonError>() {
                Some(error) if unknown_method(error.code(), &error.to_string()) => None,
                _ => Some(format!("{err:#}")),
            })
    }

    /// The next frame of `target` ("program", "preview" or a source id)
    /// newer than `after`, at most `max_width` pixels wide, or `None` if
    /// none came in time (ask again). Daemons without `frame` get a
    /// snapshot instead, a few times a second.
    pub fn frame(
        &self,
        target: &str,
        after: Option<u64>,
        max_width: u32,
    ) -> Result<Option<Frame>, String> {
        if self.frames.load(Ordering::Relaxed) != FRAMES_NO {
            let params = json!({
                "target": target,
                "max_width": max_width,
                "after": after,
                "timeout_ms": FRAME_TIMEOUT_MS,
                "quality": FRAME_QUALITY,
            });
            match self.call_value("frame", Some(params)) {
                Ok(reply) => {
                    self.frames.store(FRAMES_YES, Ordering::Relaxed);
                    let (width, height, bytes) = decode(&reply, "jpeg_base64", "image/jpeg")?;
                    return Ok(Some(Frame {
                        seq: reply.get("seq").and_then(Value::as_u64).unwrap_or(0),
                        width,
                        height,
                        mime: "image/jpeg",
                        bytes,
                        revision: reply.get("revision").and_then(Value::as_u64),
                    }));
                }
                Err((code, message)) if unknown_method(code, &message) => {
                    self.frames.store(FRAMES_NO, Ordering::Relaxed);
                }
                // The long poll ran out: nothing new to show yet.
                Err((_, message)) if message.contains("no new frame") => return Ok(None),
                Err((_, message)) => return Err(message),
            }
        }
        self.snapshot_frame(target, max_width).map(Some)
    }

    /// A snapshot standing in for a frame, paced per monitor.
    fn snapshot_frame(&self, target: &str, max_width: u32) -> Result<Frame, String> {
        let last = self.snapshots.lock().unwrap().get(target).copied();
        if let Some(wait) = last.and_then(|last| SNAPSHOT_INTERVAL.checked_sub(last.elapsed())) {
            thread::sleep(wait);
        }
        self.snapshots
            .lock()
            .unwrap()
            .insert(target.to_string(), Instant::now());
        let params = match target {
            "program" => json!({ "width": max_width }),
            "preview" => json!({ "preview": true, "width": max_width }),
            _ => match target.strip_prefix("scene:") {
                Some(scene) => json!({ "scene": scene, "width": max_width }),
                None => json!({ "source": target, "width": max_width }),
            },
        };
        let reply = self
            .call_value("snapshot", Some(params))
            .map_err(|(_, message)| message)?;
        let (width, height, bytes) = decode(&reply, "png_base64", "image/png")?;
        Ok(Frame {
            seq: self.snapshot_seq.fetch_add(1, Ordering::Relaxed) + 1,
            width,
            height,
            mime: "image/png",
            bytes,
            revision: None,
        })
    }
}

/// Whether the daemon refused a method it doesn't have (an older build;
/// builds before METHOD_NOT_FOUND said so with INVALID_REQUEST).
fn unknown_method(code: i64, message: &str) -> bool {
    code == RpcError::METHOD_NOT_FOUND
        || code == RpcError::INVALID_REQUEST && message.contains("unknown variant")
}

/// A picture's size and bytes from a `frame` or `snapshot` reply.
fn decode(reply: &Value, field: &str, what: &str) -> Result<(u32, u32, Vec<u8>), String> {
    let size = |name: &str| {
        reply
            .get(name)
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0)
    };
    let data = reply
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("the daemon's reply has no {field}"))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|err| format!("the daemon sent an unreadable {what}: {err}"))?;
    Ok((size("width"), size("height"), bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_daemons_refuse_unknown_methods_by_name() {
        let message = "unknown variant `frame`, expected one of `status`, `devices`";
        assert!(unknown_method(RpcError::INVALID_REQUEST, message));
        assert!(unknown_method(RpcError::METHOD_NOT_FOUND, message));
        assert!(!unknown_method(RpcError::FAILED, message));
        assert!(!unknown_method(
            RpcError::INVALID_REQUEST,
            "missing field `target`"
        ));
    }

    #[test]
    fn replies_decode_to_pictures() {
        let reply = json!({"width": 2, "height": 1, "jpeg_base64": "AQID"});
        assert_eq!(
            decode(&reply, "jpeg_base64", "image/jpeg").unwrap(),
            (2, 1, vec![1, 2, 3])
        );
        assert!(decode(&json!({}), "jpeg_base64", "image/jpeg").is_err());
    }
}
