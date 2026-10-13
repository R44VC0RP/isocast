//! MCP's view of isocastd's events. MCP clients can't read the `watch`
//! stream, so the server watches isocastd on a thread of its own and keeps
//! the newest events for the get_events tool to read with a cursor. Memory
//! is bounded whether or not anyone reads: the oldest events go first.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use isocast_protocol::api::{Event, EventKind, WatchParams};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::client::Client;

/// How many events are kept.
pub const CAPACITY: usize = 500;
/// The most events one read returns.
const MAX_LIMIT: usize = CAPACITY;
const DEFAULT_LIMIT: usize = 50;
/// The longest a read may wait for an event.
const MAX_WAIT_SECONDS: f64 = 30.0;
/// How often the watcher looks for isocastd while it isn't running.
const RETRY: Duration = Duration::from_secs(1);
/// How long a tool call waits for the watcher to connect before going on.
const CONNECT_WAIT: Duration = Duration::from_secs(2);
/// The kind the server adds when its watch connection starts or ends.
const CONNECTION: &str = "connection";
/// Kinds that aren't kept: 20 a second would push everything else out.
const NOT_KEPT: &str = "levels";
/// Kinds of which only the newest is kept: status comes about once a
/// second while recording, and setup_changed many times a second while
/// the app drags a source; either would push warnings out.
const NEWEST_ONLY: [&str; 2] = ["status", "setup_changed"];

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct GetEventsParams {
    /// Only events after this one: the `next` of the previous read. Without
    /// it, the newest events.
    pub after: Option<u64>,
    /// Only these kinds, like ["warning","stopped"]. All kept kinds if
    /// absent.
    pub kinds: Option<Vec<String>>,
    /// The most events to return (1 to 500, default 50).
    pub limit: Option<usize>,
    /// If no event matches yet, wait up to this many seconds (at most 30)
    /// for one.
    pub wait_seconds: Option<f64>,
}

/// get_events' reply.
#[derive(Debug, Serialize, JsonSchema)]
pub struct EventsRead {
    /// Oldest first.
    pub events: Vec<EventEntry>,
    /// Pass as `after` next time to get only newer events.
    pub next: u64,
    /// More events match than were returned: newer ones (with `after`;
    /// read again from `next`) or older ones (without; read from `after`
    /// 0).
    pub more: bool,
    /// Events after `after` were dropped to make room (the newest 500 are
    /// kept); get_status and the recording's report have the totals.
    pub missed: bool,
    /// Whether isocastd's events are being received now. While not, events
    /// are lost; a `connection` event marks each gap.
    pub watching: bool,
}

/// One event: `type` and its fields, as `isocast watch --json` prints it,
/// numbered and timed.
#[derive(Debug, Serialize, JsonSchema)]
pub struct EventEntry {
    /// Its number; numbers only grow.
    pub seq: u64,
    /// How long ago it arrived.
    pub seconds_ago: f64,
    /// The event: `type` (setup_changed, started, status, warning,
    /// scene_changed, preview_changed, marker, stream, stopped,
    /// source_changed, engine_changed, or connection when the server's
    /// connection to isocastd starts or ends) and its fields.
    #[serde(flatten)]
    pub event: Map<String, Value>,
}

pub struct Events {
    buffer: Mutex<Buffer>,
    /// Signalled when an event arrives, the connection changes, or a tool
    /// asks the watcher to connect now.
    changed: Condvar,
}

#[derive(Default)]
struct Buffer {
    entries: VecDeque<Entry>,
    /// The seq of the newest event so far (0 before any).
    last_seq: u64,
    /// The seq of the newest event dropped to make room.
    dropped_through: u64,
    connected: bool,
    /// A recording is running, as far as the events say.
    recording: bool,
    /// A tool wants the watcher to try connecting now.
    wake: bool,
}

struct Entry {
    seq: u64,
    at: Instant,
    kind: String,
    event: Map<String, Value>,
}

impl Events {
    /// Start watching isocastd (whenever it runs) on a thread of its own.
    pub fn start() -> Arc<Self> {
        let events = Arc::new(Self {
            buffer: Mutex::new(Buffer::default()),
            changed: Condvar::new(),
        });
        let watcher = events.clone();
        std::thread::Builder::new()
            .name("mcp-events".into())
            .spawn(move || watcher.watch())
            .expect("could not start the event watcher");
        events
    }

    fn lock(&self) -> MutexGuard<'_, Buffer> {
        self.buffer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Watch isocastd until the process ends, reconnecting whenever it
    /// comes back (restarted, or launched by a tool, the CLI or the app).
    fn watch(&self) {
        loop {
            // Levels aren't kept, so isocastd needn't send them (20 a
            // second): this thread then wakes only for events it keeps.
            let all_but_levels = WatchParams::except(&[EventKind::Levels]);
            if let Ok(Some(client)) = Client::connect()
                && let Ok((live, stream)) = client.watch_values_some(Some(all_but_levels))
            {
                let revision = live.get("revision").cloned().unwrap_or_default();
                self.connection(true, revision, None);
                let mut ended = "isocastd closed the connection".to_string();
                for event in stream {
                    match event {
                        Ok(Value::Object(event)) => self.push(event),
                        Ok(_) => {}
                        Err(err) => {
                            ended = format!("{err:#}");
                            break;
                        }
                    }
                }
                self.connection(false, Value::Null, Some(ended));
            }
            let buffer = self.lock();
            let (mut buffer, _) = self
                .changed
                .wait_timeout_while(buffer, RETRY, |buffer| !buffer.wake)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            buffer.wake = false;
        }
    }

    fn connection(&self, connected: bool, revision: Value, message: Option<String>) {
        let mut event = Map::new();
        event.insert("type".into(), CONNECTION.into());
        event.insert("connected".into(), connected.into());
        if connected {
            event.insert("revision".into(), revision);
            event.insert(
                "message".into(),
                "receiving isocastd's events from here on".into(),
            );
        } else {
            let why = message.unwrap_or_default();
            let mut text = format!("lost isocastd ({why}); events are missed until it is back");
            if self.lock().recording {
                text.push_str(
                    "; it was recording without stopping, so check that take's files (get_status \
                     shows the last session once isocastd is back)",
                );
            }
            event.insert("message".into(), text.into());
        }
        let mut buffer = self.lock();
        buffer.connected = connected;
        buffer.recording &= connected;
        drop(buffer);
        self.push(event);
    }

    /// Keep an event, dropping the oldest when full.
    fn push(&self, event: Map<String, Value>) {
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if kind == NOT_KEPT {
            return;
        }
        let mut buffer = self.lock();
        match kind.as_str() {
            "started" | "status" => buffer.recording = true,
            "stopped" => buffer.recording = false,
            _ => {}
        }
        if NEWEST_ONLY.contains(&kind.as_str()) {
            buffer.entries.retain(|entry| entry.kind != kind);
        }
        buffer.last_seq += 1;
        let seq = buffer.last_seq;
        buffer.entries.push_back(Entry {
            seq,
            at: Instant::now(),
            kind,
            event,
        });
        while buffer.entries.len() > CAPACITY {
            if let Some(dropped) = buffer.entries.pop_front() {
                buffer.dropped_through = dropped.seq;
            }
        }
        drop(buffer);
        self.changed.notify_all();
    }

    /// Make sure events are being received before a tool acts, so the
    /// events its action causes are kept: launch isocastd if it isn't
    /// running and wait (briefly) for the watcher to connect.
    pub fn ensure_watching(&self) {
        if self.lock().connected {
            return;
        }
        // The tool's own call reports a launch failure.
        if Client::connect_or_launch().is_err() {
            return;
        }
        let mut buffer = self.lock();
        buffer.wake = true;
        self.changed.notify_all();
        let _ = self
            .changed
            .wait_timeout_while(buffer, CONNECT_WAIT, |buffer| !buffer.connected);
    }

    /// Read events (get_events).
    pub fn read(&self, params: GetEventsParams) -> Result<EventsRead, String> {
        let kinds = params.kinds.map(check_kinds).transpose()?;
        let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
        if !(1..=MAX_LIMIT).contains(&limit) {
            return Err(format!("limit: must be 1 to {MAX_LIMIT}, not {limit}"));
        }
        let wait = params.wait_seconds.unwrap_or(0.0);
        if !(0.0..=MAX_WAIT_SECONDS).contains(&wait) {
            return Err(format!(
                "wait_seconds: must be 0 to {MAX_WAIT_SECONDS}, not {wait}"
            ));
        }
        let after = params.after.unwrap_or(0);
        let matches = |entry: &Entry| {
            entry.seq > after
                && kinds
                    .as_ref()
                    .is_none_or(|kinds| kinds.contains(&entry.kind))
        };
        let mut buffer = self.lock();
        if wait > 0.0 {
            buffer = self
                .changed
                .wait_timeout_while(buffer, Duration::from_secs_f64(wait), |buffer| {
                    !buffer.entries.iter().any(matches)
                })
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
        let matching: Vec<&Entry> = buffer.entries.iter().filter(|e| matches(e)).collect();
        let (chosen, more, next) = if params.after.is_some() {
            let chosen = &matching[..matching.len().min(limit)];
            let more = matching.len() > limit;
            let next = match chosen.last() {
                Some(last) if more => last.seq,
                _ => buffer.last_seq.max(after),
            };
            (chosen, more, next)
        } else {
            let chosen = &matching[matching.len().saturating_sub(limit)..];
            (chosen, matching.len() > limit, buffer.last_seq)
        };
        let now = Instant::now();
        let events = chosen
            .iter()
            .map(|entry| EventEntry {
                seq: entry.seq,
                seconds_ago: (now.duration_since(entry.at).as_secs_f64() * 10.0).round() / 10.0,
                event: entry.event.clone(),
            })
            .collect();
        Ok(EventsRead {
            events,
            next,
            more,
            missed: params.after.is_some() && buffer.dropped_through > after,
            watching: buffer.connected,
        })
    }
}

/// Check a `kinds` filter against the kinds that are kept, naming the bad
/// one's place and listing the valid ones.
fn check_kinds(kinds: Vec<String>) -> Result<Vec<String>, String> {
    let valid = kept_kinds();
    for (index, kind) in kinds.iter().enumerate() {
        if kind == NOT_KEPT {
            return Err(format!(
                "kinds[{index}]: {NOT_KEPT} aren't kept (about 20 a second); get_status has \
                 each source's level"
            ));
        }
        if !valid.contains(kind) {
            return Err(format!(
                "kinds[{index}]: unknown kind {kind:?}; kinds: {}",
                valid.join(", ")
            ));
        }
    }
    Ok(kinds)
}

/// Every kind of event isocastd sends (read from [`Event`]'s schema, so a
/// new kind is never forgotten) except levels, plus `connection`.
fn kept_kinds() -> Vec<String> {
    let schema = schemars::schema_for!(Event);
    let mut kinds: Vec<String> = schema
        .get("oneOf")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|variant| {
            let tag = variant.get("properties")?.get("type")?;
            tag.get("const")
                .or_else(|| tag.get("enum")?.get(0))?
                .as_str()
                .map(str::to_string)
        })
        .filter(|kind| kind != NOT_KEPT)
        .collect();
    kinds.push(CONNECTION.into());
    kinds
}
