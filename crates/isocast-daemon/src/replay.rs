//! The replay buffer's methods (G.2): `replay_start`, `replay_stop`,
//! `replay_save` and `replay_status`. The buffer itself is in
//! `isocast_engine::replay`. Each canvas has its own buffer (#96): the
//! main one's unless the params say `canvas: vertical`.

use super::*;
use isocast_engine::replay::{OnChange as OnReplayChange, ReplayBuffer};
use isocast_protocol::replay::{ReplaySaved, ReplayState, ReplayStatus};
use std::collections::BTreeMap;

fn wrong(message: impl Into<String>) -> RpcError {
    RpcError {
        code: RpcError::WRONG_STATE,
        message: message.into(),
    }
}

/// How messages name `canvas`'s buffer.
fn what(canvas: CanvasId) -> String {
    match canvas {
        CanvasId::Main => "the replay buffer".into(),
        CanvasId::Vertical => "the vertical replay buffer".into(),
        other => format!("canvas {other:?}'s replay buffer"),
    }
}

/// The CLI flag that picks `canvas`, and MCP's argument.
fn flag(canvas: CanvasId) -> String {
    match canvas {
        CanvasId::Main => String::new(),
        other => format!(" --canvas {other}"),
    }
}

fn mcp_arg(canvas: CanvasId) -> String {
    match canvas {
        CanvasId::Main => String::new(),
        other => format!(" with canvas \"{other}\""),
    }
}

/// Why `canvas`'s buffer can't start: the live setup doesn't have it.
fn no_canvas(setup: &Setup, canvas: CanvasId) -> String {
    match canvas {
        CanvasId::Vertical => "canvas: the live setup has no vertical canvas; add one with the \
             patch {\"vertical\": {}} (`isocast apply --patch`, or MCP's apply_change), then \
             start its replay buffer"
            .into(),
        other => setup.unknown_canvas("canvas", other),
    }
}

/// An empty slot, for canvases whose buffer never ran.
static NO_SLOT: ReplaySlot = ReplaySlot {
    buffer: None,
    starting: false,
    drops_reported: 0,
};

/// One canvas's replay buffer: the running one, or the last (kept for its
/// final status).
#[derive(Default)]
pub(super) struct ReplaySlot {
    pub(super) buffer: Option<Arc<ReplayBuffer>>,
    /// A start is getting the engine ready (outside the state lock).
    pub(super) starting: bool,
    /// Losses already warned about.
    pub(super) drops_reported: u64,
}

impl ReplaySlot {
    fn active(&self) -> bool {
        self.starting || self.buffer.as_ref().is_some_and(|b| b.is_running())
    }
}

/// The daemon's replay buffers, one per canvas.
#[derive(Default)]
pub(super) struct ReplayOutput {
    main: ReplaySlot,
    /// The other canvases' (`[vertical]`, `[[canvases]]`), by canvas.
    extras: BTreeMap<CanvasId, ReplaySlot>,
}

impl ReplayOutput {
    fn slot(&self, canvas: CanvasId) -> &ReplaySlot {
        match canvas {
            CanvasId::Main => &self.main,
            other => self.extras.get(&other).unwrap_or(&NO_SLOT),
        }
    }

    fn slot_mut(&mut self, canvas: CanvasId) -> &mut ReplaySlot {
        match canvas {
            CanvasId::Main => &mut self.main,
            other => self.extras.entry(other).or_default(),
        }
    }

    /// A buffer (of any canvas) keeps its program now (or is about to),
    /// so the engine is needed.
    pub(super) fn active(&self) -> bool {
        self.main.active() || self.extras.values().any(ReplaySlot::active)
    }

    /// `canvas`'s buffer keeps its program now (or is about to).
    pub(super) fn active_on(&self, canvas: CanvasId) -> bool {
        self.slot(canvas).active()
    }

    /// How to stop the running buffers, for messages refusing something
    /// while they run.
    pub(super) fn stop_hint(&self) -> String {
        let running: Vec<_> = std::iter::once(CanvasId::Main)
            .chain(self.extras.keys().copied())
            .filter(|&canvas| self.active_on(canvas))
            .collect();
        let cli: Vec<_> = running
            .iter()
            .map(|&canvas| format!("`isocast replay stop{}`", flag(canvas)))
            .collect();
        let mcp: Vec<_> = running
            .iter()
            .map(|&canvas| format!("MCP's stop_replay{}", mcp_arg(canvas)))
            .collect();
        format!("{}, or {}", cli.join(" and "), mcp.join(" and "))
    }

    /// The main canvas's buffer's status.
    pub(super) fn status_for(&self, setup: Option<&Setup>) -> ReplayStatus {
        self.status_on(CanvasId::Main, setup)
    }

    pub(super) fn status_on(&self, canvas: CanvasId, setup: Option<&Setup>) -> ReplayStatus {
        match &self.slot(canvas).buffer {
            Some(buffer) => buffer.status(),
            None => ReplayStatus {
                canvas,
                state: ReplayState::Off,
                seconds: setup.map_or(30, |setup| setup.recording.replay.seconds),
                max_memory_bytes: setup.map_or(0, |setup| {
                    u64::from(setup.recording.replay.max_memory_mb) << 20
                }),
                ..Default::default()
            },
        }
    }

    /// The vertical canvas's buffer's status, while the setup has that
    /// canvas or its buffer ran.
    pub(super) fn vertical_status(&self, setup: Option<&Setup>) -> Option<ReplayStatus> {
        self.canvas_status(CanvasId::Vertical, setup)
    }

    /// Extra canvas `canvas`'s buffer's status, while the setup has that
    /// canvas or its buffer ran.
    pub(super) fn canvas_status(
        &self,
        canvas: CanvasId,
        setup: Option<&Setup>,
    ) -> Option<ReplayStatus> {
        let has = setup.is_some_and(|setup| setup.has_canvas(canvas));
        (has || self.slot(canvas).buffer.is_some()).then(|| self.status_on(canvas, setup))
    }
}

impl Daemon {
    pub(super) fn replay_status(&self, canvas: CanvasId) -> ReplayStatus {
        let state = self.state.read();
        state.replay.status_on(canvas, state.setup.as_ref())
    }

    pub(super) fn replay_start(
        self: &Arc<Self>,
        canvas: CanvasId,
    ) -> Result<ReplayStatus, RpcError> {
        {
            let mut state = self.state.lock();
            if state.replay.active_on(canvas) {
                return Err(wrong(format!(
                    "{} is already running; save it (`isocast replay save{}`, or MCP's \
                     save_replay{}) or stop it first",
                    what(canvas),
                    flag(canvas),
                    mcp_arg(canvas)
                )));
            }
            if state.setup.is_none() {
                return Err(state.no_setup("no program to keep"));
            }
            if let Some(setup) = state.setup.as_ref()
                && !setup.has_canvas(canvas)
            {
                return Err(wrong(no_canvas(setup, canvas)));
            }
            if state.tuning {
                return Err(wrong("wait for tune to finish"));
            }
            // Until the buffer is in the state, this keeps the engine from
            // idling out and a second start out.
            state.replay.slot_mut(canvas).starting = true;
        }
        let engine = self.ensure_engine();
        let mut state = self.state.lock();
        state.replay.slot_mut(canvas).starting = false;
        let engine = engine?;
        let Some(setup) = state.setup.clone() else {
            return Err(state.no_setup("no program to keep"));
        };
        let stream = match canvas {
            CanvasId::Main => setup.stream.as_ref(),
            other => {
                let Some(vertical) = setup.extra_canvas(other) else {
                    return Err(wrong(no_canvas(&setup, other)));
                };
                // A canvas whose compositor failed has no picture to keep.
                // Checked under the lock that adds the buffer, so a
                // failure after this still ends it (#96).
                if let Some(reason) = engine.canvas_stopped(canvas) {
                    let recovery = if reason.contains("scene state is broken")
                        || reason.contains("scene worker has broken state")
                    {
                        "restart the engine once recording and streaming stop"
                    } else {
                        "after recording stops, turn the canvas off and on again \
                         (Settings › Video), or restart the engine"
                    };
                    let whose = match canvas {
                        CanvasId::Vertical => "the vertical canvas".to_string(),
                        other => format!("canvas {other:?}"),
                    };
                    return Err(wrong(format!(
                        "{whose} stopped ({reason}), so its replay buffer can't start; {recovery}"
                    )));
                }
                vertical.stream.as_ref()
            }
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
            .replay(canvas, &setup.recording.replay, stream, on_change)
            .map_err(failed)?;
        let buffer = Arc::new(buffer);
        let slot = state.replay.slot_mut(canvas);
        slot.buffer = Some(buffer.clone());
        slot.drops_reported = 0;
        drop(state);
        Ok(buffer.status())
    }

    pub(super) fn replay_stop(&self, canvas: CanvasId) -> Result<ReplayStatus, RpcError> {
        let buffer = {
            let state = self.state.read();
            match &state.replay.slot(canvas).buffer {
                Some(buffer) if buffer.is_running() => buffer.clone(),
                Some(buffer) => {
                    let status = buffer.status();
                    return Err(wrong(match status.last_error {
                        Some(error) => {
                            format!("{} isn't running: it stopped ({error})", what(canvas))
                        }
                        None => format!("{} isn't running", what(canvas)),
                    }));
                }
                None => return Err(wrong(format!("{} isn't running", what(canvas)))),
            }
        };
        // Stopped outside the lock: the last output waits for the encoder.
        let status = buffer.stop();
        self.publish(Event::Replay {
            replay: status.clone(),
        });
        Ok(status)
    }

    /// Stop every canvas's buffer that runs (the engine is stopping).
    pub(super) fn replay_stop_all(&self) {
        for canvas in [CanvasId::Main, CanvasId::Vertical] {
            let _ = self.replay_stop(canvas);
        }
    }

    /// End `canvas`'s buffer with a failure, for example when only that
    /// canvas's compositor failed (#96).
    pub(super) fn fail_canvas_replay(&self, canvas: CanvasId, why: &str) {
        let buffer = self.state.read().replay.slot(canvas).buffer.clone();
        if let Some(buffer) = buffer {
            buffer.fail(why.to_string());
        }
    }

    pub(super) fn replay_save(&self, canvas: CanvasId) -> Result<ReplaySaved, RpcError> {
        let (buffer, folder, container, keep_free) = {
            let state = self.state.read();
            let buffer = match &state.replay.slot(canvas).buffer {
                Some(buffer) if buffer.is_running() => buffer.clone(),
                _ => {
                    return Err(wrong(format!(
                        "{} isn't running; start it first (`isocast replay start{}`, or MCP's \
                         start_replay{}), then save",
                        what(canvas),
                        flag(canvas),
                        mcp_arg(canvas)
                    )));
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
        // For a network caller, where it saves (the running recording's
        // project, or the recordings folder) is inside the folder roots,
        // checked right before writing and used as checked (decision 26).
        let folder = self.checked_path(
            "folder",
            &folder.to_string_lossy(),
            crate::roots::Access::Write,
        )?;
        // The vertical canvas's replays end in "vertical", as its
        // recordings do.
        let name = format!(
            "Replay {}{}",
            chrono::Local::now().format("%Y-%m-%d %H-%M-%S"),
            match canvas {
                CanvasId::Main => String::new(),
                other => format!(" {other}"),
            }
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

    /// Warn once per new loss, like the stream's, for each canvas's buffer.
    pub(super) fn warn_replay_drops(&self) {
        for canvas in [CanvasId::Main, CanvasId::Vertical] {
            let mut state = self.state.lock();
            let slot = state.replay.slot_mut(canvas);
            let Some(status) = slot.buffer.as_ref().map(|b| b.status()) else {
                continue;
            };
            if status.state != ReplayState::Running || status.dropped_frames <= slot.drops_reported
            {
                continue;
            }
            let new = status.dropped_frames - slot.drops_reported;
            slot.drops_reported = status.dropped_frames;
            drop(state);
            let unaffected = match canvas {
                CanvasId::Main => "The program, recordings and stream are unaffected",
                _ => "The programs, recordings and streams are unaffected",
            };
            self.publish(Event::Warning {
                message: format!(
                    "{} missed {new} frame(s) ({} so far); saved replays show the picture before \
                     longer there. {unaffected}",
                    what(canvas),
                    status.dropped_frames
                ),
            });
        }
    }
}
