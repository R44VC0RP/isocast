//! The replay buffer's methods (G.2): `replay_start`, `replay_stop`,
//! `replay_save` and `replay_status`. The buffer itself is in
//! `isocast_engine::replay`.

use super::*;
use isocast_engine::replay::{OnChange as OnReplayChange, ReplayBuffer};
use isocast_protocol::replay::{ReplaySaved, ReplayState, ReplayStatus};

fn wrong(message: impl Into<String>) -> RpcError {
    RpcError {
        code: RpcError::WRONG_STATE,
        message: message.into(),
    }
}

/// The daemon's replay buffer: the running one, or the last (kept for
/// its final status).
#[derive(Default)]
pub(super) struct ReplayOutput {
    pub(super) buffer: Option<Arc<ReplayBuffer>>,
    /// A start is getting the engine ready (outside the state lock).
    pub(super) starting: bool,
    /// Losses already warned about.
    pub(super) drops_reported: u64,
}

impl ReplayOutput {
    /// It keeps the program now (or is about to), so the engine is needed.
    pub(super) fn active(&self) -> bool {
        self.starting || self.buffer.as_ref().is_some_and(|b| b.is_running())
    }

    pub(super) fn status_for(&self, setup: Option<&Setup>) -> ReplayStatus {
        match &self.buffer {
            Some(buffer) => buffer.status(),
            None => ReplayStatus {
                state: ReplayState::Off,
                seconds: setup.map_or(30, |setup| setup.recording.replay.seconds),
                max_memory_bytes: setup.map_or(0, |setup| {
                    u64::from(setup.recording.replay.max_memory_mb) << 20
                }),
                ..Default::default()
            },
        }
    }
}

impl Daemon {
    pub(super) fn replay_status(&self) -> ReplayStatus {
        let state = self.state.lock().unwrap();
        state.replay.status_for(state.setup.as_ref())
    }

    pub(super) fn replay_start(self: &Arc<Self>) -> Result<ReplayStatus, RpcError> {
        {
            let mut state = self.state.lock().unwrap();
            if state.replay.active() {
                return Err(wrong(
                    "the replay buffer is already running; save it (`isocast replay save`, or \
                     MCP's save_replay) or stop it first",
                ));
            }
            if state.setup.is_none() {
                return Err(state.no_setup("no program to keep"));
            }
            if state.tuning {
                return Err(wrong("wait for tune to finish"));
            }
            // Until the buffer is in the state, this keeps the engine from
            // idling out and a second start out.
            state.replay.starting = true;
        }
        let engine = self.ensure_engine();
        let mut state = self.state.lock().unwrap();
        state.replay.starting = false;
        let engine = engine?;
        let Some(setup) = state.setup.clone() else {
            return Err(state.no_setup("no program to keep"));
        };
        let daemon = Arc::downgrade(self);
        let on_change: OnReplayChange = Arc::new(move |replay| {
            if let Some(daemon) = daemon.upgrade() {
                daemon.publish(Event::Replay {
                    replay: replay.clone(),
                });
            }
        });
        // Under the lock, as a stream starts: a stream starting meanwhile
        // finds this buffer's encoder (or this the stream's).
        let buffer = engine
            .replay(&setup.recording.replay, setup.stream.as_ref(), on_change)
            .map_err(failed)?;
        let buffer = Arc::new(buffer);
        state.replay.buffer = Some(buffer.clone());
        state.replay.drops_reported = 0;
        drop(state);
        Ok(buffer.status())
    }

    pub(super) fn replay_stop(&self) -> Result<ReplayStatus, RpcError> {
        let buffer = {
            let state = self.state.lock().unwrap();
            match &state.replay.buffer {
                Some(buffer) if buffer.is_running() => buffer.clone(),
                Some(buffer) => {
                    let status = buffer.status();
                    return Err(wrong(match status.last_error {
                        Some(error) => {
                            format!("the replay buffer isn't running: it stopped ({error})")
                        }
                        None => "the replay buffer isn't running".into(),
                    }));
                }
                None => return Err(wrong("the replay buffer isn't running")),
            }
        };
        // Stopped outside the lock: the last output waits for the encoder.
        let status = buffer.stop();
        self.publish(Event::Replay {
            replay: status.clone(),
        });
        Ok(status)
    }

    pub(super) fn replay_save(&self) -> Result<ReplaySaved, RpcError> {
        let (buffer, folder, container, keep_free) = {
            let state = self.state.lock().unwrap();
            let buffer = match &state.replay.buffer {
                Some(buffer) if buffer.is_running() => buffer.clone(),
                _ => {
                    return Err(wrong(
                        "the replay buffer isn't running; start it first (`isocast replay \
                         start`, or MCP's start_replay), then save",
                    ));
                }
            };
            let setup = state
                .setup
                .as_ref()
                .ok_or_else(|| state.no_setup("nowhere to save"))?;
            // Into the running recording's project, as OBS saves replays
            // beside recordings; else the recordings folder.
            let folder = state
                .session
                .as_ref()
                .and_then(|session| session.project.as_ref())
                .map(|project| project.folder.clone())
                .unwrap_or_else(|| expand_home(&setup.recording.folder));
            (
                buffer,
                folder,
                setup.recording.program.encoder.container,
                (setup.recording.keep_free_gb * 1e9) as u64,
            )
        };
        if let Some(problem) = relative_folder(&folder.to_string_lossy()) {
            return Err(wrong(format!("recording.folder: {problem}")));
        }
        let name = format!(
            "Replay {}",
            chrono::Local::now().format("%Y-%m-%d %H-%M-%S")
        );
        let room = |bytes: u64| -> Result<()> {
            if let Some(free) = free_bytes(&folder)
                && free < bytes.saturating_add(keep_free)
            {
                bail!(
                    "not enough free space in {} for the replay ({:.0} MB, keeping \
                     recording.keep_free_gb free)",
                    folder.display(),
                    bytes as f64 / 1e6
                );
            }
            Ok(())
        };
        let saved = buffer
            .save(&folder, &name, container, &room)
            .map_err(failed)?;
        self.publish(Event::ReplaySaved {
            saved: saved.clone(),
        });
        Ok(saved)
    }

    /// Warn once per new loss, like the stream's.
    pub(super) fn warn_replay_drops(&self) {
        let mut state = self.state.lock().unwrap();
        let Some(status) = state.replay.buffer.as_ref().map(|b| b.status()) else {
            return;
        };
        if status.state != ReplayState::Running
            || status.dropped_frames <= state.replay.drops_reported
        {
            return;
        }
        let new = status.dropped_frames - state.replay.drops_reported;
        state.replay.drops_reported = status.dropped_frames;
        drop(state);
        self.publish(Event::Warning {
            message: format!(
                "the replay buffer missed {new} frame(s) ({} so far); saved replays show the \
                 picture before longer there. The program, recordings and stream are unaffected",
                status.dropped_frames
            ),
        });
    }
}
