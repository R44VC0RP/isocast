//! Streaming the program to the live setup's destinations at once (#73):
//! `stream_start`, `stream_stop`, the destination list's methods, and what
//! `status` says about each destination.
//!
//! Every destination is its own [`Stream`] (its own key, connection, send
//! queue, reconnects and counters), so one failing never stops another or
//! the recording. Destinations sending the same format subscribe to one
//! shared program encoder (`isocast_engine::stream::encoder`); one with its
//! own bitrate or size gets an encoder of its own only if this Mac's
//! encoders have room (#66's estimate), else the start is refused.
//!
//! The vertical canvas (#86) streams its own program to its own
//! `[vertical.stream]` destinations: their ids, keys and states are its
//! own. The daemon keeps every canvas's streams in one list, a vertical
//! destination under `vertical:<id>` (ids have no colons, so never a main
//! one's), and each call picks one canvas's.

use std::collections::HashMap;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use isocast_engine::capacity;
use isocast_engine::stream::{KEY_ENV, OnChange, Stream};
use isocast_protocol::CanvasId;
use isocast_protocol::api::{
    ApplyParams, Capacity, DESTINATION_CLEARABLE, Event, RpcError, StreamDestinationParams,
    StreamDestinations, StreamStartParams, Streaming,
};
use isocast_protocol::change::Change;
use isocast_protocol::stream::{
    Destination, MAIN_DESTINATION, MAX_DESTINATIONS, StreamDestination, StreamSettings,
    StreamState, StreamStatus,
};

use crate::{
    Daemon, STREAM_START_TIMEOUT, State, has_stream_key, key_label, key_problem, key_service,
    stream_key, stream_settings,
};

fn failed(message: impl Into<String>) -> RpcError {
    RpcError {
        code: RpcError::FAILED,
        message: message.into(),
    }
}

fn wrong(message: impl Into<String>) -> RpcError {
    RpcError {
        code: RpcError::WRONG_STATE,
        message: message.into(),
    }
}

fn invalid(message: impl Into<String>) -> RpcError {
    RpcError {
        code: RpcError::INVALID_PARAMS,
        message: message.into(),
    }
}

/// Where `canvas`'s destination `id` is kept in the daemon's state: the
/// id for the main canvas, `<canvas>:<id>` for the others (`vertical:…`;
/// ids have no colons, so never a main one's).
pub(crate) fn slot(canvas: CanvasId, id: &str) -> String {
    if canvas.is_main() {
        id.to_string()
    } else {
        format!("{canvas}:{id}")
    }
}

/// The canvas and destination id a state slot holds.
pub(crate) fn slot_canvas(slot: &str) -> (CanvasId, &str) {
    match slot
        .split_once(':')
        .and_then(|(canvas, id)| Some((CanvasId::parse(canvas).ok()?, id)))
    {
        Some((canvas, id)) => (canvas, id),
        None => (CanvasId::Main, slot),
    }
}

/// "vertical " for the vertical canvas, "canvas \"x\" " for another, else
/// nothing, before "stream".
fn canvas_words(canvas: CanvasId) -> String {
    match canvas {
        CanvasId::Main => String::new(),
        CanvasId::Vertical => "vertical ".into(),
        other => format!("{other} "),
    }
}

/// ` --canvas <id>` for commands about a canvas other than the main one.
fn canvas_flag(canvas: CanvasId) -> String {
    if canvas.is_main() {
        String::new()
    } else {
        format!(" --canvas {canvas}")
    }
}

/// The live setup's stream settings for `canvas`: `[stream]`, the
/// vertical canvas's `[vertical.stream]` or a `[[canvases]]` entry's.
pub(crate) fn canvas_stream_settings(state: &State, canvas: CanvasId) -> Option<StreamSettings> {
    if canvas.is_main() {
        stream_settings(state)
    } else {
        state.setup.as_ref()?.stream_of(canvas).cloned()
    }
}

/// A slot's status, saying its canvas (the engine's streams may not).
fn slot_status(slot: &str, output: &Output) -> StreamStatus {
    let mut status = output.status();
    status.canvas = slot_canvas(slot).0;
    status
}

/// A destination's stream in the daemon's state.
pub(crate) enum Output {
    /// Running, or ended (kept for its final status until the next start).
    Running(Stream),
    /// Its stream couldn't be created (its encoder didn't start).
    Failed(Box<StreamStatus>),
}

impl Output {
    pub(crate) fn status(&self) -> StreamStatus {
        match self {
            Self::Running(stream) => stream.status(),
            Self::Failed(status) => (**status).clone(),
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        matches!(self, Self::Running(stream) if stream.status().is_active())
    }
}

/// Losses already warned about, per destination.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Reported {
    frames: u64,
    audio: u64,
    bitrate: bool,
}

/// "the stream", or "the stream to youtube" when the canvas has more
/// than one destination ("the vertical stream…" for the vertical canvas).
fn label(state: &State, slot: &str) -> String {
    let (canvas, id) = slot_canvas(slot);
    let several = canvas_stream_settings(state, canvas).is_some_and(|s| !s.destinations.is_empty())
        || state
            .streams
            .iter()
            .filter(|(other, _)| slot_canvas(other).0 == canvas)
            .count()
            > 1;
    let what = canvas_words(canvas);
    if several {
        format!("the {what}stream to {id}")
    } else {
        format!("the {what}stream")
    }
}

/// Every main-canvas destination's status (see [`canvas_statuses`]).
pub(crate) fn stream_statuses(state: &State) -> Vec<StreamStatus> {
    canvas_statuses(state, CanvasId::Main)
}

/// Every destination's status on `canvas`, the main one first: the
/// running (or ended) stream's own, else the live setup's destination,
/// offline. A stream still running for a destination the setup no longer
/// has comes last.
pub(crate) fn canvas_statuses(state: &State, canvas: CanvasId) -> Vec<StreamStatus> {
    let mut out = Vec::new();
    let from_env = std::env::var(KEY_ENV).is_ok_and(|key| !key.trim().is_empty());
    if let Some(settings) = canvas_stream_settings(state, canvas) {
        for destination in settings.resolve_canvas(canvas) {
            let key = slot(canvas, &destination.id);
            let known = state
                .streams
                .iter()
                .find(|(other, _)| *other == key)
                .map(|(other, output)| slot_status(other, output));
            if let Some(mut status) = known.clone().filter(StreamStatus::is_active) {
                status.enabled = destination.enabled;
                out.push(status);
                continue;
            }
            // An ended stream keeps its state, error and counts, but the
            // service, server and key are the setup's: what a new stream
            // would use (#59).
            let account = &destination.key_account;
            let key_set = from_env || has_stream_key(state, account).unwrap_or(false);
            let mut offline = StreamStatus::offline_destination(&destination, key_set);
            offline.canvas = canvas;
            let mut status = match known {
                Some(ended) => StreamStatus {
                    service: offline.service,
                    server: offline.server,
                    enabled: offline.enabled,
                    key_set,
                    key_problem: None,
                    ..ended
                },
                None => offline,
            };
            if key_set && !from_env {
                status.key_problem = key_problem(account);
            }
            out.push(status);
        }
    }
    for (key, output) in &state.streams {
        let (other, id) = slot_canvas(key);
        if other == canvas && !out.iter().any(|status| status.id == id) {
            out.push(slot_status(key, output));
        }
    }
    out
}

/// What `status.stream` says: the main destination's status (or, with no
/// `[stream]` left in the setup, the first stream still known).
pub(crate) fn stream_status(state: &State) -> Option<StreamStatus> {
    let statuses = stream_statuses(state);
    statuses
        .iter()
        .find(|status| status.id == MAIN_DESTINATION)
        .or(statuses.first())
        .cloned()
}

impl State {
    /// A stream is running (or reconnecting) to any destination, so the
    /// engine is needed.
    pub(crate) fn streaming(&self) -> bool {
        self.streams.iter().any(|(_, output)| output.is_active())
    }

    /// A canvas other than the main one streams, the first such.
    pub(crate) fn streaming_canvas(&self) -> Option<CanvasId> {
        self.streams
            .iter()
            .find(|(key, output)| !slot_canvas(key).0.is_main() && output.is_active())
            .map(|(key, _)| slot_canvas(key).0)
    }

    /// Any canvas other than the main one streams.
    pub(crate) fn streaming_extra(&self) -> bool {
        self.streaming_canvas().is_some()
    }

    /// A stream of `canvas` is running (or reconnecting).
    pub(crate) fn streaming_on(&self, canvas: CanvasId) -> bool {
        self.streams
            .iter()
            .any(|(key, output)| slot_canvas(key).0 == canvas && output.is_active())
    }
}

impl Daemon {
    /// Start streaming the live engine's program to the live setup's
    /// enabled destinations (or `only` those), starting the engine if
    /// needed. Destinations already streaming are left alone.
    pub(crate) fn stream_start(
        self: &Arc<Self>,
        params: StreamStartParams,
    ) -> Result<Streaming, RpcError> {
        let canvas = params.canvas;
        let mut state = self.state.lock();
        let settings = match canvas_stream_settings(&state, canvas) {
            Some(settings) => settings,
            None if canvas.is_main() => {
                return Err(wrong(
                    "the live setup has no stream; add one (for example the patch \
                     {\"stream\": {\"service\": \"twitch\"}} with `isocast apply --patch` or \
                     MCP's apply_change, or `isocast stream destination add twitch`), then set \
                     the key (`isocast stream key set`, or MCP's set_stream_key)",
                ));
            }
            None => return Err(no_vertical_stream(&state, canvas)),
        };
        let chosen = choose(&settings, canvas, &params.only)?;
        // This canvas's destinations already streaming, by id.
        let active = active_ids(&state, canvas);
        let starting: Vec<Destination> = chosen
            .into_iter()
            .filter(|d| !active.contains(&d.id))
            .collect();
        if starting.is_empty() {
            return Err(wrong(format!(
                "already streaming {}to {}; stop it first (`isocast stream stop{}`, or MCP's \
                 stop_stream)",
                canvas_words(canvas),
                active.join(", "),
                canvas_flag(canvas),
            )));
        }
        // Every destination must be ready before any starts: nothing
        // half-starts on a setup problem.
        let mut keys = Vec::new();
        let mut problems = Vec::new();
        for destination in &starting {
            if let Err(err) = destination.settings.server() {
                let prefix = match &state.setup {
                    Some(setup) if !canvas.is_main() => format!("{}.", setup.canvas_field(canvas)),
                    _ => String::new(),
                };
                problems.push(if destination.id == MAIN_DESTINATION {
                    format!("{prefix}{err}")
                } else {
                    format!(
                        "{prefix}stream.destinations ({}): {}",
                        destination.id,
                        err.trim_start_matches("stream.")
                    )
                });
                continue;
            }
            match destination_key(&state, destination, canvas) {
                Ok(key) => keys.push(key),
                Err(message) => problems.push(message),
            }
        }
        if !problems.is_empty() {
            return Err(failed(problems.join("; ")));
        }
        self.check_stream_capacity(&state, canvas, &starting)?;
        for (destination, key) in starting.iter().zip(&keys) {
            isocast_diagnostics::redact::remember_secret(
                key_service(),
                &destination.key_account,
                key,
            );
        }
        // Starting the engine happens outside the lock; until the streams
        // are in the state, this keeps the engine from idling out.
        state.streams_starting += 1;
        drop(state);
        let engine = self.ensure_engine();
        let mut state = self.state.lock();
        state.streams_starting -= 1;
        let engine = engine?;
        // A vertical canvas whose compositor failed has no picture to send. Checked under
        // the lock that adds the streams, so a failure after this check
        // still ends them (#96).
        if !canvas.is_main()
            && let Some(reason) = engine.canvas_stopped(canvas)
        {
            let recovery = if reason.contains("scene state is broken")
                || reason.contains("scene worker has broken state")
            {
                "restart the engine once recording and streaming stop"
            } else if canvas.is_vertical() {
                "after recording stops, turn the vertical canvas off and on again (Settings › Video, \
                 or apply {\"vertical\": null} and then its settings again), or restart the engine"
            } else {
                "after recording stops, turn the canvas off and on again (Settings › Canvases), \
                 or restart the engine"
            };
            let whose = match canvas {
                CanvasId::Vertical => "the vertical canvas".to_string(),
                other => format!("canvas {other:?}"),
            };
            return Err(wrong(format!(
                "{whose} stopped ({reason}), so it can't stream; {recovery}"
            )));
        }
        let mut started = Vec::new();
        for (destination, key) in starting.into_iter().zip(keys) {
            let id = destination.id.clone();
            let slot_id = slot(canvas, &id);
            // Another start may have begun this one meanwhile.
            if state
                .streams
                .iter()
                .any(|(other, output)| *other == slot_id && output.is_active())
            {
                continue;
            }
            // Its previous stream, finished, makes way.
            state.streams.retain(|(other, _)| *other != slot_id);
            state.stream_reported.remove(&slot_id);
            self.stream_by
                .lock()
                .insert(slot_id.clone(), crate::callers::by());
            let daemon = Arc::downgrade(self);
            let attributed = slot_id.clone();
            let on_change: OnChange = Box::new(move |status| {
                if let Some(daemon) = daemon.upgrade() {
                    let mut stream = status.clone();
                    stream.canvas = canvas;
                    let by = daemon.stream_by.lock().get(&attributed).cloned().flatten();
                    daemon.publish(Event::Stream { stream, by });
                }
            });
            let settings = destination.settings.clone();
            let made = if canvas.is_main() {
                engine.stream(&id, settings, key, on_change)
            } else {
                engine.stream_canvas(canvas, &id, settings, key, on_change)
            };
            let output = match made {
                Ok(stream) => Output::Running(stream),
                Err(err) => {
                    // One destination's encoder failing doesn't stop the
                    // others.
                    let mut status = StreamStatus::offline_destination(&destination, true);
                    status.canvas = canvas;
                    status.state = StreamState::Failed;
                    status.last_error = Some(format!("{err:#}"));
                    self.publish(Event::Stream {
                        stream: status.clone(),
                        by: crate::callers::by(),
                    });
                    Output::Failed(Box::new(status))
                }
            };
            // In the state while it connects, so the engine stays up for
            // it, a second start leaves it alone and `stream stop` can
            // cancel it.
            state.streams.push((slot_id.clone(), output));
            started.push(slot_id);
        }
        // The main canvas's first, each canvas's main destination first, as
        // the setups list them.
        let canvases = state
            .setup
            .as_ref()
            .map(|setup| setup.canvas_ids())
            .unwrap_or_else(|| vec![CanvasId::Main]);
        let orders: Vec<Vec<String>> = canvases
            .iter()
            .map(|&canvas| {
                canvas_stream_settings(&state, canvas)
                    .map(|s| s.destination_ids())
                    .unwrap_or_default()
            })
            .collect();
        state.streams.sort_by_key(|(key, _)| {
            let (other, id) = slot_canvas(key);
            let place = canvases
                .iter()
                .position(|c| *c == other)
                .unwrap_or(usize::MAX);
            let order = orders.get(place).map_or(&[][..], |o| &o[..]);
            (
                place,
                order.iter().position(|o| o == id).unwrap_or(usize::MAX),
            )
        });
        drop(state);
        if started.is_empty() {
            return Err(wrong(
                "already streaming to every destination asked for; stop it first",
            ));
        }
        let deadline = Instant::now() + STREAM_START_TIMEOUT;
        let finals = loop {
            let state = self.state.read();
            let statuses: Vec<StreamStatus> = started
                .iter()
                .filter_map(|key| {
                    state
                        .streams
                        .iter()
                        .find(|(other, _)| other == key)
                        .map(|(other, output)| slot_status(other, output))
                })
                .collect();
            drop(state);
            if statuses.is_empty() {
                return Err(failed("the stream was stopped before it went live"));
            }
            let connecting = statuses
                .iter()
                .any(|status| status.state == StreamState::Connecting);
            if !connecting || Instant::now() >= deadline {
                break statuses;
            }
            thread::sleep(Duration::from_millis(20));
        };
        let ended = |status: &StreamStatus| {
            matches!(status.state, StreamState::Failed | StreamState::Offline)
        };
        if finals.iter().all(ended) {
            let why = |status: &StreamStatus| {
                status
                    .last_error
                    .clone()
                    .unwrap_or_else(|| "the stream stopped".into())
            };
            return Err(failed(match finals.as_slice() {
                [only] => why(only),
                all => format!(
                    "no destination went live: {}",
                    all.iter()
                        .map(|status| format!("{}: {}", status.id, why(status)))
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
            }));
        }
        let destinations = canvas_statuses(&self.state.read(), canvas);
        let first = finals
            .iter()
            .find(|status| status.id == MAIN_DESTINATION)
            .unwrap_or(&finals[0])
            .clone();
        Ok(Streaming {
            stream: first,
            destinations,
        })
    }

    /// Refuse a start whose destinations need more H.264 encodes than this
    /// Mac's encoders can take (#66's estimate). Destinations sharing a
    /// format on one canvas share an encode, so this only matters when one
    /// has its own bitrate or size, or when both canvases stream (each
    /// canvas's program is its own encode). Judged on the whole live setup:
    /// both canvases, with every stream running on either and these.
    fn check_stream_capacity(
        &self,
        state: &State,
        canvas: CanvasId,
        starting: &[Destination],
    ) -> Result<(), RpcError> {
        let Some(setup) = state.setup.as_ref() else {
            return Ok(());
        };
        // The destinations that will stream: the running ones (either
        // canvas's) and these.
        let on = |other: CanvasId, id: &str| {
            state
                .streams
                .iter()
                .any(|(key, output)| output.is_active() && slot_canvas(key) == (other, id))
                || (other == canvas && starting.iter().any(|d| d.id == id))
        };
        let switch = |stream: &mut StreamSettings, other: CanvasId| {
            stream.enabled = on(other, MAIN_DESTINATION);
            for extra in &mut stream.destinations {
                extra.enabled = on(other, &extra.id);
            }
        };
        let mut candidate = setup.clone();
        let mut encodes = Vec::new();
        let main_size = (setup.canvas.width, setup.canvas.height);
        if let Some(stream) = candidate.stream.as_mut() {
            switch(stream, CanvasId::Main);
            let made = stream.encodes(main_size);
            encodes.extend(made.into_iter().map(|e| (CanvasId::Main, e)));
        }
        if candidate
            .stream
            .as_ref()
            .is_some_and(|s| s.encodes(main_size).is_empty())
        {
            candidate.stream = None;
        }
        // Every other canvas (`[vertical]`, `[[canvases]]`, #84).
        for other in setup.extra_canvas_ids() {
            let Some(extra) = candidate.extra_canvas_mut(other) else {
                continue;
            };
            let size = (extra.width, extra.height);
            if let Some(stream) = extra.stream.as_mut() {
                switch(stream, other);
                let made = stream.encodes(size);
                if made.is_empty() {
                    extra.stream = None;
                }
                encodes.extend(made.into_iter().map(|e| (other, e)));
            }
        }
        if encodes.len() <= 1 {
            return Ok(());
        }
        // Judged against what runs now: the recording's files only while
        // one records (starting one later warns on its own, #66).
        if state.session.is_none() {
            candidate.recording.program.enabled = false;
            candidate.recording.iso.enabled = false;
            if let Some(vertical) = candidate.vertical.as_mut() {
                vertical.recording = None;
            }
            for extra in &mut candidate.canvases {
                extra.recording = None;
            }
        }
        let memory = capacity::memory_path(state.file.legacy());
        let estimate = match crate::estimate_capacity(
            &candidate,
            state.engine.as_deref(),
            [true; 2],
            state.session.is_some(),
            &memory,
            None,
            &crate::prores_backends(state),
        ) {
            Ok(estimate) => estimate,
            // No estimate for this Mac: nothing to judge by.
            Err(_) => return Ok(()),
        };
        if estimate.capacity != Capacity::CannotKeepUp {
            return Ok(());
        }
        let name = |other: CanvasId, id: &str| {
            if other.is_main() {
                id.to_string()
            } else {
                format!("{id} ({other})")
            }
        };
        let own: Vec<String> = encodes[1..]
            .iter()
            .flat_map(|(other, encode)| encode.ids.iter().map(move |id| (*other, id)))
            .filter(|(other, id)| *other == canvas && starting.iter().any(|d| d.id == **id))
            .map(|(other, id)| name(other, id))
            .collect();
        let (shared_canvas, shared) = &encodes[0];
        let one = own.len() <= 1;
        let flag = canvas_flag(canvas);
        Err(failed(format!(
            "not streaming: {} {} an H.264 encoder of {} own, as {} canvas, bitrate, size, \
             rate control, keyframe interval or audio bitrate differs from {}'s {}×{} at {} kb/s; \
             with {} encodes at once{}, {}. Give {} the same settings to share one encoder \
             (`isocast stream destination set ID{flag} --clear video_bitrate_kbps --clear size \
             --clear rate_control --clear keyframe_interval_seconds --clear audio_bitrate_kbps`), stop the other canvas's \
             streams, or start without {} (`isocast stream start{flag} --only …`)",
            if own.is_empty() {
                "these destinations".to_string()
            } else {
                own.join(", ")
            },
            if one { "needs" } else { "need" },
            if one { "its" } else { "their" },
            if one { "its" } else { "their" },
            name(*shared_canvas, &shared.ids[0]),
            shared.width,
            shared.height,
            shared.video_bitrate_kbps,
            encodes.len(),
            if state.session.is_some() {
                " beside the recording"
            } else {
                ""
            },
            estimate.summary,
            if one { "it" } else { "them" },
            if one { "it" } else { "them" },
        )))
    }

    /// Stop streaming to every destination (or `only` those); the others
    /// keep going.
    pub(crate) fn stream_stop(&self, params: StreamStartParams) -> Result<Streaming, RpcError> {
        let canvas = params.canvas;
        let mut state = self.state.lock();
        // This canvas's streams, by destination id; the other canvas's are
        // left alone.
        let mine = |key: &str| {
            let (other, id) = slot_canvas(key);
            (other == canvas).then(|| id.to_string())
        };
        if !params.only.is_empty() {
            let known: Vec<String> = canvas_stream_settings(&state, canvas)
                .map(|s| s.destination_ids())
                .unwrap_or_default()
                .into_iter()
                .chain(state.streams.iter().filter_map(|(key, _)| mine(key)))
                .collect();
            for id in &params.only {
                if !known.contains(id) {
                    return Err(invalid(format!(
                        "only: no destination {id:?}; destinations: {}",
                        known.join(", ")
                    )));
                }
            }
        }
        let wanted = |key: &str| {
            mine(key).is_some_and(|id| params.only.is_empty() || params.only.contains(&id))
        };
        // Their slots.
        let active: Vec<String> = state
            .streams
            .iter()
            .filter(|(key, output)| wanted(key) && output.is_active())
            .map(|(key, _)| key.clone())
            .collect();
        if active.is_empty() {
            // A contained canvas failure may have ended the stream since
            // the app's last status poll. Stop is already satisfied; keep
            // its failed status/reason visible rather than toasting an error.
            if !canvas.is_main() {
                let destinations = canvas_statuses(&state, canvas);
                if let Some(stream) = destinations
                    .iter()
                    .find(|s| params.only.is_empty() || params.only.contains(&s.id))
                    .cloned()
                {
                    return Ok(Streaming {
                        stream,
                        destinations,
                    });
                }
            }
            // A stream that failed stays in the status until the next
            // start; say so rather than only "not streaming".
            let several = state
                .streams
                .iter()
                .filter(|(key, _)| mine(key).is_some())
                .count()
                > 1;
            let failures: Vec<String> = state
                .streams
                .iter()
                .filter(|(key, _)| wanted(key))
                .map(|(key, output)| (slot_canvas(key).1, output.status()))
                .filter(|(_, status)| status.state == StreamState::Failed)
                .map(|(id, status)| {
                    let why = status.last_error.as_deref().unwrap_or("no reason given");
                    if several {
                        format!("{id}: {why}")
                    } else {
                        why.to_string()
                    }
                })
                .collect();
            let what = canvas_words(canvas);
            let message = if failures.is_empty() {
                match params.only.as_slice() {
                    [] => format!(
                        "not streaming{}",
                        match canvas {
                            CanvasId::Main => String::new(),
                            CanvasId::Vertical => " the vertical canvas".into(),
                            other => format!(" canvas {other:?}"),
                        }
                    ),
                    only => format!("not streaming {what}to {}", only.join(", ")),
                }
            } else {
                format!(
                    "not streaming: the {what}stream already failed ({})",
                    failures.join("; ")
                )
            };
            return Err(wrong(message));
        }
        let mut stopping = Vec::new();
        let mut kept = Vec::new();
        for (id, output) in std::mem::take(&mut state.streams) {
            match output {
                Output::Running(stream) if active.contains(&id) => stopping.push((id, stream)),
                output => kept.push((id, output)),
            }
        }
        state.streams = kept;
        drop(state);
        {
            let by = crate::callers::by();
            let mut stream_by = self.stream_by.lock();
            for (slot, _) in &stopping {
                stream_by.insert(slot.clone(), by.clone());
            }
        }
        // Each says goodbye to its server at once, rather than one after
        // another's grace period.
        let finals: Vec<StreamStatus> = thread::scope(|scope| {
            let handles: Vec<_> = stopping
                .into_iter()
                .map(|(_, stream)| scope.spawn(move || stream.stop()))
                .collect();
            handles
                .into_iter()
                .filter_map(|handle| handle.join().ok())
                .map(|mut status| {
                    status.canvas = canvas;
                    status
                })
                .collect()
        });
        let mut destinations = canvas_statuses(&self.state.read(), canvas);
        // The stopped ones as they ended (their offline status from the
        // setup loses the counters).
        for status in &finals {
            match destinations.iter_mut().find(|d| d.id == status.id) {
                Some(slot) => {
                    let enabled = slot.enabled;
                    *slot = StreamStatus {
                        enabled,
                        ..status.clone()
                    };
                }
                None => destinations.push(status.clone()),
            }
        }
        let first = finals
            .iter()
            .find(|status| status.id == MAIN_DESTINATION)
            .or(finals.first())
            .cloned()
            .ok_or_else(|| failed("the stream stopped without a status"))?;
        Ok(Streaming {
            stream: first,
            destinations,
        })
    }

    /// Fail every running stream (the program stopped coming).
    pub(crate) fn fail_streams(&self, why: &str) {
        for (_, output) in &self.state.read().streams {
            if let Output::Running(stream) = output {
                stream.fail(why.to_string());
            }
        }
    }

    /// End `canvas`'s streams (and only those) with a failure, for
    /// example when only that canvas's compositor failed (#96).
    pub(crate) fn fail_canvas_streams(&self, canvas: CanvasId, why: &str) {
        for (key, output) in &self.state.read().streams {
            if slot_canvas(key).0 == canvas
                && let Output::Running(stream) = output
            {
                stream.fail(why.to_string());
            }
        }
    }

    /// Warn about each destination's stream-only losses and sustained
    /// bitrate overshoot (once per episode), checked every second even
    /// when encoding/sending stalls.
    pub(crate) fn warn_stream_drops(&self) {
        let mut state = self.state.lock();
        let statuses: Vec<(String, StreamStatus)> = state
            .streams
            .iter()
            .filter_map(|(key, output)| match output {
                Output::Running(stream) => Some((key.clone(), stream.status())),
                Output::Failed(_) => None,
            })
            .collect();
        let mut warnings = Vec::new();
        for (key, status) in statuses {
            let what = label(&state, &key);
            let (canvas, _) = slot_canvas(&key);
            let reported = state.stream_reported.entry(key.clone()).or_default();
            // While reconnecting, the stream's state events say why frames
            // go.
            if status.state != StreamState::Live {
                *reported = Reported {
                    frames: status.dropped_frames,
                    audio: status.audio_dropouts,
                    bitrate: false,
                };
                continue;
            }
            if status.dropped_frames > reported.frames {
                let new = status.dropped_frames - reported.frames;
                reported.frames = status.dropped_frames;
                warnings.push(format!(
                    "{what} dropped {new} frame(s) ({} so far, congestion {:.0}%); the program \
                     and recordings are unaffected",
                    status.dropped_frames,
                    status.congestion * 100.0
                ));
            }
            if status.audio_dropouts > reported.audio {
                let new = status.audio_dropouts - reported.audio;
                reported.audio = status.audio_dropouts;
                warnings.push(format!(
                    "{what} dropped {new} audio chunk(s) ({} so far); the stream encoder or \
                     connection could not keep up",
                    status.audio_dropouts,
                ));
            }
            if let Some(warning) = &status.bitrate_warning
                && !reported.bitrate
            {
                warnings.push(if status.id == MAIN_DESTINATION && canvas.is_main() {
                    warning.clone()
                } else {
                    format!("{}{}: {warning}", canvas_words(canvas), status.id)
                });
            }
            reported.bitrate = status.bitrate_warning.is_some();
        }
        drop(state);
        for message in warnings {
            self.publish(Event::Warning { message });
        }
    }

    /// Add a destination to the live setup's `[stream]` (making it, as the
    /// main destination, if there is none).
    pub(crate) fn stream_destination_add(
        &self,
        params: StreamDestinationParams,
    ) -> Result<StreamDestinations, RpcError> {
        let Some(service) = params.service else {
            let ids: Vec<&str> = isocast_protocol::stream::STREAM_SERVICES
                .iter()
                .map(|s| s.id())
                .collect();
            return Err(invalid(format!(
                "service: a destination needs a service ({})",
                ids.join(", ")
            )));
        };
        if !params.clear.is_empty() {
            return Err(invalid(
                "clear: only `stream_destination_set` clears fields; leave them out instead",
            ));
        }
        let canvas = params.canvas;
        let (revision, mut setup) = self.live_setup()?;
        let section = stream_section(&mut setup, canvas)?;
        let id = match section.as_mut() {
            None => {
                if params
                    .id
                    .as_deref()
                    .is_some_and(|id| id != MAIN_DESTINATION)
                {
                    return Err(invalid(format!(
                        "id: the {}canvas has no stream yet, so its first destination is the \
                         {} section itself, id \"main\"; leave the id out",
                        canvas_words(canvas),
                        section_name(canvas),
                    )));
                }
                let mut stream: StreamSettings =
                    serde_json::from_value(serde_json::json!({ "service": service }))
                        .map_err(|err| failed(err.to_string()))?;
                stream.server = params.server.clone();
                stream.enabled = params.enabled.unwrap_or(true);
                if let Some(kbps) = params.video_bitrate_kbps {
                    stream.video_bitrate_kbps = kbps;
                }
                (stream.width, stream.height) = (params.width, params.height);
                stream.rate_control = params.rate_control;
                if let Some(seconds) = params.keyframe_interval_seconds {
                    stream.keyframe_interval_seconds = seconds;
                }
                if let Some(kbps) = params.audio_bitrate_kbps {
                    stream.audio_bitrate_kbps = kbps;
                }
                *section = Some(stream);
                MAIN_DESTINATION.to_string()
            }
            Some(stream) => {
                if stream.destinations.len() + 1 >= MAX_DESTINATIONS {
                    return Err(failed(format!(
                        "the live setup's {} already has {} destinations, the most it may have",
                        section_name(canvas),
                        stream.destinations.len() + 1
                    )));
                }
                let taken = stream.destination_ids();
                let id = match &params.id {
                    Some(id) if taken.contains(id) => {
                        return Err(invalid(format!(
                            "id: {id:?} is taken; destinations: {}",
                            taken.join(", ")
                        )));
                    }
                    Some(id) => id.clone(),
                    None => {
                        let base = service.id();
                        (1..)
                            .map(|n| {
                                if n == 1 {
                                    base.to_string()
                                } else {
                                    format!("{base}-{n}")
                                }
                            })
                            .find(|id| !taken.contains(id))
                            .expect("a free id")
                    }
                };
                stream.destinations.push(StreamDestination {
                    id: id.clone(),
                    service,
                    server: params.server.clone(),
                    enabled: params.enabled.unwrap_or(true),
                    video_bitrate_kbps: params.video_bitrate_kbps,
                    width: params.width,
                    height: params.height,
                    rate_control: params.rate_control,
                    keyframe_interval_seconds: params.keyframe_interval_seconds,
                    audio_bitrate_kbps: params.audio_bitrate_kbps,
                });
                id
            }
        };
        self.apply_destinations(revision, setup, canvas, Some(id))
    }

    /// Remove `canvas`'s destination `id` from the live setup (not while
    /// it streams).
    pub(crate) fn stream_destination_remove(
        &self,
        id: String,
        canvas: CanvasId,
    ) -> Result<StreamDestinations, RpcError> {
        let (revision, mut setup) = self.live_setup()?;
        let Some(stream) = stream_section(&mut setup, canvas)?.as_mut() else {
            return Err(wrong(format!(
                "the {}canvas has no stream destinations",
                canvas_words(canvas)
            )));
        };
        if id == MAIN_DESTINATION {
            let flag = canvas_flag(canvas);
            return Err(invalid(if canvas.is_main() {
                "id: \"main\" is the [stream] section itself; turn it off instead (`isocast \
                 stream destination set main --enabled false`), or remove the whole stream with \
                 the patch {\"stream\": null}"
                    .to_string()
            } else if canvas.is_vertical() {
                format!(
                    "id: \"main\" is the [vertical.stream] section itself; turn it off instead \
                     (`isocast stream destination set main{flag} --enabled false`), or remove \
                     the vertical canvas's whole stream with the patch \
                     {{\"vertical\": {{\"stream\": null}}}}"
                )
            } else {
                format!(
                    "id: \"main\" is {} itself; turn it off instead (`isocast stream \
                     destination set main{flag} --enabled false`), or remove the canvas's whole \
                     stream with `isocast canvas set {canvas} --no-stream`",
                    section_name(canvas)
                )
            }));
        }
        let Some(index) = stream.destinations.iter().position(|d| d.id == id) else {
            return Err(invalid(format!(
                "id: no destination {id:?}; destinations: {}",
                stream.destination_ids().join(", ")
            )));
        };
        let key = slot(canvas, &id);
        if self
            .state
            .read()
            .streams
            .iter()
            .any(|(other, output)| *other == key && output.is_active())
        {
            return Err(wrong(format!(
                "streaming {}to {id}; stop it first (`isocast stream stop{} --only {id}`)",
                canvas_words(canvas),
                canvas_flag(canvas),
            )));
        }
        stream.destinations.remove(index);
        let removed = self.apply_destinations(revision, setup, canvas, None)?;
        // Its ended stream goes with it: no ghost in status or `stream stop`.
        let mut state = self.state.lock();
        state
            .streams
            .retain(|(other, output)| *other != key || output.is_active());
        state.stream_reported.remove(&key);
        let destinations = canvas_statuses(&state, canvas);
        Ok(StreamDestinations {
            destinations,
            ..removed
        })
    }

    /// Change destination `params.id` (`main`: the `[stream]` section).
    pub(crate) fn stream_destination_set(
        &self,
        params: StreamDestinationParams,
    ) -> Result<StreamDestinations, RpcError> {
        let Some(id) = params.id.clone() else {
            return Err(invalid(
                "id: which destination? Give its id (main, or one of stream.destinations)",
            ));
        };
        for field in &params.clear {
            if !DESTINATION_CLEARABLE.contains(&field.as_str()) {
                return Err(invalid(format!(
                    "clear: {field:?} can't be cleared; fields: {}",
                    DESTINATION_CLEARABLE.join(", ")
                )));
            }
        }
        let clears = |field: &str| params.clear.iter().any(|c| c == field);
        let canvas = params.canvas;
        let (revision, mut setup) = self.live_setup()?;
        let Some(stream) = stream_section(&mut setup, canvas)?.as_mut() else {
            return Err(wrong(format!(
                "the {}canvas has no stream; add a destination first (`isocast stream \
                 destination add SERVICE{}`)",
                canvas_words(canvas),
                canvas_flag(canvas),
            )));
        };
        if id == MAIN_DESTINATION {
            if let Some(service) = params.service {
                stream.service = service;
            }
            if clears("server") {
                stream.server = None;
            }
            if params.server.is_some() {
                stream.server = params.server.clone();
            }
            if let Some(enabled) = params.enabled {
                stream.enabled = enabled;
            }
            if clears("video_bitrate_kbps") {
                stream.video_bitrate_kbps = 6000;
            }
            if let Some(kbps) = params.video_bitrate_kbps {
                stream.video_bitrate_kbps = kbps;
            }
            if clears("size") {
                (stream.width, stream.height) = (None, None);
            }
            if params.width.is_some() || params.height.is_some() {
                (stream.width, stream.height) = (params.width, params.height);
            }
            if clears("rate_control") {
                stream.rate_control = None;
            }
            if params.rate_control.is_some() {
                stream.rate_control = params.rate_control;
            }
            if clears("keyframe_interval_seconds") {
                stream.keyframe_interval_seconds = 2;
            }
            if let Some(seconds) = params.keyframe_interval_seconds {
                stream.keyframe_interval_seconds = seconds;
            }
            if clears("audio_bitrate_kbps") {
                stream.audio_bitrate_kbps = 160;
            }
            if let Some(kbps) = params.audio_bitrate_kbps {
                stream.audio_bitrate_kbps = kbps;
            }
        } else {
            let ids = stream.destination_ids();
            let Some(extra) = stream.destinations.iter_mut().find(|d| d.id == id) else {
                return Err(invalid(format!(
                    "id: no destination {id:?}; destinations: {}",
                    ids.join(", ")
                )));
            };
            if let Some(service) = params.service {
                extra.service = service;
            }
            if clears("server") {
                extra.server = None;
            }
            if params.server.is_some() {
                extra.server = params.server.clone();
            }
            if let Some(enabled) = params.enabled {
                extra.enabled = enabled;
            }
            if clears("video_bitrate_kbps") {
                extra.video_bitrate_kbps = None;
            }
            if params.video_bitrate_kbps.is_some() {
                extra.video_bitrate_kbps = params.video_bitrate_kbps;
            }
            if clears("size") {
                (extra.width, extra.height) = (None, None);
            }
            if params.width.is_some() || params.height.is_some() {
                (extra.width, extra.height) = (params.width, params.height);
            }
            if clears("rate_control") {
                extra.rate_control = None;
            }
            if params.rate_control.is_some() {
                extra.rate_control = params.rate_control;
            }
            if clears("keyframe_interval_seconds") {
                extra.keyframe_interval_seconds = None;
            }
            if params.keyframe_interval_seconds.is_some() {
                extra.keyframe_interval_seconds = params.keyframe_interval_seconds;
            }
            if clears("audio_bitrate_kbps") {
                extra.audio_bitrate_kbps = None;
            }
            if params.audio_bitrate_kbps.is_some() {
                extra.audio_bitrate_kbps = params.audio_bitrate_kbps;
            }
        }
        self.apply_destinations(revision, setup, canvas, Some(id))
    }

    /// The live setup and its revision, to change.
    fn live_setup(&self) -> Result<(u64, isocast_protocol::Setup), RpcError> {
        let state = self.state.read();
        match &state.setup {
            Some(setup) => Ok((state.revision, setup.clone())),
            None => Err(state.no_setup("no setup to add a stream destination to; make one first")),
        }
    }

    fn apply_destinations(
        &self,
        revision: u64,
        setup: isocast_protocol::Setup,
        canvas: CanvasId,
        id: Option<String>,
    ) -> Result<StreamDestinations, RpcError> {
        let applied = self.apply(ApplyParams {
            revision,
            change: Change::Setup(Box::new(setup)),
            live: false,
        })?;
        Ok(StreamDestinations {
            revision: applied.revision,
            id,
            destinations: canvas_statuses(&self.state.read(), canvas),
            notes: applied.notes,
        })
    }
}

/// How the setup names `canvas`'s stream section.
fn section_name(canvas: CanvasId) -> String {
    match canvas {
        CanvasId::Main => "[stream]".into(),
        CanvasId::Vertical => "[vertical.stream]".into(),
        other => format!("canvas {other:?}'s stream"),
    }
}

/// `canvas`'s stream section of `setup`, to change: `[stream]`, or the
/// vertical canvas's `[vertical.stream]` (which needs a `[vertical]`).
fn stream_section(
    setup: &mut isocast_protocol::Setup,
    canvas: CanvasId,
) -> Result<&mut Option<StreamSettings>, RpcError> {
    if canvas.is_main() {
        return Ok(&mut setup.stream);
    }
    if !setup.has_canvas(canvas) {
        return Err(wrong(no_canvas(setup, canvas)));
    }
    Ok(&mut setup.extra_canvas_mut(canvas).expect("checked").stream)
}

/// Why `canvas` can't do something: the live setup doesn't have it.
pub(crate) fn no_canvas(setup: &isocast_protocol::Setup, canvas: CanvasId) -> String {
    if canvas.is_vertical() {
        NO_VERTICAL.to_string()
    } else {
        format!(
            "{}; add it with `isocast canvas add {canvas}` (or MCP's canvas_add)",
            setup.unknown_canvas("canvas", canvas)
        )
    }
}

/// Why the vertical canvas can't do something without a `[vertical]`.
pub(crate) const NO_VERTICAL: &str = "canvas: the live setup has no vertical canvas; add one with the \
     patch {\"vertical\": {}} (`isocast apply --patch`, or MCP's apply_change), then its \
     stream destinations with `isocast stream destination add SERVICE --canvas vertical`";

/// Why the vertical canvas can't stream: no `[vertical]`, or no
/// `[vertical.stream]` in it.
fn no_vertical_stream(state: &State, canvas: CanvasId) -> RpcError {
    if let Some(setup) = &state.setup
        && !setup.has_canvas(canvas)
    {
        return wrong(no_canvas(setup, canvas));
    }
    let whose = match canvas {
        CanvasId::Vertical => "the vertical canvas".to_string(),
        other => format!("canvas {other:?}"),
    };
    wrong(format!(
        "canvas: {whose} has no stream; add one (`isocast stream destination add twitch \
         --canvas {canvas}`, or MCP's stream_destination_add with canvas \"{canvas}\"), then \
         set its key (`isocast stream key set --canvas {canvas}`)"
    ))
}

/// `canvas`'s destinations streaming (or reconnecting), by id.
fn active_ids(state: &State, canvas: CanvasId) -> Vec<String> {
    state
        .streams
        .iter()
        .filter(|(_, output)| output.is_active())
        .map(|(key, _)| slot_canvas(key))
        .filter(|(other, _)| *other == canvas)
        .map(|(_, id)| id.to_string())
        .collect()
}

/// The destinations a start means: `only` (each must exist; a turned-off
/// one starts too), else every enabled one.
fn choose(
    settings: &StreamSettings,
    canvas: CanvasId,
    only: &[String],
) -> Result<Vec<Destination>, RpcError> {
    let all = settings.resolve_canvas(canvas);
    if only.is_empty() {
        let enabled: Vec<Destination> = all.into_iter().filter(|d| d.enabled).collect();
        if enabled.is_empty() {
            let flag = canvas_flag(canvas);
            return Err(wrong(format!(
                "every {}stream destination is turned off; turn one on (`isocast stream \
                 destination set ID{flag} --enabled true`) or name it (`isocast stream \
                 start{flag} --only ID`); destinations: {}",
                canvas_words(canvas),
                settings.destination_ids().join(", ")
            )));
        }
        return Ok(enabled);
    }
    let mut chosen = Vec::new();
    for id in only {
        match all.iter().find(|d| d.id == *id) {
            Some(destination) => {
                if !chosen.iter().any(|d: &Destination| d.id == *id) {
                    chosen.push(destination.clone());
                }
            }
            None => {
                return Err(invalid(format!(
                    "only: no destination {id:?}; destinations: {}",
                    settings.destination_ids().join(", ")
                )));
            }
        }
    }
    // In the setup's order, main first.
    chosen.sort_by_key(|d| all.iter().position(|a| a.id == d.id));
    Ok(chosen)
}

/// The key `destination` streams with: `ISOCAST_STREAM_KEY` (local
/// testing), else its Keychain item. Says what to do when there is none.
/// Resolved under the state lock (`state`), as [`crate::stream_key`] needs.
fn destination_key(
    state: &State,
    destination: &Destination,
    canvas: CanvasId,
) -> Result<String, String> {
    if let Ok(key) = std::env::var(KEY_ENV)
        && !key.trim().is_empty()
    {
        return Ok(key.trim().to_string());
    }
    let account = &destination.key_account;
    let flag = canvas_flag(canvas);
    match stream_key(state, account) {
        Ok(Some(key)) => Ok(key),
        Ok(None) if destination.id == MAIN_DESTINATION && canvas.is_main() => Err(format!(
            "no stream key is set for {}; run `isocast stream key set` (it reads the key from \
             stdin), or MCP's set_stream_key",
            key_label(destination.settings.service, account)
        )),
        Ok(None) if canvas.is_main() => Err(format!(
            "no stream key is set for destination {} ({}); run `isocast stream key set \
             --destination {}`, or MCP's set_stream_key with destination",
            destination.id,
            destination.settings.service.id(),
            destination.id
        )),
        Ok(None) => Err(format!(
            "no stream key is set for the {} canvas's destination {} ({}; its keys are \
             its own, not the main canvas's); run `isocast stream key set{flag} --destination \
             {}`, or MCP's set_stream_key with canvas \"{canvas}\"",
            canvas,
            destination.id,
            destination.settings.service.id(),
            destination.id
        )),
        Err(err) => Err(format!("{err:#}")),
    }
}

/// Losses warned about per destination, kept in the daemon's state.
pub(crate) type ReportedMap = HashMap<String, Reported>;
