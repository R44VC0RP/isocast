//! One client's connection: the handshake (Hello, Identify with the
//! SHA-256 challenge, Identified, Reidentify), requests, request batches
//! and events, as obs-websocket's protocol describes them.

use std::io;
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::protocol::CloseFrame;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::{Message, WebSocket};

use crate::events::{self, Tracker, sub};
use crate::requests::{self, Ctx, code};
use crate::{OBS_WEBSOCKET_VERSION, RPC_VERSION, Shared, Watching};

/// How often a session looks for events while the client is quiet.
const POLL: Duration = Duration::from_millis(15);
/// A client that stops reading is let go after this.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Messages larger than this close the connection (OBS has no limit; a
/// screenshot request is the largest anyone sends, and it's small).
const MAX_MESSAGE: usize = 16 << 20;

const JSON_PROTOCOL: &str = "obswebsocket.json";
const MSGPACK_PROTOCOL: &str = "obswebsocket.msgpack";

/// obs-websocket's WebSocketCloseCode values.
mod close {
    pub const UNKNOWN_REASON: u16 = 4000;
    pub const MESSAGE_DECODE_ERROR: u16 = 4002;
    pub const MISSING_DATA_FIELD: u16 = 4003;
    pub const INVALID_DATA_FIELD_TYPE: u16 = 4004;
    pub const INVALID_DATA_FIELD_VALUE: u16 = 4005;
    pub const UNKNOWN_OP_CODE: u16 = 4006;
    pub const NOT_IDENTIFIED: u16 = 4007;
    pub const ALREADY_IDENTIFIED: u16 = 4008;
    pub const AUTHENTICATION_FAILED: u16 = 4009;
    pub const UNSUPPORTED_RPC_VERSION: u16 = 4010;
    pub const UNSUPPORTED_FEATURE: u16 = 4012;
}

/// The protocol's OpCodes.
mod op {
    pub const HELLO: u64 = 0;
    pub const IDENTIFY: u64 = 1;
    pub const IDENTIFIED: u64 = 2;
    pub const REIDENTIFY: u64 = 3;
    pub const EVENT: u64 = 5;
    pub const REQUEST: u64 = 6;
    pub const REQUEST_RESPONSE: u64 = 7;
    pub const REQUEST_BATCH: u64 = 8;
    pub const REQUEST_BATCH_RESPONSE: u64 = 9;
}

/// A session's message counts, for `GetStats`.
#[derive(Default)]
pub struct Stats {
    pub incoming: AtomicU64,
    pub outgoing: AtomicU64,
}

/// Send `data` to every identified session of `shared`'s server (sessions
/// that left are forgotten here).
pub fn broadcast(shared: &Shared, data: Value) {
    shared
        .custom
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retain(|tx| tx.send(data.clone()).is_ok());
}

/// Why a session ends: a close code and reason for the client.
struct Close(u16, String);

fn closing(code: u16, reason: impl Into<String>) -> Close {
    Close(code, reason.into())
}

struct Session<'a> {
    /// Messages are MessagePack (binary frames), not JSON.
    msgpack: bool,
    shared: &'a Shared,
    peer: String,
    challenge: Option<String>,
    identified: bool,
    subscriptions: u32,
    stats: Stats,
    tracker: Option<Tracker>,
    watching: Option<Watching>,
    custom: Option<mpsc::Receiver<Value>>,
}

pub fn run(stream: TcpStream, peer: SocketAddr, shared: &Arc<Shared>) {
    // On macOS an accepted socket inherits the listener's O_NONBLOCK, which
    // would make the read timeouts below no-ops (a busy loop per client).
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    // JSON unless the client asks for MessagePack first; refuse one that
    // offers neither.
    let mut msgpack = false;
    #[allow(clippy::result_large_err)] // tungstenite's callback type.
    let negotiate =
        |request: &Request, mut response: Response| -> Result<Response, ErrorResponse> {
            // Browsers always send Origin and OBS tools don't: without a
            // password, a web page must not drive the recorder (#74).
            if shared.password.is_none() && request.headers().contains_key("Origin") {
                let mut refusal = ErrorResponse::new(Some(
                    "Isocast's obs-websocket has no password, so it refuses connections from \
                     web pages; set a password to connect from a browser"
                        .into(),
                ));
                *refusal.status_mut() = tungstenite::http::StatusCode::FORBIDDEN;
                return Err(refusal);
            }
            let offered: Vec<String> = request
                .headers()
                .get_all("Sec-WebSocket-Protocol")
                .iter()
                .filter_map(|v| v.to_str().ok())
                .flat_map(|v| v.split(',').map(|p| p.trim().to_string()))
                .collect();
            if let Some(chosen) = offered
                .iter()
                .find(|p| *p == JSON_PROTOCOL || *p == MSGPACK_PROTOCOL)
            {
                msgpack = chosen == MSGPACK_PROTOCOL;
                response.headers_mut().insert(
                    "Sec-WebSocket-Protocol",
                    chosen.parse().expect("a valid header value"),
                );
            } else if !offered.is_empty() {
                let mut refusal = ErrorResponse::new(Some(
                    "Isocast speaks obswebsocket.json and obswebsocket.msgpack".into(),
                ));
                *refusal.status_mut() = tungstenite::http::StatusCode::BAD_REQUEST;
                return Err(refusal);
            }
            Ok(response)
        };
    let config = tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let Ok(mut ws) = tungstenite::accept_hdr_with_config(stream, negotiate, Some(config)) else {
        return;
    };
    let _ = ws.get_ref().set_read_timeout(Some(POLL));
    let mut session = Session {
        msgpack,
        shared,
        peer: format!("obs-websocket {peer}"),
        challenge: None,
        identified: false,
        subscriptions: 0,
        stats: Stats::default(),
        tracker: None,
        watching: None,
        custom: None,
    };
    let ended = session.serve(&mut ws);
    if let Err(Close(code, reason)) = ended {
        let _ = ws.close(Some(CloseFrame {
            code: CloseCode::from(code),
            reason: reason.into(),
        }));
        // Let the client answer the close, briefly.
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            match ws.read() {
                Ok(_) => {}
                Err(tungstenite::Error::Io(e)) if would_block(&e) => {}
                Err(_) => break,
            }
        }
    }
    let _ = ws.get_ref().shutdown(std::net::Shutdown::Both);
    // Its custom-event sender goes at the next broadcast.
}

fn would_block(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

impl Session<'_> {
    fn ctx(&self) -> Ctx<'_> {
        Ctx {
            shared: self.shared,
            peer: &self.peer,
            stats: &self.stats,
        }
    }

    /// Serve the client until it leaves (`Ok`) or must be closed (`Err`).
    fn serve(&mut self, ws: &mut WebSocket<TcpStream>) -> Result<(), Close> {
        let mut hello = json!({
            "obsWebSocketVersion": OBS_WEBSOCKET_VERSION,
            "rpcVersion": RPC_VERSION,
        });
        if self.shared.password.is_some() {
            let challenge = crate::random_base64(32)
                .map_err(|_| closing(close::UNKNOWN_REASON, "no randomness for the challenge"))?;
            hello["authentication"] = json!({ "challenge": challenge, "salt": self.shared.salt });
            self.challenge = Some(challenge);
        }
        self.send(ws, op::HELLO, hello)?;
        let connected = Instant::now();
        loop {
            if self.shared.stop.load(Ordering::SeqCst) {
                if self.identified && self.subscriptions & sub::GENERAL != 0 {
                    let _ = self.send(
                        ws,
                        op::EVENT,
                        json!({ "eventType": "ExitStarted", "eventIntent": sub::GENERAL }),
                    );
                }
                return Err(closing(1001, "Isocast's obs-websocket server is stopping"));
            }
            // Clients that never identify are let go.
            if !self.identified && connected.elapsed() > Duration::from_secs(10) {
                return Err(closing(
                    close::NOT_IDENTIFIED,
                    "You did not identify in time.",
                ));
            }
            match ws.read() {
                Ok(Message::Text(text)) => {
                    self.stats.incoming.fetch_add(1, Ordering::Relaxed);
                    let message = serde_json::from_str(text.as_str()).map_err(|e| {
                        closing(
                            close::MESSAGE_DECODE_ERROR,
                            format!("Unable to decode JSON: {e}"),
                        )
                    })?;
                    for (op, data) in self.message(&message)? {
                        self.send(ws, op, data)?;
                    }
                }
                Ok(Message::Binary(bytes)) => {
                    self.stats.incoming.fetch_add(1, Ordering::Relaxed);
                    let message = rmp_serde::from_slice(&bytes).map_err(|e| {
                        closing(
                            close::MESSAGE_DECODE_ERROR,
                            format!("Unable to decode MessagePack: {e}"),
                        )
                    })?;
                    for (op, data) in self.message(&message)? {
                        self.send(ws, op, data)?;
                    }
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(e)) if would_block(&e) => {}
                Err(_) => return Ok(()),
            }
            if self.identified {
                self.pump(ws)?;
            }
        }
    }

    fn send(&self, ws: &mut WebSocket<TcpStream>, op: u64, data: Value) -> Result<(), Close> {
        let message = json!({ "op": op, "d": data });
        let message = if self.msgpack {
            match rmp_serde::to_vec_named(&message) {
                Ok(bytes) => Message::binary(bytes),
                Err(_) => return Err(closing(close::UNKNOWN_REASON, "could not encode a message")),
            }
        } else {
            Message::text(message.to_string())
        };
        match ws.send(message) {
            Ok(()) => {
                self.stats.outgoing.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            // The client is gone or not reading: nothing more to say.
            Err(_) => Err(closing(close::UNKNOWN_REASON, "the connection failed")),
        }
    }

    /// Pass on the events that came since last time.
    fn pump(&mut self, ws: &mut WebSocket<TcpStream>) -> Result<(), Close> {
        let mut out = Vec::new();
        let mut lost = false;
        if let Some(watching) = &self.watching {
            loop {
                match watching.events.try_recv() {
                    Ok(bytes) => {
                        let Ok(note) =
                            serde_json::from_slice::<isocast_protocol::api::Notification>(&bytes)
                        else {
                            continue;
                        };
                        let ctx = Ctx {
                            shared: self.shared,
                            peer: &self.peer,
                            stats: &self.stats,
                        };
                        if let Some(tracker) = self.tracker.as_mut() {
                            out.extend(tracker.translate(&ctx, note.params));
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        lost = true;
                        break;
                    }
                }
            }
        }
        if lost {
            // It fell behind and was dropped: watch again, and send what
            // changed meanwhile.
            self.watch();
            let ctx = Ctx {
                shared: self.shared,
                peer: &self.peer,
                stats: &self.stats,
            };
            if let Some(tracker) = self.tracker.as_mut() {
                out.extend(tracker.resync(&ctx));
            }
        }
        if let Some(custom) = &self.custom {
            while let Ok(data) = custom.try_recv() {
                if data.is_object() {
                    out.push(events::ObsEvent {
                        subscription: sub::GENERAL,
                        kind: "CustomEvent",
                        data,
                    });
                }
            }
        }
        for event in out {
            if self.subscriptions & event.subscription == 0 {
                continue;
            }
            let mut data = json!({ "eventType": event.kind, "eventIntent": event.subscription });
            if !event.data.is_null() {
                data["eventData"] = event.data;
            }
            self.send(ws, op::EVENT, data)?;
        }
        Ok(())
    }

    fn watch(&mut self) {
        let meters = self.subscriptions & sub::INPUT_VOLUME_METERS != 0;
        self.watching = Some(self.shared.engine.watch(events::kinds(meters)));
    }

    /// Handle one message; the replies to send, or why to close.
    fn message(&mut self, message: &Value) -> Result<Vec<(u64, Value)>, Close> {
        let Some(op) = message.get("op") else {
            return Err(closing(
                close::MISSING_DATA_FIELD,
                "Your request is missing an `op`.",
            ));
        };
        let Some(op) = op.as_u64() else {
            return Err(closing(
                close::INVALID_DATA_FIELD_TYPE,
                "Your `op` is not a number.",
            ));
        };
        let Some(data) = message.get("d") else {
            return Err(closing(
                close::MISSING_DATA_FIELD,
                "Your request is missing a `d`.",
            ));
        };
        if !data.is_object() {
            return Err(closing(
                close::INVALID_DATA_FIELD_TYPE,
                "Your `d` is not an object.",
            ));
        }
        if !self.identified && op != op::IDENTIFY {
            return Err(closing(
                close::NOT_IDENTIFIED,
                "You must identify before you can send other messages.",
            ));
        }
        match op {
            op::IDENTIFY => {
                if self.identified {
                    return Err(closing(
                        close::ALREADY_IDENTIFIED,
                        "You are already identified with the obs-websocket server.",
                    ));
                }
                let Some(rpc) = data.get("rpcVersion") else {
                    return Err(closing(
                        close::MISSING_DATA_FIELD,
                        "You must specify an `rpcVersion`.",
                    ));
                };
                let Some(rpc) = rpc.as_u64() else {
                    return Err(closing(
                        close::INVALID_DATA_FIELD_TYPE,
                        "Your `rpcVersion` is not a number.",
                    ));
                };
                if rpc != u64::from(RPC_VERSION) {
                    return Err(closing(
                        close::UNSUPPORTED_RPC_VERSION,
                        format!(
                            "Your requested RPC version is not supported. Supported: {RPC_VERSION}."
                        ),
                    ));
                }
                if let (Some(password), Some(challenge)) = (&self.shared.password, &self.challenge)
                {
                    let Some(given) = data.get("authentication") else {
                        return Err(closing(
                            close::AUTHENTICATION_FAILED,
                            "Your payload's data is missing an `authentication` string, however authentication is required.",
                        ));
                    };
                    let expected = crate::authentication(password, &self.shared.salt, challenge);
                    if given
                        .as_str()
                        .is_none_or(|g| !constant_time_eq(g.as_bytes(), expected.as_bytes()))
                    {
                        return Err(closing(
                            close::AUTHENTICATION_FAILED,
                            "Authentication failed.",
                        ));
                    }
                }
                self.subscriptions = subscriptions(data)?.unwrap_or(sub::ALL);
                self.identified = true;
                self.tracker = Tracker::new(&self.ctx());
                self.watch();
                let (tx, rx) = mpsc::channel();
                self.shared
                    .custom
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(tx);
                self.custom = Some(rx);
                Ok(vec![(
                    op::IDENTIFIED,
                    json!({ "negotiatedRpcVersion": RPC_VERSION }),
                )])
            }
            op::REIDENTIFY => {
                let meters = self.subscriptions & sub::INPUT_VOLUME_METERS;
                if let Some(subs) = subscriptions(data)? {
                    self.subscriptions = subs;
                }
                if self.subscriptions & sub::INPUT_VOLUME_METERS != meters {
                    self.watch();
                }
                Ok(vec![(
                    op::IDENTIFIED,
                    json!({ "negotiatedRpcVersion": RPC_VERSION }),
                )])
            }
            op::REQUEST => {
                let Some(id) = data.get("requestId") else {
                    return Err(closing(
                        close::MISSING_DATA_FIELD,
                        "Your payload data is missing a `requestId`.",
                    ));
                };
                let Some(kind) = data.get("requestType") else {
                    return Err(closing(
                        close::MISSING_DATA_FIELD,
                        "Your payload's data is missing an `requestType`.",
                    ));
                };
                let Some(kind) = kind.as_str() else {
                    return Err(closing(
                        close::INVALID_DATA_FIELD_TYPE,
                        "Your `requestType` is not a string.",
                    ));
                };
                // Sleep only works in a batch; OBS answers 206 elsewhere
                // (and a sleep here would stall the session's events).
                let mut result = if kind == "Sleep" {
                    json!({
                        "requestType": kind,
                        "requestStatus": {
                            "result": false,
                            "code": code::UNSUPPORTED_REQUEST_BATCH_EXECUTION_TYPE,
                            "comment": "Sleep only works in a request batch.",
                        },
                    })
                } else {
                    self.request(kind, data.get("requestData"))
                };
                result["requestId"] = id.clone();
                Ok(vec![(op::REQUEST_RESPONSE, result)])
            }
            op::REQUEST_BATCH => {
                let Some(id) = data.get("requestId") else {
                    return Err(closing(
                        close::MISSING_DATA_FIELD,
                        "Your payload data is missing a `requestId`.",
                    ));
                };
                let execution = match data.get("executionType").filter(|v| !v.is_null()) {
                    None => 0,
                    Some(v) => match v.as_i64() {
                        Some(n @ 0..=2) => n,
                        Some(_) => {
                            return Err(closing(
                                close::INVALID_DATA_FIELD_VALUE,
                                "Your `executionType` has an invalid value.",
                            ));
                        }
                        None => {
                            return Err(closing(
                                close::INVALID_DATA_FIELD_TYPE,
                                "Your `executionType` is not a number.",
                            ));
                        }
                    },
                };
                if execution == 2 && data.get("variables").is_some_and(|v| !v.is_null()) {
                    return Err(closing(
                        close::UNSUPPORTED_FEATURE,
                        "Variables are not supported in Parallel mode.",
                    ));
                }
                let halt = match data.get("haltOnFailure").filter(|v| !v.is_null()) {
                    None => false,
                    Some(Value::Bool(b)) => *b,
                    Some(_) => {
                        return Err(closing(
                            close::INVALID_DATA_FIELD_TYPE,
                            "Your `haltOnFailure` is not a boolean.",
                        ));
                    }
                };
                let Some(requests) = data.get("requests") else {
                    return Err(closing(
                        close::MISSING_DATA_FIELD,
                        "Your payload data is missing a `requests`.",
                    ));
                };
                let Some(requests) = requests.as_array() else {
                    return Err(closing(
                        close::INVALID_DATA_FIELD_TYPE,
                        "Your `requests` is not an array.",
                    ));
                };
                // Parallel batches run in order too: nothing promises an
                // order, so in order is a valid one.
                let mut results = Vec::new();
                for request in requests {
                    let kind = request
                        .get("requestType")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let mut data = request.get("requestData").cloned().unwrap_or(Value::Null);
                    // Sleep in a frame batch counts frames; elsewhere ms.
                    if kind == "Sleep" && execution != 1 {
                        if let Some(obj) = data.as_object_mut() {
                            obj.remove("sleepFrames");
                        }
                    } else if kind == "Sleep"
                        && let Some(obj) = data.as_object_mut()
                    {
                        obj.remove("sleepMillis");
                    }
                    let mut result = self.request(kind, Some(&data));
                    if let Some(id) = request.get("requestId") {
                        result["requestId"] = id.clone();
                    }
                    let failed = result["requestStatus"]["result"] != json!(true);
                    results.push(result);
                    if failed && halt {
                        break;
                    }
                }
                Ok(vec![(
                    op::REQUEST_BATCH_RESPONSE,
                    json!({ "requestId": id, "results": results }),
                )])
            }
            op::HELLO
            | op::IDENTIFIED
            | op::EVENT
            | op::REQUEST_RESPONSE
            | op::REQUEST_BATCH_RESPONSE => Err(closing(
                close::UNKNOWN_OP_CODE,
                format!("Unknown OpCode: {op}"),
            )),
            other => Err(closing(
                close::UNKNOWN_OP_CODE,
                format!("Unknown OpCode: {other}"),
            )),
        }
    }

    /// Run one request: its RequestResponse fields (without the id).
    fn request(&mut self, kind: &str, data: Option<&Value>) -> Value {
        let empty = json!({});
        let data = data.filter(|d| d.is_object()).unwrap_or(&empty);
        let answer = requests::handle(&self.ctx(), kind, data);
        let mut out = json!({ "requestType": kind });
        match answer {
            Ok(response) => {
                out["requestStatus"] = json!({ "result": true, "code": code::SUCCESS });
                if response.is_object() {
                    out["responseData"] = response;
                }
            }
            Err(fail) => {
                out["requestStatus"] =
                    json!({ "result": false, "code": fail.code, "comment": fail.comment });
            }
        }
        out
    }
}

fn subscriptions(data: &Value) -> Result<Option<u32>, Close> {
    match data.get("eventSubscriptions").filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => v.as_u64().map(|n| Some(n as u32)).ok_or_else(|| {
            closing(
                close::INVALID_DATA_FIELD_TYPE,
                "Your `eventSubscriptions` is not a number.",
            )
        }),
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
