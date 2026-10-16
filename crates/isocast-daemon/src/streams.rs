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

use std::collections::HashMap;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use isocast_engine::capacity;
use isocast_engine::stream::{KEY_ENV, OnChange, Stream};
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

/// "the stream", or "the stream to youtube" when the setup has more than
/// one destination.
fn label(state: &State, id: &str) -> String {
    let several = stream_settings(state).is_some_and(|s| !s.destinations.is_empty())
        || state.streams.len() > 1;
    if several {
        format!("the stream to {id}")
    } else {
        "the stream".into()
    }
}

/// Every destination's status, the main one first: the running (or ended)
/// stream's own, else the live setup's destination, offline. A stream
/// still running for a destination the setup no longer has comes last.
pub(crate) fn stream_statuses(state: &State) -> Vec<StreamStatus> {
    let mut out = Vec::new();
    let from_env = std::env::var(KEY_ENV).is_ok_and(|key| !key.trim().is_empty());
    if let Some(settings) = stream_settings(state) {
        for destination in settings.resolve() {
            let known = state
                .streams
                .iter()
                .find(|(id, _)| *id == destination.id)
                .map(|(_, output)| output.status());
            if let Some(mut status) = known.clone().filter(StreamStatus::is_active) {
                status.enabled = destination.enabled;
                out.push(status);
                continue;
            }
            // An ended stream keeps its state, error and counts, but the
            // service, server and key are the setup's: what a new stream
            // would use (#59).
            let account = &destination.key_account;
            let key_set = from_env || has_stream_key(account).unwrap_or(false);
            let offline = StreamStatus::offline_destination(&destination, key_set);
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
    for (id, output) in &state.streams {
        if !out.iter().any(|status| status.id == *id) {
            out.push(output.status());
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
}

impl Daemon {
    /// Start streaming the live engine's program to the live setup's
    /// enabled destinations (or `only` those), starting the engine if
    /// needed. Destinations already streaming are left alone.
    pub(crate) fn stream_start(
        self: &Arc<Self>,
        params: StreamStartParams,
    ) -> Result<Streaming, RpcError> {
        let mut state = self.state.lock().unwrap();
        let settings = stream_settings(&state).ok_or_else(|| {
            wrong(
                "the live setup has no stream; add one (for example the patch \
                 {\"stream\": {\"service\": \"twitch\"}} with `isocast apply --patch` or MCP's \
                 apply_change, or `isocast stream destination add twitch`), then set the key \
                 (`isocast stream key set`, or MCP's set_stream_key)",
            )
        })?;
        let chosen = choose(&settings, &params.only)?;
        let active: Vec<String> = state
            .streams
            .iter()
            .filter(|(_, output)| output.is_active())
            .map(|(id, _)| id.clone())
            .collect();
        let starting: Vec<Destination> = chosen
            .into_iter()
            .filter(|d| !active.contains(&d.id))
            .collect();
        if starting.is_empty() {
            return Err(wrong(format!(
                "already streaming to {}; stop it first (`isocast stream stop`, or MCP's \
                 stop_stream)",
                active.join(", ")
            )));
        }
        // Every destination must be ready before any starts: nothing
        // half-starts on a setup problem.
        let mut keys = Vec::new();
        let mut problems = Vec::new();
        for destination in &starting {
            if let Err(err) = destination.settings.server() {
                problems.push(if destination.id == MAIN_DESTINATION {
                    err
                } else {
                    format!(
                        "stream.destinations ({}): {}",
                        destination.id,
                        err.trim_start_matches("stream.")
                    )
                });
                continue;
            }
            match destination_key(destination) {
                Ok(key) => keys.push(key),
                Err(message) => problems.push(message),
            }
        }
        if !problems.is_empty() {
            return Err(failed(problems.join("; ")));
        }
        self.check_stream_capacity(&state, &settings, &starting, &active)?;
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
        let mut state = self.state.lock().unwrap();
        state.streams_starting -= 1;
        let engine = engine?;
        let mut started = Vec::new();
        for (destination, key) in starting.into_iter().zip(keys) {
            let id = destination.id.clone();
            // Another start may have begun this one meanwhile.
            if state
                .streams
                .iter()
                .any(|(other, output)| *other == id && output.is_active())
            {
                continue;
            }
            // Its previous stream, finished, makes way.
            state.streams.retain(|(other, _)| *other != id);
            state.stream_reported.remove(&id);
            let daemon = Arc::downgrade(self);
            let on_change: OnChange = Box::new(move |status| {
                if let Some(daemon) = daemon.upgrade() {
                    daemon.publish(Event::Stream {
                        stream: status.clone(),
                    });
                }
            });
            let output = match engine.stream(&id, destination.settings.clone(), key, on_change) {
                Ok(stream) => Output::Running(stream),
                Err(err) => {
                    // One destination's encoder failing doesn't stop the
                    // others.
                    let mut status = StreamStatus::offline_destination(&destination, true);
                    status.state = StreamState::Failed;
                    status.last_error = Some(format!("{err:#}"));
                    self.publish(Event::Stream {
                        stream: status.clone(),
                    });
                    Output::Failed(Box::new(status))
                }
            };
            // In the state while it connects, so the engine stays up for
            // it, a second start leaves it alone and `stream stop` can
            // cancel it.
            state.streams.push((id.clone(), output));
            started.push(id);
        }
        // Main first, as the setup lists them.
        let order = settings.destination_ids();
        state.streams.sort_by_key(|(id, _)| {
            order
                .iter()
                .position(|other| other == id)
                .unwrap_or(usize::MAX)
        });
        drop(state);
        if started.is_empty() {
            return Err(wrong(
                "already streaming to every destination asked for; stop it first",
            ));
        }
        let deadline = Instant::now() + STREAM_START_TIMEOUT;
        let finals = loop {
            let state = self.state.lock().unwrap();
            let statuses: Vec<StreamStatus> = started
                .iter()
                .filter_map(|id| {
                    state
                        .streams
                        .iter()
                        .find(|(other, _)| other == id)
                        .map(|(_, output)| output.status())
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
        let destinations = stream_statuses(&self.state.lock().unwrap());
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
    /// format share an encode, so this only matters when one has its own
    /// bitrate or size.
    fn check_stream_capacity(
        &self,
        state: &State,
        settings: &StreamSettings,
        starting: &[Destination],
        active: &[String],
    ) -> Result<(), RpcError> {
        let Some(setup) = state.setup.as_ref() else {
            return Ok(());
        };
        // The destinations that will stream: the running ones and these.
        let mut candidate = setup.clone();
        let on = |id: &str| active.iter().any(|a| a == id) || starting.iter().any(|d| d.id == id);
        let mut stream = settings.clone();
        stream.enabled = on(MAIN_DESTINATION);
        for extra in &mut stream.destinations {
            extra.enabled = on(&extra.id);
        }
        let canvas = (setup.canvas.width, setup.canvas.height);
        let encodes = stream.encodes(canvas);
        if encodes.len() <= 1 {
            return Ok(());
        }
        candidate.stream = Some(stream);
        // Judged against what runs now: the recording's files only while
        // one records (starting one later warns on its own, #66).
        if state.session.is_none() {
            candidate.recording.program.enabled = false;
            candidate.recording.iso.enabled = false;
        }
        let memory = capacity::memory_path(state.file.legacy());
        let estimate = match crate::estimate_capacity(
            &candidate,
            state.engine.as_deref(),
            true,
            state.session.is_some(),
            &memory,
            None,
        ) {
            Ok(estimate) => estimate,
            // No estimate for this Mac: nothing to judge by.
            Err(_) => return Ok(()),
        };
        if estimate.capacity != Capacity::CannotKeepUp {
            return Ok(());
        }
        let own: Vec<String> = encodes[1..]
            .iter()
            .flat_map(|encode| encode.ids.clone())
            .filter(|id| starting.iter().any(|d| d.id == *id))
            .collect();
        let shared = &encodes[0];
        let one = own.len() == 1;
        Err(failed(format!(
            "not streaming: {} {} an H.264 encoder of {} own, as {} bitrate or size differs \
             from {}'s {}×{} at {} kb/s; with {} encodes at once{}, {}. Give {} the same \
             video_bitrate_kbps and size to share one encoder (`isocast stream destination set \
             ID --clear video_bitrate_kbps --clear size`), or start without {} (`isocast stream \
             start --only …`)",
            own.join(", "),
            if one { "needs" } else { "need" },
            if one { "its" } else { "their" },
            if one { "its" } else { "their" },
            shared.ids[0],
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
        let mut state = self.state.lock().unwrap();
        if !params.only.is_empty() {
            let known: Vec<String> = stream_settings(&state)
                .map(|s| s.destination_ids())
                .unwrap_or_default()
                .into_iter()
                .chain(state.streams.iter().map(|(id, _)| id.clone()))
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
        let wanted = |id: &str| params.only.is_empty() || params.only.iter().any(|o| o == id);
        let active: Vec<String> = state
            .streams
            .iter()
            .filter(|(id, output)| wanted(id) && output.is_active())
            .map(|(id, _)| id.clone())
            .collect();
        if active.is_empty() {
            // A stream that failed stays in the status until the next
            // start; say so rather than only "not streaming".
            let failures: Vec<String> = state
                .streams
                .iter()
                .filter(|(id, _)| wanted(id))
                .map(|(id, output)| (id, output.status()))
                .filter(|(_, status)| status.state == StreamState::Failed)
                .map(|(id, status)| {
                    let why = status.last_error.as_deref().unwrap_or("no reason given");
                    if state.streams.len() > 1 {
                        format!("{id}: {why}")
                    } else {
                        why.to_string()
                    }
                })
                .collect();
            let message = if failures.is_empty() {
                match params.only.as_slice() {
                    [] => "not streaming".to_string(),
                    only => format!("not streaming to {}", only.join(", ")),
                }
            } else {
                format!(
                    "not streaming: the stream already failed ({})",
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
                .collect()
        });
        let mut destinations = stream_statuses(&self.state.lock().unwrap());
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
        for (_, output) in &self.state.lock().unwrap().streams {
            if let Output::Running(stream) = output {
                stream.fail(why.to_string());
            }
        }
    }

    /// Warn about each destination's stream-only losses and sustained
    /// bitrate overshoot (once per episode), checked every second even
    /// when encoding/sending stalls.
    pub(crate) fn warn_stream_drops(&self) {
        let mut state = self.state.lock().unwrap();
        let statuses: Vec<StreamStatus> = state
            .streams
            .iter()
            .filter_map(|(_, output)| match output {
                Output::Running(stream) => Some(stream.status()),
                Output::Failed(_) => None,
            })
            .collect();
        let mut warnings = Vec::new();
        for status in statuses {
            let what = label(&state, &status.id);
            let reported = state.stream_reported.entry(status.id.clone()).or_default();
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
                warnings.push(if status.id == MAIN_DESTINATION {
                    warning.clone()
                } else {
                    format!("{}: {warning}", status.id)
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
        let (revision, mut setup) = self.live_setup()?;
        let id = match setup.stream.as_mut() {
            None => {
                if params
                    .id
                    .as_deref()
                    .is_some_and(|id| id != MAIN_DESTINATION)
                {
                    return Err(invalid(
                        "id: the live setup has no stream yet, so its first destination is the \
                         [stream] section itself, id \"main\"; leave the id out",
                    ));
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
                setup.stream = Some(stream);
                MAIN_DESTINATION.to_string()
            }
            Some(stream) => {
                if stream.destinations.len() + 1 >= MAX_DESTINATIONS {
                    return Err(failed(format!(
                        "the live setup already has {} destinations, the most it may have",
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
                });
                id
            }
        };
        self.apply_destinations(revision, setup, Some(id))
    }

    /// Remove destination `id` from the live setup (not while it streams).
    pub(crate) fn stream_destination_remove(
        &self,
        id: String,
    ) -> Result<StreamDestinations, RpcError> {
        let (revision, mut setup) = self.live_setup()?;
        let Some(stream) = setup.stream.as_mut() else {
            return Err(wrong("the live setup has no stream destinations"));
        };
        if id == MAIN_DESTINATION {
            return Err(invalid(
                "id: \"main\" is the [stream] section itself; turn it off instead (`isocast \
                 stream destination set main --enabled false`), or remove the whole stream with \
                 the patch {\"stream\": null}",
            ));
        }
        let Some(index) = stream.destinations.iter().position(|d| d.id == id) else {
            return Err(invalid(format!(
                "id: no destination {id:?}; destinations: {}",
                stream.destination_ids().join(", ")
            )));
        };
        if self
            .state
            .lock()
            .unwrap()
            .streams
            .iter()
            .any(|(other, output)| *other == id && output.is_active())
        {
            return Err(wrong(format!(
                "streaming to {id}; stop it first (`isocast stream stop --only {id}`)"
            )));
        }
        stream.destinations.remove(index);
        let removed = self.apply_destinations(revision, setup, None)?;
        // Its ended stream goes with it: no ghost in status or `stream stop`.
        let mut state = self.state.lock().unwrap();
        state
            .streams
            .retain(|(other, output)| *other != id || output.is_active());
        state.stream_reported.remove(&id);
        let destinations = stream_statuses(&state);
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
        let (revision, mut setup) = self.live_setup()?;
        let Some(stream) = setup.stream.as_mut() else {
            return Err(wrong(
                "the live setup has no stream; add a destination first (`isocast stream \
                 destination add SERVICE`)",
            ));
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
        }
        self.apply_destinations(revision, setup, Some(id))
    }

    /// The live setup and its revision, to change.
    fn live_setup(&self) -> Result<(u64, isocast_protocol::Setup), RpcError> {
        let state = self.state.lock().unwrap();
        match &state.setup {
            Some(setup) => Ok((state.revision, setup.clone())),
            None => Err(state.no_setup("no setup to add a stream destination to; make one first")),
        }
    }

    fn apply_destinations(
        &self,
        revision: u64,
        setup: isocast_protocol::Setup,
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
            destinations: stream_statuses(&self.state.lock().unwrap()),
        })
    }
}

/// The destinations a start means: `only` (each must exist; a turned-off
/// one starts too), else every enabled one.
fn choose(settings: &StreamSettings, only: &[String]) -> Result<Vec<Destination>, RpcError> {
    let all = settings.resolve();
    if only.is_empty() {
        let enabled: Vec<Destination> = all.into_iter().filter(|d| d.enabled).collect();
        if enabled.is_empty() {
            return Err(wrong(format!(
                "every stream destination is turned off; turn one on (`isocast stream \
                 destination set ID --enabled true`) or name it (`isocast stream start --only \
                 ID`); destinations: {}",
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
fn destination_key(destination: &Destination) -> Result<String, String> {
    if let Ok(key) = std::env::var(KEY_ENV)
        && !key.trim().is_empty()
    {
        return Ok(key.trim().to_string());
    }
    let account = &destination.key_account;
    match stream_key(account) {
        Ok(Some(key)) => Ok(key),
        Ok(None) if destination.id == MAIN_DESTINATION => Err(format!(
            "no stream key is set for {}; run `isocast stream key set` (it reads the key from \
             stdin), or MCP's set_stream_key",
            key_label(destination.settings.service, account)
        )),
        Ok(None) => Err(format!(
            "no stream key is set for destination {} ({}); run `isocast stream key set \
             --destination {}`, or MCP's set_stream_key with destination",
            destination.id,
            destination.settings.service.id(),
            destination.id
        )),
        Err(err) => Err(format!("{err:#}")),
    }
}

/// Losses warned about per destination, kept in the daemon's state.
pub(crate) type ReportedMap = HashMap<String, Reported>;
